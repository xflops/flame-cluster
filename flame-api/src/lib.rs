//! Shared Kubernetes contracts consumed by the Flame controller and flmlet.

pub mod validation;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    pub r#type: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    pub observed_generation: i64,
    pub last_transition_time: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct OperatorStatus {
    pub observed_generation: i64,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub resolved_versions: BTreeMap<String, String>,
    #[serde(default)]
    pub endpoints: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Public control-plane CA certificate bundle; never includes a client identity or private key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_plane_ca: Option<String>,
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[kube(
    group = "flame.xflops.io",
    version = "v1alpha1",
    kind = "FlameCluster",
    plural = "flameclusters",
    namespaced,
    status = "OperatorStatus",
    printcolumn = r#"{"name":"Version","type":"string","jsonPath":".status.resolvedVersions.flameRelease"}"#
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FlameClusterSpec {
    /// SRE-selected release. Controllers never poll for a newer image or release.
    #[serde(default = "latest")]
    pub version: String,
    /// Optional LAN frontend ip:port advertised to SRE-managed workers.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
    /// Externally reachable HTTPS Kubernetes API used in operator-issued host credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes_endpoint: Option<String>,
    /// Frontend NodePort; the worker backend is always nodePort + 1.
    #[serde(default = "frontend_node_port")]
    pub node_port: u16,
    #[serde(default = "session_image")]
    pub image: String,
    /// Cache endpoints managed outside Kubernetes by SREs.
    #[serde(default)]
    pub cache_endpoints: Vec<CacheEndpoint>,
    #[serde(default)]
    pub rollout_revision: String,
    #[serde(default = "resource_request")]
    pub resource_request: String,
    #[serde(default = "policies")]
    pub policies: Vec<String>,
    pub gateway: Gateway,
    /// Explicit opt-in for a controlled migration from existing Helm resources.
    #[serde(default)]
    pub adopt_existing: bool,
    #[serde(default = "storage_size")]
    pub storage_size: String,
    #[serde(default)]
    pub storage_class: Option<String>,
    #[serde(default)]
    pub existing_claim: Option<String>,
    #[serde(default = "resources")]
    #[schemars(schema_with = "arbitrary_object")]
    pub resources: serde_json::Value,
    #[serde(default)]
    pub node_selector: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CacheEndpoint {
    pub name: String,
    pub address: String,
    #[serde(default = "cache_port")]
    pub cache_port: u16,
    #[serde(default = "default_true")]
    pub ready: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Gateway {
    pub hostname: String,
    #[serde(default = "gateway_class")]
    pub class_name: String,
    /// Optional advertised node IPv4 address. NodePort exposure does not require DNS resolution.
    #[serde(default)]
    pub ingress_ip: Option<String>,
    /// Public TLS listener and Kubernetes NodePort for session traffic.
    #[serde(default = "session_tls_port")]
    #[schemars(range(min = 30000, max = 32767))]
    pub session_port: u16,
    /// Public TLS listener and Kubernetes NodePort for cache traffic.
    #[serde(default = "cache_tls_port")]
    #[schemars(range(min = 30000, max = 32767))]
    pub cache_port: u16,
    #[serde(default = "envoy_namespace")]
    pub envoy_namespace: String,
    #[serde(default)]
    pub service_annotations: BTreeMap<String, String>,
    #[serde(default = "issuer")]
    pub issuer_name: String,
    #[serde(default = "default_true")]
    pub client_auth: bool,
}

pub fn default_true() -> bool {
    true
}
pub fn latest() -> String {
    "latest".into()
}

fn arbitrary_object(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    serde_json::from_value(
        serde_json::json!({"type":"object","x-kubernetes-preserve-unknown-fields":true}),
    )
    .expect("static object schema")
}
pub fn session_image() -> String {
    "docker.io/xflops/flame-session-manager:latest".into()
}
pub fn resource_request() -> String {
    "cpu=1,mem=2g".into()
}
pub fn policies() -> Vec<String> {
    vec!["priority".into(), "drf".into(), "das".into()]
}
pub fn storage_size() -> String {
    "20Gi".into()
}
pub fn frontend_node_port() -> u16 {
    30080
}
pub fn cache_port() -> u16 {
    9090
}
pub fn session_tls_port() -> u16 {
    30443
}
pub fn cache_tls_port() -> u16 {
    31443
}
pub fn gateway_class() -> String {
    "flame-envoy".into()
}
pub fn envoy_namespace() -> String {
    "envoy-gateway-system".into()
}
pub fn issuer() -> String {
    "flame-client-ca".into()
}
pub fn resources() -> serde_json::Value {
    serde_json::json!({"requests":{"cpu":"1","memory":"1Gi"},"limits":{"cpu":"4","memory":"8Gi"}})
}

/// An SRE-installed host executor manager reconciled by the local flmlet.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[kube(
    group = "flame.xflops.io",
    version = "v1alpha1",
    kind = "FlameWorker",
    plural = "flameworkers",
    namespaced,
    status = "FlameWorkerStatus",
    printcolumn = r#"{"name":"Cluster","type":"string","jsonPath":".spec.clusterRef"}"#,
    printcolumn = r#"{"name":"Registered","type":"boolean","jsonPath":".status.registered"}"#
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FlameWorkerSpec {
    /// Name of a FlameCluster in this worker's namespace.
    pub cluster_ref: String,
    /// RFC 1918 worker LAN cache address, never a control-plane lookup.
    pub address: String,
    /// Optional frontend ip:NodePort override; otherwise use FlameCluster.spec.endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default = "cache_port")]
    pub cache_port: u16,
    #[serde(default = "default_shims")]
    pub shims: Vec<String>,
    #[serde(default = "max_executors")]
    pub max_executors: u32,
    #[serde(default = "python_version")]
    pub python_version: String,
    /// Only manage the separately installed cache service when explicitly enabled.
    #[serde(default)]
    pub manage_object_cache: bool,
    #[serde(default)]
    pub rollout_revision: String,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FlameWorkerStatus {
    pub observed_generation: i64,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    pub last_heartbeat_time: String,
    pub config_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default)]
    pub registered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<NodeCapacity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pci_devices: Option<Vec<NodePciDevice>>,
    #[serde(default)]
    pub cache_ready: bool,
    #[serde(default)]
    pub fem: HostServiceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_cache: Option<HostServiceStatus>,
}

/// Hardware capacity reported by the matched Ready FLM node. Unknown values stay null.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct NodeCapacity {
    /// CPU cores.
    #[serde(default)]
    pub cpu: Option<u64>,
    /// GPU count, including zero on hosts without GPUs.
    #[serde(default)]
    pub gpu: Option<u64>,
    /// Memory in bytes.
    #[serde(default)]
    pub memory: Option<u64>,
}

/// A PCI device detected by flmlet on the worker host.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct NodePciDevice {
    pub address: String,
    pub vendor_id: String,
    pub device_id: String,
    pub class_id: String,
    #[serde(default)]
    pub driver: Option<String>,
    #[serde(default)]
    pub numa_node: Option<i32>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HostServiceStatus {
    pub active_state: String,
    pub sub_state: String,
    #[serde(default)]
    pub message: String,
}

pub fn default_shims() -> Vec<String> {
    vec!["cri".into(), "host".into()]
}
pub fn max_executors() -> u32 {
    10
}
pub fn python_version() -> String {
    "3.12".into()
}

#[cfg(test)]
mod schema_tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn gateway_crd_exposes_nodeport_defaults_and_range_without_loadbalancer_configuration() {
        let crd = serde_json::to_value(FlameCluster::crd()).unwrap();
        let gateway = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["gateway"]["properties"];
        for (field, port) in [("sessionPort", 30443), ("cachePort", 31443)] {
            assert_eq!(gateway[field]["default"], port);
            assert_eq!(gateway[field]["minimum"].as_f64(), Some(30000.0));
            assert_eq!(gateway[field]["maximum"].as_f64(), Some(32767.0));
        }
        assert!(gateway.get("loadBalancerClass").is_none());
        assert!(gateway.get("serviceAnnotations").is_some());
    }
    #[test]
    fn worker_capacity_is_optional_and_members_are_unsigned_nullable_values() {
        let old = serde_json::to_value(FlameWorkerStatus::default()).unwrap();
        assert!(old.get("capacity").is_none());
        assert!(
            serde_json::from_value::<FlameWorkerStatus>(old)
                .unwrap()
                .capacity
                .is_none()
        );
        let capacity = NodeCapacity {
            cpu: Some(8),
            gpu: Some(0),
            memory: None,
        };
        assert_eq!(
            serde_json::to_value(capacity).unwrap(),
            serde_json::json!({"cpu":8,"gpu":0,"memory":null})
        );
        let crd = serde_json::to_value(FlameWorker::crd()).unwrap();
        let fields = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["status"]
            ["properties"]["capacity"];
        for field in ["cpu", "gpu", "memory"] {
            assert_eq!(fields["properties"][field]["minimum"].as_f64(), Some(0.0));
            assert_eq!(fields["properties"][field]["nullable"], true);
        }
        assert!(fields.get("required").is_none());
    }

    #[test]
    fn worker_crd_separates_sre_desired_state_and_observed_host_health() {
        let crd = serde_json::to_value(FlameWorker::crd()).unwrap();
        assert_eq!(crd["spec"]["group"], "flame.xflops.io");
        assert_eq!(crd["spec"]["scope"], "Namespaced");
        let properties = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"];
        let desired = &properties["spec"]["properties"];
        assert_eq!(desired["cachePort"]["default"], 9090);
        assert_eq!(
            desired["shims"]["default"],
            serde_json::json!(["cri", "host"])
        );
        assert_eq!(desired["maxExecutors"]["default"], 10);
        assert_eq!(desired["pythonVersion"]["default"], "3.12");
        assert_eq!(desired["manageObjectCache"]["default"], false);
        for omitted in [
            "prefix",
            "kubeconfig",
            "systemdDir",
            "registryMirror",
            "installBinary",
        ] {
            assert!(
                desired.get(omitted).is_none(),
                "local/SRE configuration leaked into spec: {omitted}"
            );
        }
        let observed = &properties["status"]["properties"];
        for field in [
            "fem",
            "registered",
            "capacity",
            "cacheReady",
            "configHash",
            "lastHeartbeatTime",
            "installedVersion",
            "hostname",
            "objectCache",
        ] {
            assert!(
                observed.get(field).is_some(),
                "missing observed field {field}"
            );
        }
        assert!(
            crd["spec"]["versions"][0]["subresources"]
                .get("status")
                .is_some()
        );
    }
}
