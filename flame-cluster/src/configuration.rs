//! Read the selected session manager's generated configuration, never its Secrets.
use crate::AppState;
use k8s_openapi::api::core::v1::ConfigMap;
use serde_json::{Value, json};
use std::time::Duration;

const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const UNAVAILABLE: &str =
    "The selected Flame's session-manager configuration is unavailable. Refresh and retry.";

async fn read(client: kube::Client, namespace: &str, name: &str) -> Result<Value, ()> {
    let configs: kube::Api<ConfigMap> = kube::Api::namespaced(client, namespace);
    let config = tokio::time::timeout(
        Duration::from_secs(12),
        configs.get(&format!("{name}-config")),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    let contents = config
        .data
        .and_then(|mut data| data.remove("flame-cluster.yaml"))
        .ok_or(())?;
    if contents.len() > MAX_CONFIG_BYTES {
        return Err(());
    }
    let configuration: Value = serde_yaml::from_str(&contents).map_err(|_| ())?;
    if !configuration.is_object() {
        return Err(());
    }
    Ok(configuration)
}

pub(crate) async fn append(state: &AppState, metadata: &mut Value) {
    let (Some(client), Some(namespace), Some(name)) =
        (&state.kube_client, &state.namespace, &state.name)
    else {
        return;
    };
    match read(client.clone(), namespace, name).await {
        Ok(configuration) => metadata["configuration"] = configuration,
        Err(()) => metadata["configurationError"] = json!(UNAVAILABLE),
    }
}
