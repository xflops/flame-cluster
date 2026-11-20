//! Enrollment is reached only through the Kubernetes API's authenticated service proxy.
use crate::{
    Error,
    api::{FlameCluster, FlameWorker, FlameWorkerSpec},
    bootstrap, digest, enrollment,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client, ResourceExt, api::PostParams};
use serde::Deserialize;
use serde_json::json;
const KEY_ANNOTATION: &str = "flame.xflops.io/enrollment-key-sha256";
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    namespace: String,
    cluster: String,
    worker: String,
    address: String,
    enrollment_key: String,
    token: String,
    max_executors: Option<u32>,
    manage_object_cache: Option<bool>,
    shims: Option<Vec<String>>,
}
struct Failure(StatusCode, &'static str);
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        tracing::error!(%error,"worker enrollment failed");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "enrollment is temporarily unavailable",
        )
    }
}
impl From<kube::Error> for Failure {
    fn from(error: kube::Error) -> Self {
        Self::from(Error::Kube(error))
    }
}
fn dns_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
}
fn dns(value: &str, max: usize) -> bool {
    if max > 63 {
        return value.len() <= max && value.split('.').all(dns_label);
    }
    value.len() <= max && dns_label(value)
}
fn claim(worker: &FlameWorker, cluster: &FlameCluster, key_hash: &str) -> bool {
    !worker.spec.disabled
        && worker.metadata.deletion_timestamp.is_none()
        && worker.spec.cluster_ref == cluster.name_any()
        && worker
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(KEY_ANNOTATION))
            .is_some_and(|stored| stored == key_hash)
        && worker
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|owners| {
                owners.iter().any(|owner| {
                    Some(&owner.uid) == cluster.metadata.uid.as_ref()
                        && owner.controller == Some(true)
                })
            })
}
async fn enroll(
    State(client): State<Client>,
    Json(request): Json<Request>,
) -> Result<Response, Failure> {
    if !dns(&request.namespace, 63)
        || !dns(&request.cluster, 63)
        || !dns(&request.worker, 253)
        || request.enrollment_key.len() < 32
        || request.enrollment_key.len() > 256
        || request.token.len() > 32768
    {
        return Err(Failure(
            StatusCode::BAD_REQUEST,
            "invalid enrollment request",
        ));
    }
    let clusters = Api::<FlameCluster>::namespaced(client.clone(), &request.namespace);
    let cluster = clusters
        .get_opt(&request.cluster)
        .await?
        .filter(|cluster| cluster.metadata.deletion_timestamp.is_none())
        .ok_or(Failure(
            StatusCode::FORBIDDEN,
            "invalid or expired cluster join token",
        ))?;
    if cluster.spec.kubernetes_endpoint.is_none() {
        return Err(Failure(
            StatusCode::FORBIDDEN,
            "cluster enrollment is not enabled",
        ));
    }
    let secret = Api::<Secret>::namespaced(client.clone(), &request.namespace)
        .get_opt(&bootstrap::secret_name(&cluster))
        .await?
        .ok_or(Failure(
            StatusCode::FORBIDDEN,
            "invalid or expired cluster join token",
        ))?;
    crate::cluster::check_owner(
        &serde_json::to_value(&secret).map_err(Error::from)?,
        &crate::cluster::Owner::of(&cluster)?,
        false,
    )?;
    if !bootstrap::valid(&secret, &request.token, chrono::Utc::now()) {
        return Err(Failure(
            StatusCode::FORBIDDEN,
            "invalid or expired cluster join token",
        ));
    }
    let envelope: bootstrap::Envelope = STANDARD
        .decode(&request.token)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(Failure(
            StatusCode::FORBIDDEN,
            "invalid or expired cluster join token",
        ))?;
    if envelope.namespace != request.namespace || envelope.cluster != request.cluster {
        return Err(Failure(
            StatusCode::FORBIDDEN,
            "invalid or expired cluster join token",
        ));
    }
    let key_hash = digest(&request.enrollment_key)?;
    let workers = Api::<FlameWorker>::namespaced(client.clone(), &request.namespace);
    let worker = if let Some(worker) = workers.get_opt(&request.worker).await? {
        worker
    } else {
        let spec: FlameWorkerSpec = serde_json::from_value(json!({
            "clusterRef": request.cluster,
            "address": request.address,
            "maxExecutors": request.max_executors.unwrap_or(10),
            "manageObjectCache": request.manage_object_cache.unwrap_or(false),
            "shims": request.shims.unwrap_or_else(flame_api::default_shims)
        }))
        .map_err(Error::from)?;
        flame_api::validation::worker_spec(&spec)
            .and_then(|()| flame_api::validation::worker_endpoint(&spec, &cluster.spec).map(|_| ()))
            .map_err(|_| {
                Failure(
                    StatusCode::BAD_REQUEST,
                    "invalid worker configuration or FlameCluster endpoint",
                )
            })?;
        let mut worker = FlameWorker::new(&request.worker, spec);
        worker.metadata.namespace = Some(request.namespace.clone());
        worker.metadata.annotations = Some([(KEY_ANNOTATION.into(), key_hash.clone())].into());
        worker.metadata.owner_references = Some(vec![
            k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                api_version: "flame.xflops.io/v1alpha1".into(),
                kind: "FlameCluster".into(),
                name: cluster.name_any(),
                uid: cluster
                    .uid()
                    .ok_or_else(|| Error::Invalid("cluster UID is missing".into()))?,
                controller: Some(true),
                block_owner_deletion: Some(true),
            },
        ]);
        match workers.create(&PostParams::default(), &worker).await {
            Ok(worker) => worker,
            Err(kube::Error::Api(error)) if error.code == 409 => {
                workers.get(&request.worker).await?
            }
            Err(error) => return Err(error.into()),
        }
    };
    if !claim(&worker, &cluster, &key_hash) {
        return Err(Failure(
            StatusCode::CONFLICT,
            "worker is disabled or belongs to another enrollment",
        ));
    }
    match enrollment::worker_kubeconfig(client,&cluster,&worker).await? {
        Some(kubeconfig)=>Ok((StatusCode::OK,Json(json!({"namespace":request.namespace,"worker":request.worker,"kubeconfig":kubeconfig}))).into_response()),
        None=>Ok((StatusCode::ACCEPTED,Json(json!({"namespace":request.namespace,"worker":request.worker,"status":"pending"}))).into_response()),
    }
}
pub async fn serve(client: Client) -> Result<(), Error> {
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .route("/join", post(enroll))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(client);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8081").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_worker_requires_bound_key_and_cluster_owner() {
        let mut cluster = FlameCluster::new(
            "flame",
            serde_json::from_value(json!({"gateway":{"hostname":"test.local"}})).unwrap(),
        );
        cluster.metadata.uid = Some("cluster-uid".into());
        let mut worker = FlameWorker::new(
            "host",
            serde_json::from_value(json!({"clusterRef":"flame","address":"192.168.1.8"})).unwrap(),
        );
        worker.metadata.annotations = Some([(KEY_ANNOTATION.into(), "correct".into())].into());
        assert!(!claim(&worker, &cluster, "correct"));
        worker.metadata.owner_references=Some(vec![serde_json::from_value(json!({"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameCluster","name":"flame","uid":"cluster-uid","controller":true})).unwrap()]);
        assert!(claim(&worker, &cluster, "correct"));
        assert!(!claim(&worker, &cluster, "incorrect"));
        worker.spec.disabled = true;
        assert!(!claim(&worker, &cluster, "correct"));
    }
    #[tokio::test]
    async fn handler_rejects_old_expired_and_foreign_claims_before_issuing_credentials() {
        use http_body_util::BodyExt;
        use kube::client::Body;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        for case in [
            "old",
            "expired",
            "foreign",
            "pending",
            "disabled-enrollment",
            "pending-host",
            "invalid-shim",
            "rejoin-host",
        ] {
            let creates = Arc::new(AtomicUsize::new(0));
            let counted = creates.clone();
            let envelope = bootstrap::Envelope {
                namespace: "team-a".into(),
                cluster: "flame".into(),
                service_namespace: "operator".into(),
                service: "flame-operator".into(),
                kubeconfig: "{}".into(),
            };
            let token = STANDARD.encode(serde_json::to_vec(&envelope).unwrap());
            let current = token.clone();
            let client = Client::new(
                tower::service_fn(move |request: http::Request<Body>| {
                    let counted = counted.clone();
                    let current = current.clone();
                    async move {
                        let method = request.method().clone();
                        let path = request.uri().path().to_owned();
                        let mut value = if method == http::Method::POST
                            && path.ends_with("/flameworkers")
                        {
                            counted.fetch_add(1, Ordering::SeqCst);
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            let mut value: serde_json::Value =
                                serde_json::from_slice(&bytes).unwrap();
                            assert_eq!(
                                value["spec"]["shims"],
                                json!(if case == "pending-host" {
                                    vec!["host"]
                                } else {
                                    vec!["cri"]
                                })
                            );
                            value["metadata"]["uid"] = json!("new-worker");
                            value
                        } else if method == http::Method::PATCH {
                            assert!(
                                !path.contains("/flameworkers/"),
                                "rejoin must preserve existing Worker spec"
                            );
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            serde_json::from_slice(&bytes).unwrap()
                        } else if path.ends_with("/flameclusters/flame") {
                            let mut cluster = json!({"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameCluster","metadata":{"name":"flame","namespace":"team-a","uid":"cluster-uid"},"spec":{"gateway":{"hostname":"flame.test"},"endpoint":"192.168.1.2:30080","kubernetesEndpoint":"https://api.example.test:6443"}});
                            if case == "disabled-enrollment" {
                                cluster["spec"]
                                    .as_object_mut()
                                    .unwrap()
                                    .remove("kubernetesEndpoint");
                            }
                            cluster
                        } else if path.ends_with("/secrets/flame-join") {
                            let expiry = chrono::Utc::now()
                                + chrono::Duration::seconds(if case == "expired" {
                                    -1
                                } else {
                                    900
                                });
                            json!({"apiVersion":"v1","kind":"Secret","metadata":{"name":"flame-join","namespace":"team-a","labels":{"flame.xflops.io/owner-uid":"cluster-uid"}},"data":{"token":STANDARD.encode(current),"expiresAt":STANDARD.encode(expiry.to_rfc3339())}})
                        } else if case == "rejoin-host" && path.ends_with("/flameworkers/host") {
                            json!({"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameWorker","metadata":{"name":"host","namespace":"team-a","uid":"existing","annotations":{KEY_ANNOTATION:digest(&"a".repeat(64)).unwrap()},"ownerReferences":[{"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameCluster","name":"flame","uid":"cluster-uid","controller":true}]},"spec":{"clusterRef":"flame","address":"192.168.1.8","shims":["host"]}})
                        } else if case == "foreign" && path.ends_with("/flameworkers/host") {
                            json!({"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameWorker","metadata":{"name":"host","namespace":"team-a","uid":"existing"},"spec":{"clusterRef":"flame","address":"192.168.1.8"}})
                        } else {
                            return Ok::<_,std::convert::Infallible>(http::Response::builder().status(404).body(Body::from(serde_json::to_vec(&json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404,"message":"not found"})).unwrap())).unwrap());
                        };
                        if value["kind"] == "Secret" {
                            value["type"] = json!("Opaque");
                        }
                        Ok(http::Response::new(Body::from(
                            serde_json::to_vec(&value).unwrap(),
                        )))
                    }
                }),
                "default",
            );
            let request = Request {
                namespace: "team-a".into(),
                cluster: "flame".into(),
                worker: "host".into(),
                address: "192.168.1.8".into(),
                enrollment_key: "a".repeat(64),
                token: if case == "old" {
                    "old-token".into()
                } else {
                    token
                },
                max_executors: Some(2),
                manage_object_cache: Some(true),
                shims: match case {
                    "pending-host" => Some(vec!["host".into()]),
                    "invalid-shim" => Some(vec!["unknown".into()]),
                    "rejoin-host" => Some(vec!["cri".into()]),
                    _ => None,
                },
            };
            let response = match enroll(State(client), Json(request)).await {
                Ok(response) => response,
                Err(error) => error.into_response(),
            };
            assert_eq!(
                response.status(),
                match case {
                    "foreign" => StatusCode::CONFLICT,
                    "pending" | "pending-host" | "rejoin-host" => StatusCode::ACCEPTED,
                    "invalid-shim" => StatusCode::BAD_REQUEST,
                    _ => StatusCode::FORBIDDEN,
                },
                "case {case}"
            );
            assert_eq!(
                creates.load(Ordering::SeqCst),
                usize::from(matches!(case, "pending" | "pending-host"))
            );
        }
    }
}
