//! Nodes are enrolled FlameWorkers, including hosts whose FEM is not registered yet.
use crate::{AppState, connection::ApiError, listing::PageQuery};
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Duration, Utc};
use kube::{
    Api,
    api::ListParams,
    core::{ApiResource, DynamicObject, GroupVersionKind},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const STATES: [&str; 5] = ["Pending", "Ready", "Not ready", "Disabled", "Deleting"];
const CHUNK_SIZE: u32 = 500;
const MAX_CHUNKS: usize = 100;

fn unavailable() -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(
            json!({"error":"The selected Flame's worker inventory is unavailable. Refresh and retry."}),
        ),
    )
}

fn readiness(worker: &Value, now: DateTime<Utc>) -> &'static str {
    if worker
        .pointer("/metadata/deletionTimestamp")
        .is_some_and(|value| !value.is_null())
    {
        return "Deleting";
    }
    if worker.pointer("/spec/disabled").and_then(Value::as_bool) == Some(true) {
        return "Disabled";
    }
    let Some(generation) = worker
        .pointer("/metadata/generation")
        .and_then(Value::as_i64)
    else {
        return "Pending";
    };
    let Some(status) = worker.get("status").filter(|status| status.is_object()) else {
        return "Pending";
    };
    if status.get("observedGeneration").and_then(Value::as_i64) != Some(generation) {
        return "Pending";
    }
    let current_ready = status
        .get("conditions")
        .and_then(Value::as_array)
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition.get("type").and_then(Value::as_str) == Some("Ready")
                    && condition.get("status").and_then(Value::as_str) == Some("True")
                    && condition.get("observedGeneration").and_then(Value::as_i64)
                        == Some(generation)
            })
        });
    let fresh = status
        .get("lastHeartbeatTime")
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .is_some_and(|heartbeat| {
            let age = now.signed_duration_since(heartbeat.with_timezone(&Utc));
            age >= Duration::zero() && age <= Duration::seconds(120)
        });
    let cache_ready = worker
        .pointer("/spec/manageObjectCache")
        .and_then(Value::as_bool)
        != Some(true)
        || (status.get("cacheReady").and_then(Value::as_bool) == Some(true)
            && status
                .pointer("/objectCache/activeState")
                .and_then(Value::as_str)
                == Some("active"));
    if current_ready
        && fresh
        && cache_ready
        && status.get("registered").and_then(Value::as_bool) == Some(true)
        && status.pointer("/fem/activeState").and_then(Value::as_str) == Some("active")
    {
        "Ready"
    } else {
        "Not ready"
    }
}

