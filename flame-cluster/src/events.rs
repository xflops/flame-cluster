//! Session lifecycle events require GetSession; ListSessions omits them.
use crate::{AppState, connection::ApiError, listing::validate_session_id, rpc::GetSessionRequest};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};

pub(crate) async fn list(
    State(mut state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    validate_session_id(&id)?;
    let session = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state
            .client
            .get_session(GetSessionRequest { session_id: id }),
    )
    .await
    .map_err(|_| {
        (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({"error":"Session event listing timed out. Retry this session."})),
        )
    })?
    .map_err(|error| {
        (
            if error.code() == tonic::Code::NotFound {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_GATEWAY
            },
            Json(json!({"error":"Session events are unavailable. Refresh and retry."})),
        )
    })?
    .into_inner();
    // Explicitly select public event fields; never serialize the session or spec.
    let items: Vec<_> = session
        .status
        .into_iter()
        .flat_map(|status| status.events)
        .map(|event| {
            json!({
                "code":event.code, "message":event.message, "creation_time":event.creation_time,
            })
        })
        .collect();
    Ok(Json(json!({"items":items})))
}
