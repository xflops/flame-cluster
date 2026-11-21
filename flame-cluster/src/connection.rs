//! Explicit Flame connections with optional exact, namespaced enrollment Secret reads.
use crate::rpc::frontend_client::FrontendClient;
use axum::{
    Json,
    http::{StatusCode, Uri},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::PathBuf, time::Duration};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

pub type ApiError = (StatusCode, Json<Value>);
pub type ConfigError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileConnection {
    id: String,
    namespace: Option<String>,
    name: Option<String>,
    endpoint: String,
    display_name: Option<String>,
    ca_file: Option<PathBuf>,
    cert_file: Option<PathBuf>,
    key_file: Option<PathBuf>,
    tls_domain: Option<String>,
    join_token_file: Option<PathBuf>,
    join_expiry_file: Option<PathBuf>,
    join_namespace: Option<String>,
    join_secret: Option<String>,
    client_config_secret: Option<String>,
}

pub(crate) fn validate_connection_id(id: &str) -> Result<(), ConfigError> {
    if id.is_empty()
        || id.len() > 63
        || !id.as_bytes()[0].is_ascii_alphanumeric()
        || !id.as_bytes()[id.len() - 1].is_ascii_alphanumeric()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(
            "connection IDs must be DNS labels of at most 63 lowercase letters, digits, or hyphens"
                .into(),
        );
    }
    Ok(())
}

/// Load the ordered connection catalog, retaining the single-endpoint environment mode.
pub fn load_connections() -> Result<Vec<(String, ConnectionConfig)>, ConfigError> {
    connections_from_values(|name| std::env::var(name).ok())
}

fn connections_from_values(
    get: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, ConnectionConfig)>, ConfigError> {
    let Some(path) = get("FLAME_CONNECTIONS_FILE") else {
        return Ok(vec![("flame".into(), ConnectionConfig::from_values(get)?)]);
    };
    let entries: Vec<FileConnection> = serde_json::from_slice(&std::fs::read(path)?)?;
    if entries.is_empty() {
        return Err("FLAME_CONNECTIONS_FILE must contain at least one connection".into());
    }
    let mut ids = BTreeSet::new();
    entries
        .into_iter()
        .map(|entry| {
            validate_connection_id(&entry.id)?;
            if !ids.insert(entry.id.clone()) {
                return Err("FLAME_CONNECTIONS_FILE contains duplicate connection IDs".into());
            }
            let mut config = ConnectionConfig::new(entry.endpoint);
            config.namespace = entry.namespace;
            config.name = entry.name;
            config.display_name = entry.display_name.unwrap_or_else(|| entry.id.clone());
            config.ca_file = entry.ca_file;
            config.cert_file = entry.cert_file;
            config.key_file = entry.key_file;
            config.tls_domain = entry.tls_domain;
            config.join_token_file = entry.join_token_file;
            config.join_expiry_file = entry.join_expiry_file;
            config.join_namespace = entry.join_namespace;
            config.join_secret = entry.join_secret;
            config.client_config_secret = entry.client_config_secret;
            if let Some(hosts) = get("CONSOLE_ALLOWED_HOSTS") {
                config.allowed_hosts = hosts
                    .split(',')
                    .map(|host| host.trim().to_owned())
                    .collect();
            }
            config.validate()?;
            Ok((entry.id, config))
        })
        .collect()
}

pub(crate) fn upstream(_error: impl std::fmt::Display) -> ApiError {
    tracing::warn!("Flame upstream request failed");
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({"error":"Flame upstream request failed"})),
    )
}

#[derive(Clone, Debug)]
pub struct ConnectionConfig {
    pub namespace: Option<String>,
    pub name: Option<String>,
    pub endpoint: String,
    pub display_name: String,
    pub ca_file: Option<PathBuf>,
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
    pub tls_domain: Option<String>,
    pub allowed_hosts: Vec<String>,
    pub join_token_file: Option<PathBuf>,
    pub join_expiry_file: Option<PathBuf>,
    pub join_namespace: Option<String>,
    pub join_secret: Option<String>,
    pub client_config_secret: Option<String>,
}

