use crate::{Error, api::*};
use serde_json::{Value, json};
use std::net::Ipv4Addr;

pub fn frontend_port(spec: &FlameClusterSpec) -> Result<u16, Error> {
    if !spec.endpoint.is_empty() {
        return Ok(manager_endpoint(&spec.endpoint)?.port());
    }
    if !(30000..32767).contains(&spec.node_port) {
        return Err(Error::Invalid(
            "nodePort must be within 30000..32766; backend uses nodePort + 1".into(),
        ));
    }
    Ok(spec.node_port)
}

fn dns_label(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

pub fn session_image(spec: &FlameClusterSpec) -> String {
    if spec.image == crate::api::session_image() {
        format!("docker.io/xflops/flame-session-manager:{}", spec.version)
    } else {
        spec.image.clone()
    }
}

pub fn validate_flame(name: &str, spec: &FlameClusterSpec) -> Result<(), Error> {
    let invalid = |message: &str| Error::Invalid(message.into());
    if !dns_label(name, 32) {
        return Err(invalid(
            "cluster name must be a DNS label of at most 32 characters",
        ));
    }
    if spec.image.is_empty() {
        return Err(invalid("session-manager image is required"));
    }
    frontend_port(spec)?;
    validate_version(&spec.version)?;
    validate_cache_endpoints(&spec.cache_endpoints)?;
    let gateway = &spec.gateway;
    if gateway.hostname.is_empty() || gateway.hostname.contains(['/', ':', ' ', '*']) {
        return Err(invalid(
            "gateway.hostname must be a concrete DNS hostname or IPv4 address",
        ));
    }
    let frontend = frontend_port(spec)?;
    if !(30000..=32767).contains(&gateway.session_port)
        || !(30000..=32767).contains(&gateway.cache_port)
        || gateway.session_port == gateway.cache_port
        || [gateway.session_port, gateway.cache_port]
            .iter()
            .any(|port| [frontend, frontend + 1].contains(port))
    {
        return Err(invalid(
            "gateway sessionPort/cachePort must be distinct NodePorts within 30000..32767 and must not overlap session-manager ports",
        ));
    }
    if let Some(ip) = &gateway.ingress_ip {
        let ip = ip
            .parse::<Ipv4Addr>()
            .map_err(|_| invalid("ingressIp must be an IPv4 address"))?;
        if ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() {
            return Err(invalid("ingressIp must be a unicast IPv4 address"));
        }
    }
    Ok(())
}

pub fn allocations_overlap(a: &FlameClusterSpec, b: &FlameClusterSpec) -> bool {
    let a_port = frontend_port(a).unwrap_or(0);
    let b_port = frontend_port(b).unwrap_or(0);
    let a_ports = [
        a_port,
        a_port.saturating_add(1),
        a.gateway.session_port,
        a.gateway.cache_port,
    ];
    let b_ports = [
        b_port,
        b_port.saturating_add(1),
        b.gateway.session_port,
        b.gateway.cache_port,
    ];
    a_ports.iter().any(|port| b_ports.contains(port))
}

/// URI SAN shared by this cluster's client identity and both gateway listeners.
/// The API object's UID prevents reuse when a cluster is deleted and recreated.
pub fn client_identity(name: &str, namespace: &str, cluster_uid: &str) -> Result<String, Error> {
    if cluster_uid.is_empty()
        || !cluster_uid
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(Error::Invalid(
            "cluster UID is required for client TLS identity".into(),
        ));
    }
    Ok(format!(
        "spiffe://flame.xflops.io/ns/{namespace}/clusters/{name}/uid/{cluster_uid}/client/admin"
    ))
}

