use flame_operator::{api::*, cluster::Owner};
use flame_operator::{cluster, resources};
use http::{Request, Response};
use http_body_util::BodyExt;
use kube::{Client, client::Body};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

fn spec() -> FlameClusterSpec {
    serde_json::from_value(json!({
        "version":"latest",
        "gateway":{"hostname":"flame.example.test","ingressIp":"100.117.27.41"}
    }))
    .unwrap()
}

fn workers() -> Vec<CacheEndpoint> {
    vec![
        CacheEndpoint {
            name: "flm-worker-01".into(),
            address: "192.168.3.8".into(),
            cache_port: 9090,
            ready: true,
        },
        CacheEndpoint {
            name: "flm-worker-02".into(),
            address: "192.168.3.197".into(),
            cache_port: 9090,
            ready: true,
        },
    ]
}

fn owner() -> Owner {
    Owner {
        api_version: "flame.xflops.io/v1alpha1".into(),
        kind: "FlameCluster".into(),
        name: "flame".into(),
        uid: "owner-1".into(),
        namespace: Some("flame-system".into()),
    }
}

#[test]
fn only_control_plane_pods_are_rendered() {
    let objects = resources::flame_resources(
        "flame",
        "flame-system",
        "cluster-uid",
        &spec(),
        "docker.io/xflops/flame-session-manager@sha256:abc",
        &[],
    )
    .unwrap();
    let deployments: Vec<_> = objects
        .iter()
        .filter(|o| o["kind"] == "Deployment")
        .collect();
    assert_eq!(deployments.len(), 1);
    let pod = &deployments[0]["spec"]["template"]["spec"];
    assert_eq!(pod["containers"][0]["name"], "session-manager");
    assert_eq!(pod["containers"].as_array().unwrap().len(), 1);
    assert_eq!(pod["automountServiceAccountToken"], false);
    assert!(
        !objects
            .iter()
            .any(|o| ["Pod", "StatefulSet", "DaemonSet"].contains(&o["kind"].as_str().unwrap()))
    );
    let service = objects
        .iter()
        .find(|o| o["kind"] == "Service" && o["metadata"]["name"] == "flame-session-manager")
        .unwrap();
    assert_eq!(service["spec"]["ports"][0]["nodePort"], 30080);
    assert_eq!(service["spec"]["ports"][1]["nodePort"], 30081);
    let pvc = objects
        .iter()
        .find(|o| o["kind"] == "PersistentVolumeClaim")
        .unwrap();
    assert_eq!(
        pvc["metadata"]["annotations"]["flame.xflops.io/retain"],
        "true"
    );
}

#[test]
fn owner_header_routes_point_to_exact_lan_caches() {
    let spec = spec();
    let objects = resources::flame_resources(
        "flame",
        "flame-system",
        "cluster-uid",
        &spec,
        &spec.image,
        &workers(),
    )
    .unwrap();
    for worker in &workers() {
        let name = format!("flame-cache-{}", worker.name);
        let route = objects
            .iter()
            .find(|o| o["kind"] == "HTTPRoute" && o["metadata"]["name"] == name)
            .unwrap();
        assert_eq!(route["spec"]["hostnames"][0], spec.gateway.hostname);
        assert_eq!(
            route["spec"]["rules"][0]["matches"][0]["headers"][0],
            json!({"name":"x-flame-object-cache","type":"Exact","value":format!("{}:{}",worker.address,worker.cache_port)})
        );
        assert_eq!(route["spec"]["rules"][0]["backendRefs"][0]["name"], name);
        let slice = objects
            .iter()
            .find(|o| o["kind"] == "EndpointSlice" && o["metadata"]["name"] == name)
            .unwrap();
        assert_eq!(slice["endpoints"][0]["addresses"][0], worker.address);
    }
    assert!(!objects.iter().any(|o| o["kind"] == "Backend"));
}

