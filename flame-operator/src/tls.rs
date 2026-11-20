//! Only verified public CA certificates may be distributed in cluster status.
use crate::Error;
use base64::{Engine, engine::general_purpose::STANDARD};

pub fn public_ca(pem: &str) -> Result<String, Error> {
    let invalid = || {
        Error::Invalid(
            "control-plane CA must contain only valid public CERTIFICATE PEM blocks".into(),
        )
    };
    let mut rest = pem.trim();
    let mut roots = rustls::RootCertStore::empty();
    while !rest.is_empty() {
        rest = rest
            .strip_prefix("-----BEGIN CERTIFICATE-----")
            .ok_or_else(invalid)?;
        let (body, tail) = rest
            .split_once("-----END CERTIFICATE-----")
            .ok_or_else(invalid)?;
        let encoded: String = body
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect();
        let der = STANDARD.decode(encoded).map_err(|_| invalid())?;
        roots
            .add(rustls::pki_types::CertificateDer::from(der))
            .map_err(|_| invalid())?;
        rest = tail.trim();
    }
    if roots.is_empty() {
        return Err(invalid());
    }
    Ok(pem.to_owned())
}

/// Preserve strict IP verification for declared alternate worker frontend nodes.
pub fn server_addresses(
    cluster: &crate::api::FlameCluster,
    workers: &[crate::api::FlameWorker],
) -> Result<Vec<String>, Error> {
    use kube::ResourceExt;
    let mut addresses = std::collections::BTreeSet::new();
    let endpoints = std::iter::once(cluster.spec.endpoint.as_str())
        .filter(|endpoint| !endpoint.is_empty())
        .chain(
            workers
                .iter()
                .filter(|worker| {
                    worker.namespace() == cluster.namespace()
                        && worker.spec.cluster_ref == cluster.name_any()
                })
                .filter_map(|worker| worker.spec.endpoint.as_deref()),
        );
    for endpoint in endpoints {
        let address = flame_api::validation::frontend_endpoint(endpoint)
            .map_err(|error| Error::Invalid(error.to_string()))?;
        addresses.insert(address.ip().to_string());
    }
    Ok(addresses.into_iter().collect())
}

/// Rollouts depend exclusively on public certificate/trust bytes, never private-key material.
pub fn certificate_hash(
    secret: Option<&k8s_openapi::api::core::v1::Secret>,
    ca: Option<&str>,
) -> Result<Option<String>, Error> {
    secret
        .and_then(|secret| secret.data.as_ref())
        .and_then(|data| data.get("tls.crt"))
        .map(|certificate| crate::digest(&(certificate, ca)))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_keys_and_mislabeled_noncertificates_never_reach_public_status() {
        for value in [
            "",
            "-----BEGIN PRIVATE KEY-----\nYQ==\n-----END PRIVATE KEY-----",
            "-----BEGIN CERTIFICATE-----\nYQ==\n-----END CERTIFICATE-----",
        ] {
            assert!(public_ca(value).is_err());
        }
    }
    #[test]
    fn accepts_only_public_certificate_bundles_and_rolls_on_public_changes() {
        use k8s_openapi::{ByteString, api::core::v1::Secret};
        let pem = include_str!("../tests/fixtures/control-plane-ca.pem");
        assert_eq!(public_ca(pem).unwrap(), pem);
        assert!(
            public_ca(&format!(
                "{pem}-----BEGIN PRIVATE KEY-----\nYQ==\n-----END PRIVATE KEY-----"
            ))
            .is_err()
        );
        let mut secret = Secret {
            data: Some(
                [
                    ("tls.crt".into(), ByteString(pem.as_bytes().into())),
                    ("tls.key".into(), ByteString(b"private-key-1".to_vec())),
                ]
                .into(),
            ),
            ..Default::default()
        };
        let original = certificate_hash(Some(&secret), Some(pem)).unwrap();
        secret
            .data
            .as_mut()
            .unwrap()
            .insert("tls.key".into(), ByteString(b"private-key-2".to_vec()));
        assert_eq!(
            certificate_hash(Some(&secret), Some(pem)).unwrap(),
            original
        );
        assert_ne!(
            certificate_hash(Some(&secret), Some("different public CA")).unwrap(),
            original
        );
        secret.data.as_mut().unwrap().insert(
            "tls.crt".into(),
            ByteString(b"renewed public certificate".to_vec()),
        );
        assert_ne!(
            certificate_hash(Some(&secret), Some(pem)).unwrap(),
            original
        );
    }
    #[test]
    fn certificate_addresses_include_scoped_worker_overrides_without_duplicates() {
        use crate::api::{FlameCluster, FlameWorker};
        use serde_json::json;
        let mut cluster = FlameCluster::new(
            "flame",
            serde_json::from_value(
                json!({"endpoint":"192.168.1.2:30080","gateway":{"hostname":"flame.test"}}),
            )
            .unwrap(),
        );
        cluster.metadata.namespace = Some("team-a".into());
        let mut worker=FlameWorker::new("worker",serde_json::from_value(json!({"clusterRef":"flame","address":"192.168.1.8","endpoint":"192.168.1.3:30080"})).unwrap());
        worker.metadata.namespace = Some("team-a".into());
        let mut other = worker.clone();
        other.metadata.namespace = Some("team-b".into());
        other.spec.endpoint = Some("192.168.1.4:30080".into());
        assert_eq!(
            server_addresses(&cluster, &[worker.clone(), worker, other]).unwrap(),
            vec!["192.168.1.2", "192.168.1.3"]
        );
    }
}