/// Pure desired-state rendering: workers are external endpoints, never pods.
pub fn flame_resources(
    name: &str,
    namespace: &str,
    cluster_uid: &str,
    spec: &FlameClusterSpec,
    image: &str,
    workers: &[CacheEndpoint],
) -> Result<Vec<Value>, Error> {
    validate_flame(name, spec)?;
    let frontend_port = frontend_port(spec)?;
    validate_cache_endpoints(workers)?;
    let sm = format!("{name}-session-manager");
    let gateway_name = format!("{name}-gateway");
    let config_name = format!("{name}-config");
    let claim = spec
        .existing_claim
        .clone()
        .unwrap_or_else(|| format!("{sm}-data"));
    let labels = json!({"app.kubernetes.io/name":"flame","app.kubernetes.io/instance":name,"app.kubernetes.io/component":"session-manager"});
    let config = serde_yaml::to_string(&json!({
        "cluster":{"name":name,"endpoint":format!("https://{sm}:8080"),"tls":{"cert_file":"/etc/flame/control-plane-tls/tls.crt","key_file":"/etc/flame/control-plane-tls/tls.key","ca_file":"/etc/flame/control-plane-tls/ca.crt"},"resreq":spec.resource_request,
            "policies":spec.policies,"storage":"fs:///var/lib/flame/session","schedule_interval":100,
            "executors":{"shims":["host","cri"]},"limits":{"node":{"max_executors":10}}},
        "cache":{"endpoint":format!("grpc://{name}-cache-bootstrap:9090")}
    }))?;
    let mut output = Vec::new();
    if spec.existing_claim.is_none() {
        let mut pvc = object(
            "v1",
            "PersistentVolumeClaim",
            &claim,
            namespace,
            json!({"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":spec.storage_size}}}),
        );
        if let Some(class) = &spec.storage_class {
            pvc["spec"]["storageClassName"] = json!(class);
        }
        pvc["metadata"]["annotations"] = json!({"flame.xflops.io/retain":"true"});
        output.push(pvc);
    }
    output.push(json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":config_name,"namespace":namespace},"data":{"flame-cluster.yaml":config}}));
    output.push(object("v1", "Service", &sm, namespace, json!({"type":"NodePort","selector":labels,"ports":[
        {"name":"frontend","port":8080,"targetPort":"frontend","nodePort":frontend_port,"appProtocol":"kubernetes.io/h2c"},
        {"name":"backend","port":8081,"targetPort":"backend","nodePort":frontend_port+1}
    ]})));
    output.push(object("apps/v1", "Deployment", &sm, namespace, json!({
        "replicas":1,"strategy":{"type":"Recreate"},"selector":{"matchLabels":labels},
        "template":{"metadata":{"labels":labels,"annotations":{"flame.xflops.io/config-hash":crate::digest(&config)?,"flame.xflops.io/rollout-revision":spec.rollout_revision}},
            "spec":{"automountServiceAccountToken":false,"nodeSelector":spec.node_selector,
                "containers":[{"name":"session-manager","image":image,"imagePullPolicy":"Always","workingDir":"/usr/local/flame",
                    "command":["/usr/local/flame/bin/flame-session-manager"],"args":["--config","/usr/local/flame/conf/flame-cluster.yaml"],
                    "env":[{"name":"FLAME_HOME","value":"/usr/local/flame"},{"name":"RUST_LOG","value":"info"}],
                    "resources":spec.resources,"ports":[{"name":"frontend","containerPort":8080},{"name":"backend","containerPort":8081}],
                    "startupProbe":{"tcpSocket":{"port":"frontend"},"failureThreshold":30,"periodSeconds":2},
                    "readinessProbe":{"tcpSocket":{"port":"frontend"},"periodSeconds":5},
                    "livenessProbe":{"tcpSocket":{"port":"frontend"},"periodSeconds":10},
                    "volumeMounts":[{"name":"config","mountPath":"/usr/local/flame/conf/flame-cluster.yaml","subPath":"flame-cluster.yaml","readOnly":true},
                        {"name":"data","mountPath":"/var/lib/flame/session","subPath":"session"},{"name":"data","mountPath":"/usr/local/flame/events","subPath":"events"},
                        {"name":"logs","mountPath":"/usr/local/flame/logs"},{"name":"control-plane-tls","mountPath":"/etc/flame/control-plane-tls","readOnly":true}]}],
                "volumes":[{"name":"config","configMap":{"name":config_name}},{"name":"data","persistentVolumeClaim":{"claimName":claim}},{"name":"logs","emptyDir":{}},{"name":"control-plane-tls","projected":{"sources":[{"secret":{"name":format!("{name}-control-plane-tls"),"items":[{"key":"tls.crt","path":"tls.crt"},{"key":"tls.key","path":"tls.key"}]}},{"configMap":{"name":spec.gateway.issuer_name,"items":[{"key":"ca.crt","path":"ca.crt"}]}}]}}]}}
    })));
    let mut server_certificate = object(
        "cert-manager.io/v1",
        "Certificate",
        &format!("{name}-control-plane-tls"),
        namespace,
        json!({
            "secretName":format!("{name}-control-plane-tls"),"dnsNames":[sm,format!("{sm}.{namespace}.svc"),format!("{sm}.{namespace}.svc.cluster.local")],
            "duration":"24h","renewBefore":"8h","usages":["server auth"],"privateKey":{"algorithm":"ECDSA","size":256,"rotationPolicy":"Always"},
            "issuerRef":{"group":"cert-manager.io","kind":"ClusterIssuer","name":spec.gateway.issuer_name}
        }),
    );
    if !spec.endpoint.is_empty() {
        server_certificate["spec"]["ipAddresses"] =
            json!([manager_endpoint(&spec.endpoint)?.ip().to_string()]);
    }
    output.push(server_certificate);
    output.push(object("gateway.networking.k8s.io/v1","BackendTLSPolicy",&format!("{name}-control-plane-tls"),namespace,json!({
        "targetRefs":[{"group":"","kind":"Service","name":sm,"sectionName":"frontend"}],
        "validation":{"hostname":format!("{sm}.{namespace}.svc"),"caCertificateRefs":[{"group":"","kind":"ConfigMap","name":spec.gateway.issuer_name}]}
    })));
    let bootstrap = format!("{name}-cache-bootstrap");
    output.push(cache_service(&bootstrap, namespace));
    // One Service port can target different per-worker cache ports through EndpointSlices.
    for worker in workers {
        let id = if dns_label(&worker.name, 24) {
            worker.name.clone()
        } else {
            format!("node-{}", &crate::digest(&worker.name)?[..16])
        };
        output.push(cache_slice(
            &format!("{bootstrap}-{id}"),
            &bootstrap,
            namespace,
            worker,
        ));
        let service = format!("{name}-cache-{id}");
        output.push(cache_service(&service, namespace));
        output.push(cache_slice(&service, &service, namespace, worker));
        output.push(route(&service, namespace, &gateway_name, "cache-grpc", &spec.gateway.hostname,
            json!([{"matches":[{"headers":[{"name":"x-flame-object-cache","type":"Exact","value":format!("{}:{}",worker.address,worker.cache_port)}]}],
                "backendRefs":[{"name":service,"port":9090}]}])));
    }
    output.push(route(
        &format!("{name}-cache-initial"),
        namespace,
        &gateway_name,
        "cache-grpc",
        &spec.gateway.hostname,
        json!([{"backendRefs":[{"name":bootstrap,"port":9090}]}]),
    ));
    output.push(route(
        &format!("{name}-session"),
        namespace,
        &gateway_name,
        "session-https",
        &spec.gateway.hostname,
        json!([{"backendRefs":[{"name":sm,"port":8080}]}]),
    ));

    let g = &spec.gateway;
    let infrastructure = json!({
        "envoyDeployment":{"replicas":1,"pod":{"nodeSelector":spec.node_selector}},
        "envoyService":{"type":"NodePort","externalTrafficPolicy":"Cluster","annotations":g.service_annotations,
            "patch":{"type":"StrategicMerge","value":{"spec":{"ports":[
                {"port":g.session_port,"nodePort":g.session_port},
                {"port":g.cache_port,"nodePort":g.cache_port}
            ]}}}}
    });
    output.push(object(
        "gateway.envoyproxy.io/v1alpha1",
        "EnvoyProxy",
        &gateway_name,
        namespace,
        json!({"provider":{"type":"Kubernetes","kubernetes":infrastructure},"backendTLS":{"alpnProtocols":["h2"]}}),
    ));
    let client_identity = client_identity(name, namespace, cluster_uid)?;
    let tls_secret = format!("{name}-gateway-tls");
    let ip_hostname = g.hostname.parse::<Ipv4Addr>().is_ok();
    let mut gateway_certificate = json!({"secretName":tls_secret,"issuerRef":{"group":"cert-manager.io","kind":"ClusterIssuer","name":g.issuer_name}});
    gateway_certificate[if ip_hostname {
        "ipAddresses"
    } else {
        "dnsNames"
    }] = json!([g.hostname]);
    output.push(object(
        "cert-manager.io/v1",
        "Certificate",
        &tls_secret,
        namespace,
        gateway_certificate,
    ));
    output.push(object("cert-manager.io/v1", "Certificate", &format!("{name}-admin-client"), namespace,
        json!({"commonName":format!("{name}-admin"),"uris":[client_identity],"duration":"8760h","renewBefore":"720h","secretName":format!("{name}-admin-client-tls"),
        "privateKey":{"algorithm":"ECDSA","size":256,"rotationPolicy":"Always"},"usages":["client auth"],
        "issuerRef":{"group":"cert-manager.io","kind":"ClusterIssuer","name":g.issuer_name}})));
    let listener = |section: &str, port: u16| {
        let mut listener = json!({"name":section,"port":port,"protocol":"HTTPS",
        "tls":{"mode":"Terminate","certificateRefs":[{"kind":"Secret","name":tls_secret}]},"allowedRoutes":{"namespaces":{"from":"Same"}}});
        // Gateway API hostname fields allow DNS names only. IP clients verify the IP SAN.
        if !ip_hostname {
            listener["hostname"] = json!(g.hostname);
        }
        listener
    };
    output.push(object("gateway.networking.k8s.io/v1", "Gateway", &gateway_name, namespace, json!({
        "gatewayClassName":g.class_name,"infrastructure":{"parametersRef":{"group":"gateway.envoyproxy.io","kind":"EnvoyProxy","name":gateway_name}},
        "listeners":[listener("session-https",g.session_port),listener("cache-grpc",g.cache_port)]
    })));
    if g.client_auth {
        for section in ["session-https", "cache-grpc"] {
            output.push(object("gateway.envoyproxy.io/v1alpha1", "ClientTrafficPolicy", &format!("{name}-{section}-mtls"), namespace,
                json!({"targetRefs":[{"group":"gateway.networking.k8s.io","kind":"Gateway","name":gateway_name,"sectionName":section}],
                    "tls":{"clientValidation":{"caCertificateRefs":[{"group":"","kind":"ConfigMap","name":g.issuer_name}],"subjectAltNames":{"uris":[{"type":"Exact","value":client_identity}]}}}})));
        }
    }
    Ok(output)
}

