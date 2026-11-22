//! A canonical session snapshot, including lifecycle events.
use crate::{AppState, connection::ApiError, listing::validate_session_id, rpc::GetSessionRequest};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};

pub(crate) async fn detail(
    State(mut state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    validate_session_id(&id)?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let session = state
            .client
            .get_session(GetSessionRequest {
                session_id: id.clone(),
            })
            .await
            .map_err(|error| {
                (
                    if error.code() == tonic::Code::NotFound {
                        StatusCode::NOT_FOUND
                    } else {
                        StatusCode::BAD_GATEWAY
                    },
                    Json(json!({"error":"Session details are unavailable. Refresh and retry."})),
                )
            })?
            .into_inner();
        // The session wire type deliberately omits common_data and credentials.
        Ok::<_, ApiError>(json!(session))
    })
    .await
    .map_err(|_| {
        (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({"error":"Session detail loading timed out. Retry this session."})),
        )
    })??;
    Ok(Json(result))
}
