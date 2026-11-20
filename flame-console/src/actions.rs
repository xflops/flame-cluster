//! State-changing session operations use JSON POST, never GET.
use crate::{
    AppState,
    connection::ApiError,
    rpc::{self, frontend_client::FrontendClient},
};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use tonic::transport::Channel;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionBody {}
fn bad(status: StatusCode, message: &str) -> ApiError {
    (status, Json(json!({"error":message})))
}
pub(crate) fn validate(headers: &HeaderMap, session: &str, action: &str) -> Result<(), ApiError> {
    if session.is_empty() || session.len() > 256 || session.chars().any(char::is_control) {
        return Err(bad(StatusCode::BAD_REQUEST, "Invalid session ID"));
    }
    if !["suspend", "resume", "close", "delete"].contains(&action) {
        return Err(bad(StatusCode::NOT_FOUND, "Unknown session action"));
    }
    if let Some(origin) = headers.get("origin") {
        let origin = origin
            .to_str()
            .ok()
            .and_then(|value| value.parse::<axum::http::Uri>().ok());
        let host = headers.get("host").and_then(|value| value.to_str().ok());
        let allowed = origin.as_ref().is_some_and(|origin| {
            matches!(origin.scheme_str(), Some("http" | "https"))
                && origin.path() == "/"
                && origin.authority().is_some_and(|authority| {
                    host.is_some_and(|host| authority.as_str().eq_ignore_ascii_case(host))
                })
        });
        if !allowed {
            return Err(bad(
                StatusCode::FORBIDDEN,
                "Session actions require the same origin",
            ));
        }
    }
    Ok(())
}
pub(crate) async fn execute(
    mut client: FrontendClient<Channel>,
    session: String,
    action: &str,
) -> Result<Response, ApiError> {
    let result = async {
        let current = client
            .get_session(rpc::GetSessionRequest {
                session_id: session.clone(),
            })
            .await?
            .into_inner();
        let state = current.status.as_ref().map(|status| status.state);
        let allowed = matches!(
            (state, action),
            (Some(0), "suspend" | "close") | (Some(1), "delete") | (Some(3), "resume")
        );
        if !allowed {
            return Err(tonic::Status::failed_precondition("Session state changed"));
        }
        match action {
            "suspend" => {
                client
                    .suspend_session(rpc::SuspendSessionRequest {
                        session_id: session,
                    })
                    .await
            }
            "resume" => {
                client
                    .resume_session(rpc::ResumeSessionRequest {
                        session_id: session,
                    })
                    .await
            }
            "close" => {
                client
                    .close_session(rpc::CloseSessionRequest {
                        session_id: session,
                    })
                    .await
            }
            "delete" => {
                client
                    .delete_session(rpc::DeleteSessionRequest {
                        session_id: session,
                    })
                    .await
            }
            _ => Err(tonic::Status::invalid_argument("Unknown action")),
        }
    };
    let response = tokio::time::timeout(std::time::Duration::from_secs(12), result)
        .await
        .map_err(|_| {
            bad(
                StatusCode::GATEWAY_TIMEOUT,
                "Session action timed out. Refresh before retrying.",
            )
        })?
        .map_err(|error| {
            let status = match error.code() {
                tonic::Code::NotFound => StatusCode::NOT_FOUND,
                tonic::Code::InvalidArgument => StatusCode::BAD_REQUEST,
                tonic::Code::FailedPrecondition | tonic::Code::AlreadyExists => {
                    StatusCode::CONFLICT
                }
                tonic::Code::PermissionDenied => StatusCode::FORBIDDEN,
                tonic::Code::Unauthenticated => StatusCode::UNAUTHORIZED,
                tonic::Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                tonic::Code::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
                _ => StatusCode::BAD_GATEWAY,
            };
            tracing::warn!(action,code=?error.code(),"session action failed");
            bad(
                status,
                "Unable to change this session. Refresh its state before retrying.",
            )
        })?;
    Ok(Json(json!({"session":response.into_inner()})).into_response())
}
pub(crate) async fn action(
    State(state): State<AppState>,
    Path((session, action)): Path<(String, String)>,
    headers: HeaderMap,
    Json(_): Json<ActionBody>,
) -> Result<Response, ApiError> {
    validate(&headers, &session, &action)?;
    execute(state.client, session, &action).await
}