fn cache_service(name: &str, namespace: &str) -> Value {
    object(
        "v1",
        "Service",
        name,
        namespace,
        json!({"type":"ClusterIP","ports":[{"name":"grpc","port":9090,"targetPort":9090,"appProtocol":"kubernetes.io/h2c"}]}),
    )
}
fn cache_slice(name: &str, service: &str, namespace: &str, worker: &CacheEndpoint) -> Value {
    json!({"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":{"name":name,"namespace":namespace,
        "labels":{"kubernetes.io/service-name":service,"endpointslice.kubernetes.io/managed-by":"flame-operator"}},
        "addressType":"IPv4","ports":[{"name":"grpc","protocol":"TCP","port":worker.cache_port}],
        "endpoints":[{"addresses":[worker.address],"conditions":{"ready":worker.ready}}]})
}
fn route(
    name: &str,
    namespace: &str,
    gateway: &str,
    section: &str,
    host: &str,
    rules: Value,
) -> Value {
    let mut spec = json!({"parentRefs":[{"name":gateway,"sectionName":section}],"rules":rules});
    if host.parse::<Ipv4Addr>().is_err() {
        spec["hostnames"] = json!([host]);
    }
    object(
        "gateway.networking.k8s.io/v1",
        "HTTPRoute",
        name,
        namespace,
        spec,
    )
}