async fn inventory(state: AppState) -> Result<Vec<DynamicObject>, ApiError> {
    let (Some(client), Some(namespace), Some(name)) =
        (state.kube_client, state.namespace, state.name)
    else {
        return Err(unavailable());
    };
    let resource = ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("flame.xflops.io", "v1alpha1", "FlameWorker"),
        "flameworkers",
    );
    let workers: Api<DynamicObject> = Api::namespaced_with(client, &namespace, &resource);
    let mut params = ListParams::default().limit(CHUNK_SIZE);
    let mut items = Vec::new();
    let mut tokens = BTreeSet::new();
    let mut names = BTreeSet::new();
    for _ in 0..MAX_CHUNKS {
        let page = workers.list(&params).await.map_err(|_| unavailable())?;
        if page.items.len() > CHUNK_SIZE as usize {
            return Err(unavailable());
        }
        for worker in page.items {
            let Some(worker_name) = worker
                .metadata
                .name
                .as_ref()
                .filter(|name| !name.is_empty())
            else {
                return Err(unavailable());
            };
            let Some(cluster) = worker
                .data
                .pointer("/spec/clusterRef")
                .and_then(Value::as_str)
            else {
                return Err(unavailable());
            };
            if worker.metadata.namespace.as_deref() != Some(namespace.as_str())
                || !names.insert(worker_name.clone())
            {
                return Err(unavailable());
            }
            if cluster == name {
                items.push(worker);
            }
        }
        match page.metadata.continue_.filter(|token| !token.is_empty()) {
            Some(token) if tokens.insert(token.clone()) => params = params.continue_token(&token),
            Some(_) => return Err(unavailable()),
            None if page
                .metadata
                .remaining_item_count
                .is_none_or(|count| count == 0) =>
            {
                return Ok(items);
            }
            None => return Err(unavailable()),
        }
    }
    Err(unavailable())
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    let offset = query.offset()?;
    if !query.state.is_empty() && !STATES.contains(&query.state.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Invalid resource state"})),
        ));
    }
    let mut workers = tokio::time::timeout(std::time::Duration::from_secs(12), inventory(state))
        .await
        .map_err(|_| unavailable())??;
    workers.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
    let now = Utc::now();
    let items = workers
        .into_iter()
        .map(|worker| {
            let mut row = serde_json::to_value(worker).map_err(|_| unavailable())?;
            row["readiness"] = json!(readiness(&row, now));
            Ok(row)
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let filtered = items
        .into_iter()
        .filter(|row| query.state.is_empty() || row["readiness"] == query.state)
        .collect::<Vec<_>>();
    let total = filtered.len();
    let items = filtered
        .into_iter()
        .skip(offset)
        .take(query.page_size)
        .collect::<Vec<_>>();
    Ok(Json(
        json!({"items":items,"page":query.page,"pageSize":query.page_size,"total":total,
        "hasNext":offset.saturating_add(query.page_size)<total,"observedAt":now.to_rfc3339()}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_readiness_requires_current_fresh_service_and_registration_reports() {
        let now = DateTime::parse_from_rfc3339("2030-01-01T00:10:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let worker = json!({"metadata":{"generation":3},"spec":{"disabled":false,"manageObjectCache":true},
            "status":{"observedGeneration":3,"registered":true,"cacheReady":true,"lastHeartbeatTime":now.to_rfc3339(),
                "fem":{"activeState":"active"},"objectCache":{"activeState":"active"},
                "conditions":[{"type":"Ready","status":"True","observedGeneration":3}]}});
        assert_eq!(readiness(&worker, now), "Ready");
        for (path, value, expected) in [
            ("/status", Value::Null, "Pending"),
            ("/status/observedGeneration", json!(2), "Pending"),
            ("/status/registered", json!(false), "Not ready"),
            ("/status/fem/activeState", json!("inactive"), "Not ready"),
            ("/status/cacheReady", json!(false), "Not ready"),
            (
                "/status/objectCache/activeState",
                json!("failed"),
                "Not ready",
            ),
            (
                "/status/conditions/0/observedGeneration",
                json!(2),
                "Not ready",
            ),
            ("/status/conditions/0/status", json!("False"), "Not ready"),
            ("/status/lastHeartbeatTime", json!("invalid"), "Not ready"),
            (
                "/status/lastHeartbeatTime",
                json!((now - Duration::seconds(120)).to_rfc3339()),
                "Ready",
            ),
            (
                "/status/lastHeartbeatTime",
                json!((now - Duration::seconds(121)).to_rfc3339()),
                "Not ready",
            ),
            (
                "/status/lastHeartbeatTime",
                json!((now + Duration::seconds(1)).to_rfc3339()),
                "Not ready",
            ),
        ] {
            let mut changed = worker.clone();
            *changed.pointer_mut(path).unwrap() = value;
            assert_eq!(readiness(&changed, now), expected, "{path}");
        }
        let mut no_cache = worker;
        no_cache["spec"]["manageObjectCache"] = json!(false);
        no_cache["status"]["objectCache"] = Value::Null;
        no_cache["status"]["cacheReady"] = json!(false);
        assert_eq!(readiness(&no_cache, now), "Ready");
        no_cache["status"] = Value::Null;
        no_cache["spec"]["disabled"] = json!(true);
        assert_eq!(readiness(&no_cache, now), "Disabled");
        no_cache["metadata"]["deletionTimestamp"] = json!(now.to_rfc3339());
        assert_eq!(readiness(&no_cache, now), "Deleting");
    }
}
