//! Per-worker host credentials issued by the control plane, with no cluster-wide worker rights.
use crate::{
    Error,
    api::{FlameCluster, FlameWorker},
    cluster::{self, Owner},
    digest,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use kube::{
    Client, ResourceExt,
    api::{DeleteParams, Preconditions},
};
use serde_json::{Value, json};

pub const CA_FILE: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";
pub const CREDENTIAL_LABEL: &str = "flame.xflops.io/credential";

fn base_name(worker: &FlameWorker) -> Result<String, Error> {
    let uid = worker
        .uid()
        .ok_or_else(|| Error::Invalid("worker UID is required for credentials".into()))?;
    let name: String = worker.name_any().chars().take(24).collect();
    Ok(format!(
        "flmlet-{}-{}",
        name.trim_end_matches(['-', '.']),
        &digest(&uid)?[..12]
    ))
}

/// All rights are resourceName-scoped and stay in the worker's namespace.
pub fn credential_resources(worker: &FlameWorker) -> Result<Vec<Value>, Error> {
    let namespace = worker
        .namespace()
        .ok_or_else(|| Error::Invalid("worker namespace is required".into()))?;
    let name = base_name(worker)?;
    let secret = format!("{name}-api-token");
    Ok(vec![
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":name,"namespace":namespace},"automountServiceAccountToken":false}),
        json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":name,"namespace":namespace},"rules":[
            {"apiGroups":["flame.xflops.io"],"resources":["flameworkers"],"resourceNames":[worker.name_any()],"verbs":["get"]},
            {"apiGroups":["flame.xflops.io"],"resources":["flameworkers/status"],"resourceNames":[worker.name_any()],"verbs":["patch"]},
            {"apiGroups":["flame.xflops.io"],"resources":["flameclusters"],"resourceNames":[worker.spec.cluster_ref],"verbs":["get"]}
        ]}),
        json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{"name":name,"namespace":namespace},
            "roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":name},
            "subjects":[{"kind":"ServiceAccount","name":name,"namespace":namespace}]}),
        json!({"apiVersion":"v1","kind":"Secret","metadata":{"name":secret,"namespace":namespace,
            "annotations":{"kubernetes.io/service-account.name":name},"labels":{CREDENTIAL_LABEL:"api-token"}},
            "type":"kubernetes.io/service-account-token"}),
    ])
}

/// A disabled worker's existing identity is revoked without touching foreign resources.
pub async fn revoke(client: Client, worker: &FlameWorker) -> Result<(), Error> {
    let owner = Owner::of(worker)?;
    let resources = credential_resources(worker)?;
    for resource in resources {
        let api = cluster::dynamic_api(
            client.clone(),
            resource["apiVersion"].as_str().unwrap(),
            resource["kind"].as_str().unwrap(),
            owner.namespace.as_deref(),
        )?;
        let name = resource["metadata"]["name"].as_str().unwrap();
        if let Some(existing) = api.get_opt(name).await? {
            cluster::check_owner(&serde_json::to_value(&existing)?, &owner, false)?;
            api.delete(
                name,
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: existing.uid(),
                        resource_version: existing.resource_version(),
                    }),
                    ..Default::default()
                },
            )
            .await?;
        }
    }
    Ok(())
}

/// Return false only while Kubernetes is populating the scoped service-account token.
/// An omitted external API endpoint skips enrollment without changing cluster readiness.
pub async fn ensure(
    client: Client,
    cluster: &FlameCluster,
    worker: &FlameWorker,
) -> Result<bool, Error> {
    if worker.namespace() != cluster.namespace() || worker.spec.cluster_ref != cluster.name_any() {
        return Err(Error::Invalid(
            "worker credential scope does not match FlameCluster".into(),
        ));
    }
    if worker.spec.disabled || worker.metadata.deletion_timestamp.is_some() {
        revoke(client, worker).await?;
        return Ok(true);
    }
    let Some(endpoint) = cluster.spec.kubernetes_endpoint.as_deref() else {
        return Ok(true);
    };
    flame_api::validation::kubernetes_endpoint(endpoint)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    let owner = Owner::of(worker)?;
    let mut token = None;
    for resource in credential_resources(worker)? {
        let is_token = resource["kind"] == "Secret";
        let current = cluster::apply_owned(client.clone(), resource, &owner, false).await?;
        if is_token {
            token = current["data"]["token"]
                .as_str()
                .map(|token| STANDARD.decode(token))
                .transpose()
                .map_err(|_| Error::Invalid("worker API token encoding is invalid".into()))?;
        }
    }
    if token.is_none_or(|token| token.is_empty()) {
        return Ok(false);
    }

    Ok(true)
}