#[test]
fn gateway_ipv4_endpoints_use_ip_sans_without_gateway_api_hostnames() {
    let mut spec = spec();
    spec.gateway.hostname = "192.168.3.149".into();
    let objects = resources::flame_resources(
        "flame",
        "flame-system",
        "uid",
        &spec,
        &spec.image,
        &workers(),
    )
    .unwrap();
    let certificate = objects
        .iter()
        .find(|object| {
            object["kind"] == "Certificate" && object["metadata"]["name"] == "flame-gateway-tls"
        })
        .unwrap();
    assert_eq!(certificate["spec"]["ipAddresses"], json!(["192.168.3.149"]));
    assert!(certificate["spec"].get("dnsNames").is_none());
    let gateway = objects
        .iter()
        .find(|object| object["kind"] == "Gateway")
        .unwrap();
    for listener in gateway["spec"]["listeners"].as_array().unwrap() {
        assert!(listener.get("hostname").is_none());
    }
    let routes: Vec<_> = objects
        .iter()
        .filter(|object| object["kind"] == "HTTPRoute")
        .collect();
    assert_eq!(routes.len(), workers().len() + 2);
    for route in routes {
        assert!(route["spec"].get("hostnames").is_none());
    }
    use base64::{Engine, engine::general_purpose::STANDARD};
    let source = ["ca.crt", "tls.crt", "tls.key"]
        .into_iter()
        .map(|key| {
            (
                key.into(),
                k8s_openapi::ByteString(b"test-credential".to_vec()),
            )
        })
        .collect();
    let secret = resources::client_secret("flame", "flame-system", &spec, &source)
        .unwrap()
        .unwrap();
    let config = STANDARD
        .decode(secret["data"]["flame.yaml"].as_str().unwrap())
        .unwrap();
    let config: Value = serde_yaml::from_slice(&config).unwrap();
    assert_eq!(
        config["contexts"][0]["cluster"]["endpoint"],
        "https://192.168.3.149:30443"
    );
    assert_eq!(
        config["contexts"][0]["cache"]["endpoint"],
        "grpcs-proxy://192.168.3.149:31443"
    );
}

#[test]
fn gateway_dns_endpoints_keep_dns_sans_and_gateway_api_hostnames() {
    let spec = spec();
    let objects =
        resources::flame_resources("flame", "flame-system", "uid", &spec, &spec.image, &[])
            .unwrap();
    let certificate = objects
        .iter()
        .find(|object| {
            object["kind"] == "Certificate" && object["metadata"]["name"] == "flame-gateway-tls"
        })
        .unwrap();
    assert_eq!(
        certificate["spec"]["dnsNames"],
        json!([spec.gateway.hostname])
    );
    assert!(certificate["spec"].get("ipAddresses").is_none());
    let gateway = objects
        .iter()
        .find(|object| object["kind"] == "Gateway")
        .unwrap();
    for listener in gateway["spec"]["listeners"].as_array().unwrap() {
        assert_eq!(listener["hostname"], spec.gateway.hostname);
    }
    for route in objects
        .iter()
        .filter(|object| object["kind"] == "HTTPRoute")
    {
        assert_eq!(route["spec"]["hostnames"], json!([spec.gateway.hostname]));
    }
}

#[test]
fn gateway_uses_parameters_ref_and_explicit_nodeports() {
    let mut spec = spec();
    spec.gateway.hostname = "flame.example.com".into();
    spec.gateway.ingress_ip = Some("192.168.3.149".into());
    let objects = resources::flame_resources(
        "flame",
        "flame-system",
        "cluster-uid",
        &spec,
        &spec.image,
        &workers(),
    )
    .unwrap();
    let gateway = objects.iter().find(|o| o["kind"] == "Gateway").unwrap();
    assert_eq!(
        gateway["spec"]["infrastructure"]["parametersRef"]["kind"],
        "EnvoyProxy"
    );
    assert!(!objects.iter().any(|o| o["kind"] == "GatewayClass"));
    let proxy = objects.iter().find(|o| o["kind"] == "EnvoyProxy").unwrap();
    assert_eq!(proxy["spec"]["backendTLS"]["alpnProtocols"], json!(["h2"]));
    let k = &proxy["spec"]["provider"]["kubernetes"];
    assert_eq!(k["envoyService"]["type"], "NodePort");
    assert_eq!(k["envoyService"]["externalTrafficPolicy"], "Cluster");
    assert_eq!(k["envoyService"]["patch"]["type"], "StrategicMerge");
    assert_eq!(
        k["envoyService"]["patch"]["value"]["spec"]["ports"],
        json!([
            {"port":30443,"nodePort":30443},{"port":31443,"nodePort":31443}
        ])
    );
    assert_eq!(
        k["envoyDeployment"]["pod"]["nodeSelector"],
        json!(spec.node_selector)
    );
    assert!(k["envoyDeployment"]["patch"].is_null());
    for forbidden in ["hostPort", "hostIP", "hostNetwork", "LoadBalancer"] {
        assert!(!proxy.to_string().contains(forbidden));
    }
    assert_eq!(gateway["spec"]["listeners"][0]["port"], 30443);
    assert_eq!(gateway["spec"]["listeners"][1]["port"], 31443);
}

