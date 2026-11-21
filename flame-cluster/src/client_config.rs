//! Download only the selected Flame's explicitly configured client bundle.
use crate::{AppState, connection::ApiError};
use axum::{
    Json,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::json;

fn unavailable() -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(
            json!({"error":"The selected Flame's client configuration is unavailable. Refresh and retry."}),
        ),
    )
}

pub(crate) async fn download(State(state): State<AppState>) -> Result<Response, ApiError> {
    let (Some(client), Some(namespace), Some(name)) = (
        state.kube_client,
        state.namespace,
        state.client_config_secret,
    ) else {
        return Err(unavailable());
    };
    let secrets: kube::Api<k8s_openapi::api::core::v1::Secret> =
        kube::Api::namespaced(client, &namespace);
    let secret = tokio::time::timeout(std::time::Duration::from_secs(12), secrets.get(&name))
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
    let contents = secret
        .data
        .and_then(|mut data| data.remove("flame.yaml"))
        .ok_or_else(unavailable)?;
    let contents = String::from_utf8(contents.0).map_err(|_| unavailable())?;
    if contents.trim().is_empty() {
        return Err(unavailable());
    }
    Ok((
        [
            (header::CONTENT_TYPE, "application/yaml"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"flame.yaml\"",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        contents,
    )
        .into_response())
}
