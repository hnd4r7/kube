#[macro_use] extern crate kube_derive;
#[macro_use] extern crate serde_derive;
use k8s_openapi::Resource;
use kube::api::ObjectMeta;


#[derive(CustomResource, Serialize, Deserialize, Debug, Clone)]
#[kube(group = "clux.dev", version = "v1", kind = "Foo", namespaced)]
#[kube(status = "FooStatus")]
#[kube(scale = r#"{"specReplicasPath":".spec.replicas", "statusReplicasPath":".status.replicas"}"#)]
#[kube(
    printcolumn = r#"{"name":"Spec", "type":"string", "description":"name of foo", "jsonPath":".spec.name"}"#
)]
pub struct MyFoo {
    name: String,
    info: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FooStatus {
    is_bad: bool,
}

fn main() {
    println!("Kind {}", Foo::KIND);
    let foo = Foo {
        metadata: ObjectMeta::default(),
        spec: MyFoo {
            name: "hi".into(),
            info: None,
        },
        status: Some(FooStatus { is_bad: true }),
    };
    println!("Spec: {:?}", foo.spec);
    println!("Foo CRD: {:?}", Foo::crd());
}

#[test]
fn verify_resource() {
    assert_eq!(Foo::KIND, "Foo");
    assert_eq!(Foo::GROUP, "clux.dev");
    assert_eq!(Foo::VERSION, "v1");
    assert_eq!(Foo::API_VERSION, "clux.dev/v1");
}

// Verify Foo::crd
#[test]
fn verify_crd() {
    use serde_json::{self, json};
    let crd = Foo::crd();
    let output = json!({
      "apiVersion": "apiextensions.k8s.io/v1",
      "kind": "CustomResourceDefinition",
      "metadata": {
        "name": "foos.clux.dev"
      },
      "spec": {
        "group": "clux.dev",
        "names": {
          "kind": "Foo",
          "plural": "foos",
          "shortNames": [],
          "singular": "foo"
        },
        "scope": "Namespaced",
        "versions": [
          {
            "additionalPrinterColumns": [
              {
                "description": "name of foo",
                "jsonPath": ".spec.name",
                "name": "Spec",
                "type": "string"
              }
            ],
            "name": "v1",
            "served": true,
            "storage": true
          }
        ]
      }
    });
    let outputcrd = serde_json::from_value(output).expect("expected output is valid");
    assert_eq!(crd, outputcrd);
}
