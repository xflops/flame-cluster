use serde_json::Value;

/// A Gateway is not enough: require Envoy to accept the current generation of
/// each client identity policy before advertising the Flame cluster as Ready.
pub fn accepted(policy: &Value, gateway: &str, namespace: &str) -> bool {
    let generation = policy["metadata"]["generation"].as_i64().unwrap_or(0);
    let Some(ancestors) = policy["status"]["ancestors"].as_array() else {
        return false;
    };
    let relevant: Vec<_> = ancestors
        .iter()
        .filter(|ancestor| {
            let reference = &ancestor["ancestorRef"];
            ancestor["controllerName"] == "gateway.envoyproxy.io/gatewayclass-controller"
                && reference["kind"].as_str().unwrap_or("Gateway") == "Gateway"
                && reference["group"]
                    .as_str()
                    .unwrap_or("gateway.networking.k8s.io")
                    == "gateway.networking.k8s.io"
                && reference["name"] == gateway
                && reference["namespace"].as_str().unwrap_or(namespace) == namespace
        })
        .collect();
    !relevant.is_empty()
        && relevant.iter().all(|ancestor| {
            ancestor["conditions"].as_array().is_some_and(|conditions| {
                conditions.iter().any(|condition| {
                    condition["type"] == "Accepted"
                        && condition["status"] == "True"
                        && condition["observedGeneration"]
                            .as_i64()
                            .is_some_and(|observed| observed >= generation)
                })
            })
        })
}

/// Backend TLS must resolve its public trust anchor and target the intended frontend Service.
/// Envoy Gateway v1.10 processBackendTLSPolicy emits both Accepted and ResolvedRefs
/// with the policy generation; unresolved CA references set both conditions False.
pub fn backend_accepted(policy: &Value, service: &str, gateway: &str, namespace: &str) -> bool {
    let targets = policy["spec"]["targetRefs"]
        .as_array()
        .is_some_and(|targets| {
            targets.iter().any(|target| {
                target["group"] == ""
                    && target["kind"] == "Service"
                    && target["name"] == service
                    && target["sectionName"] == "frontend"
            })
        });
    if !targets || !accepted(policy, gateway, namespace) {
        return false;
    }
    let generation = policy["metadata"]["generation"].as_i64().unwrap_or(0);
    policy["status"]["ancestors"]
        .as_array()
        .is_some_and(|ancestors| {
            ancestors
                .iter()
                .filter(|ancestor| {
                    let reference = &ancestor["ancestorRef"];
                    ancestor["controllerName"] == "gateway.envoyproxy.io/gatewayclass-controller"
                        && reference["name"] == gateway
                        && reference["kind"].as_str().unwrap_or("Gateway") == "Gateway"
                        && reference["group"]
                            .as_str()
                            .unwrap_or("gateway.networking.k8s.io")
                            == "gateway.networking.k8s.io"
                        && reference["namespace"].as_str().unwrap_or(namespace) == namespace
                })
                .all(|ancestor| {
                    ancestor["conditions"].as_array().is_some_and(|conditions| {
                        conditions.iter().any(|condition| {
                            condition["type"] == "ResolvedRefs"
                                && condition["status"] == "True"
                                && condition["observedGeneration"]
                                    .as_i64()
                                    .is_some_and(|observed| observed >= generation)
                        })
                    })
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn readiness_requires_current_policy_acceptance_for_the_correct_gateway() {
        let mut policy = json!({"metadata":{"generation":2},"status":{"ancestors":[{"controllerName":"gateway.envoyproxy.io/gatewayclass-controller","ancestorRef":{"group":"gateway.networking.k8s.io","kind":"Gateway","name":"flame-gateway","namespace":"training"},"conditions":[{"type":"Accepted","status":"True","observedGeneration":2}]}]}});
        assert!(accepted(&policy, "flame-gateway", "training"));
        assert!(!accepted(&policy, "other-gateway", "training"));
        assert!(!accepted(&policy, "flame-gateway", "other-namespace"));
        policy["status"]["ancestors"][0]["conditions"][0]["observedGeneration"] = json!(1);
        assert!(!accepted(&policy, "flame-gateway", "training"));
        policy["status"]["ancestors"][0]["conditions"][0]["observedGeneration"] = json!(2);
        policy["status"]["ancestors"][0]["conditions"][0]["status"] = json!("False");
        assert!(!accepted(&policy, "flame-gateway", "training"));
        assert!(!accepted(
            &json!({"status":{}}),
            "flame-gateway",
            "training"
        ));
    }
    #[test]
    fn backend_tls_readiness_requires_resolved_ca_and_correct_service_and_generation() {
        let mut policy = json!({"metadata":{"generation":2},"spec":{"targetRefs":[{"group":"","kind":"Service","name":"flame-session-manager","sectionName":"frontend"}]},"status":{"ancestors":[{"controllerName":"gateway.envoyproxy.io/gatewayclass-controller","ancestorRef":{"group":"gateway.networking.k8s.io","kind":"Gateway","name":"flame-gateway","namespace":"training"},"conditions":[{"type":"Accepted","status":"True","observedGeneration":2},{"type":"ResolvedRefs","status":"True","observedGeneration":2}]}]}});
        assert!(backend_accepted(
            &policy,
            "flame-session-manager",
            "flame-gateway",
            "training"
        ));
        assert!(!backend_accepted(
            &policy,
            "other-service",
            "flame-gateway",
            "training"
        ));
        assert!(!backend_accepted(
            &policy,
            "flame-session-manager",
            "other-gateway",
            "training"
        ));
        policy["status"]["ancestors"][0]["conditions"][1]["observedGeneration"] = json!(1);
        assert!(!backend_accepted(
            &policy,
            "flame-session-manager",
            "flame-gateway",
            "training"
        ));
        policy["status"]["ancestors"][0]["conditions"][1]["observedGeneration"] = json!(2);
        policy["status"]["ancestors"][0]["conditions"][1]["status"] = json!("False");
        assert!(!backend_accepted(
            &policy,
            "flame-session-manager",
            "flame-gateway",
            "training"
        ));
    }
}
