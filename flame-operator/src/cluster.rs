use crate::{
    Error,
    api::{Condition, OperatorStatus},
};
use kube::{
    Api, Client, ResourceExt,
    api::{DynamicObject, Patch, PatchParams},
    core::{ApiResource, GroupVersionKind},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const OWNER_LABEL: &str = "flame.xflops.io/owner-uid";

#[derive(Clone, Debug)]
pub struct Owner {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub uid: String,
    pub namespace: Option<String>,
}

impl Owner {
    /// Ownership metadata for any typed Kubernetes resource.
    pub fn of<K: kube::Resource<DynamicType = ()>>(resource: &K) -> Result<Self, Error> {
        Ok(Self {
            api_version: K::api_version(&()).into_owned(),
            kind: K::kind(&()).into_owned(),
            name: resource.name_any(),
            uid: resource
                .uid()
                .ok_or_else(|| Error::Invalid("owner UID is missing".into()))?,
            namespace: resource.namespace(),
        })
    }
}

pub fn dynamic_api(
    client: Client,
    api_version: &str,
    kind: &str,
    namespace: Option<&str>,
) -> Result<Api<DynamicObject>, Error> {
    let (group, version) = api_version.split_once('/').unwrap_or(("", api_version));
    let plural = match kind {
        "ConfigMap" => "configmaps",
        "ServiceAccount" => "serviceaccounts",
        "Role" => "roles",
        "RoleBinding" => "rolebindings",
        "Service" => "services",
        "Secret" => "secrets",
        "PersistentVolumeClaim" => "persistentvolumeclaims",
        "Deployment" => "deployments",
        "EndpointSlice" => "endpointslices",
        "NetworkPolicy" => "networkpolicies",
        "Gateway" => "gateways",
        "HTTPRoute" => "httproutes",
        "BackendTLSPolicy" => "backendtlspolicies",
        "EnvoyProxy" => "envoyproxies",
        "ClientTrafficPolicy" => "clienttrafficpolicies",
        "Certificate" => "certificates",
        "FlameCluster" => "flameclusters",
        _ => return Err(Error::Invalid(format!("unsupported resource kind {kind}"))),
    };
    let mut resource = ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind));
    resource.plural = plural.into();
    Ok(match namespace {
        Some(ns) => Api::namespaced_with(client, ns, &resource),
        None => Api::all_with(client, &resource),
    })
}

pub fn check_owner(existing: &Value, owner: &Owner, adopt: bool) -> Result<(), Error> {
    if let Some(references) = existing["metadata"]["ownerReferences"].as_array()
        && references
            .iter()
            .any(|reference| reference["controller"] == true && reference["uid"] != owner.uid)
    {
        return Err(Error::Invalid(
            "resource belongs to another controller; refusing adoption".into(),
        ));
    }
    if let Some(uid) = existing["metadata"]["labels"][OWNER_LABEL].as_str() {
        if uid != owner.uid {
            return Err(Error::Invalid(
                "resource belongs to another custom-resource UID".into(),
            ));
        }
        return Ok(());
    }
    let already_owned = existing["metadata"]["ownerReferences"]
        .as_array()
        .is_some_and(|refs| refs.iter().any(|reference| reference["uid"] == owner.uid));
    if !adopt && !already_owned {
        return Err(Error::Invalid(
            "existing resource is unmanaged; explicit adoption is required".into(),
        ));
    }
    Ok(())
}

pub async fn apply_owned(
    client: Client,
    resource: Value,
    owner: &Owner,
    adopt: bool,
) -> Result<Value, Error> {
    apply_managed(client, resource, owner, adopt, false).await
}

/// Apply with controller ownership or explicit label ownership for external resources.
pub async fn apply_managed(
    client: Client,
    mut resource: Value,
    owner: &Owner,
    adopt: bool,
    external: bool,
) -> Result<Value, Error> {
    let api_version = resource["apiVersion"]
        .as_str()
        .ok_or_else(|| Error::Invalid("apiVersion missing".into()))?
        .to_string();
    let kind = resource["kind"]
        .as_str()
        .ok_or_else(|| Error::Invalid("kind missing".into()))?
        .to_string();
    let name = resource["metadata"]["name"]
        .as_str()
        .ok_or_else(|| Error::Invalid("name missing".into()))?
        .to_string();
    let namespace = resource["metadata"]["namespace"]
        .as_str()
        .map(str::to_string);
    if let Some(parent_ns) = &owner.namespace
        && namespace.as_ref() != Some(parent_ns)
        && !external
    {
        return Err(Error::Invalid(
            "namespaced owner cannot own cluster-scoped or cross-namespace resources".into(),
        ));
    }
    let api = dynamic_api(client, &api_version, &kind, namespace.as_deref())?;
    if let Some(existing) = api.get_opt(&name).await? {
        check_owner(&serde_json::to_value(existing)?, owner, adopt)?;
    }
    if resource["metadata"]["labels"].is_null() {
        resource["metadata"]["labels"] = json!({});
    }
    resource["metadata"]["labels"][OWNER_LABEL] = json!(owner.uid);
    if !external && resource["metadata"]["annotations"]["flame.xflops.io/retain"] != "true" {
        resource["metadata"]["ownerReferences"] = json!([{"apiVersion":owner.api_version,"kind":owner.kind,"name":owner.name,"uid":owner.uid,"controller":true,"blockOwnerDeletion":true}]);
    }
    let result = api
        .patch(
            &name,
            &PatchParams::apply("flame-operator").force(),
            &Patch::Apply(&resource),
        )
        .await?;
    Ok(serde_json::to_value(result)?)
}

