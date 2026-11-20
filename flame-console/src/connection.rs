//! A single explicitly configured Flame Frontend connection. No Kubernetes access.
use crate::rpc::frontend_client::FrontendClient;
use axum::{
    Json,
    http::{StatusCode, Uri},
};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

pub type ApiError = (StatusCode, Json<Value>);
pub type ConfigError = Box<dyn std::error::Error + Send + Sync>;

pub(crate) fn upstream(_error: impl std::fmt::Display) -> ApiError {
    tracing::warn!("Flame upstream request failed");
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({"error":"Flame upstream request failed"})),
    )
}

#[derive(Clone, Debug)]
pub struct ConnectionConfig {
    pub endpoint: String,
    pub display_name: String,
    pub ca_file: Option<PathBuf>,
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
    pub tls_domain: Option<String>,
    pub allowed_hosts: Vec<String>,
}

impl ConnectionConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            display_name: "Flame".into(),
            ca_file: None,
            cert_file: None,
            key_file: None,
            tls_domain: None,
            allowed_hosts: vec!["localhost".into(), "127.0.0.1".into(), "::1".into()],
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
    pub(crate) allowed_hosts: Vec<String>,
}

impl AppState {
    pub fn connect(config: ConnectionConfig) -> Result<Self, ConfigError> {
        config.validate()?;
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
}
