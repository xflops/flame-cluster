mod actions;
mod client_config;
mod configuration;
pub mod connection;
mod join;
mod listing;
mod nodes;
mod sessions;

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
    build_router(state.clone(), vec![("flame".into(), state)])
}

/// Bind every configured Flame to its own client and credential files.
pub fn fleet_router(
    connections: Vec<(String, AppState)>,
) -> Result<Router, connection::ConfigError> {
    let default = connections
        .first()
        .ok_or("at least one Flame connection is required")?
        .1
        .clone();
    let mut ids = std::collections::BTreeSet::new();
    for (id, state) in &connections {
        connection::validate_connection_id(id)?;
        if !ids.insert(id) {
            return Err("Flame connection IDs must be unique".into());
        }
        if state.allowed_hosts != default.allowed_hosts {
            return Err("all Flame connections must use the same trusted API hosts".into());
        }
    }
    Ok(build_router(default, connections))
}

fn resource_routes() -> Router<AppState> {
    Router::new()
        .route("/connection", get(connection))
        .route("/join", get(join::command))
        .route("/flame.yaml", get(client_config::download))
        .route("/nodes", get(nodes::list))
        .route("/{resource}", get(listing::resources))
        .route("/sessions/{id}/tasks", get(listing::tasks))
        .route("/sessions/{id}", get(sessions::detail))
        .route("/sessions/{id}/actions/{action}", post(actions::action))
}

fn build_router(default: AppState, connections: Vec<(String, AppState)>) -> Router {
    let items = connections
        .iter()
        .map(|(id, state)| {
            let mut metadata = json!({
                "id":id, "endpoint":state.endpoint, "displayName":state.display_name,
                "clientConfigAvailable":state.client_config_secret.is_some(),
            });
            if let Some(namespace) = &state.namespace {
                metadata["namespace"] = json!(namespace);
            }
            if let Some(name) = &state.name {
                metadata["name"] = json!(name);
            }
            metadata
        })
        .collect::<Vec<_>>();
    let mut api = resource_routes().with_state(default.clone()).route(
        "/connections",
        get(move || {
            let items = items.clone();
            async move { Json(json!({"items":items})) }
        }),
    );
    for (id, state) in connections {
        api = api.nest(
            &format!("/connections/{id}"),
            resource_routes().with_state(state),
        );
    }
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api", api)
        .layer(axum::middleware::from_fn_with_state(default, trusted_host))
        .layer(axum::middleware::map_response(
            |mut response: axum::response::Response| async move {
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static("no-store"),
                );
                response
            },
        ))
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
    let mut metadata = json!({"endpoint":state.endpoint,"displayName":state.display_name,"clientConfigAvailable":state.client_config_secret.is_some()});
    configuration::append(&state, &mut metadata).await;
    if let Some(namespace) = state.namespace {
        metadata["namespace"] = json!(namespace);
    }
    if let Some(name) = state.name {
        metadata["name"] = json!(name);
    }
    Json(metadata)
}

#[cfg(test)]
mod tests;
