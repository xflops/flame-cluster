//! Local validation shared by the controller and host worker reconciler.
use crate::{CacheEndpoint, FlameClusterSpec, FlameWorkerSpec};
use std::net::{Ipv4Addr, SocketAddrV4};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ValidationError(pub String);

fn invalid(message: &str) -> ValidationError {
    ValidationError(message.into())
}

pub fn lan_ipv4(address: &str) -> Result<Ipv4Addr, ValidationError> {
    let ip = address
        .parse::<Ipv4Addr>()
        .map_err(|_| invalid("cache endpoints require LAN IPv4 addresses"))?;
    if !ip.is_private() {
        return Err(invalid("cache endpoints require RFC 1918 addresses"));
    }
    Ok(ip)
}

pub fn cache_endpoint(endpoint: &CacheEndpoint) -> Result<(), ValidationError> {
    if endpoint.name.is_empty() {
        return Err(invalid("cache endpoint names must be nonempty"));
    }
    lan_ipv4(&endpoint.address)?;
    if endpoint.cache_port == 0 {
        return Err(invalid("cache endpoints require nonzero ports"));
    }
    Ok(())
}

/// The bare-metal protocol connects to the next port for worker backend traffic.
pub fn frontend_endpoint(value: &str) -> Result<SocketAddrV4, ValidationError> {
    let endpoint = value
        .parse::<SocketAddrV4>()
        .map_err(|_| invalid("endpoint must be a LAN IPv4 ip:port"))?;
    if !endpoint.ip().is_private() || !(30000..32767).contains(&endpoint.port()) {
        return Err(invalid(
            "endpoint requires an RFC 1918 IP and frontend NodePort in 30000..32766; backend uses port + 1",
        ));
    }
    Ok(endpoint)
}

pub fn worker_endpoint(
    worker: &FlameWorkerSpec,
    cluster: &FlameClusterSpec,
) -> Result<SocketAddrV4, ValidationError> {
    let endpoint = worker.endpoint.as_deref().unwrap_or(&cluster.endpoint);
    if endpoint.is_empty() {
        return Err(invalid(
            "FlameWorker.endpoint or FlameCluster.endpoint is required for bare-metal workers",
        ));
    }
    frontend_endpoint(endpoint)
}

pub fn worker_spec(spec: &FlameWorkerSpec) -> Result<(), ValidationError> {
    let cluster = &spec.cluster_ref;
    if cluster.is_empty()
        || cluster.len() > 32
        || cluster.starts_with('-')
        || cluster.ends_with('-')
        || !cluster
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(invalid(
            "clusterRef must be a FlameCluster DNS label of at most 32 characters",
        ));
    }
    cache_endpoint(&CacheEndpoint {
        name: "worker".into(),
        address: spec.address.clone(),
        cache_port: spec.cache_port,
        ready: false,
    })?;
    if let Some(endpoint) = &spec.endpoint {
        frontend_endpoint(endpoint)?;
    }
    if spec.shims.is_empty()
        || spec
            .shims
            .iter()
            .any(|shim| !matches!(shim.as_str(), "host" | "cri"))
        || spec
            .shims
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != spec.shims.len()
    {
        return Err(invalid(
            "shims must be a nonempty, unique list of host and/or cri",
        ));
    }
    if spec.max_executors == 0 {
        return Err(invalid("maxExecutors must be greater than zero"));
    }
    let Some(minor) = spec.python_version.strip_prefix("3.") else {
        return Err(invalid(
            "pythonVersion must be a Python 3 major.minor version",
        ));
    };
    if minor.is_empty() || !minor.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid(
            "pythonVersion must be a Python 3 major.minor version",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> FlameWorkerSpec {
        serde_json::from_value(json!({"clusterRef":"flame","address":"192.168.3.8"})).unwrap()
    }

    #[test]
    fn worker_defaults_are_explicit_and_do_not_install_dependencies() {
        let spec = spec();
        assert_eq!(spec.shims, ["cri", "host"]);
        assert_eq!(spec.cache_port, 9090);
        assert_eq!(spec.max_executors, 10);
        assert_eq!(spec.python_version, "3.12");
        assert!(!spec.manage_object_cache);
        assert!(!spec.disabled);
        worker_spec(&spec).unwrap();
    }

    #[test]
    fn unsafe_or_unusable_worker_configuration_is_rejected() {
        for update in [
            json!({"address":"100.64.0.1"}),
            json!({"address":"localhost"}),
            json!({"cachePort":0}),
            json!({"endpoint":"http://192.168.3.1:30080"}),
            json!({"endpoint":"192.168.3.1:32767"}),
            json!({"clusterRef":"../flame"}),
            json!({"shims":[]}),
            json!({"shims":["host","host"]}),
            json!({"shims":["unknown"]}),
            json!({"maxExecutors":0}),
            json!({"pythonVersion":"3.12/bin/python"}),
        ] {
            let mut value = serde_json::to_value(spec()).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .extend(update.as_object().unwrap().clone());
            assert!(
                worker_spec(&serde_json::from_value(value).unwrap()).is_err(),
                "accepted {update}"
            );
        }
    }

    #[test]
    fn bare_metal_endpoint_is_explicit_and_worker_override_takes_precedence() {
        let mut worker = spec();
        let mut cluster: FlameClusterSpec =
            serde_json::from_value(json!({"gateway":{"hostname":"flame.example.test"}})).unwrap();
        assert!(worker_endpoint(&worker, &cluster).is_err());
        cluster.endpoint = "192.168.3.1:30080".into();
        assert_eq!(
            worker_endpoint(&worker, &cluster).unwrap().to_string(),
            cluster.endpoint
        );
        worker.endpoint = Some("192.168.3.2:30082".into());
        assert_eq!(
            worker_endpoint(&worker, &cluster).unwrap().to_string(),
            "192.168.3.2:30082"
        );
    }
}

