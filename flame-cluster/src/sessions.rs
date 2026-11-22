//! A canonical session snapshot and one native task metadata page.
use crate::{
    AppState,
    connection::ApiError,
    listing::{PageQuery, read_task_page, validate_session_id, validate_task_query},
    rpc::GetSessionRequest,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde_json::{Value, json};

pub(crate) async fn detail(
    State(mut state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    validate_session_id(&id)?;
    validate_task_query(&query)?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        // Resolve the session first so its canonical not-found result takes priority.
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
        let tasks = read_task_page(state.client, id, &query).await?;
        // The wire types deliberately omit common_data, credentials, and task payloads.
        Ok::<_, ApiError>(json!({"session":session,"tasks":tasks}))
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
