//! Short-lived cluster enrollment credentials; joined worker identities stay independent.
use crate::{
    Error,
    api::FlameCluster,
    cluster::{self, Owner},
    digest,
    enrollment::CREDENTIAL_LABEL,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::{Secret, ServiceAccount};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, PostParams, Preconditions},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Read;

pub const LIFETIME_SECONDS: i64 = 900;
const CA_FILE: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";
#[derive(Clone)]
pub struct Service {
    pub namespace: String,
    pub name: String,
}
impl Service {
    pub fn from_env() -> Result<Self, Error> {
        Ok(Self {
            namespace: std::env::var("OPERATOR_NAMESPACE")
                .map_err(|_| Error::Invalid("OPERATOR_NAMESPACE is required".into()))?,
            name: std::env::var("OPERATOR_SERVICE_NAME")
                .map_err(|_| Error::Invalid("OPERATOR_SERVICE_NAME is required".into()))?,
        })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub namespace: String,
    pub cluster: String,
    pub service_namespace: String,
    pub service: String,
    pub kubeconfig: String,
}
fn base_name(cluster: &FlameCluster) -> Result<String, Error> {
    let uid = cluster
        .uid()
        .ok_or_else(|| Error::Invalid("cluster UID is missing".into()))?;
    Ok(format!("flame-join-{}", &digest(&uid)?[..20]))
}
pub fn secret_name(cluster: &FlameCluster) -> String {
    format!("{}-join", cluster.name_any())
}
pub fn resources(cluster: &FlameCluster, service: &Service) -> Result<Vec<Value>, Error> {
    let namespace = cluster
        .namespace()
        .ok_or_else(|| Error::Invalid("cluster namespace is missing".into()))?;
    let name = base_name(cluster)?;
    Ok(vec![
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":name,"namespace":namespace},"automountServiceAccountToken":false}),
        json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":name,"namespace":service.namespace},"rules":[{"apiGroups":[""],"resources":["services/proxy"],"resourceNames":[format!("http:{}:8081",service.name)],"verbs":["create"]}]}),
        json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{"name":name,"namespace":service.namespace},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":name},"subjects":[{"kind":"ServiceAccount","name":name,"namespace":namespace}]}),
    ])
}
pub fn expires(secret: &Secret) -> Option<DateTime<Utc>> {
    let value = secret.data.as_ref()?.get("expiresAt")?;
    DateTime::parse_from_rfc3339(std::str::from_utf8(&value.0).ok()?)
        .ok()
        .map(|date| date.with_timezone(&Utc))
}
pub fn token(secret: &Secret) -> Option<&[u8]> {
    Some(&secret.data.as_ref()?.get("token")?.0)
}
pub fn valid(secret: &Secret, submitted: &str, now: DateTime<Utc>) -> bool {
    let Some(expected) = token(secret) else {
        return false;
    };
    let equal = expected.len() == submitted.len()
        && expected
            .iter()
            .zip(submitted.as_bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0;
    equal && expires(secret).is_some_and(|expiry| now < expiry)
}
pub fn needs_rotation(secret: &Secret, now: DateTime<Utc>) -> bool {
    let issued = secret
        .data
        .as_ref()
        .and_then(|data| data.get("issuedAt"))
        .and_then(|value| std::str::from_utf8(&value.0).ok())
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok());
    token(secret).is_none()
        || expires(secret).is_none_or(|expiry| now >= expiry)
        || issued.is_none_or(|issued| {
            now.signed_duration_since(issued).num_seconds() >= LIFETIME_SECONDS
        })
}
pub fn kubeconfig(
    namespace: &str,
    endpoint: &str,
    ca: &[u8],
    jwt: &str,
    context: &str,
) -> Result<String, Error> {
    let server = flame_api::validation::kubernetes_endpoint(endpoint)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    if ca.is_empty() || jwt.is_empty() {
        return Err(Error::Invalid(
            "Kubernetes CA and API token must be populated".into(),
        ));
    }
    Ok(serde_json::to_string(
        &json!({"apiVersion":"v1","kind":"Config",
        "clusters":[{"name":"flame","cluster":{"server":server,"certificate-authority-data":STANDARD.encode(ca)}}],
        "users":[{"name":"flmlet","user":{"token":jwt}}],
        "contexts":[{"name":context,"context":{"cluster":"flame","user":"flmlet","namespace":namespace}}],"current-context":context}),
    )?)
}
/// Return false when enrollment is disabled by missing external API configuration.
pub async fn ensure(
    client: Client,
    cluster: &FlameCluster,
    service: &Service,
) -> Result<bool, Error> {
    let Some(endpoint) = cluster.spec.kubernetes_endpoint.as_deref() else {
        return Ok(false);
    };
    let namespace = cluster.namespace().expect("namespaced cluster");
    let owner = Owner::of(cluster)?;
    let api = Api::<Secret>::namespaced(client.clone(), &namespace);
    let current = api.get_opt(&secret_name(cluster)).await?;
    if let Some(secret) = &current {
        cluster::check_owner(&serde_json::to_value(secret)?, &owner, false)?;
    }
    for resource in resources(cluster, service)? {
        let external = resource["metadata"]["namespace"] != namespace;
        cluster::apply_managed(client.clone(), resource, &owner, false, external).await?;
    }
    let now = Utc::now();
    let config_changed = current.as_ref().is_some_and(|secret| {
        secret
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("flame.xflops.io/bootstrap-config"))
            != Some(&format!(
                "{endpoint}|{}|{}",
                service.namespace, service.name
            ))
    });
    if current
        .as_ref()
        .is_some_and(|secret| !needs_rotation(secret, now))
        && !config_changed
    {
        return Ok(true);
    }
    let response: Value=Api::<ServiceAccount>::namespaced(client.clone(),&namespace).create_subresource("token",&base_name(cluster)?,&PostParams::default(),&json!({"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest","spec":{"expirationSeconds":LIFETIME_SECONDS}})).await?;
    let jwt = response["status"]["token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| Error::Invalid("Kubernetes bootstrap token is not populated".into()))?;
    let actual_expiry = response["status"]["expirationTimestamp"]
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|date| date.with_timezone(&Utc))
        .ok_or_else(|| {
            Error::Invalid("Kubernetes bootstrap token expiry is not populated".into())
        })?;
    let expiry = actual_expiry.min(now + chrono::Duration::seconds(LIFETIME_SECONDS));
    if expiry <= now {
        return Err(Error::Invalid(
            "Kubernetes returned an expired bootstrap token".into(),
        ));
    }
    let mut randomness = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut randomness)?;
    let context = format!(
        "flame-join-{}",
        randomness
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let envelope = Envelope {
        namespace: namespace.clone(),
        cluster: cluster.name_any(),
        service_namespace: service.namespace.clone(),
        service: service.name.clone(),
        kubeconfig: kubeconfig(
            &namespace,
            endpoint,
            &std::fs::read(CA_FILE)?,
            jwt,
            &context,
        )?,
    };
    let secret = json!({"apiVersion":"v1","kind":"Secret","metadata":{"name":secret_name(cluster),"namespace":namespace,
        "labels":{CREDENTIAL_LABEL:"cluster-join"},"annotations":{"flame.xflops.io/bootstrap-config":format!("{endpoint}|{}|{}",service.namespace,service.name)}},"type":"Opaque",
        "stringData":{"token":STANDARD.encode(serde_json::to_vec(&envelope)?),"issuedAt":now.to_rfc3339(),"expiresAt":expiry.to_rfc3339()}});
    cluster::apply_owned(client, secret, &owner, false).await?;
    Ok(true)
}
/// Wake at the token expiry rather than extending its renewal interval by a controller period.
pub async fn next_delay(
    client: Client,
    cluster: &FlameCluster,
    maximum: std::time::Duration,
) -> Result<std::time::Duration, Error> {
    if cluster.spec.kubernetes_endpoint.is_none() {
        return Ok(maximum);
    }
    let secret =
        Api::<Secret>::namespaced(client, &cluster.namespace().expect("namespaced cluster"))
            .get_opt(&secret_name(cluster))
            .await?;
    Ok(secret
        .as_ref()
        .and_then(expires)
        .map(|expiry| {
            expiry
                .signed_duration_since(Utc::now())
                .to_std()
                .unwrap_or(std::time::Duration::from_millis(1))
                .min(maximum)
        })
        .unwrap_or(maximum))
}
/// Cross-namespace RBAC cannot use namespaced owner references; the cluster finalizer removes it.
pub async fn cleanup(
    client: Client,
    cluster: &FlameCluster,
    service: &Service,
) -> Result<(), Error> {
    let owner = Owner::of(cluster)?;
    for resource in resources(cluster, service)? {
        let namespace = resource["metadata"]["namespace"].as_str().unwrap();
        if Some(namespace) == owner.namespace.as_deref() {
            continue;
        }
        let api = cluster::dynamic_api(
            client.clone(),
            resource["apiVersion"].as_str().unwrap(),
            resource["kind"].as_str().unwrap(),
            Some(namespace),
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expires_at_boundary_and_rejects_old_token() {
        let now = Utc::now();
        let secret:Secret=serde_json::from_value(json!({"data":{"token":STANDARD.encode("new"),"issuedAt":STANDARD.encode(now.to_rfc3339()),"expiresAt":STANDARD.encode((now+chrono::Duration::seconds(900)).to_rfc3339())}})).unwrap();
        assert!(!needs_rotation(
            &secret,
            now + chrono::Duration::seconds(899)
        ));
        assert!(needs_rotation(
            &secret,
            now + chrono::Duration::seconds(900)
        ));
        assert!(valid(&secret, "new", now));
        assert!(!valid(&secret, "old", now));
        assert!(!valid(&secret, "new", now + chrono::Duration::seconds(900)));
    }
    #[test]
    fn bootstrap_rights_are_only_named_service_proxy() {
        let mut cluster = FlameCluster::new(
            "test",
            serde_json::from_value(json!({"gateway":{"hostname":"flame.test"}})).unwrap(),
        );
        cluster.metadata.namespace = Some("workers".into());
        cluster.metadata.uid = Some("uid".into());
        let resources = resources(
            &cluster,
            &Service {
                namespace: "operator".into(),
                name: "custom-operator".into(),
            },
        )
        .unwrap();
        assert_eq!(
            resources[1]["rules"],
            json!([{"apiGroups":[""],"resources":["services/proxy"],"resourceNames":["http:custom-operator:8081"],"verbs":["create"]}])
        );
        assert_eq!(resources[2]["subjects"][0]["namespace"], "workers");
    }
}
