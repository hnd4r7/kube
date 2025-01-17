use super::ObjectRef;
use crate::{
    utils::delayed_init::{self, DelayedInit},
    watcher,
};
use ahash::AHashMap;
use derivative::Derivative;
use kube_client::Resource;
use parking_lot::RwLock;
use std::{fmt::Debug, hash::Hash, sync::Arc};
use thiserror::Error;

type Cache<K> = Arc<RwLock<AHashMap<ObjectRef<K>, Arc<K>>>>;

/// A writable Store handle
///
/// This is exclusive since it's not safe to share a single `Store` between multiple reflectors.
/// In particular, `Restarted` events will clobber the state of other connected reflectors.
#[derive(Debug)]
pub struct Writer<K: 'static + Resource>
where
    K::DynamicType: Eq + Hash,
{
    store: Cache<K>,
    dyntype: K::DynamicType,
    ready_tx: Option<delayed_init::Initializer<()>>,
    ready_rx: Arc<DelayedInit<()>>,
}

impl<K: 'static + Resource + Clone> Writer<K>
where
    K::DynamicType: Eq + Hash + Clone,
{
    /// Creates a new Writer with the specified dynamic type.
    ///
    /// If the dynamic type is default-able (for example when writer is used with
    /// `k8s_openapi` types) you can use `Default` instead.
    pub fn new(dyntype: K::DynamicType) -> Self {
        let (ready_tx, ready_rx) = DelayedInit::new();
        Writer {
            store: Default::default(),
            dyntype,
            ready_tx: Some(ready_tx),
            ready_rx: Arc::new(ready_rx),
        }
    }

    /// Return a read handle to the store
    ///
    /// Multiple read handles may be obtained, by either calling `as_reader` multiple times,
    /// or by calling `Store::clone()` afterwards.
    #[must_use]
    pub fn as_reader(&self) -> Store<K> {
        Store {
            store: self.store.clone(),
            ready_rx: self.ready_rx.clone(),
        }
    }

    /// Applies a single watcher event to the store
    pub fn apply_watcher_event(&mut self, event: &watcher::Event<K>) {
        match event {
            watcher::Event::Applied(obj) => {
                let key = ObjectRef::from_obj_with(obj, self.dyntype.clone());
                let obj = Arc::new(obj.clone());
                self.store.write().insert(key, obj);
            }
            watcher::Event::Deleted(obj) => {
                let key = ObjectRef::from_obj_with(obj, self.dyntype.clone());
                self.store.write().remove(&key);
            }
            watcher::Event::Restarted(new_objs) => {
                let new_objs = new_objs
                    .iter()
                    .map(|obj| {
                        (
                            ObjectRef::from_obj_with(obj, self.dyntype.clone()),
                            Arc::new(obj.clone()),
                        )
                    })
                    .collect::<AHashMap<_, _>>();
                *self.store.write() = new_objs;
            }
        }

        // Mark as ready after the first event, "releasing" any calls to Store::wait_until_ready()
        if let Some(ready_tx) = self.ready_tx.take() {
            ready_tx.init(())
        }
    }
}
impl<K> Default for Writer<K>
where
    K: Resource + Clone + 'static,
    K::DynamicType: Default + Eq + Hash + Clone,
{
    fn default() -> Self {
        Self::new(K::DynamicType::default())
    }
}

/// A readable cache of Kubernetes objects of kind `K`
///
/// Cloning will produce a new reference to the same backing store.
///
/// Cannot be constructed directly since one writer handle is required,
/// use `Writer::as_reader()` instead.
#[derive(Derivative)]
#[derivative(Debug(bound = "K: Debug, K::DynamicType: Debug"), Clone)]
pub struct Store<K: 'static + Resource>
where
    K::DynamicType: Hash + Eq,
{
    store: Cache<K>,
    ready_rx: Arc<DelayedInit<()>>,
}

#[derive(Debug, Error)]
#[error("writer was dropped before store became ready")]
pub struct WriterDropped(delayed_init::InitDropped);