#[test]
fn invalid_topologies_are_rejected() {
    let mut value = spec();
    value.node_port = 32767;
    assert!(resources::validate_flame("flame", &value).is_err());
    let mut value = workers();
    value[0].address = "100.90.101.118".into();
    assert!(resources::validate_cache_endpoints(&value).is_err());
    let mut value = workers();
    value.push(value[0].clone());
    assert!(resources::validate_cache_endpoints(&value).is_err());
    let mut value = spec();
    value.gateway.ingress_ip = Some("0.0.0.0".into());
    assert!(resources::validate_flame("flame", &value).is_err());
}

#[test]
fn clusters_have_independent_resources_and_sre_managed_dependency_references() {
    let spec = spec();
    let first = resources::flame_resources(
        "first",
        "team-a",
        "cluster-uid",
        &spec,
        &spec.image,
        &workers(),
    )
    .unwrap();
    let second = resources::flame_resources(
        "second",
        "team-b",
        "cluster-uid",
        &spec,
        &spec.image,
        &workers(),
    )
    .unwrap();
    for (objects, namespace, name) in [(&first, "team-a", "first"), (&second, "team-b", "second")] {
        assert!(
            objects
                .iter()
                .all(|o| o["metadata"]["namespace"] == namespace)
        );
        assert!(!objects.iter().any(|o| {
            ["GatewayClass", "ClusterIssuer", "Bundle"].contains(&o["kind"].as_str().unwrap())
        }));
        let client = objects
            .iter()
            .find(|o| {
                o["kind"] == "Certificate"
                    && o["metadata"]["name"] == format!("{name}-admin-client")
            })
            .unwrap();
        assert_eq!(
            client["spec"]["secretName"],
            format!("{name}-admin-client-tls")
        );
    }
    let first_policy = resources::gateway_policy("same", "team-a", &spec, &workers());
    let second_policy = resources::gateway_policy("same", "team-b", &spec, &workers());
    assert_ne!(
        first_policy["metadata"]["name"],
        second_policy["metadata"]["name"]
    );
}

#[test]
fn multiple_clusters_must_allocate_distinct_ports() {
    let first = spec();
    let mut second = first.clone();
    assert!(resources::allocations_overlap(&first, &second));
    second.node_port = 30082;
    assert!(resources::allocations_overlap(&first, &second));
    second.gateway.session_port = 30444;
    second.gateway.cache_port = 31444;
    assert!(!resources::allocations_overlap(&first, &second));
}

#[test]
fn gateway_nodeports_are_globally_unique_across_listener_and_manager_services() {
    let first = spec();
    let mut second = first.clone();
    second.node_port = 30082;
    second.gateway.session_port = 30444;
    second.gateway.cache_port = 31444;
    second.gateway.hostname = "another.example.test".into();
    second.gateway.ingress_ip = Some("192.168.9.9".into());
    assert!(!resources::allocations_overlap(&first, &second));
    for port in [
        first.node_port,
        first.node_port + 1,
        first.gateway.session_port,
        first.gateway.cache_port,
    ] {
        for field in 0..4 {
            let mut conflict = second.clone();
            match field {
                0 => conflict.node_port = port,
                1 => conflict.node_port = port - 1,
                2 => conflict.gateway.session_port = port,
                _ => conflict.gateway.cache_port = port,
            }
            assert!(
                resources::allocations_overlap(&first, &conflict),
                "missed globally allocated port {port}, field {field}"
            );
        }
    }
}

