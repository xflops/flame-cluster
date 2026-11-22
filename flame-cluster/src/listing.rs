use crate::{
    AppState,
    connection::{self, ApiError},
    rpc::{self, frontend_client::FrontendClient},
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tonic::transport::Channel;

#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct PageQuery {
    pub page: usize,
    pub page_size: usize,
    // Retained only to reject obsolete search requests explicitly.
    pub search: String,
    pub state: String,
}
impl Default for PageQuery {
    fn default() -> Self {
        Self {
            page: 1,
            page_size: 12,
            search: String::new(),
            state: String::new(),
        }
    }
}
fn bad(message: &str) -> ApiError {
    (StatusCode::BAD_REQUEST, Json(json!({"error":message})))
}
pub(crate) fn validate_session_id(id: &str) -> Result<(), ApiError> {
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        return Err(bad("Invalid session ID"));
    }
    Ok(())
}
pub(crate) fn validate_task_query(query: &PageQuery) -> Result<usize, ApiError> {
    let offset = query.offset()?;
    if !query.state.is_empty() {
        return Err(bad("Task pages do not support state filters yet"));
    }
    Ok(offset)
}
impl PageQuery {
    pub fn offset(&self) -> Result<usize, ApiError> {
        if !self.search.is_empty() {
            return Err(bad("Resource search is not supported"));
        }
        if self.page == 0 || !(1..=100).contains(&self.page_size) {
            return Err(bad("page must be positive; pageSize must be 1–100"));
        }
        self.page
            .checked_sub(1)
            .and_then(|p| p.checked_mul(self.page_size))
            .filter(|offset| *offset <= 1_000_000)
            .ok_or_else(|| bad("Page offset exceeds 1000000"))
    }
}
fn states(resource: &str) -> Option<&'static [&'static str]> {
    match resource {
        "applications" => Some(&["Enabled", "Disabled"]),
        "sessions" => Some(&["Open", "Closed", "Suspending", "Suspended"]),
        "executors" => Some(&[
            "Unknown",
            "Void",
            "Idle",
            "Binding",
            "Bound",
            "Unbinding",
            "Releasing",
            "Released",
        ]),
        _ => None,
    }
}
fn resource_state(resource: &str, query: &PageQuery) -> Result<Option<i32>, ApiError> {
    query.offset()?;
    let labels = states(resource).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"Unknown resource"})),
        )
    })?;
    if query.state.is_empty() {
        return Ok(None);
    }
    labels
        .iter()
        .position(|label| *label == query.state)
        .map(|index| Some(index as i32))
        .ok_or_else(|| bad("Invalid resource state"))
}

// Every list request selects exactly one upstream page; filtering and ordering
// belong to Flame.
pub(crate) async fn read_resource(
    mut client: FrontendClient<Channel>,
    resource: &str,
    query: &PageQuery,
) -> Result<Value, ApiError> {
    let state = resource_state(resource, query)?;
    let requested = rpc::Paging {
        limit: query.page_size as i32,
        number: query.page as i64,
        total: None,
    };
    macro_rules! fetch {
        ($method:ident, $request:ident, $field:ident) => {{
            let response = client.$method(rpc::$request {
                paging: Some(requested), state, ..Default::default()
            }).await.map_err(|error| connection::upstream(error.message()))?.into_inner();
            let total = page_total(response.paging.as_ref()
                .ok_or_else(|| connection::upstream("Flame resource listing requires native paging support"))?,
                &requested, response.$field.len())?;
            json!({"items":response.$field,"page":query.page,"pageSize":query.page_size,
                "total":total,"hasNext":(query.offset()? as u64).saturating_add(query.page_size as u64)<total,
                "observedAt":chrono::Utc::now().to_rfc3339()})
        }};
    }
    Ok(match resource {
        "applications" => fetch!(list_applications, ListApplicationsRequest, applications),
        "sessions" => fetch!(list_sessions, ListSessionsRequest, sessions),
        "executors" => fetch!(list_executors, ListExecutorsRequest, executors),
        _ => unreachable!("validated resource"),
    })
}

fn page_total(
    paging: &rpc::Paging,
    requested: &rpc::Paging,
    count: usize,
) -> Result<u64, ApiError> {
    if paging.limit != requested.limit
        || paging.number != requested.number
        || count > requested.limit as usize
    {
        return Err(connection::upstream("Invalid upstream paging metadata"));
    }
    paging
        .total
        .ok_or_else(|| connection::upstream("Missing upstream page total"))
}

pub(crate) async fn resources(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(read_resource(state.client, &resource, &query).await?))
}

// Tasks request exactly one native metadata page, including for deep pages.
pub(crate) async fn read_task_page(
    mut client: FrontendClient<Channel>,
    id: String,
    query: &PageQuery,
) -> Result<Value, ApiError> {
    let offset = validate_task_query(query)?;
    let requested = rpc::Paging {
        limit: query.page_size as i32,
        number: query.page as i64,
        total: None,
    };
    let response = client
        .list_tasks(rpc::ListTasksRequest {
            session_id: id,
            paging: Some(requested),
            ..Default::default()
        })
        .await
        .map_err(connection::upstream)?
        .into_inner();
    let paging = response
        .paging
        .as_ref()
        .ok_or_else(|| connection::upstream("Flame task listing requires native paging support"))?;
    let total = page_total(paging, &requested, response.tasks.len())?;
    Ok(
        json!({"items":response.tasks,"page":query.page,"pageSize":query.page_size,
        "hasNext":(offset as u64).saturating_add(query.page_size as u64)<total,
        "total":total,"observedAt":chrono::Utc::now().to_rfc3339()}),
    )
}
