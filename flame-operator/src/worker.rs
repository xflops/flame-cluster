//! Project observed host-worker cache readiness into cluster-owned routing.
use crate::{
    Error,
    api::{CacheEndpoint, FlameCluster, FlameWorker},
    resources,
};
use chrono::{DateTime, Duration, Utc};
use kube::ResourceExt;

/// Ignore stale reports; cache discovery never drives host service lifecycle.
pub fn cache_ready(worker: &FlameWorker, now: DateTime<Utc>) -> bool {
    if worker.spec.disabled || worker.metadata.deletion_timestamp.is_some() {
        return false;
    }
    let Some(status) = &worker.status else {
        return false;
    };
    let generation = worker.metadata.generation.unwrap_or(0);
    if status.observed_generation != generation
        || !status.cache_ready
        || !status.conditions.iter().any(|condition| {
            condition.r#type == "Ready"
                && condition.status == "True"
                && condition.observed_generation == generation
        })
    {
        return false;
    }
    let Ok(heartbeat) = DateTime::parse_from_rfc3339(&status.last_heartbeat_time) else {
        return false;
    };
    let age = now.signed_duration_since(heartbeat.with_timezone(&Utc));
    age >= Duration::zero() && age <= Duration::seconds(120)
}

/// Merge explicit SRE endpoints and matching workers without silently overriding either.
pub fn cache_endpoints(
    cluster: &FlameCluster,
    workers: &[FlameWorker],
    now: DateTime<Utc>,
) -> Result<Vec<CacheEndpoint>, Error> {
    let mut endpoints = cluster.spec.cache_endpoints.clone();
    for worker in workers {
        if worker.namespace() != cluster.namespace()
            || worker.spec.cluster_ref != cluster.name_any()
            || worker.spec.disabled
            || worker.metadata.deletion_timestamp.is_some()
        {
            continue;
        }
        endpoints.push(CacheEndpoint {
            name: worker.name_any(),
            address: worker.spec.address.clone(),
            cache_port: worker.spec.cache_port,
            ready: cache_ready(worker, now),
        });
    }
    resources::validate_cache_endpoints(&endpoints)?;
    endpoints.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(endpoints)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Condition, FlameWorkerSpec, FlameWorkerStatus};
    use serde_json::json;

    fn cluster() -> FlameCluster {
        let mut cluster = FlameCluster::new(
            "flame",
            serde_json::from_value(json!({
                "gateway":{"hostname":"flame.example.test"},
                "cacheEndpoints":[{"name":"manual","address":"192.168.3.3"}]
            }))
            .unwrap(),
        );
        cluster.metadata.namespace = Some("team-a".into());
        cluster
    }

    fn worker(now: DateTime<Utc>) -> FlameWorker {
        let spec: FlameWorkerSpec = serde_json::from_value(json!({
            "clusterRef":"flame", "address":"192.168.3.8"
        }))
        .unwrap();
        let mut worker = FlameWorker::new("worker-01", spec);
        worker.metadata.namespace = Some("team-a".into());
        worker.metadata.generation = Some(2);
        worker.status = Some(FlameWorkerStatus {
            observed_generation: 2,
            cache_ready: true,
            registered: true,
            last_heartbeat_time: now.to_rfc3339(),
            conditions: vec![Condition {
                r#type: "Ready".into(),
                status: "True".into(),
                observed_generation: 2,
                ..Default::default()
            }],
            ..Default::default()
        });
        worker
    }

    #[test]
    fn worker_caches_and_manual_endpoints_are_merged_without_readiness_gating() {
        let now = Utc::now();
        let worker = worker(now);
        let endpoints = cache_endpoints(&cluster(), std::slice::from_ref(&worker), now).unwrap();
        assert_eq!(endpoints.len(), 2);
        assert!(endpoints.iter().all(|endpoint| endpoint.ready));
        assert_eq!(endpoints[1].name, "worker-01");
        let mut unreported = worker;
        unreported.status = None;
        let endpoints = cache_endpoints(&cluster(), &[unreported], now).unwrap();
        assert!(endpoints[0].ready);
        assert!(!endpoints[1].ready);
        // Empty or unreported workers are valid routing inputs and do not affect
        // the Deployment/certificate/Gateway checks used for cluster readiness.
        assert_eq!(cache_endpoints(&cluster(), &[], now).unwrap().len(), 1);
    }

    #[test]
    fn heartbeat_freshness_requires_current_generation_and_cache_readiness() {
        let now = Utc::now();
        let worker = worker(now);
        assert!(cache_ready(&worker, now));
        assert!(cache_ready(&worker, now + Duration::seconds(120)));
        assert!(!cache_ready(&worker, now + Duration::seconds(121)));
        assert!(!cache_ready(&worker, now - Duration::seconds(1)));
        let mut changed = worker.clone();
        changed.metadata.generation = Some(3);
        assert!(!cache_ready(&changed, now));
        let mut changed = worker.clone();
        changed.status.as_mut().unwrap().cache_ready = false;
        assert!(!cache_ready(&changed, now));
        let mut changed = worker.clone();
        changed.status.as_mut().unwrap().conditions[0].status = "False".into();
        assert!(!cache_ready(&changed, now));
        let mut changed = worker.clone();
        changed.status.as_mut().unwrap().conditions[0].observed_generation = 1;
        assert!(!cache_ready(&changed, now));
        let mut changed = worker;
        changed.status.as_mut().unwrap().last_heartbeat_time = "invalid".into();
        assert!(!cache_ready(&changed, now));
    }

    #[test]
    fn disabled_deleted_or_unrelated_workers_are_not_routed() {
        let now = Utc::now();
        let original = worker(now);
        let mut disabled = original.clone();
        disabled.spec.disabled = true;
        let mut deleted = original.clone();
        deleted.metadata.deletion_timestamp =
            Some(serde_json::from_value(json!(now.to_rfc3339())).unwrap());
        let mut other_cluster = original.clone();
        other_cluster.spec.cluster_ref = "other".into();
        let mut other_namespace = original;
        other_namespace.metadata.namespace = Some("team-b".into());
        let endpoints = cache_endpoints(
            &cluster(),
            &[disabled, deleted, other_cluster, other_namespace],
            now,
        )
        .unwrap();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].name, "manual");
    }

    #[test]
    fn duplicate_cache_names_or_addresses_are_rejected() {
        let now = Utc::now();
        let mut original = worker(now);
        original.metadata.name = Some("manual".into());
        assert!(cache_endpoints(&cluster(), &[original], now).is_err());
        let mut original = worker(now);
        original.spec.address = "192.168.3.3".into();
        assert!(cache_endpoints(&cluster(), &[original], now).is_err());
        let original = worker(now);
        assert!(cache_endpoints(&cluster(), &[original.clone(), original], now).is_err());
    }

    #[test]
    fn invalid_worker_cache_addresses_and_ports_are_rejected() {
        let now = Utc::now();
        let mut original = worker(now);
        original.spec.address = "100.64.1.2".into();
        assert!(cache_endpoints(&cluster(), &[original], now).is_err());
        let mut original = worker(now);
        original.spec.cache_port = 0;
        assert!(cache_endpoints(&cluster(), &[original], now).is_err());
    }
}