#[test]
fn gateway_ports_use_nodeport_range_and_cannot_overlap_own_manager() {
    for port in [0, 29999, 32768, u16::MAX, 30080, 30081, 31443] {
        let mut value = spec();
        value.gateway.session_port = port;
        assert!(
            resources::validate_flame("flame", &value).is_err(),
            "accepted invalid session port {port}"
        );
    }
    for port in [0, 29999, 32768, u16::MAX, 30080, 30081, 30443] {
        let mut value = spec();
        value.gateway.cache_port = port;
        assert!(
            resources::validate_flame("flame", &value).is_err(),
            "accepted invalid cache port {port}"
        );
    }
    for port in [30000, 32767] {
        let mut value = spec();
        value.gateway.session_port = port;
        resources::validate_flame("flame", &value).unwrap();
    }
}

#[test]
fn control_plane_starts_without_workers_and_membership_controls_cache_routes() {
    let spec = spec();
    let empty = resources::flame_resources(
        "flame",
        "flame-system",
        "cluster-uid",
        &spec,
        &spec.image,
        &[],
    )
    .unwrap();
    assert!(empty.iter().any(|o| o["kind"] == "Deployment"));
    assert!(!empty.iter().any(|o| o["kind"] == "EndpointSlice"));
    let mut joined = workers();
    joined[1].ready = false;
    let objects = resources::flame_resources(
        "flame",
        "flame-system",
        "cluster-uid",
        &spec,
        &spec.image,
        &joined,
    )
    .unwrap();
    let slices: Vec<_> = objects
        .iter()
        .filter(|o| o["kind"] == "EndpointSlice")
        .collect();
    assert_eq!(slices.len(), 4);
    assert_eq!(
        slices
            .iter()
            .filter(|o| o["endpoints"][0]["conditions"]["ready"] == true)
            .count(),
        2
    );
}

#[tokio::test]
async fn external_gateway_policy_uses_uid_label_without_invalid_owner_reference() {
    let service = tower::service_fn(|request: Request<Body>| async move {
        assert_eq!(
            request.uri().path(),
            "/apis/networking.k8s.io/v1/namespaces/envoy-gateway-system/networkpolicies/test"
        );
        let response = if request.method() == http::Method::GET {
            Response::builder().status(404).body(Body::from(serde_json::to_vec(&json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404,"message":"not found"})).unwrap())).unwrap()
        } else {
            let bytes = request.into_body().collect().await.unwrap().to_bytes();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["metadata"]["labels"][cluster::OWNER_LABEL], "owner-1");
            assert!(value["metadata"].get("ownerReferences").is_none());
            Response::new(Body::from(serde_json::to_vec(&value).unwrap()))
        };
        Ok::<_, std::convert::Infallible>(response)
    });
    let resource = resources::object(
        "networking.k8s.io/v1",
        "NetworkPolicy",
        "test",
        "envoy-gateway-system",
        json!({"podSelector":{}}),
    );
    cluster::apply_gateway_policy(Client::new(service, "default"), resource, &owner(), false)
        .await
        .unwrap();
}

#[tokio::test]
async fn gateway_policy_cleanup_uses_uid_preconditions() {
    let deleted = Arc::new(Mutex::new(false));
    let captured = deleted.clone();
    let service = tower::service_fn(move |request: Request<Body>| {
        let captured = captured.clone();
        async move {
            let response = if request.method() == http::Method::GET {
                assert_eq!(
                    request.uri().path(),
                    "/apis/networking.k8s.io/v1/networkpolicies"
                );
                assert!(request.uri().query().unwrap().contains("owner-uid"));
                json!({"apiVersion":"networking.k8s.io/v1","kind":"NetworkPolicyList","items":[{
                    "apiVersion":"networking.k8s.io/v1","kind":"NetworkPolicy",
                    "metadata":{"name":"old-policy","namespace":"envoy-gateway-system","uid":"policy-uid","resourceVersion":"42","labels":{"flame.xflops.io/owner-uid":"owner-1"}},
                    "spec":{"podSelector":{}}
                }]})
            } else {
                assert_eq!(request.method(), http::Method::DELETE);
                assert_eq!(
                    request.uri().path(),
                    "/apis/networking.k8s.io/v1/namespaces/envoy-gateway-system/networkpolicies/old-policy"
                );
                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                let options: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(options["preconditions"]["uid"], "policy-uid");
                assert_eq!(options["preconditions"]["resourceVersion"], "42");
                *captured.lock().unwrap() = true;
                json!({"apiVersion":"v1","kind":"Status","status":"Success"})
            };
            Ok::<_, std::convert::Infallible>(Response::new(Body::from(
                serde_json::to_vec(&response).unwrap(),
            )))
        }
    });
    cluster::prune_gateway_policies(Client::new(service, "default"), &owner(), None)
        .await
        .unwrap();
    assert!(*deleted.lock().unwrap());
}

