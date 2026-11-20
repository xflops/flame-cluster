mod actions;
pub mod connection;
mod join;
mod listing;

pub mod rpc {
    tonic::include_proto!("flame.v1");
}

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
pub use connection::AppState;
use serde_json::{Value, json};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/connection", get(connection))
        .route("/api/join", get(join::command))
        .route("/api/{resource}", get(listing::resources))
        .route("/api/sessions/{id}/tasks", get(listing::tasks))
        .route("/api/sessions/{id}/actions/{action}", post(actions::action))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            trusted_host,
        ))
        .layer(axum::middleware::map_response(
            |mut response: axum::response::Response| async move {
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static("no-store"),
                );
                response
            },
        ))
        .with_state(state)
}

async fn trusted_host(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if request.uri().path().starts_with("/api/") {
        let headers = request.headers();
        let host = headers
            .get(header::HOST)
            .filter(|_| headers.get_all(header::HOST).iter().count() == 1)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.contains('@'))
            .and_then(|value| value.parse::<axum::http::uri::Authority>().ok())
            .filter(|authority| {
                authority.as_str() == authority.host() || authority.port_u16().is_some()
            })
            .map(|authority| {
                authority
                    .host()
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_ascii_lowercase()
            });
        if !host.is_some_and(|host| state.allowed_hosts.contains(&host)) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error":"Untrusted API host"})),
            )
                .into_response();
        }
    }
    next.run(request).await
}

async fn connection(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"endpoint":state.endpoint,"displayName":state.display_name}))
}

#[cfg(test)]
mod tests;