pub fn condition_ready(resource: &Value, condition_type: &str) -> bool {
    resource["status"]["conditions"]
        .as_array()
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition["type"] == condition_type
                    && condition["status"] == "True"
                    && condition["observedGeneration"]
                        .as_i64()
                        .is_none_or(|observed| {
                            observed >= resource["metadata"]["generation"].as_i64().unwrap_or(0)
                        })
            })
        })
}

pub fn deployment_ready(resource: &Value) -> bool {
    let wanted = resource["spec"]["replicas"].as_i64().unwrap_or(1);
    resource["status"]["observedGeneration"]
        .as_i64()
        .unwrap_or(0)
        >= resource["metadata"]["generation"].as_i64().unwrap_or(1)
        && resource["status"]["updatedReplicas"].as_i64().unwrap_or(0) >= wanted
        && resource["status"]["availableReplicas"]
            .as_i64()
            .unwrap_or(0)
            >= wanted
}

pub fn status(
    previous: Option<&OperatorStatus>,
    generation: i64,
    ready: bool,
    reason: &str,
    message: &str,
    versions: BTreeMap<String, String>,
    endpoints: BTreeMap<String, String>,
) -> OperatorStatus {
    let state = if ready { "True" } else { "False" };
    let transition = previous
        .and_then(|status| {
            status
                .conditions
                .iter()
                .find(|condition| condition.r#type == "Ready" && condition.status == state)
        })
        .map(|condition| condition.last_transition_time.clone())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    OperatorStatus {
        observed_generation: generation,
        conditions: vec![Condition {
            r#type: "Ready".into(),
            status: state.into(),
            reason: reason.into(),
            message: message.into(),
            observed_generation: generation,
            last_transition_time: transition,
        }],
        resolved_versions: versions,
        endpoints,
        client_secret: None,
        control_plane_ca: None,
    }
}

pub async fn apply_gateway_policy(
    client: Client,
    resource: Value,
    owner: &Owner,
    adopt: bool,
) -> Result<Value, Error> {
    if resource["kind"] != "NetworkPolicy" || owner.kind != "FlameCluster" {
        return Err(Error::Invalid(
            "external ownership is restricted to Flame gateway policies".into(),
        ));
    }
    apply_managed(client, resource, owner, adopt, true).await
}

pub async fn prune(client: Client, owner: &Owner, desired: &[Value]) -> Result<(), Error> {
    let wanted: BTreeSet<_> = desired
        .iter()
        .filter_map(|v| {
            Some((
                v["apiVersion"].as_str()?.to_string(),
                v["kind"].as_str()?.to_string(),
                v["metadata"]["name"].as_str()?.to_string(),
            ))
        })
        .collect();
    for (api_version, kind) in [
        ("v1", "ConfigMap"),
        ("v1", "Service"),
        ("apps/v1", "Deployment"),
        ("discovery.k8s.io/v1", "EndpointSlice"),
        ("gateway.networking.k8s.io/v1", "HTTPRoute"),
        ("gateway.networking.k8s.io/v1", "BackendTLSPolicy"),
        ("gateway.networking.k8s.io/v1", "Gateway"),
        ("gateway.envoyproxy.io/v1alpha1", "EnvoyProxy"),
        ("gateway.envoyproxy.io/v1alpha1", "ClientTrafficPolicy"),
        ("cert-manager.io/v1", "Certificate"),
    ] {
        let api = dynamic_api(
            client.clone(),
            api_version,
            kind,
            owner.namespace.as_deref(),
        )?;
        let objects = api
            .list(&kube::api::ListParams::default().labels(&format!("{OWNER_LABEL}={}", owner.uid)))
            .await?;
        for object in objects {
            let controlled = object
                .metadata
                .owner_references
                .as_ref()
                .is_some_and(|refs| {
                    refs.iter()
                        .any(|r| r.controller == Some(true) && r.uid == owner.uid)
                });
            if controlled && !wanted.contains(&(api_version.into(), kind.into(), object.name_any()))
            {
                api.delete(&object.name_any(), &Default::default()).await?;
            }
        }
    }
    Ok(())
}

pub async fn prune_gateway_policies(
    client: Client,
    owner: &Owner,
    desired: Option<(&str, &str)>,
) -> Result<(), Error> {
    use k8s_openapi::api::networking::v1::NetworkPolicy;
    use kube::api::{DeleteParams, Preconditions};
    let policies = Api::<NetworkPolicy>::all(client.clone())
        .list(&kube::api::ListParams::default().labels(&format!("{OWNER_LABEL}={}", owner.uid)))
        .await?;
    for policy in policies {
        let name = policy.name_any();
        let namespace = policy.namespace().expect("namespaced policy");
        if desired == Some((namespace.as_str(), name.as_str())) {
            continue;
        }
        check_owner(&serde_json::to_value(&policy)?, owner, false)?;
        Api::<NetworkPolicy>::namespaced(client.clone(), &namespace)
            .delete(
                &name,
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: policy.uid(),
                        resource_version: policy.resource_version(),
                    }),
                    ..Default::default()
                },
            )
            .await?;
    }
    Ok(())
}
