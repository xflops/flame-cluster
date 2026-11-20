//! Read only the selected Flame's configured enrollment credential source.
use crate::{AppState, connection::ApiError};
use axum::{Json, extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

fn unavailable() -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(
            json!({"error":"A current node join command is unavailable. Check the console join Secret configuration and refresh."}),
        ),
    )
}

pub(crate) async fn command(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let (token, expires_at) = if let (Some(client), Some(namespace), Some(name)) = (
        &state.kube_client,
        &state.join_namespace,
        &state.join_secret,
    ) {
        let secrets: kube::Api<k8s_openapi::api::core::v1::Secret> =
            kube::Api::namespaced(client.clone(), namespace);
        let secret = tokio::time::timeout(std::time::Duration::from_secs(12), secrets.get(name))
            .await
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())?;
        let mut data = secret.data.ok_or_else(unavailable)?;
        let token = data.remove("token").ok_or_else(unavailable)?;
        let expiry = data.remove("expiresAt").ok_or_else(unavailable)?;
        (
            String::from_utf8(token.0).map_err(|_| unavailable())?,
            String::from_utf8(expiry.0).map_err(|_| unavailable())?,
        )
    } else {
        read_files(&state).await?
    };
    let token = token.trim();
    let expires_at = DateTime::parse_from_rfc3339(expires_at.trim()).map_err(|_| unavailable())?;
    if token.is_empty()
        || token.len() > 32768
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte))
        || expires_at <= Utc::now()
    {
        return Err(unavailable());
    }
    Ok(Json(json!({
        "command": format!("sudo sh setup.sh --join '{token}'"),
        "expiresAt": expires_at.to_rfc3339(),
    })))
}

async fn read_files(state: &AppState) -> Result<(String, String), ApiError> {
    let (Some(token_file), Some(expiry_file)) = (&state.join_token_file, &state.join_expiry_file)
    else {
        return Err(unavailable());
    };
    // Pin both projected Secret files to one generation during atomic rotation.
    let token_path = tokio::fs::canonicalize(token_file)
        .await
        .map_err(|_| unavailable())?;
    let expiry_path = tokio::fs::canonicalize(expiry_file)
        .await
        .map_err(|_| unavailable())?;
    if token_file.parent() == expiry_file.parent() && token_path.parent() != expiry_path.parent() {
        return Err(unavailable());
    }
    tokio::try_join!(
        tokio::fs::read_to_string(token_path),
        tokio::fs::read_to_string(expiry_path),
    )
    .map_err(|_| unavailable())
}