impl<K: 'static + Clone + Resource> Store<K>
where
    K::DynamicType: Eq + Hash + Clone,
{
    /// Wait for the store to be populated by Kubernetes.
    ///
    /// Note that this will _not_ await the source calling the associated [`Writer`] (such as the [`reflector`]).
    ///
    /// # Errors
    /// Returns an error if the [`Writer`] was dropped before any value was written.
    pub async fn wait_until_ready(&self) -> Result<(), WriterDropped> {
        self.ready_rx.get().await.map_err(WriterDropped)
    }

    /// Retrieve a `clone()` of the entry referred to by `key`, if it is in the cache.
    ///
    /// `key.namespace` is ignored for cluster-scoped resources.
    ///
    /// Note that this is a cache and may be stale. Deleted objects may still exist in the cache
    /// despite having been deleted in the cluster, and new objects may not yet exist in the cache.
    /// If any of these are a problem for you then you should abort your reconciler and retry later.
    /// If you use `kube_rt::controller` then you can do this by returning an error and specifying a
    /// reasonable `error_policy`.
    #[must_use]
    pub fn get(&self, key: &ObjectRef<K>) -> Option<Arc<K>> {
        let store = self.store.read();
        store
            .get(key)
            // Try to erase the namespace and try again, in case the object is cluster-scoped
            .or_else(|| {
                store.get(&{
                    let mut cluster_key = key.clone();
                    cluster_key.namespace = None;
                    cluster_key
                })
            })
            // Clone to let go of the entry lock ASAP
            .cloned()
    }

    /// Return a full snapshot of the current values
    #[must_use]
    pub fn state(&self) -> Vec<Arc<K>> {
        let s = self.store.read();
        s.values().cloned().collect()
    }

    /// Retrieve a `clone()` of the entry found by the given predicate
    #[must_use]
    pub fn find<P>(&self, predicate: P) -> Option<Arc<K>>
    where
        P: Fn(&K) -> bool,
    {
        self.store
            .read()
            .iter()
            .map(|(_, k)| k)
            .find(|k| predicate(k.as_ref()))
            .cloned()
    }

    /// Return the number of elements in the store
    #[must_use]
    pub fn len(&self) -> usize {
        self.store.read().len()
    }

    /// Return whether the store is empty
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.store.read().is_empty()
    }
}

/// Create a (Reader, Writer) for a `Store<K>` for a typed resource `K`
///
/// The `Writer` should be passed to a [`reflector`](crate::reflector()),
/// and the [`Store`] is a read-only handle.
#[must_use]
pub fn store<K>() -> (Store<K>, Writer<K>)
where
    K: Resource + Clone + 'static,
    K::DynamicType: Eq + Hash + Clone + Default,
{
    let w = Writer::<K>::default();
    let r = w.as_reader();
    (r, w)
}

#[cfg(test)]
mod tests {
    use super::{store, Writer};
    use crate::{reflector::ObjectRef, watcher};
    use k8s_openapi::api::core::v1::ConfigMap;
    use kube_client::api::ObjectMeta;

    #[test]
    fn should_allow_getting_namespaced_object_by_namespaced_ref() {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some("obj".to_string()),
                namespace: Some("ns".to_string()),
                ..ObjectMeta::default()
            },
            ..ConfigMap::default()
        };
        let mut store_w = Writer::default();
        store_w.apply_watcher_event(&watcher::Event::Applied(cm.clone()));
        let store = store_w.as_reader();
        assert_eq!(store.get(&ObjectRef::from_obj(&cm)).as_deref(), Some(&cm));
    }

    #[test]
    fn should_not_allow_getting_namespaced_object_by_clusterscoped_ref() {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some("obj".to_string()),
                namespace: Some("ns".to_string()),
                ..ObjectMeta::default()
            },
            ..ConfigMap::default()
        };
        let mut cluster_cm = cm.clone();
        cluster_cm.metadata.namespace = None;
        let mut store_w = Writer::default();
        store_w.apply_watcher_event(&watcher::Event::Applied(cm));
        let store = store_w.as_reader();
        assert_eq!(store.get(&ObjectRef::from_obj(&cluster_cm)), None);
    }

    #[test]
    fn should_allow_getting_clusterscoped_object_by_clusterscoped_ref() {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some("obj".to_string()),
                namespace: None,
                ..ObjectMeta::default()
            },
            ..ConfigMap::default()
        };
        let (store, mut writer) = store();
        writer.apply_watcher_event(&watcher::Event::Applied(cm.clone()));
        assert_eq!(store.get(&ObjectRef::from_obj(&cm)).as_deref(), Some(&cm));
    }

    #[test]
    fn should_allow_getting_clusterscoped_object_by_namespaced_ref() {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some("obj".to_string()),
                namespace: None,
                ..ObjectMeta::default()
            },
            ..ConfigMap::default()
        };
        #[allow(clippy::redundant_clone)] // false positive
        let mut nsed_cm = cm.clone();
        nsed_cm.metadata.namespace = Some("ns".to_string());
        let mut store_w = Writer::default();
        store_w.apply_watcher_event(&watcher::Event::Applied(cm.clone()));
        let store = store_w.as_reader();
        assert_eq!(store.get(&ObjectRef::from_obj(&nsed_cm)).as_deref(), Some(&cm));
    }

    #[test]
    fn find_element_in_store() {
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some("obj".to_string()),
                namespace: None,
                ..ObjectMeta::default()
            },
            ..ConfigMap::default()
        };
        let mut target_cm = cm.clone();

        let (reader, mut writer) = store::<ConfigMap>();
        assert!(reader.is_empty());
        writer.apply_watcher_event(&watcher::Event::Applied(cm));

        assert_eq!(reader.len(), 1);
        assert!(reader.find(|k| k.metadata.generation == Some(1234)).is_none());

        target_cm.metadata.name = Some("obj1".to_string());
        target_cm.metadata.generation = Some(1234);
        writer.apply_watcher_event(&watcher::Event::Applied(target_cm.clone()));
        assert!(!reader.is_empty());
        assert_eq!(reader.len(), 2);
        let found = reader.find(|k| k.metadata.generation == Some(1234));
        assert_eq!(found.as_deref(), Some(&target_cm));
    }
}
