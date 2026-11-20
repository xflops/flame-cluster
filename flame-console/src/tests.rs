use super::*;
use crate::{connection::ConnectionConfig, rpc::*};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request as GrpcRequest, Response as GrpcResponse, Status};
use tower::ServiceExt;

#[derive(Default)]
struct Flame {
    fail_nodes: bool,
}
#[tonic::async_trait]
impl frontend_server::Frontend for Flame {
    async fn get_session(
        &self,
        request: GrpcRequest<GetSessionRequest>,
    ) -> Result<GrpcResponse<Session>, Status> {
        let id = request.into_inner().session_id;
        let state = if id.contains("closed") {
            1
        } else if id.contains("suspended") {
            3
        } else if id.contains("suspending") {
            2
        } else {
            0
        };
        Ok(GrpcResponse::new(Session {
            metadata: Some(Metadata {
                id,
                name: "test".into(),
            }),
            status: Some(SessionStatus {
                state,
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn close_session(
        &self,
        request: GrpcRequest<CloseSessionRequest>,
    ) -> Result<GrpcResponse<Session>, Status> {
        let id = request.into_inner().session_id;
        if id == "wrong-state" {
            return Err(Status::failed_precondition("state changed"));
        }
        Ok(GrpcResponse::new(Session {
            metadata: Some(Metadata {
                id,
                name: "close_session".into(),
            }),
            status: Some(SessionStatus {
                state: 1,
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn suspend_session(
        &self,
        request: GrpcRequest<SuspendSessionRequest>,
    ) -> Result<GrpcResponse<Session>, Status> {
        let id = request.into_inner().session_id;
        if id == "wrong-state" {
            return Err(Status::failed_precondition("state changed"));
        }
        Ok(GrpcResponse::new(Session {
            metadata: Some(Metadata {
                id,
                name: "suspend_session".into(),
            }),
            status: Some(SessionStatus {
                state: 2,
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn resume_session(
        &self,
        request: GrpcRequest<ResumeSessionRequest>,
    ) -> Result<GrpcResponse<Session>, Status> {
        let id = request.into_inner().session_id;
        if id == "wrong-state" {
            return Err(Status::failed_precondition("state changed"));
        }
        Ok(GrpcResponse::new(Session {
            metadata: Some(Metadata {
                id,
                name: "resume_session".into(),
            }),
            status: Some(SessionStatus {
                state: 0,
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn delete_session(
        &self,
        request: GrpcRequest<DeleteSessionRequest>,
    ) -> Result<GrpcResponse<Session>, Status> {
        let id = request.into_inner().session_id;
        if id == "wrong-state" {
            return Err(Status::failed_precondition("state changed"));
        }
        Ok(GrpcResponse::new(Session {
            metadata: Some(Metadata {
                id,
                name: "delete_session".into(),
            }),
            status: Some(SessionStatus {
                state: 1,
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn list_nodes(
        &self,
        request: GrpcRequest<ListNodesRequest>,
    ) -> Result<GrpcResponse<NodeList>, Status> {
        let mut paging = request.into_inner().paging.unwrap();
        assert_eq!(paging.limit, 12, "request only the displayed page");
        assert!(
            [1, 84].contains(&paging.number),
            "must not traverse other pages"
        );
        if self.fail_nodes {
            return Err(Status::unavailable("SYNTHETIC_SECRET_NODES"));
        }
        let offset = (paging.number - 1) * i64::from(paging.limit);
        paging.total = Some(1003);
        Ok(GrpcResponse::new(NodeList {
            nodes: (offset..1003)
                .take(paging.limit as usize)
                .map(|i| Node {
                    metadata: Some(Metadata {
                        name: format!("worker-{}", i + 1),
                        id: String::new(),
                    }),
                    ..Default::default()
                })
                .collect(),
            paging: Some(paging),
        }))
    }
    async fn list_applications(
        &self,
        request: GrpcRequest<ListApplicationsRequest>,
    ) -> Result<GrpcResponse<ApplicationList>, Status> {
        let request = request.into_inner();
        let mut paging = request.paging.unwrap();
        let applications = if request.state == Some(1) {
            assert_eq!((paging.limit, paging.number), (2, 4));
            vec![
                Application {
                    metadata: Some(Metadata {
                        name: "z-last".into(),
                        id: String::new(),
                    }),
                    ..Default::default()
                },
                Application {
                    metadata: Some(Metadata {
                        name: "a-first".into(),
                        id: String::new(),
                    }),
                    ..Default::default()
                },
            ]
        } else {
            vec![]
        };
        paging.total = Some(if request.state == Some(1) { 500 } else { 0 });
        Ok(GrpcResponse::new(ApplicationList {
            applications,
            paging: Some(paging),
        }))
    }
    async fn list_sessions(
        &self,
        request: GrpcRequest<ListSessionsRequest>,
    ) -> Result<GrpcResponse<SessionList>, Status> {
        let mut paging = request.into_inner().paging.unwrap();
        paging.total = Some(0);
        Ok(GrpcResponse::new(SessionList {
            sessions: vec![],
            paging: Some(paging),
        }))
    }
    async fn list_executors(
        &self,
        _: GrpcRequest<ListExecutorsRequest>,
    ) -> Result<GrpcResponse<ExecutorList>, Status> {
        Err(Status::unavailable("executor service unavailable"))
    }
    async fn list_tasks(
        &self,
        request: GrpcRequest<ListTasksRequest>,
    ) -> Result<GrpcResponse<TaskList>, Status> {
        let request = request.into_inner();
        if request.session_id == "secret-error" {
            return Err(Status::internal("SYNTHETIC_SECRET_TASKS"));
        }
        let count = if request.session_id == "large-session" {
            1000
        } else {
            3
        };
        let mut paging = request.paging.unwrap();
        if request.session_id == "missing-paging" {
            return Ok(GrpcResponse::new(TaskList::default()));
        }
        if request.session_id == "missing-total" {
            return Ok(GrpcResponse::new(TaskList {
                tasks: vec![],
                paging: Some(paging),
            }));
        }
        assert_eq!(paging.limit, 12, "request only the displayed page");
        let offset = (paging.number - 1) * i64::from(paging.limit);
        paging.total = Some(count);
        let tasks = (0..count)
            .rev()
            .skip(offset as usize)
            .take(paging.limit as usize)
            .map(|id| Task {
                metadata: Some(Metadata {
                    id: id.to_string(),
                    name: String::new(),
                }),
                spec: Some(TaskSpec {
                    session_id: "test".into(),
                }),
                ..Default::default()
            })
            .collect();
        Ok(GrpcResponse::new(TaskList {
            tasks,
            paging: Some(paging),
        }))
    }
}
async fn frontend(service: Flame) -> (AppState, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(frontend_server::FrontendServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    (
        AppState::connect(ConnectionConfig::new(endpoint)).unwrap(),
        stop,
    )
}
async fn get(app: Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("host", "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn connection_and_health_need_no_upstream_or_kubernetes() {
    let mut config = ConnectionConfig::new("http://127.0.0.1:9");
    config.display_name = "Training".into();
    let app = router(AppState::connect(config).unwrap());
    let (status, data) = get(app.clone(), "/api/connection").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        data,
        json!({"endpoint":"http://127.0.0.1:9","displayName":"Training"})
    );
    assert_eq!(get(app.clone(), "/healthz").await.0, StatusCode::OK);
    for path in [
        "/",
        "/api/clusters",
        "/api/dashboard",
        "/api/clusters/training/flame/client-config",
    ] {
        assert_eq!(get(app.clone(), path).await.0, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn lists_one_native_node_page_and_preserves_total_and_order() {
    let (state, stop) = frontend(Flame::default()).await;
    let app = router(state);
    let (status, first) = get(app.clone(), "/api/nodes").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["total"], 1003);
    assert_eq!(first["items"].as_array().unwrap().len(), 12);
    assert_eq!(first["items"][0]["metadata"]["name"], "worker-1");
    assert_eq!(first["hasNext"], true);
    let (status, last) = get(app, "/api/nodes?page=84").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(last["items"].as_array().unwrap().len(), 7);
    assert_eq!(last["items"][0]["metadata"]["name"], "worker-997");
    assert_eq!(last["hasNext"], false);
    let _ = stop.send(());
}

#[tokio::test]
async fn forwards_native_state_filter_and_paging_without_sorting_locally() {
    let (state, stop) = frontend(Flame::default()).await;
    let (status, data) = get(
        router(state),
        "/api/applications?page=4&pageSize=2&state=Disabled",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["total"], 500);
    assert_eq!(data["items"][0]["metadata"]["name"], "z-last");
    assert_eq!(data["items"][1]["metadata"]["name"], "a-first");
    assert_eq!(data["hasNext"], true);
    let _ = stop.send(());
}

#[tokio::test]
async fn upstream_failure_is_not_reported_as_a_healthy_empty_list() {
    let (state, stop) = frontend(Flame { fail_nodes: true }).await;
    let app = router(state);
    let (status, data) = get(app.clone(), "/api/nodes").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(data["error"], "Flame upstream request failed");
    let (status, data) = get(app, "/api/sessions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["items"], json!([]));
    assert_eq!(data["total"], 0);
    let _ = stop.send(());
}

#[tokio::test]
async fn invalid_filters_and_paging_are_rejected_before_connecting() {
    let app = router(AppState::connect(ConnectionConfig::new("http://127.0.0.1:9")).unwrap());
    for query in [
        "page=0",
        "pageSize=0",
        "pageSize=101",
        "page=18446744073709551615",
        "state=Invalid",
        "search=worker",
    ] {
        assert_eq!(
            get(app.clone(), &format!("/api/nodes?{query}")).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        get(app, "/api/sessions/session/tasks?state=Running")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn tasks_use_native_bounded_pages_and_require_upstream_totals() {
    let (state, stop) = frontend(Flame::default()).await;
    let app = router(state);
    let (status, first) = get(app.clone(), "/api/sessions/large-session/tasks").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["items"].as_array().unwrap().len(), 12);
    assert_eq!(first["items"][0]["metadata"]["id"], "999");
    assert_eq!(first["total"], 1000);
    assert_eq!(first["hasNext"], true);
    let (status, last) = get(app.clone(), "/api/sessions/large-session/tasks?page=84").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(last["items"].as_array().unwrap().len(), 4);
    assert_eq!(last["hasNext"], false);
    for session in ["missing-paging", "missing-total"] {
        assert_eq!(
            get(app.clone(), &format!("/api/sessions/{session}/tasks"))
                .await
                .0,
            StatusCode::BAD_GATEWAY
        );
    }
    let _ = stop.send(());
}

#[tokio::test]
async fn session_actions_forward_ids_and_surface_conflicting_states() {
    let (state, stop) = frontend(Flame::default()).await;
    let app = router(state);
    for (action, id, new_state) in [
        ("close", "open", 1),
        ("suspend", "open", 2),
        ("resume", "suspended", 0),
        ("delete", "closed", 1),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/sessions/{id}/actions/{action}"))
                    .header("host", "localhost")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let data: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(data["session"]["metadata"]["id"], id);
        assert_eq!(data["session"]["status"]["state"], new_state);
    }
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/wrong-state/actions/close")
                .header("host", "localhost")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let _ = stop.send(());
}

#[tokio::test]
async fn rejects_unsafe_or_unknown_mutations_before_connecting() {
    let app = router(AppState::connect(ConnectionConfig::new("http://127.0.0.1:9")).unwrap());
    for (action, content, origin, body, status) in [
        (
            "close",
            "application/json",
            Some("https://another.example"),
            "{}",
            StatusCode::FORBIDDEN,
        ),
        (
            "close",
            "text/plain",
            None,
            "{}",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "open",
            "application/json",
            None,
            "{}",
            StatusCode::NOT_FOUND,
        ),
        (
            "close",
            "application/json",
            None,
            "{\"unexpected\":true}",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let mut request = Request::builder()
            .method("POST")
            .uri(format!("/api/sessions/session/actions/{action}"))
            .header("content-type", content)
            .header("host", "localhost");
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
    assert_eq!(
        get(app, "/api/sessions/session/actions/close").await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn direct_tls_and_mtls_verify_the_server_and_client_identity() {
    use tonic::transport::{Certificate, Identity, ServerTlsConfig};
    let directory = tempfile::tempdir().unwrap();
    let openssl = |args: &[&str]| {
        let output = std::process::Command::new("openssl")
            .current_dir(directory.path())
            .args(args)
            .output()
            .expect("OpenSSL is required for TLS transport tests");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    openssl(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-days",
        "1",
        "-keyout",
        "ca.key",
        "-out",
        "ca.crt",
        "-subj",
        "/CN=Flame Test CA",
    ]);
    openssl(&[
        "req",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        "identity.key",
        "-out",
        "identity.csr",
        "-subj",
        "/CN=localhost",
    ]);
    std::fs::write(directory.path().join("extensions"),"subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth,clientAuth\nkeyUsage=critical,digitalSignature,keyEncipherment\n").unwrap();
    openssl(&[
        "x509",
        "-req",
        "-in",
        "identity.csr",
        "-CA",
        "ca.crt",
        "-CAkey",
        "ca.key",
        "-CAcreateserial",
        "-out",
        "identity.crt",
        "-days",
        "1",
        "-extfile",
        "extensions",
    ]);
    let certificate = std::fs::read(directory.path().join("identity.crt")).unwrap();
    let key = std::fs::read(directory.path().join("identity.key")).unwrap();
    let ca = std::fs::read(directory.path().join("ca.crt")).unwrap();
    for mtls in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", listener.local_addr().unwrap());
        let mut tls =
            ServerTlsConfig::new().identity(Identity::from_pem(certificate.clone(), key.clone()));
        if mtls {
            tls = tls.client_ca_root(Certificate::from_pem(ca.clone()));
        }
        let (stop, stopped) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .tls_config(tls)
                .unwrap()
                .add_service(frontend_server::FrontendServer::new(Flame::default()))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        let mut config = ConnectionConfig::new(endpoint);
        config.ca_file = Some(directory.path().join("ca.crt"));
        config.tls_domain = Some("localhost".into());
        if mtls {
            config.cert_file = Some(directory.path().join("identity.crt"));
            config.key_file = Some(directory.path().join("identity.key"));
        }
        let app = router(AppState::connect(config.clone()).unwrap());
        assert_eq!(get(app, "/api/sessions").await.0, StatusCode::OK);
        if mtls {
            let mut anonymous = config.clone();
            anonymous.cert_file = None;
            anonymous.key_file = None;
            assert_eq!(
                get(
                    router(AppState::connect(anonymous).unwrap()),
                    "/api/sessions"
                )
                .await
                .0,
                StatusCode::BAD_GATEWAY
            );
        }
        config.tls_domain = Some("wrong.example".into());
        assert_eq!(
            get(router(AppState::connect(config).unwrap()), "/api/sessions")
                .await
                .0,
            StatusCode::BAD_GATEWAY
        );
        let _ = stop.send(());
    }
}

#[tokio::test]
async fn upstream_secrets_are_absent_from_responses_and_logs() {
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || LogWriter(writer.clone()))
        .finish();
    // Capture warnings from the HTTP handler and any spawned transport tasks.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    {
        let (state, stop) = frontend(Flame { fail_nodes: true }).await;
        for path in ["/api/nodes", "/api/sessions/secret-error/tasks"] {
            let (status, body) = get(router(state.clone()), path).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert_eq!(body, json!({"error":"Flame upstream request failed"}));
            assert!(!body.to_string().contains("SYNTHETIC_SECRET"));
        }
        let _ = stop.send(());
    }
    let output = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(output.contains("Flame upstream request failed"));
    assert!(!output.contains("SYNTHETIC_SECRET"));
}

#[tokio::test]
async fn api_hosts_require_explicit_trust_and_health_probes_are_exempt() {
    let mut config = ConnectionConfig::new("http://127.0.0.1:9");
    config.allowed_hosts.push("Console.Example".into());
    let app = router(AppState::connect(config).unwrap());
    for host in [
        None,
        Some("attacker.invalid"),
        Some("console.example.attacker.invalid"),
        Some("user@localhost"),
        Some("localhost:invalid"),
        Some("*.example"),
    ] {
        let mut request = Request::builder().uri("/api/connection");
        if let Some(host) = host {
            request = request.header("host", host);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "host {host:?}");
    }
    for host in [
        "localhost",
        "LOCALHOST:3000",
        "127.0.0.1:8080",
        "[::1]:3000",
        "CONSOLE.EXAMPLE:443",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/connection")
                    .header("host", host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "host {host}");
    }
    for host in [None, Some("10.0.0.5:3000")] {
        let mut request = Request::builder().uri("/healthz");
        if let Some(host) = host {
            request = request.header("host", host);
        }
        assert_eq!(
            app.clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/connection")
                .header("host", "localhost")
                .header("host", "attacker.invalid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let (state, stop) = frontend(Flame::default()).await;
    for (host, origin, status) in [
        (
            "rebind.attacker.invalid:3000",
            "http://rebind.attacker.invalid:3000",
            StatusCode::FORBIDDEN,
        ),
        (
            "localhost:3000",
            "http://localhost:4000",
            StatusCode::FORBIDDEN,
        ),
        ("localhost:3000", "http://localhost:3000", StatusCode::OK),
    ] {
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sessions/open/actions/close")
                    .header("host", host)
                    .header("origin", origin)
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
    let _ = stop.send(());
}