pub fn gateway_policy(
    name: &str,
    flame_namespace: &str,
    spec: &FlameClusterSpec,
    workers: &[CacheEndpoint],
) -> Value {
    let caches: Vec<Value> = workers
        .iter()
        .map(|worker| {
            json!({"to":[{"ipBlock":{"cidr":format!("{}/32",worker.address)}}],
        "ports":[{"protocol":"TCP","port":worker.cache_port}]})
        })
        .collect();
    let mut egress = caches;
    egress.push(json!({"to":[{"namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":flame_namespace}},
        "podSelector":{"matchLabels":{"app.kubernetes.io/name":"flame","app.kubernetes.io/instance":name,"app.kubernetes.io/component":"session-manager"}}}],"ports":[{"protocol":"TCP","port":8080}]}));
    egress.push(json!({"to":[{"namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":"kube-system"}},"podSelector":{"matchLabels":{"k8s-app":"kube-dns"}}}],
        "ports":[{"protocol":"UDP","port":53},{"protocol":"TCP","port":53}]}));
    egress.push(json!({"to":[{"podSelector":{"matchLabels":{"control-plane":"envoy-gateway"}}}]}));
    object(
        "networking.k8s.io/v1",
        "NetworkPolicy",
        &format!(
            "{name}-gateway-egress-{}",
            &crate::digest(&flame_namespace).expect("namespace serialization")[..12]
        ),
        &spec.gateway.envoy_namespace,
        json!({"podSelector":{"matchLabels":{"gateway.envoyproxy.io/owning-gateway-namespace":flame_namespace,
            "gateway.envoyproxy.io/owning-gateway-name":format!("{name}-gateway")}},"policyTypes":["Egress"],"egress":egress}),
    )
}