impl ConnectionConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            namespace: None,
            name: None,
            endpoint: endpoint.into(),
            display_name: "Flame".into(),
            ca_file: None,
            cert_file: None,
            key_file: None,
            tls_domain: None,
            allowed_hosts: vec!["localhost".into(), "127.0.0.1".into(), "::1".into()],
            join_token_file: None,
            join_expiry_file: None,
            join_namespace: None,
            join_secret: None,
            client_config_secret: None,
        }
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_values(|name| std::env::var(name).ok())
    }

    fn from_values(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let endpoint = get("FLAME_ENDPOINT")
            .filter(|value| !value.is_empty())
            .ok_or("FLAME_ENDPOINT is required (http://host:port or https://host:port)")?;
        let mut config = Self::new(endpoint);
        config.display_name = get("FLAME_NAME").unwrap_or_else(|| "Flame".into());
        config.ca_file = get("FLAME_CA_FILE").map(PathBuf::from);
        config.cert_file = get("FLAME_CERT_FILE").map(PathBuf::from);
        config.key_file = get("FLAME_KEY_FILE").map(PathBuf::from);
        config.tls_domain = get("FLAME_TLS_DOMAIN");
        config.join_token_file = get("FLAME_JOIN_TOKEN_FILE").map(PathBuf::from);
        config.join_expiry_file = get("FLAME_JOIN_EXPIRY_FILE").map(PathBuf::from);
        if let Some(hosts) = get("CONSOLE_ALLOWED_HOSTS") {
            config.allowed_hosts = hosts
                .split(',')
                .map(|host| host.trim().to_owned())
                .collect();
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for value in [
            &self.namespace,
            &self.name,
            &self.join_namespace,
            &self.join_secret,
            &self.client_config_secret,
        ]
        .into_iter()
        .flatten()
        {
            validate_connection_id(value)?;
        }
        if self.client_config_secret.is_some() && self.namespace.is_none() {
            return Err("clientConfigSecret requires an explicit namespace".into());
        }
        if self.join_namespace.is_some() != self.join_secret.is_some() {
            return Err("joinNamespace and joinSecret must be configured together".into());
        }
        if self.join_namespace.is_some()
            && (self.join_token_file.is_some() || self.join_expiry_file.is_some())
        {
            return Err(
                "configure either a namespaced join Secret or local join files, not both".into(),
            );
        }
        if let (Some(namespace), Some(join_namespace)) = (&self.namespace, &self.join_namespace)
            && namespace != join_namespace
        {
            return Err("a Flame's join Secret must belong to its configured namespace".into());
        }
        if self.join_token_file.is_some() != self.join_expiry_file.is_some() {
            return Err(
                "FLAME_JOIN_TOKEN_FILE and FLAME_JOIN_EXPIRY_FILE must be configured together"
                    .into(),
            );
        }
        if self.allowed_hosts.is_empty() {
            return Err("CONSOLE_ALLOWED_HOSTS must contain at least one hostname".into());
        }
        for host in &self.allowed_hosts {
            allowed_host(host)?;
        }
        let uri: Uri = self.endpoint.parse()?;
        if !matches!(uri.scheme_str(), Some("http" | "https"))
            || uri.authority().is_none()
            || uri
                .authority()
                .is_some_and(|value| value.as_str().contains('@'))
            || uri.path() != "/"
            || uri.query().is_some()
        {
            return Err("FLAME_ENDPOINT must be an http:// or https:// authority without credentials, a path, or a query".into());
        }
        if self.cert_file.is_some() != self.key_file.is_some() {
            return Err("FLAME_CERT_FILE and FLAME_KEY_FILE must be configured together".into());
        }
        let has_tls = self.ca_file.is_some()
            || self.cert_file.is_some()
            || self.key_file.is_some()
            || self.tls_domain.is_some();
        if uri.scheme_str() == Some("http") && has_tls {
            return Err("TLS settings require an https:// FLAME_ENDPOINT".into());
        }
        if self.tls_domain.as_ref().is_some_and(|name| name.is_empty()) {
            return Err("FLAME_TLS_DOMAIN must not be empty".into());
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct AppState {
    pub(crate) client: FrontendClient<Channel>,
    pub(crate) endpoint: String,
    pub(crate) display_name: String,
    pub(crate) namespace: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) allowed_hosts: Vec<String>,
    pub(crate) join_token_file: Option<PathBuf>,
    pub(crate) join_expiry_file: Option<PathBuf>,
    pub(crate) join_namespace: Option<String>,
    pub(crate) join_secret: Option<String>,
    pub(crate) client_config_secret: Option<String>,
    pub(crate) kube_client: Option<kube::Client>,
}

impl AppState {
    pub fn connect(config: ConnectionConfig) -> Result<Self, ConfigError> {
        Self::connect_with_kubernetes(config, None)
    }

    pub fn connect_with_kubernetes(
        config: ConnectionConfig,
        kube_client: Option<kube::Client>,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        if (config.namespace.is_some()
            || config.join_secret.is_some()
            || config.client_config_secret.is_some())
            && kube_client.is_none()
        {
            return Err(
                "a Kubernetes client is required for configured worker and Secret sources".into(),
            );
        }
        let mut endpoint = Endpoint::from_shared(config.endpoint.clone())?
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(12));
        if config.endpoint.starts_with("https://") {
            let mut tls = ClientTlsConfig::new();
            if let Some(path) = &config.ca_file {
                tls = tls.ca_certificate(Certificate::from_pem(std::fs::read(path)?));
            } else {
                tls = tls.with_native_roots();
            }
            if let (Some(cert), Some(key)) = (&config.cert_file, &config.key_file) {
                tls = tls.identity(Identity::from_pem(
                    std::fs::read(cert)?,
                    std::fs::read(key)?,
                ));
            }
            if let Some(domain) = &config.tls_domain {
                tls = tls.domain_name(domain);
            }
            endpoint = endpoint.tls_config(tls)?;
        }
        Ok(Self {
            client: FrontendClient::new(endpoint.connect_lazy()),
            endpoint: config.endpoint,
            display_name: config.display_name,
            namespace: config.namespace,
            name: config.name,
            join_token_file: config.join_token_file,
            join_expiry_file: config.join_expiry_file,
            join_namespace: config.join_namespace,
            join_secret: config.join_secret,
            client_config_secret: config.client_config_secret,
            kube_client,
            allowed_hosts: config
                .allowed_hosts
                .iter()
                .map(|host| allowed_host(host))
                .collect::<Result<_, _>>()?,
        })
    }
}

fn allowed_host(value: &str) -> Result<String, ConfigError> {
    if value.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok(value.to_ascii_lowercase());
    }
    let authority: axum::http::uri::Authority = value.parse()?;
    if authority.port().is_some()
        || value.contains('@')
        || value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
    {
        return Err("CONSOLE_ALLOWED_HOSTS requires exact hostnames or IP addresses without ports, wildcards, or credentials".into());
    }
    Ok(authority.host().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_file_preserves_order_and_inherits_global_hosts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connections.json");
        std::fs::write(&path, serde_json::to_vec(&json!([
            {"id":"flame-2","endpoint":"https://second:8080","displayName":"Second","caFile":"/trust/second.crt","certFile":"/identity/second.crt","keyFile":"/identity/second.key","tlsDomain":"second.internal","joinTokenFile":"/join/second/token","joinExpiryFile":"/join/second/expiresAt"},
            {"id":"flame-0","endpoint":"http://first:8080"}
        ])).unwrap()).unwrap();
        let configs = connections_from_values(|name| match name {
            "FLAME_CONNECTIONS_FILE" => Some(path.to_str().unwrap().into()),
            "CONSOLE_ALLOWED_HOSTS" => Some("localhost,ui.internal".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(
            configs
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["flame-2", "flame-0"]
        );
        let (_, second) = &configs[0];
        assert_eq!(second.display_name, "Second");
        assert_eq!(second.ca_file, Some("/trust/second.crt".into()));
        assert_eq!(second.cert_file, Some("/identity/second.crt".into()));
        assert_eq!(second.key_file, Some("/identity/second.key".into()));
        assert_eq!(second.tls_domain.as_deref(), Some("second.internal"));
        assert_eq!(second.join_token_file, Some("/join/second/token".into()));
        assert_eq!(
            second.join_expiry_file,
            Some("/join/second/expiresAt".into())
        );
        for (_, config) in configs {
            assert_eq!(config.allowed_hosts, ["localhost", "ui.internal"]);
        }
    }

    #[test]
    fn malformed_empty_and_ambiguous_connection_files_fail_loading() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connections.json");
        for contents in [
            "not JSON",
            "[]",
            "{}",
            r#"[{"id":"","endpoint":"http://flm:8080"}]"#,
            r#"[{"id":"Flame","endpoint":"http://flm:8080"}]"#,
            r#"[{"id":"-flame","endpoint":"http://flm:8080"}]"#,
            r#"[{"id":"flame/other","endpoint":"http://flm:8080"}]"#,
            r#"[{"id":"flame","endpoint":"http://flm:8080"},{"id":"flame","endpoint":"http://other:8080"}]"#,
            r#"[{"id":"flame","endpoint":"http://user:secret@flm:8080"}]"#,
            r#"[{"id":"flame","endpoint":"http://flm:8080","caFile":"/ca.crt"}]"#,
            r#"[{"id":"flame","endpoint":"https://flm:8080","certFile":"/cert.pem"}]"#,
            r#"[{"id":"flame","endpoint":"http://flm:8080","joinTokenFile":"/token"}]"#,
            r#"[{"id":"flame","endpoint":"http://flm:8080","allowedHosts":["*"]}]"#,
            r#"[{"id":"flame","endpoint":"http://flm:8080","joinNamespace":"flame-1"}]"#,
            r#"[{"id":"flame","endpoint":"http://flm:8080","joinNamespace":"flame-1","joinSecret":"flame-join","joinTokenFile":"/token","joinExpiryFile":"/expiry"}]"#,
            r#"[{"id":"flame","endpoint":"http://flm:8080","namespace":"flame-1","joinNamespace":"flame-2","joinSecret":"flame-join"}]"#,
            r#"[{"id":"flame","endpoint":"http://flm:8080","joinNamespace":"../other","joinSecret":"flame-join"}]"#,
            r#"[{"id":"flame","endpoint":"http://flm:8080","clientConfigSecret":"flame-client"}]"#,
            r#"[{"id":"flame","endpoint":"http://flm:8080","namespace":"","clientConfigSecret":"flame-client"}]"#,
            r#"[{"id":"flame","endpoint":"http://flm:8080","namespace":"flame-1","clientConfigSecret":"../client"}]"#,
        ] {
            std::fs::write(&path, contents).unwrap();
            assert!(
                connections_from_values(|name| (name == "FLAME_CONNECTIONS_FILE")
                    .then(|| path.to_str().unwrap().into()))
                .is_err(),
                "accepted {contents}"
            );
        }
        std::fs::remove_file(&path).unwrap();
        assert!(
            connections_from_values(
                |name| (name == "FLAME_CONNECTIONS_FILE").then(|| path.to_str().unwrap().into())
            )
            .is_err()
        );
    }

    #[test]
    fn legacy_connection_environment_remains_available() {
        let configs = connections_from_values(|name| match name {
            "FLAME_ENDPOINT" => Some("http://flm:8080".into()),
            "FLAME_NAME" => Some("Training".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].0, "flame");
        assert_eq!(configs[0].1.display_name, "Training");
    }

    #[test]
    fn managed_connection_files_retain_namespaces_and_exact_join_sources() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connections.json");
        std::fs::write(&path, serde_json::to_vec(&json!([
            {"id":"flame-1","namespace":"flame-1","name":"flame-1","displayName":"flame-1/flame-1","endpoint":"https://flame-1-session-manager.flame-1.svc:8080","joinNamespace":"flame-1","joinSecret":"flame-1-join","clientConfigSecret":"flame-1-client"},
            {"id":"flame-2","namespace":"flame-2","name":"flame-2","displayName":"flame-2/flame-2","endpoint":"https://flame-2-session-manager.flame-2.svc:8080","joinNamespace":"flame-2","joinSecret":"flame-2-join","clientConfigSecret":"flame-2-client"}
        ])).unwrap()).unwrap();
        let configs = connections_from_values(|name| {
            (name == "FLAME_CONNECTIONS_FILE").then(|| path.to_str().unwrap().into())
        })
        .unwrap();
        assert_eq!(configs.len(), 2);
        for (index, (id, config)) in configs.into_iter().enumerate() {
            let number = index + 1;
            assert_eq!(id, format!("flame-{number}"));
            assert_eq!(config.namespace, Some(format!("flame-{number}")));
            assert_eq!(config.join_namespace, config.namespace);
            assert_eq!(config.join_secret, Some(format!("flame-{number}-join")));
            assert_eq!(
                config.client_config_secret,
                Some(format!("flame-{number}-client"))
            );
            assert!(config.join_token_file.is_none());
        }
    }
    #[tokio::test]
    async fn allowed_hosts_reject_ambiguous_or_unrestricted_configuration() {
        for hosts in [
            "",
            "*",
            "*.example",
            "https://console.example",
            "console.example:443",
            "user@host",
            "localhost,,example",
        ] {
            assert!(
                ConnectionConfig::from_values(|name| match name {
                    "FLAME_ENDPOINT" => Some("http://127.0.0.1:8080".into()),
                    "CONSOLE_ALLOWED_HOSTS" => Some(hosts.into()),
                    _ => None,
                })
                .is_err(),
                "accepted {hosts}"
            );
        }
        let config = ConnectionConfig::from_values(|name| match name {
            "FLAME_ENDPOINT" => Some("http://127.0.0.1:8080".into()),
            "CONSOLE_ALLOWED_HOSTS" => Some("localhost, Console.Example, ::1".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(
            AppState::connect(config).unwrap().allowed_hosts,
            ["localhost", "console.example", "::1"]
        );
    }

    #[test]
    fn requires_endpoint_and_rejects_ambiguous_tls_settings() {
        assert!(ConnectionConfig::from_values(|_| None).is_err());
        for endpoint in [
            "",
            "unix:///socket",
            "http://name:secret@host",
            "http://host/path",
            "http://host?token=secret",
        ] {
            assert!(
                ConnectionConfig::new(endpoint).validate().is_err(),
                "accepted {endpoint}"
            );
        }
        let mut config = ConnectionConfig::new("http://127.0.0.1:8080");
        config.ca_file = Some("ca.pem".into());
        assert!(config.validate().is_err());
        config.endpoint = "https://flame.example".into();
        assert!(config.validate().is_ok());
        config.cert_file = Some("client.pem".into());
        assert!(config.validate().is_err());
        config.key_file = Some("client.key".into());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn join_files_must_be_configured_together() {
        assert!(
            ConnectionConfig::from_values(|name| match name {
                "FLAME_ENDPOINT" => Some("http://127.0.0.1:8080".into()),
                "FLAME_JOIN_TOKEN_FILE" => Some("/etc/flame/join/token".into()),
                _ => None,
            })
            .is_err()
        );
        let config = ConnectionConfig::from_values(|name| match name {
            "FLAME_ENDPOINT" => Some("http://127.0.0.1:8080".into()),
            "FLAME_JOIN_TOKEN_FILE" => Some("/etc/flame/join/token".into()),
            "FLAME_JOIN_EXPIRY_FILE" => Some("/etc/flame/join/expiresAt".into()),
            _ => None,
        })
        .unwrap();
        assert!(config.join_token_file.is_some());
        assert!(config.join_expiry_file.is_some());
    }
}
