//! Read the operator's rotating enrollment Secret without Kubernetes API access.
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
    let (token, expires_at) = tokio::try_join!(
        tokio::fs::read_to_string(token_path),
        tokio::fs::read_to_string(expiry_path),
    )
    .map_err(|_| unavailable())?;
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