/// A mountable client bundle, kept in sync with the cert-manager client identity.
/// flame.yaml embeds TLS credentials so clients can use the file on its own.
pub fn client_secret(
    name: &str,
    namespace: &str,
    spec: &FlameClusterSpec,
    source: &std::collections::BTreeMap<String, k8s_openapi::ByteString>,
) -> Result<Option<Value>, Error> {
    use k8s_openapi::{ByteString, api::core::v1::Secret};
    use kube::api::ObjectMeta;
    if ["ca.crt", "tls.crt", "tls.key"]
        .iter()
        .any(|key| source.get(*key).is_none_or(|value| value.0.is_empty()))
    {
        return Ok(None);
    }
    use base64::{Engine, engine::general_purpose::STANDARD};
    let tls = json!({"ca_data":STANDARD.encode(&source["ca.crt"].0),"cert_data":STANDARD.encode(&source["tls.crt"].0),"key_data":STANDARD.encode(&source["tls.key"].0)});
    let config = serde_yaml::to_string(&json!({"current-context":name,"contexts":[{"name":name,
        "cluster":{"endpoint":format!("https://{}:{}",spec.gateway.hostname,spec.gateway.session_port),"tls":tls},
        "cache":{"endpoint":format!("grpcs-proxy://{}:{}",spec.gateway.hostname,spec.gateway.cache_port),"tls":tls}
    }]}))?;
    let mut data: std::collections::BTreeMap<_, _> = ["ca.crt", "tls.crt", "tls.key"]
        .into_iter()
        .map(|key| (key.into(), source[key].clone()))
        .collect();
    data.insert("flame.yaml".into(), ByteString(config.into_bytes()));
    Ok(Some(serde_json::to_value(Secret {
        metadata: ObjectMeta {
            name: Some(format!("{name}-client")),
            namespace: Some(namespace.into()),
            ..Default::default()
        },
        type_: Some("Opaque".into()),
        data: Some(data),
        ..Default::default()
    })?))
}

pub fn manager_endpoint(value: &str) -> Result<std::net::SocketAddrV4, Error> {
    flame_api::validation::frontend_endpoint(value)
        .map_err(|error| Error::Invalid(error.to_string()))
}

pub fn object(api: &str, kind: &str, name: &str, namespace: &str, spec: Value) -> Value {
    let mut value = json!({"apiVersion":api,"kind":kind,"metadata":{"name":name},"spec":spec});
    if !namespace.is_empty() {
        value["metadata"]["namespace"] = json!(namespace);
    }
    value
}

pub fn validate_version(version: &str) -> Result<(), Error> {
    if version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
        || version.starts_with(['.', '-'])
    {
        return Err(Error::Invalid(
            "version must be latest or a valid Flame release tag".into(),
        ));
    }
    Ok(())
}

pub fn validate_cache_endpoints(endpoints: &[CacheEndpoint]) -> Result<(), Error> {
    let mut names = std::collections::BTreeSet::new();
    let mut addresses = std::collections::BTreeSet::new();
    for endpoint in endpoints {
        flame_api::validation::cache_endpoint(endpoint)
            .map_err(|error| Error::Invalid(error.to_string()))?;
        if !names.insert(&endpoint.name)
            || !addresses.insert((&endpoint.address, endpoint.cache_port))
        {
            return Err(Error::Invalid(
                "cache endpoint names and address/port pairs must be unique".into(),
            ));
        }
    }
    Ok(())
}