/// Enrollment credentials always verify the externally reachable API over TLS.
pub fn kubernetes_endpoint(value: &str) -> Result<String, ValidationError> {
    let uri: http::Uri = value.parse()
        .map_err(|_| invalid("kubernetesEndpoint must be an HTTPS API authority without credentials, path, query, or fragment"))?;
    if uri.scheme_str() != Some("https")
        || uri
            .authority()
            .is_none_or(|authority| authority.as_str().contains('@'))
        || uri.host().is_none_or(str::is_empty)
        || uri.path() != "/"
        || uri.query().is_some()
        || value.contains('#')
    {
        return Err(invalid(
            "kubernetesEndpoint must be an HTTPS API authority without credentials, path, query, or fragment",
        ));
    }
    let authority = uri.authority().expect("validated authority").as_str();
    let port = if authority.starts_with('[') {
        let (host, suffix) = authority
            .split_once(']')
            .ok_or_else(|| invalid("kubernetesEndpoint has an invalid IPv6 authority"))?;
        host.trim_start_matches('[')
            .parse::<std::net::Ipv6Addr>()
            .map_err(|_| invalid("kubernetesEndpoint has an invalid IPv6 authority"))?;
        if suffix.is_empty() {
            None
        } else {
            Some(
                suffix
                    .strip_prefix(':')
                    .ok_or_else(|| invalid("kubernetesEndpoint has an invalid authority"))?,
            )
        }
    } else {
        authority.split_once(':').map(|(_, port)| port)
    };
    if let Some(port) = port
        && (port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || port.parse::<u16>().ok().is_none_or(|port| port == 0))
    {
        return Err(invalid(
            "kubernetesEndpoint requires a valid nonzero HTTPS port",
        ));
    }
    Ok(value.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod enrollment_tests {
    use super::*;

    #[test]
    fn api_authority_is_https_without_credential_or_tls_bypass_paths() {
        for endpoint in [
            "https://192.168.3.1:6443",
            "https://api.example.test",
            "https://[fd00::1]:6443/",
        ] {
            assert!(kubernetes_endpoint(endpoint).is_ok(), "rejected {endpoint}");
        }
        for endpoint in [
            "http://192.168.3.1:6443",
            "https://user:secret@api.example.test",
            "https://api.example.test/path",
            "https://api.example.test/?insecure=true",
            "https://api.example.test#fragment",
            "https://api.example.test:0",
            "https://api.example.test:99999",
            "https://api.example.test:",
            "https://api.example.test:https",
            "https://[not-ipv6]:6443",
        ] {
            assert!(
                kubernetes_endpoint(endpoint).is_err(),
                "accepted {endpoint}"
            );
        }
    }
}
