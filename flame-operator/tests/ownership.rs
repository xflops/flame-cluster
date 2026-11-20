use flame_operator::cluster::{self, Owner};
use http::{Request, Response};
use http_body_util::BodyExt;
use kube::{Client, client::Body};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

fn owner() -> Owner {
    Owner {
        api_version: "flame.xflops.io/v1alpha1".into(),
        kind: "FlameCluster".into(),
        name: "flame".into(),
        uid: "owner-1".into(),
        namespace: Some("flame-system".into()),
    }
}

#[test]
fn active_owner_cannot_be_taken_over_even_with_adoption_enabled() {
    let object = json!({"metadata":{"ownerReferences":[{"controller":true,"uid":"someone-else"}]}});
    assert!(cluster::check_owner(&object, &owner(), true).is_err());
    assert!(cluster::check_owner(&json!({"metadata":{}}), &owner(), false).is_err());
    assert!(cluster::check_owner(&json!({"metadata":{}}), &owner(), true).is_ok());
}

#[test]
fn readiness_requires_the_current_deployment_generation() {
    let mut object = json!({"metadata":{"generation":3},"spec":{"replicas":1},"status":{"observedGeneration":2,"updatedReplicas":1,"availableReplicas":1}});
    assert!(!cluster::deployment_ready(&object));
    object["status"]["observedGeneration"] = json!(3);
    assert!(cluster::deployment_ready(&object));
}

#[test]
fn repeated_status_does_not_change_transition_time() {
    let first = cluster::status(
        None,
        1,
        true,
        "Reconciled",
        "ready",
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let second = cluster::status(
        Some(&first),
        1,
        true,
        "Reconciled",
        "ready",
        BTreeMap::new(),
        BTreeMap::new(),
    );
    assert_eq!(first, second);
}

#[tokio::test]
async fn apply_attaches_controller_ownership_and_does_not_claim_cross_namespace_objects() {
    let patches = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = patches.clone();
    let service = tower::service_fn(move |request: Request<Body>| {
        let captured = captured.clone();
        async move {
            assert_eq!(
                request.uri().path(),
                "/api/v1/namespaces/flame-system/configmaps/test"
            );
            let response = if request.method() == http::Method::GET {
                Response::builder().status(404).body(Body::from(serde_json::to_vec(&json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404,"message":"not found"})).unwrap())).unwrap()
            } else {
                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                captured.lock().unwrap().push(value.clone());
                Response::new(Body::from(serde_json::to_vec(&value).unwrap()))
            };
            Ok::<_, std::convert::Infallible>(response)
        }
    });
    let client = Client::new(service, "default");
    let resource = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"test","namespace":"flame-system"},"data":{"key":"value"}});
    cluster::apply_owned(client.clone(), resource.clone(), &owner(), false)
        .await
        .unwrap();
    {
        let values = patches.lock().unwrap();
        assert_eq!(
            values[0]["metadata"]["ownerReferences"][0]["uid"],
            "owner-1"
        );
        assert_eq!(
            values[0]["metadata"]["labels"][cluster::OWNER_LABEL],
            "owner-1"
        );
    }
    let mut other = resource;
    other["metadata"]["namespace"] = json!("other");
    assert!(
        cluster::apply_owned(client, other, &owner(), true)
            .await
            .is_err()
    );
}
