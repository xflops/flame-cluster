mod client_policy;

use flame_operator::{
    Error,
    api::{FlameCluster, FlameWorker, OperatorStatus},
};
use flame_operator::{
    bootstrap,
    cluster::{self, Owner},
    enrollment, resources, worker,
};
use futures::StreamExt;
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{ConfigMap, Secret},
};
use kube::{
    Api, Client, ResourceExt,
    api::{ListParams, Patch, PatchParams},
    runtime::{Controller, controller::Action, watcher},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

struct Context {
    client: Client,
    service: bootstrap::Service,
}

const FINALIZER: &str = "flame.xflops.io/gateway-policy";

async fn set_finalizer(cluster: &FlameCluster, client: Client, add: bool) -> Result<(), Error> {
    let mut finalizers = cluster.metadata.finalizers.clone().unwrap_or_default();
    finalizers.retain(|value| value != FINALIZER);
    if add {
        finalizers.push(FINALIZER.into());
    }
    Api::<FlameCluster>::namespaced(client, &cluster.namespace().expect("namespaced CR"))
        .patch(
            &cluster.name_any(),
            &PatchParams::default(),
            &Patch::Merge(json!({"metadata":{
                "resourceVersion":cluster.resource_version(),"finalizers":finalizers
            }})),
        )
        .await?;
    Ok(())
}

async fn write_status(
    cluster: &FlameCluster,
    ctx: &Context,
    status: &OperatorStatus,
) -> Result<(), Error> {
    if cluster.status.as_ref() != Some(status) {
        Api::<FlameCluster>::namespaced(
            ctx.client.clone(),
            &cluster.namespace().expect("namespaced CR"),
        )
        .patch_status(
            &cluster.name_any(),
            &PatchParams::default(),
            &Patch::Merge(json!({"status":status})),
        )
        .await?;
    }
    Ok(())
}

async fn revoke_workers(
    cluster: &FlameCluster,
    ctx: &Context,
    workers: &[FlameWorker],
    all: bool,
) -> Result<(), Error> {
    let mut first_error = None;
    for worker in workers {
        if worker.spec.cluster_ref == cluster.name_any()
            && worker.namespace() == cluster.namespace()
            && (all || worker.spec.disabled || worker.metadata.deletion_timestamp.is_some())
            && let Err(error) = enrollment::revoke(ctx.client.clone(), worker).await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn reconcile(cluster: Arc<FlameCluster>, ctx: Arc<Context>) -> Result<Action, Error> {
    if cluster.metadata.deletion_timestamp.is_some() {
        let workers = Api::<FlameWorker>::namespaced(
            ctx.client.clone(),
            &cluster.namespace().expect("namespaced CR"),
        )
        .list(&ListParams::default())
        .await?;
        revoke_workers(&cluster, &ctx, &workers.items, true).await?;
        if cluster.finalizers().iter().any(|value| value == FINALIZER) {
            cluster::prune_gateway_policies(
                ctx.client.clone(),
                &Owner::of(cluster.as_ref())?,
                None,
            )
            .await?;
            bootstrap::cleanup(ctx.client.clone(), &cluster, &ctx.service).await?;
            set_finalizer(&cluster, ctx.client.clone(), false).await?;
        }
        return Ok(Action::await_change());
    }
    if !cluster.finalizers().iter().any(|value| value == FINALIZER) {
        set_finalizer(&cluster, ctx.client.clone(), true).await?;
        return Ok(Action::requeue(Duration::from_secs(1)));
    }
    match reconcile_inner(&cluster, &ctx).await {
        Ok((status, enrollment_pending)) => {
            let ready = status.conditions[0].status == "True";
            write_status(&cluster, &ctx, &status).await?;
            let maximum = Duration::from_secs(if ready && !enrollment_pending { 30 } else { 10 });
            let delay = bootstrap::next_delay(ctx.client.clone(), &cluster, maximum).await?;
            Ok(Action::requeue(delay))
        }
        Err(error) => {
            let status = cluster::status(
                cluster.status.as_ref(),
                cluster.metadata.generation.unwrap_or(0),
                false,
                "ReconcileFailed",
                &error.to_string(),
                BTreeMap::new(),
                BTreeMap::new(),
            );
            let _ = write_status(&cluster, &ctx, &status).await;
            Err(error)
        }
    }
}

async fn reconcile_inner(
    cluster: &FlameCluster,
    ctx: &Context,
) -> Result<(OperatorStatus, bool), Error> {
    let name = cluster.name_any();
    let namespace = cluster.namespace().expect("namespaced CR");
    let owner = Owner::of(cluster)?;
    // Revocation is a separate first pass: unrelated configuration/issuance failures
    // must never preserve a disabled host's durable Kubernetes identity.
    let host_workers = Api::<FlameWorker>::namespaced(ctx.client.clone(), &namespace)
        .list(&ListParams::default())
        .await?;
    revoke_workers(cluster, ctx, &host_workers.items, false).await?;
    bootstrap::ensure(ctx.client.clone(), cluster, &ctx.service).await?;
    let spec = cluster.spec.clone();
    resources::validate_flame(&name, &spec)?;
    for peer in Api::<FlameCluster>::all(ctx.client.clone())
        .list(&ListParams::default())
        .await?
    {
        if peer.uid() != cluster.uid()
            && peer.metadata.deletion_timestamp.is_none()
            && resources::allocations_overlap(&spec, &peer.spec)
        {
            return Err(Error::Invalid(format!(
                "NodePorts overlap with FlameCluster {}/{}; assign distinct ports",
                peer.namespace().unwrap_or_default(),
                peer.name_any()
            )));
        }
    }
    let mut enrollment_pending = false;
    for host in &host_workers.items {
        if host.spec.cluster_ref == name
            && host.namespace() == cluster.namespace()
            && !host.spec.disabled
            && host.metadata.deletion_timestamp.is_none()
            && !enrollment::ensure(ctx.client.clone(), cluster, host).await?
        {
            enrollment_pending = true;
        }
    }
    let cache_endpoints =
        worker::cache_endpoints(cluster, &host_workers.items, chrono::Utc::now())?;
    let cache_endpoints = cache_endpoints.as_slice();
    let policy = resources::gateway_policy(&name, &namespace, &spec, cache_endpoints);
    cluster::apply_gateway_policy(
        ctx.client.clone(),
        policy.clone(),
        &owner,
        spec.adopt_existing,
    )
    .await?;
    cluster::prune_gateway_policies(
        ctx.client.clone(),
        &owner,
        Some((
            policy["metadata"]["namespace"]
                .as_str()
                .expect("policy namespace"),
            policy["metadata"]["name"].as_str().expect("policy name"),
        )),
    )
    .await?;
    let version = &spec.version;
    let image = resources::session_image(&spec);
    let public_ca = Api::<ConfigMap>::namespaced(ctx.client.clone(), &namespace)
        .get_opt(&spec.gateway.issuer_name)
        .await?
        .and_then(|config| config.data.and_then(|data| data.get("ca.crt").cloned()))
        .map(|pem| flame_operator::tls::public_ca(&pem))
        .transpose()?;
    let server_secret = Api::<Secret>::namespaced(ctx.client.clone(), &namespace)
        .get_opt(&format!("{name}-control-plane-tls"))
        .await?;
    let certificate_hash =
        flame_operator::tls::certificate_hash(server_secret.as_ref(), public_ca.as_deref())?;
    let mut resources = resources::flame_resources(
        &name,
        &namespace,
        &cluster.uid().expect("cluster UID"),
        &spec,
        &image,
        cache_endpoints,
    )?;
    let server_addresses = flame_operator::tls::server_addresses(cluster, &host_workers.items)?;
    if !server_addresses.is_empty() {
        for resource in &mut resources {
            if resource["kind"] == "Certificate"
                && resource["metadata"]["name"] == format!("{name}-control-plane-tls")
            {
                resource["spec"]["ipAddresses"] = json!(server_addresses);
            }
        }
    }
    if let Some(hash) = certificate_hash {
        for resource in &mut resources {
            if resource["kind"] == "Deployment"
                && resource["metadata"]["name"] == format!("{name}-session-manager")
            {
                resource["spec"]["template"]["metadata"]["annotations"]["flame.xflops.io/control-plane-tls-hash"] =
                    json!(hash);
            }
        }
    }
    let mut applied = BTreeMap::<String, Value>::new();
    for resource in &resources {
        let key = format!(
            "{}/{}",
            resource["kind"].as_str().expect("kind"),
            resource["metadata"]["name"].as_str().expect("name")
        );
        applied.insert(
            key,
            cluster::apply_owned(
                ctx.client.clone(),
                resource.clone(),
                &owner,
                spec.adopt_existing,
            )
            .await?,
        );
    }
    cluster::prune(ctx.client.clone(), &owner, &resources).await?;
    let session = &applied[&format!("Deployment/{name}-session-manager")];
    let gateway = &applied[&format!("Gateway/{name}-gateway")];
    let certificate = &applied[&format!("Certificate/{name}-gateway-tls")];
    let client_certificate = &applied[&format!("Certificate/{name}-admin-client")];
    let mut client_ready = false;
    if cluster::condition_ready(client_certificate, "Ready")
        && let Some(source) = Api::<Secret>::namespaced(ctx.client.clone(), &namespace)
            .get_opt(&format!("{name}-admin-client-tls"))
            .await?
        && let Some(data) = source.data
        && let Some(bundle) = resources::client_secret(&name, &namespace, &spec, &data)?
    {
        cluster::apply_owned(ctx.client.clone(), bundle, &owner, spec.adopt_existing).await?;
        client_ready = true;
    }
    let policies_ready = !spec.gateway.client_auth
        || ["session-https", "cache-grpc"].iter().all(|section| {
            applied
                .get(&format!("ClientTrafficPolicy/{name}-{section}-mtls"))
                .is_some_and(|policy| {
                    client_policy::accepted(policy, &format!("{name}-gateway"), &namespace)
                })
        });
    let server_certificate = &applied[&format!("Certificate/{name}-control-plane-tls")];
    let backend_tls = &applied[&format!("BackendTLSPolicy/{name}-control-plane-tls")];
    let control_plane_tls_ready = public_ca.is_some()
        && cluster::condition_ready(server_certificate, "Ready")
        && client_policy::backend_accepted(
            backend_tls,
            &format!("{name}-session-manager"),
            &format!("{name}-gateway"),
            &namespace,
        );
    let ready = policies_ready
        && control_plane_tls_ready
        && client_ready
        && cluster::deployment_ready(session)
        && cluster::condition_ready(gateway, "Programmed")
        && cluster::condition_ready(certificate, "Ready");
    let g = &spec.gateway;
    let mut status = cluster::status(
        cluster.status.as_ref(),
        cluster.metadata.generation.unwrap_or(0),
        ready,
        if ready {
            "Reconciled"
        } else {
            "WaitingForControlPlane"
        },
        if ready {
            "Session manager, certificate, and Gateway are ready"
        } else {
            "Waiting for Deployment, Certificate, Gateway, and client identity policy readiness"
        },
        BTreeMap::from([
            ("sessionManagerImage".into(), image),
            ("flameRelease".into(), version.clone()),
        ]),
        BTreeMap::from([
            (
                "sessionManager".into(),
                format!("https://{}:{}", g.hostname, g.session_port),
            ),
            (
                "cache".into(),
                format!("grpcs-proxy://{}:{}", g.hostname, g.cache_port),
            ),
        ]),
    );
    if control_plane_tls_ready {
        status.control_plane_ca = public_ca;
    }
    if client_ready {
        status.client_secret = Some(format!("{name}-client"));
    }
    if !spec.endpoint.is_empty() {
        let endpoint = resources::manager_endpoint(&spec.endpoint)?;
        status
            .endpoints
            .insert("frontend".into(), endpoint.to_string());
        status.endpoints.insert(
            "backend".into(),
            format!("{}:{}", endpoint.ip(), endpoint.port() + 1),
        );
    }
    if let Some(ip) = &spec.gateway.ingress_ip {
        status.endpoints.insert("ingressIp".into(), ip.clone());
    }
    Ok((status, enrollment_pending))
}

fn error_policy(object: Arc<FlameCluster>, error: &Error, _: Arc<Context>) -> Action {
    tracing::error!(cluster=%object.name_any(),%error,"reconciliation failed");
    Action::requeue(Duration::from_secs(15))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    flame_operator::init_tracing();
    let client = Client::try_default().await?;
    let context = Arc::new(Context {
        client: client.clone(),
        service: bootstrap::Service::from_env()?,
    });
    let controller = Controller::new(
        Api::<FlameCluster>::all(client.clone()),
        watcher::Config::default(),
    );
    let controller = controller
        .owns(
            Api::<Deployment>::all(client.clone()),
            watcher::Config::default(),
        )
        .watches(
            Api::<FlameWorker>::all(client.clone()),
            watcher::Config::default(),
            |worker| {
                worker.namespace().map(|namespace| {
                    kube::runtime::reflector::ObjectRef::<FlameCluster>::new(
                        &worker.spec.cluster_ref,
                    )
                    .within(&namespace)
                })
            },
        )
        .watches(
            Api::<Secret>::all(client.clone()),
            watcher::Config::default(),
            |secret| {
                let name = secret.name_any();
                name.strip_suffix("-admin-client-tls")
                    .or_else(|| name.strip_suffix("-control-plane-tls"))
                    .and_then(|name| {
                        secret.namespace().map(|namespace| {
                            kube::runtime::reflector::ObjectRef::<FlameCluster>::new(name)
                                .within(&namespace)
                        })
                    })
            },
        )
        .shutdown_on_signal()
        .run(reconcile, error_policy, context)
        .for_each(|result| async {
            if let Err(error) = result {
                tracing::error!(%error,"controller error");
            }
        });
    tokio::select! { _=controller=>Ok(()), result=flame_operator::join::serve(client)=>result }
}

#[cfg(test)]
mod security_tests {
    use super::*;
    use http::{Request, Response};
    use kube::client::Body;
    use std::sync::Mutex;

    #[tokio::test]
    async fn disabled_and_deleting_credentials_are_revoked_before_unrelated_reconciliation_failures()
     {
        for failure in [
            "gateway",
            "bootstrap",
            "port-conflict",
            "worker-cache",
            "foreign-worker",
            "cluster-delete",
        ] {
            let deleted = Arc::new(Mutex::new(Vec::<String>::new()));
            let captured = deleted.clone();
            let client = Client::new(
                tower::service_fn(move |request: Request<Body>| {
                    let captured = captured.clone();
                    async move {
                        let path = request.uri().path();
                        let value = if request.method() == http::Method::DELETE {
                            captured.lock().unwrap().push(path.to_owned());
                            json!({"apiVersion":"v1","kind":"Status","status":"Success","code":200})
                        } else if path.ends_with("/flameworkers") {
                            let disabled = json!({"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameWorker","metadata":{"name":"disabled","namespace":"flame-system","uid":"disabled-uid"},"spec":{"clusterRef":"flame","address":"192.168.1.8","disabled":true}});
                            let deleting = json!({"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameWorker","metadata":{"name":"deleting","namespace":"flame-system","uid":"deleting-uid","deletionTimestamp":"2026-11-20T01:00:00Z"},"spec":{"clusterRef":"flame","address":"192.168.1.9"}});
                            let active = json!({"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameWorker","metadata":{"name":"active","namespace":"flame-system","uid":"active-uid"},"spec":{"clusterRef":"flame","address":if failure=="worker-cache" {"0.0.0.0"} else {"192.168.1.10"}}});
                            json!({"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameWorkerList","metadata":{},"items":[disabled,deleting,active]})
                        } else if path.ends_with("/networkpolicies") {
                            assert_eq!(failure, "cluster-delete");
                            return Ok::<_,std::convert::Infallible>(Response::builder().status(500).body(Body::from(serde_json::to_vec(&json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"InternalError","code":500,"message":"cleanup unavailable"})).unwrap())).unwrap());
                        } else if path.ends_with("/flameclusters") {
                            json!({"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameClusterList","metadata":{},"items":if failure=="port-conflict" {vec![json!({"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameCluster","metadata":{"name":"peer","namespace":"other","uid":"peer-uid"},"spec":{"gateway":{"hostname":"peer.test"}}})]}else{vec![]}})
                        } else if path.ends_with("/secrets/flame-join") {
                            assert_eq!(failure, "bootstrap");
                            return Ok::<_,std::convert::Infallible>(Response::builder().status(500).body(Body::from(serde_json::to_vec(&json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"InternalError","code":500,"message":"bootstrap unavailable"})).unwrap())).unwrap());
                        } else {
                            let (name, uid) = if path.contains("flmlet-disabled-") {
                                (
                                    "disabled",
                                    if failure == "foreign-worker" {
                                        "foreign-uid"
                                    } else {
                                        "disabled-uid"
                                    },
                                )
                            } else if path.contains("flmlet-active-") {
                                assert_eq!(failure, "cluster-delete");
                                ("active", "active-uid")
                            } else {
                                assert!(
                                    path.contains("flmlet-deleting-"),
                                    "unexpected request {path}"
                                );
                                ("deleting", "deleting-uid")
                            };
                            let kind = if path.contains("/serviceaccounts/") {
                                "ServiceAccount"
                            } else if path.contains("/rolebindings/") {
                                "RoleBinding"
                            } else if path.contains("/roles/") {
                                "Role"
                            } else {
                                "Secret"
                            };
                            json!({"apiVersion":if kind.starts_with("Role") {"rbac.authorization.k8s.io/v1"}else{"v1"},"kind":kind,"metadata":{"name":path.rsplit('/').next().unwrap(),"namespace":"flame-system","uid":"credential-uid","resourceVersion":"7","ownerReferences":[{"apiVersion":"flame.xflops.io/v1alpha1","kind":"FlameWorker","name":name,"uid":uid,"controller":true}]}})
                        };
                        Ok(Response::new(Body::from(
                            serde_json::to_vec(&value).unwrap(),
                        )))
                    }
                }),
                "default",
            );
            let mut cluster=FlameCluster::new("flame",serde_json::from_value(json!({"gateway":{"hostname":if failure=="gateway" {"https://invalid.test"}else{"flame.test"}}})).unwrap());
            cluster.metadata.namespace = Some("flame-system".into());
            cluster.metadata.uid = Some("cluster-uid".into());
            if failure == "bootstrap" {
                cluster.spec.kubernetes_endpoint = Some("https://api.example.test:6443".into());
            }
            let context = Context {
                client,
                service: bootstrap::Service {
                    namespace: "operator".into(),
                    name: "flame-operator".into(),
                },
            };
            let failed = if failure == "cluster-delete" {
                cluster.metadata.deletion_timestamp =
                    Some(serde_json::from_value(json!("2026-11-20T01:00:00Z")).unwrap());
                cluster.metadata.finalizers = Some(vec![FINALIZER.into()]);
                reconcile(Arc::new(cluster), Arc::new(context))
                    .await
                    .is_err()
            } else {
                reconcile_inner(&cluster, &context).await.is_err()
            };
            assert!(failed, "expected {failure} to fail after revocation");
            let deleted = deleted.lock().unwrap();
            assert_eq!(
                deleted.len(),
                if failure == "foreign-worker" {
                    4
                } else if failure == "cluster-delete" {
                    12
                } else {
                    8
                },
                "revocation blocked by {failure}: {deleted:?}"
            );
            assert!(
                deleted
                    .iter()
                    .filter(|path| path.contains("flmlet-deleting-"))
                    .count()
                    == 4
            );
        }
    }
}