#[test]
fn nodeports_and_gateway_resolution_are_derived_without_fixed_ip_fields() {
    let spec = spec();
    assert!(spec.endpoint.is_empty());
    assert_eq!(resources::frontend_port(&spec).unwrap(), 30080);
    let mut override_spec = spec;
    override_spec.endpoint = "192.168.3.150:30090".into();
    assert_eq!(resources::frontend_port(&override_spec).unwrap(), 30090);
    assert_eq!(override_spec.gateway.session_port, 30443);
    assert_eq!(override_spec.gateway.cache_port, 31443);
}

#[test]
fn nodeport_rendering_does_not_resolve_gateway_dns_or_require_a_host_address() {
    let mut spec = spec();
    spec.gateway.hostname = "unresolvable.invalid".into();
    spec.gateway.ingress_ip = None;
    resources::flame_resources("flame", "flame-system", "uid", &spec, &spec.image, &[]).unwrap();
}

#[test]
fn client_secret_contains_config_and_rotating_tls_material() {
    use k8s_openapi::{ByteString, api::core::v1::Secret};
    let spec = spec();
    let mut data = BTreeMap::from([
        ("ca.crt".into(), ByteString(b"ca".to_vec())),
        ("tls.crt".into(), ByteString(b"certificate-1".to_vec())),
        ("tls.key".into(), ByteString(b"private-key-1".to_vec())),
    ]);
    let secret: Secret = serde_json::from_value(
        resources::client_secret("flame", "flame-system", &spec, &data)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(secret.metadata.name.as_deref(), Some("flame-client"));
    let bundle = secret.data.unwrap();
    assert_eq!(bundle.len(), 4);
    let config: Value = serde_yaml::from_slice(&bundle["flame.yaml"].0).unwrap();
    assert_eq!(
        config["contexts"][0]["cluster"]["endpoint"],
        "https://flame.example.test:30443"
    );
    assert_eq!(
        config["contexts"][0]["cluster"]["tls"]["key_data"],
        "cHJpdmF0ZS1rZXktMQ=="
    );
    assert_eq!(
        config["contexts"][0]["cache"]["tls"],
        config["contexts"][0]["cluster"]["tls"]
    );
    data.insert("tls.crt".into(), ByteString(b"certificate-2".to_vec()));
    let renewed: Secret = serde_json::from_value(
        resources::client_secret("flame", "flame-system", &spec, &data)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let renewed = renewed.data.unwrap();
    assert_eq!(renewed["tls.crt"].0, b"certificate-2");
    let renewed_config: Value = serde_yaml::from_slice(&renewed["flame.yaml"].0).unwrap();
    assert_eq!(
        renewed_config["contexts"][0]["cluster"]["tls"]["cert_data"],
        "Y2VydGlmaWNhdGUtMg=="
    );
    assert!(
        config["contexts"][0]["cluster"]["tls"]
            .get("ca_file")
            .is_none()
    );
    assert_eq!(config["contexts"][0]["cluster"]["tls"]["ca_data"], "Y2E=");
    data.remove("tls.key");
    assert!(
        resources::client_secret("flame", "flame-system", &spec, &data)
            .unwrap()
            .is_none()
    );
}

#[test]
fn server_and_client_certificates_have_separate_lifetimes() {
    let spec = spec();
    let objects =
        resources::flame_resources("flame", "flame-system", "uid", &spec, &spec.image, &[])
            .unwrap();
    for (name, duration, renew_before, usage) in [
        ("flame-admin-client", "8760h", "720h", "client auth"),
        ("flame-control-plane-tls", "720h", "8h", "server auth"),
    ] {
        let certificate = objects
            .iter()
            .find(|object| object["kind"] == "Certificate" && object["metadata"]["name"] == name)
            .unwrap();
        assert_eq!(certificate["spec"]["duration"], duration);
        assert_eq!(certificate["spec"]["renewBefore"], renew_before);
        assert_eq!(certificate["spec"]["usages"], json!([usage]));
    }
}

#[test]
fn client_certificates_are_bound_to_namespace_name_and_cluster_uid() {
    let spec = spec();
    let first =
        resources::flame_resources("flame", "team-a", "uid-a", &spec, &spec.image, &[]).unwrap();
    let same_name_other_namespace =
        resources::flame_resources("flame", "team-b", "uid-b", &spec, &spec.image, &[]).unwrap();
    let recreated =
        resources::flame_resources("flame", "team-a", "uid-new", &spec, &spec.image, &[]).unwrap();
    let identity = |resources: &[Value]| {
        resources
            .iter()
            .find(|resource| {
                resource["kind"] == "Certificate"
                    && resource["metadata"]["name"] == "flame-admin-client"
            })
            .unwrap()["spec"]["uris"][0]
            .as_str()
            .unwrap()
            .to_string()
    };
    let expected = identity(&first);
    assert_ne!(expected, identity(&same_name_other_namespace));
    assert_ne!(expected, identity(&recreated));
    assert_eq!(
        expected,
        resources::client_identity("flame", "team-a", "uid-a").unwrap()
    );
    for objects in [&first, &same_name_other_namespace, &recreated] {
        let client_identity = identity(objects);
        let policies: Vec<_> = objects
            .iter()
            .filter(|resource| resource["kind"] == "ClientTrafficPolicy")
            .collect();
        assert_eq!(policies.len(), 2);
        for policy in policies {
            let validation = &policy["spec"]["tls"]["clientValidation"];
            assert_eq!(
                validation["subjectAltNames"]["uris"],
                json!([{"type":"Exact","value":client_identity}])
            );
            assert_eq!(
                validation["caCertificateRefs"][0]["name"],
                spec.gateway.issuer_name
            );
        }
    }
    assert!(resources::client_identity("flame", "team-a", "").is_err());
}

#[test]
fn session_images_use_the_selected_release_unless_explicitly_overridden() {
    let mut spec = spec();
    assert_eq!(resources::session_image(&spec), session_image());
    spec.version = "v0.8.0".into();
    assert_eq!(
        resources::session_image(&spec),
        "docker.io/xflops/flame-session-manager:v0.8.0"
    );
    spec.image = "docker.io/xflops/flame-session-manager@sha256:custom".into();
    assert_eq!(resources::session_image(&spec), spec.image);
    for invalid in ["", "../main", "--version", "main/tag", "v1\nlatest"] {
        assert!(resources::validate_version(invalid).is_err());
    }
}

#[test]
fn standalone_schema_has_no_platform_agent_or_dependency_installer_contract() {
    use kube::CustomResourceExt;
    let schema = serde_json::to_value(FlameCluster::crd()).unwrap();
    let spec = &schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"];
    assert_eq!(spec["version"]["default"], "latest");
    assert!(spec.get("cacheEndpoints").is_some());
    for removed in [
        "workers",
        "trackLatest",
        "apiEndpoint",
        "registryMirror",
        "flameManager",
        "certManager",
    ] {
        assert!(spec.get(removed).is_none(), "obsolete property {removed}");
    }
    let status = &schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["status"]
        ["properties"];
    for removed in ["workers", "capacity", "joinSecret"] {
        assert!(status.get(removed).is_none(), "obsolete status {removed}");
    }
    assert!(
        serde_json::from_value::<FlameClusterSpec>(json!({
            "gateway":{"hostname":"flame.example.test"}, "trackLatest":true
        }))
        .is_err()
    );
}

#[test]
fn cache_endpoints_are_explicit_sre_configuration() {
    let spec: FlameClusterSpec = serde_json::from_value(json!({
        "version":"v0.8.0",
        "gateway":{"hostname":"flame.example.test","ingressIp":"100.117.27.41"},
        "cacheEndpoints":[{"name":"worker-01","address":"192.168.3.8","cachePort":9191}]
    }))
    .unwrap();
    let objects = resources::flame_resources(
        "flame",
        "team-a",
        "cluster-uid",
        &spec,
        &resources::session_image(&spec),
        &spec.cache_endpoints,
    )
    .unwrap();
    assert!(
        objects
            .iter()
            .any(|object| object["kind"] == "EndpointSlice"
                && object["endpoints"][0]["addresses"][0] == "192.168.3.8"
                && object["ports"][0]["port"] == 9191)
    );
    assert!(!objects.iter().any(|object| matches!(
        object["kind"].as_str(),
        Some(
            "ClusterRole"
                | "ClusterRoleBinding"
                | "Role"
                | "RoleBinding"
                | "CustomResourceDefinition"
                | "Job"
                | "DaemonSet"
        )
    )));
}

#[test]
fn native_control_plane_uses_server_tls_for_both_ports_without_distributing_private_keys() {
    let mut spec = spec();
    spec.endpoint = "192.168.3.149:30080".into();
    let objects =
        resources::flame_resources("flame", "flame-system", "uid", &spec, &spec.image, &[])
            .unwrap();
    let certificate = objects
        .iter()
        .find(|object| {
            object["kind"] == "Certificate"
                && object["metadata"]["name"] == "flame-control-plane-tls"
        })
        .unwrap();
    assert_eq!(
        certificate["spec"]["dnsNames"],
        json!([
            "flame-session-manager",
            "flame-session-manager.flame-system.svc",
            "flame-session-manager.flame-system.svc.cluster.local"
        ])
    );
    assert_eq!(certificate["spec"]["ipAddresses"], json!(["192.168.3.149"]));
    assert_eq!(certificate["spec"]["usages"], json!(["server auth"]));
    assert_eq!(
        certificate["spec"]["privateKey"]["rotationPolicy"],
        "Always"
    );
    let deployment = objects
        .iter()
        .find(|object| object["kind"] == "Deployment")
        .unwrap();
    let pod = &deployment["spec"]["template"]["spec"];
    let volume = pod["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|volume| volume["name"] == "control-plane-tls")
        .unwrap();
    assert_eq!(
        volume["projected"]["sources"][0]["secret"]["name"],
        "flame-control-plane-tls"
    );
    assert_eq!(
        volume["projected"]["sources"][1]["configMap"]["name"],
        spec.gateway.issuer_name
    );
    assert_eq!(
        volume["projected"]["sources"][1]["configMap"]["items"],
        json!([{"key":"ca.crt","path":"ca.crt"}])
    );
    let configmap = objects
        .iter()
        .find(|object| object["kind"] == "ConfigMap")
        .unwrap();
    let config: Value =
        serde_yaml::from_str(configmap["data"]["flame-cluster.yaml"].as_str().unwrap()).unwrap();
    assert_eq!(
        config["cluster"]["endpoint"],
        "https://flame-session-manager:8080"
    );
    assert_eq!(
        config["cluster"]["tls"],
        json!({"cert_file":"/etc/flame/control-plane-tls/tls.crt","key_file":"/etc/flame/control-plane-tls/tls.key","ca_file":"/etc/flame/control-plane-tls/ca.crt"})
    );
    assert!(!configmap.to_string().contains("BEGIN PRIVATE KEY"));
    let policy = objects
        .iter()
        .find(|object| object["kind"] == "BackendTLSPolicy")
        .unwrap();
    assert_eq!(policy["apiVersion"], "gateway.networking.k8s.io/v1");
    assert_eq!(
        policy["spec"]["targetRefs"],
        json!([{"group":"","kind":"Service","name":"flame-session-manager","sectionName":"frontend"}])
    );
    assert_eq!(
        policy["spec"]["validation"],
        json!({"hostname":"flame-session-manager.flame-system.svc","caCertificateRefs":[{"group":"","kind":"ConfigMap","name":spec.gateway.issuer_name}]})
    );
}