/// Retrieve the durable per-worker kubeconfig after its token controller has populated it.
pub async fn worker_kubeconfig(
    client: Client,
    cluster: &FlameCluster,
    worker: &FlameWorker,
) -> Result<Option<String>, Error> {
    if !ensure(client.clone(), cluster, worker).await? {
        return Ok(None);
    }
    let namespace = worker.namespace().expect("namespaced worker");
    let name = format!("{}-api-token", base_name(worker)?);
    let secret = kube::Api::<k8s_openapi::api::core::v1::Secret>::namespaced(client, &namespace)
        .get(&name)
        .await?;
    let jwt = secret
        .data
        .as_ref()
        .and_then(|data| data.get("token"))
        .and_then(|data| std::str::from_utf8(&data.0).ok())
        .ok_or_else(|| Error::Invalid("worker API token encoding is invalid".into()))?;
    Ok(Some(crate::bootstrap::kubeconfig(
        &namespace,
        cluster.spec.kubernetes_endpoint.as_deref().ok_or_else(|| {
            Error::Invalid("kubernetesEndpoint is required for enrollment".into())
        })?,
        &std::fs::read(CA_FILE)?,
        jwt,
        "flmlet",
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::FlameWorkerSpec;
    use http::{Request, Response};
    use http_body_util::BodyExt;
    use kube::client::Body;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    fn worker() -> FlameWorker {
        let spec: FlameWorkerSpec =
            serde_json::from_value(json!({"clusterRef":"flame","address":"192.168.3.8"})).unwrap();
        let mut worker = FlameWorker::new("worker-01", spec);
        worker.metadata.namespace = Some("team-a".into());
        worker.metadata.uid = Some("worker-uid".into());
        worker
    }
    fn cluster() -> FlameCluster {
        let mut cluster = FlameCluster::new("flame", serde_json::from_value(json!({
            "gateway":{"hostname":"flame.example.test"},"kubernetesEndpoint":"https://api.example.test:6443"
        })).unwrap());
        cluster.metadata.namespace = Some("team-a".into());
        cluster
    }
    fn not_found() -> Response<Body> {
        Response::builder().status(404).body(Body::from(serde_json::to_vec(&json!({
            "apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404,"message":"not found"
        })).unwrap())).unwrap()
    }

    #[test]
    fn credential_rights_are_named_worker_and_cluster_only() {
        let worker = worker();
        let objects = credential_resources(&worker).unwrap();
        assert_eq!(objects.len(), 4);
        assert!(!objects.iter().any(
            |object| object["kind"] == "ClusterRole" || object["kind"] == "ClusterRoleBinding"
        ));
        let role = objects
            .iter()
            .find(|object| object["kind"] == "Role")
            .unwrap();
        assert_eq!(role["metadata"]["namespace"], "team-a");
        assert_eq!(
            role["rules"],
            json!([
                {"apiGroups":["flame.xflops.io"],"resources":["flameworkers"],"resourceNames":["worker-01"],"verbs":["get"]},
                {"apiGroups":["flame.xflops.io"],"resources":["flameworkers/status"],"resourceNames":["worker-01"],"verbs":["patch"]},
                {"apiGroups":["flame.xflops.io"],"resources":["flameclusters"],"resourceNames":["flame"],"verbs":["get"]}
            ])
        );
        let sa = objects
            .iter()
            .find(|object| object["kind"] == "ServiceAccount")
            .unwrap();
        assert_eq!(sa["automountServiceAccountToken"], false);
        let token = objects
            .iter()
            .find(|object| object["kind"] == "Secret")
            .unwrap();
        assert_eq!(
            token["metadata"]["annotations"]["kubernetes.io/service-account.name"],
            sa["metadata"]["name"]
        );
        assert_eq!(token["metadata"]["labels"][CREDENTIAL_LABEL], "api-token");
        assert!(token.get("data").is_none());
        let mut recreated = worker;
        recreated.metadata.uid = Some("new-worker-uid".into());
        assert_ne!(
            role["metadata"]["name"],
            credential_resources(&recreated).unwrap()[1]["metadata"]["name"]
        );
    }

    #[tokio::test]
    async fn absent_external_api_skips_enrollment_without_api_writes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let client = Client::new(
            tower::service_fn(move |_: Request<Body>| {
                counted.fetch_add(1, Ordering::SeqCst);
                async { Ok::<_, std::convert::Infallible>(not_found()) }
            }),
            "default",
        );
        let mut cluster = cluster();
        cluster.spec.kubernetes_endpoint = None;
        assert!(ensure(client, &cluster, &worker()).await.unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn issued_objects_are_worker_owned_and_wait_for_the_real_token_controller() {
        let applied = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = applied.clone();
        let client = Client::new(
            tower::service_fn(move |request: Request<Body>| {
                let captured = captured.clone();
                async move {
                    if request.method() == http::Method::GET {
                        return Ok::<_, std::convert::Infallible>(not_found());
                    }
                    assert_eq!(request.method(), http::Method::PATCH);
                    let bytes = request.into_body().collect().await.unwrap().to_bytes();
                    let value: Value = serde_json::from_slice(&bytes).unwrap();
                    captured.lock().unwrap().push(value.clone());
                    Ok(Response::new(Body::from(
                        serde_json::to_vec(&value).unwrap(),
                    )))
                }
            }),
            "default",
        );
        assert!(!ensure(client, &cluster(), &worker()).await.unwrap());
        let objects = applied.lock().unwrap();
        assert_eq!(objects.len(), 4);
        for object in objects.iter() {
            assert_eq!(
                object["metadata"]["ownerReferences"][0]["kind"],
                "FlameWorker"
            );
            assert_eq!(
                object["metadata"]["ownerReferences"][0]["uid"],
                "worker-uid"
            );
            assert_eq!(
                object["metadata"]["labels"][cluster::OWNER_LABEL],
                "worker-uid"
            );
        }
    }

    #[tokio::test]
    async fn a_foreign_credential_secret_is_never_overwritten() {
        let writes = Arc::new(AtomicUsize::new(0));
        let counted = writes.clone();
        let client = Client::new(
            tower::service_fn(move |request: Request<Body>| {
                let counted = counted.clone();
                async move {
                    if request.method() != http::Method::GET {
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok::<_, std::convert::Infallible>(Response::new(Body::from(serde_json::to_vec(&json!({
                    "apiVersion":"v1","kind":"Secret","metadata":{"name":"join","namespace":"team-a",
                        "ownerReferences":[{"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameWorker","name":"other","uid":"foreign-worker","controller":true}]}
                })).unwrap())))
                }
            }),
            "default",
        );
        let worker = worker();
        let secret = credential_resources(&worker).unwrap().pop().unwrap();
        assert!(
            cluster::apply_owned(client, secret, &Owner::of(&worker).unwrap(), false)
                .await
                .is_err()
        );
        assert_eq!(writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn disabled_credentials_are_revoked_with_uid_and_resource_version_preconditions() {
        let deleted = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = deleted.clone();
        let client = Client::new(
            tower::service_fn(move |request: Request<Body>| {
                let captured = captured.clone();
                async move {
                    if request.method() == http::Method::DELETE {
                        let bytes = request.into_body().collect().await.unwrap().to_bytes();
                        captured
                            .lock()
                            .unwrap()
                            .push(serde_json::from_slice(&bytes).unwrap());
                        return Ok::<_, std::convert::Infallible>(Response::new(Body::from(serde_json::to_vec(&json!({"apiVersion":"v1","kind":"Status","status":"Success","code":200})).unwrap())));
                    }
                    assert_eq!(request.method(), http::Method::GET);
                    let path = request.uri().path();
                    let kind = if path.contains("/serviceaccounts/") {
                        "ServiceAccount"
                    } else if path.contains("/rolebindings/") {
                        "RoleBinding"
                    } else if path.contains("/roles/") {
                        "Role"
                    } else {
                        "Secret"
                    };
                    Ok(Response::new(Body::from(serde_json::to_vec(&json!({
                    "apiVersion":if kind.starts_with("Role") {"rbac.authorization.k8s.io/v1"} else {"v1"},"kind":kind,
                    "metadata":{"name":path.rsplit('/').next().unwrap(),"namespace":"team-a","uid":"object-uid","resourceVersion":"42",
                        "ownerReferences":[{"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameWorker","name":"worker-01","uid":"worker-uid","controller":true}]}
                })).unwrap())))
                }
            }),
            "default",
        );
        let mut worker = worker();
        worker.spec.disabled = true;
        assert!(ensure(client, &cluster(), &worker).await.unwrap());
        let deleted = deleted.lock().unwrap();
        assert_eq!(deleted.len(), 4);
        assert!(deleted.iter().all(|options| options["preconditions"]
            == json!({"uid":"object-uid","resourceVersion":"42"})));
    }
}
