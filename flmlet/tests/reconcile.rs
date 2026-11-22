use flame_api::{FlameCluster, FlameWorker};
use flmlet::{CommandOutput, Error, FEM, FOC, Host, Input, Result, Settings, desired, reconcile};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddrV4,
    path::{Path, PathBuf},
};

#[derive(Default)]
struct FakeHost {
    files: BTreeMap<PathBuf, Vec<u8>>,
    writes: Vec<PathBuf>,
    calls: Vec<Vec<String>>,
    active: BTreeSet<String>,
    enabled: BTreeSet<String>,
    missing: BTreeSet<PathBuf>,
    registration: Value,
    node_pages: BTreeMap<usize, Value>,
    cache_ready: bool,
    fail_restart: bool,
    fail_bootstrap: bool,
    events: Vec<String>,
    bootstraps: Vec<(String, String, bool)>,
    reported_version: String,
    fail_write: Option<PathBuf>,
    modes: BTreeMap<PathBuf, u32>,
}
impl FakeHost {
    fn healthy() -> Self {
        let mut host = Self {
            cache_ready: true,
            reported_version: "v0.8.0".into(),
            registration: json!({"items":[{"hostname":"actual-host","name":"unrelated-id","state":"Ready"}]}),
            ..Default::default()
        };
        host.files
            .insert("/srv/flame/VERSION".into(), b"v0.8.0\n".to_vec());
        for unit in [FEM, FOC] {
            host.files.insert(
                settings().systemd_dir.join(unit),
                upstream_unit(unit).into_bytes(),
            );
            host.files.insert(
                drop_in_path(&settings(), unit),
                b"# Managed by flmlet\n# Worker: flame-system/worker-one\n[Service]\n".to_vec(),
            );
        }
        host
    }
    fn mutations(&self) -> Vec<&[String]> {
        self.calls
            .iter()
            .filter(|call| {
                call.first().is_some_and(|program| program == "systemctl")
                    && call.get(1).is_some_and(|verb| {
                        matches!(
                            verb.as_str(),
                            "start" | "stop" | "restart" | "enable" | "disable" | "daemon-reload"
                        )
                    })
            })
            .map(Vec::as_slice)
            .collect()
    }
}
impl Host for FakeHost {
    fn read(&mut self, path: &Path) -> Result<Option<Vec<u8>>> {
        Ok(self.files.get(path).cloned())
    }
    fn read_dir(&mut self, path: &Path) -> Result<Vec<PathBuf>> {
        Ok(self
            .files
            .keys()
            .filter(|entry| entry.parent() == Some(path))
            .cloned()
            .collect())
    }
    fn write_atomic(&mut self, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        if self.fail_write.as_deref() == Some(path) {
            self.fail_write = None;
            return Err(std::io::Error::other("injected atomic write failure").into());
        }
        self.events.push(
            if path.ends_with("bootstrap.json") {
                "bootstrap-marker"
            } else {
                "configure"
            }
            .into(),
        );
        self.writes.push(path.into());
        self.modes.insert(path.into(), mode);
        self.files.insert(path.into(), bytes.into());
        Ok(())
    }
    fn executable(&mut self, path: &Path) -> bool {
        !self.missing.contains(path)
    }
    fn command(&mut self, program: &Path, args: &[String]) -> Result<CommandOutput> {
        let mut call = vec![program.to_string_lossy().into_owned()];
        call.extend_from_slice(args);
        self.calls.push(call);
        if program == Path::new("systemctl") {
            let verb = args[0].as_str();
            let unit = args.get(1).map(String::as_str).unwrap_or("");
            match verb {
                "show" => {
                    return Ok(CommandOutput {
                        success: true,
                        stdout: format!(
                            "ActiveState={}\nSubState={}\nResult=success\n",
                            if self.active.contains(unit) {
                                "active"
                            } else {
                                "inactive"
                            },
                            if self.active.contains(unit) {
                                "running"
                            } else {
                                "dead"
                            }
                        ),
                        ..Default::default()
                    });
                }
                "is-enabled" => {
                    return Ok(CommandOutput {
                        success: self.enabled.contains(unit),
                        ..Default::default()
                    });
                }
                "start" | "restart" => {
                    self.events.push("start".into());
                    if self.fail_restart {
                        return Ok(CommandOutput {
                            success: false,
                            stderr: "service failed".into(),
                            ..Default::default()
                        });
                    }
                    self.active.insert(unit.into());
                }
                "stop" => {
                    self.active.remove(unit);
                }
                "enable" => {
                    self.enabled.insert(unit.into());
                }
                "disable" => {
                    self.enabled.remove(unit);
                }
                "daemon-reload" => {}
                _ => panic!("unexpected systemctl verb {verb}"),
            }
        } else if program.ends_with("flame-executor-manager") && args == ["--version"] {
            return Ok(CommandOutput {
                success: true,
                stdout: format!("flame-executor-manager {}\n", self.reported_version),
                ..Default::default()
            });
        } else if program.ends_with("flmctl") {
            return Ok(CommandOutput {
                success: true,
                stdout: self
                    .node_pages
                    .get(
                        &args
                            .iter()
                            .position(|argument| argument == "--number")
                            .and_then(|index| args.get(index + 1))
                            .and_then(|number| number.parse::<usize>().ok())
                            .unwrap_or(1),
                    )
                    .unwrap_or(&self.registration)
                    .to_string(),
                ..Default::default()
            });
        } else {
            panic!("unexpected command {}", program.display());
        }
        Ok(CommandOutput {
            success: true,
            ..Default::default()
        })
    }
    fn bootstrap(
        &mut self,
        settings: &Settings,
        version: &str,
        python: &str,
        force: bool,
    ) -> Result<()> {
        self.events.push("install".into());
        self.bootstraps.push((version.into(), python.into(), force));
        self.missing.clear();
        self.reported_version = version.into();
        self.files.insert(
            settings.prefix.join("conf/flame-cluster.yaml"),
            b"# Flame cluster configuration, generated by setup.sh from FLAME_* settings\ncluster: {}\n".to_vec(),
        );
        for unit in [FEM, FOC] {
            self.files.insert(
                settings.systemd_dir.join(unit),
                upstream_unit_at(unit, settings.prefix.to_str().unwrap()).into_bytes(),
            );
        }
        if self.fail_bootstrap {
            return Err(Error::Command("installer failed".into()));
        }
        Ok(())
    }
    fn hostname(&mut self) -> Result<String> {
        Ok("actual-host".into())
    }
    fn tcp_ready(&mut self, _: SocketAddrV4) -> bool {
        self.cache_ready
    }
}
fn drop_in_path(settings: &Settings, unit: &str) -> PathBuf {
    settings.systemd_dir.join(format!("{unit}.d/flmlet.conf"))
}
fn upstream_unit(unit: &str) -> String {
    upstream_unit_at(unit, "/srv/flame")
}
fn upstream_unit_at(unit: &str, prefix: &str) -> String {
    let binary = unit.strip_suffix(".service").unwrap();
    let log = if unit == FEM { "fem" } else { "foc" };
    format!(
        "[Unit]\nDescription={unit}\nDocumentation=https://github.com/xflops-io/flame\nAfter=network.target\nWants=network-online.target\n[Service]\nType=simple\nEnvironment=\"RUST_LOG=info\"\nEnvironment=\"FLAME_HOME={prefix}\"\nWorkingDirectory={prefix}\nExecStart={prefix}/bin/{binary} --config {prefix}/conf/flame-cluster.yaml\nStandardOutput=append:{prefix}/logs/{log}.log\nStandardError=append:{prefix}/logs/{log}.log\nRestart=on-failure\nRestartSec=5s\nLimitNOFILE=65536\n[Install]\nWantedBy=multi-user.target\n"
    )
}

fn settings() -> Settings {
    Settings {
        worker: "worker-one".into(),
        namespace: "flame-system".into(),
        prefix: "/srv/flame".into(),
        systemd_dir: "/test/systemd".into(),
        adopt_existing: false,
    }
}
fn worker() -> FlameWorker {
    serde_json::from_value(json!({"metadata":{"name":"worker-one","namespace":"flame-system","generation":3},"spec":{"clusterRef":"flame","address":"192.168.3.8","shims":["host"],"manageObjectCache":true}})).unwrap()
}
// Public certificate fixture only; no corresponding private key is retained.
const PUBLIC_CA: &str = r#"-----BEGIN CERTIFICATE-----
MIIDITCCAgmgAwIBAgIUU+HjPaPHkzq/GkWBTv+M0HF3c/0wDQYJKoZIhvcNAQEL
BQAwIDEeMBwGA1UEAwwVRmxtbGV0LVRlc3QtUHVibGljLUNBMB4XDTI2MTEyMDA5
NTkxN1oXDTI2MTEyMTA5NTkxN1owIDEeMBwGA1UEAwwVRmxtbGV0LVRlc3QtUHVi
bGljLUNBMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAo793RGhH3xju
VR2oDKrslWQasum0+D0Lzvg2dcI/XqXln/Kttw6f8/3OGVAll5yPA7Ei7Uu06HmQ
aLl5uBFq2+2EZNQX1usAfhDX0lMk9+52JEGV4LfcvhMVJOqCfOOiFx84IsR6eZPW
2hzcr35+krsS2d7NabgsENYQPFz2PoAmtJKrDsj2olFIrrnncNGE/M9Cor8fJKcg
EY/xiKvZQgd17fuDpYawe0/Lf+Mt9OZUmR/fg7zS2uQA8AfeiDxNNeAxtsxzGHPK
77VVqqMGI9Y6egM45E3LAoQYs26wuFTV3ayyrWrXKU72peetP7W6rYV0ZecUYAg4
RyQZ/TehsQIDAQABo1MwUTAdBgNVHQ4EFgQUq7t460Lke9+KQPVn2pRpD4cEL94w
HwYDVR0jBBgwFoAUq7t460Lke9+KQPVn2pRpD4cEL94wDwYDVR0TAQH/BAUwAwEB
/zANBgkqhkiG9w0BAQsFAAOCAQEAOQ20Tvxtu4oTfJ95bQarsWDzaY2ROPTWqR3S
8YC2Kf40IT98C5RaOhR662pSnC7oFBlIzlIgfvAec7Vk0+9+mHXDtgtE2Mx1u8Go
ipbu5M+NiPUK3nY8r8cMDg83icz86slEfz7AbuOti/VzkVd5LhJaLXhlQa/bYLwz
DytH1ODf1YqYz8Q6L2LXIDhZu+5VxH2TZL4px8f0hles8vqnl2IiZHONPSkxNZBJ
uTF+OqdVnCmxO/RA01Cu2SLFByK+6JkSh4RL5nVPksTUYFcAaKUdL4jc+WyrTPA/
g1zISSW8rB/U0r/eVACkG61eFA32+k/RYk4hS2f5zLFHH/v6Pw==
-----END CERTIFICATE-----
"#;

fn cluster() -> FlameCluster {
    serde_json::from_value(json!({"metadata":{"name":"flame","namespace":"flame-system"},"status":{"observedGeneration":1,"controlPlaneCa":PUBLIC_CA},"spec":{"version":"v0.8.0","endpoint":"192.168.3.1:30080","resourceRequest":"cpu=1,mem=128m","gateway":{"hostname":"flame.example.test"}}})).unwrap()
}
fn run(
    host: &mut FakeHost,
    settings: &Settings,
    worker: &FlameWorker,
    cluster: &FlameCluster,
) -> Result<flmlet::Outcome> {
    reconcile(
        host,
        settings,
        Input::Present {
            worker,
            cluster: Some(cluster),
        },
    )
}
#[test]
fn desired_yaml_and_units_use_lan_endpoint_explicit_python_and_worker_settings() {
    let desired = desired(&settings(), &worker(), &cluster()).unwrap();
    let config: Value =
        serde_yaml::from_str(&desired.files[&PathBuf::from("/srv/flame/conf/flame-cluster.yaml")])
            .unwrap();
    assert_eq!(config["cluster"]["endpoint"], "https://192.168.3.1:30080");
    assert_eq!(config["cluster"]["executors"]["shims"], json!(["host"]));
    assert_eq!(config["cluster"]["limits"]["node"]["max_executors"], 10);
    assert_eq!(config["cache"]["endpoint"], "grpc://192.168.3.8:9090");
    let environment = &desired.files[&PathBuf::from("/srv/flame/conf/flmlet.env")];
    for value in [
        "FLAME_HOME=\"/srv/flame\"",
        "FLAME_PYTHON_VERSION=\"3.12\"",
        "PYTHONPATH=\"/srv/flame/lib/python3.12/site-packages:",
        "FLAME_CACHE_ADVERTISE=\"grpc://192.168.3.8:9090\"",
    ] {
        assert!(environment.contains(value), "{environment}");
    }
    assert!(desired.units[FEM].contains("ExecStart=\"/srv/flame/bin/flame-executor-manager\" --config \"/srv/flame/conf/flame-cluster.yaml\""));
    assert!(desired.units.contains_key(FOC));
}
#[test]
fn fem_configuration_uses_both_default_shims_and_preserves_explicit_overrides() {
    for selection in [None, Some(vec!["cri"]), Some(vec!["host"])] {
        let mut enrolled = worker();
        let mut spec = serde_json::to_value(&enrolled.spec).unwrap();
        match &selection {
            None => {
                spec.as_object_mut().unwrap().remove("shims");
            }
            Some(shims) => spec["shims"] = json!(shims),
        }
        enrolled.spec = serde_json::from_value(spec).unwrap();
        let desired = desired(&settings(), &enrolled, &cluster()).unwrap();
        let config: Value = serde_yaml::from_str(
            &desired.files[&PathBuf::from("/srv/flame/conf/flame-cluster.yaml")],
        )
        .unwrap();
        assert_eq!(
            config["cluster"]["executors"]["shims"],
            json!(selection.unwrap_or_else(|| vec!["cri", "host"]))
        );
    }
}
#[test]
fn identical_reconcile_is_idempotent_but_rechecks_live_services_and_registration() {
    let mut host = FakeHost::healthy();
    let first = run(&mut host, &settings(), &worker(), &cluster()).unwrap();
    assert!(first.ready && first.changed);
    assert_eq!(
        first.status.unwrap().hostname.as_deref(),
        Some("actual-host")
    );
    assert!(host.active.contains(FEM) && host.active.contains(FOC));
    host.calls.clear();
    host.writes.clear();
    let second = run(&mut host, &settings(), &worker(), &cluster()).unwrap();
    assert!(second.ready && !second.changed);
    assert!(host.writes.is_empty());
    assert!(host.mutations().is_empty());
    assert!(host.calls.iter().any(|call| call[0].ends_with("flmctl")));
    assert!(
        host.calls
            .iter()
            .any(|call| call.get(1).is_some_and(|arg| arg == "show"))
    );
}
#[test]
fn rollout_changes_restart_owned_services_once_and_stopped_services_are_started() {
    let mut host = FakeHost::healthy();
    let mut worker = worker();
    run(&mut host, &settings(), &worker, &cluster()).unwrap();
    host.calls.clear();
    worker.spec.rollout_revision = "sre-rollout-2".into();
    assert!(
        run(&mut host, &settings(), &worker, &cluster())
            .unwrap()
            .changed
    );
    assert_eq!(
        host.calls
            .iter()
            .filter(|call| call.get(1).is_some_and(|arg| arg == "restart"))
            .count(),
        2
    );
    host.calls.clear();
    host.active.remove(FEM);
    assert!(
        run(&mut host, &settings(), &worker, &cluster())
            .unwrap()
            .ready
    );
    assert!(
        host.calls
            .iter()
            .any(|call| call.get(1).is_some_and(|arg| arg == "start") && call[2] == FEM)
    );
    assert!(!host.calls.iter().any(|call| {
        call.get(1)
            .is_some_and(|arg| arg == "restart" || arg == "enable")
    }));
}
#[test]
fn foreign_unit_requires_explicit_adoption_before_any_host_mutation() {
    let mut host = FakeHost::healthy();
    host.files.remove(&drop_in_path(&settings(), FEM));
    host.files.insert(
        settings().systemd_dir.join(FEM),
        b"# SRE-managed service\n".to_vec(),
    );
    let error = run(&mut host, &settings(), &worker(), &cluster()).unwrap_err();
    assert!(error.to_string().contains("foreign unit"));
    assert!(host.writes.is_empty() && host.mutations().is_empty());
    let mut settings = settings();
    settings.adopt_existing = true;
    assert!(
        run(&mut host, &settings, &worker(), &cluster())
            .unwrap()
            .ready
    );
}
#[test]
fn foreign_configuration_is_also_protected_from_implicit_adoption() {
    let mut host = FakeHost::healthy();
    host.files.insert(
        settings().prefix.join("conf/flmlet.env"),
        b"SRE_SETTING=value\n".to_vec(),
    );
    assert!(run(&mut host, &settings(), &worker(), &cluster()).is_err());
    assert!(host.writes.is_empty() && host.mutations().is_empty());
}
#[test]
fn api_outage_keeps_services_and_configuration_but_confirmed_deletion_stops_only_owned_units() {
    let mut host = FakeHost::healthy();
    run(&mut host, &settings(), &worker(), &cluster()).unwrap();
    host.calls.clear();
    host.writes.clear();
    assert!(matches!(
        reconcile(
            &mut host,
            &settings(),
            Input::Unavailable("transport error")
        ),
        Err(Error::Unavailable(_))
    ));
    assert!(host.calls.is_empty() && host.writes.is_empty());
    assert!(host.active.contains(FEM));
    // A different process owns this cache; deleting our worker must leave it alone.
    host.files.remove(&drop_in_path(&settings(), FOC));
    host.files.insert(
        settings().systemd_dir.join(FOC),
        b"# different owner\n".to_vec(),
    );
    reconcile(&mut host, &settings(), Input::Missing).unwrap();
    assert!(!host.active.contains(FEM));
    assert!(host.active.contains(FOC));
    assert!(
        host.files
            .contains_key(&settings().prefix.join("conf/flame-cluster.yaml"))
    );
}
#[test]
fn disabled_worker_stops_owned_services_without_needing_a_cluster_observation() {
    let mut host = FakeHost::healthy();
    let mut worker = worker();
    run(&mut host, &settings(), &worker, &cluster()).unwrap();
    worker.spec.disabled = true;
    let outcome = reconcile(
        &mut host,
        &settings(),
        Input::Present {
            worker: &worker,
            cluster: None,
        },
    )
    .unwrap();
    assert!(!outcome.ready);
    assert!(host.active.is_empty() && host.enabled.is_empty());
    assert_eq!(outcome.status.unwrap().conditions[1].reason, "Disabled");
}
#[test]
fn failed_restart_does_not_commit_applied_hash_and_is_retried() {
    let mut host = FakeHost::healthy();
    host.fail_restart = true;
    assert!(run(&mut host, &settings(), &worker(), &cluster()).is_err());
    assert!(
        !host
            .files
            .contains_key(&settings().prefix.join("conf/flmlet-applied-hash"))
    );
    host.fail_restart = false;
    assert!(
        run(&mut host, &settings(), &worker(), &cluster())
            .unwrap()
            .ready
    );
}
#[test]
fn pinned_version_mismatch_installs_before_reconfiguration_and_latest_does_not_poll() {
    let mut host = FakeHost::healthy();
    run(&mut host, &settings(), &worker(), &cluster()).unwrap();
    host.events.clear();
    let mut cluster = cluster();
    cluster.spec.version = "v0.9.0".into();
    assert!(
        run(&mut host, &settings(), &worker(), &cluster)
            .unwrap()
            .ready
    );
    assert_eq!(
        host.events.first().map(String::as_str),
        Some("bootstrap-marker")
    );
    assert_eq!(host.events[1], "install");
    assert_eq!(host.bootstraps.len(), 1);
    assert!(host.bootstraps[0].2);
    assert_eq!(
        host.files[&PathBuf::from("/srv/flame/VERSION")],
        b"v0.8.0\n"
    );
    assert_eq!(host.reported_version, "v0.9.0");
    let configured = host
        .events
        .iter()
        .position(|event| event == "configure")
        .unwrap();
    let started = host
        .events
        .iter()
        .position(|event| event == "start")
        .unwrap();
    assert!(configured < started);
    cluster.spec.version = "latest".into();
    run(&mut host, &settings(), &worker(), &cluster).unwrap();
    assert_eq!(host.bootstraps.len(), 1);
}

#[test]
fn live_registration_and_managed_cache_readiness_are_required() {
    let mut host = FakeHost::healthy();
    host.registration = json!({"items":[{"hostname":"other-host","state":"Ready"}]});
    assert!(
        !run(&mut host, &settings(), &worker(), &cluster())
            .unwrap()
            .ready
    );
    host.registration = json!({"items":[{"metadata":{"name":"actual-host"},"status":{"state":1}}]});
    host.cache_ready = false;
    let result = run(&mut host, &settings(), &worker(), &cluster()).unwrap();
    assert!(!result.ready);
    assert_eq!(
        result.status.unwrap().conditions[1].reason,
        "CacheUnreachable"
    );
    let mut worker = worker();
    worker.spec.manage_object_cache = false;
    assert!(
        run(&mut host, &settings(), &worker, &cluster())
            .unwrap()
            .ready
    );
    assert!(!host.active.contains(FOC));
}
#[test]
fn unsafe_configuration_and_missing_sre_binaries_do_not_touch_the_host() {
    let mut host = FakeHost::healthy();
    let mut worker = worker();
    worker.spec.endpoint = Some("http://192.168.3.1:30080".into());
    assert!(run(&mut host, &settings(), &worker, &cluster()).is_err());
    assert!(host.writes.is_empty() && host.mutations().is_empty());
    worker.spec.endpoint = None;
    host.missing
        .insert("/srv/flame/bin/flame-executor-manager".into());
    host.files.remove(&settings().systemd_dir.join(FEM));
    host.files.remove(&drop_in_path(&settings(), FEM));
    assert!(run(&mut host, &settings(), &worker, &cluster()).is_err());
    assert!(host.writes.is_empty() && host.mutations().is_empty());
}
#[test]
fn local_paths_are_literal_and_dry_run_does_not_need_a_host() {
    let mut settings = settings();
    settings.prefix = "/srv/flame space%name$literal".into();
    let desired = desired(&settings, &worker(), &cluster()).unwrap();
    assert!(desired.units[FEM].contains("flame space%%name$$literal"));
    assert!(
        desired.units[FEM]
            .contains("EnvironmentFile=/srv/flame space%%name$literal/conf/flmlet.env\n")
    );
    assert!(desired.units[FEM].contains("WorkingDirectory=/srv/flame space%%name$literal\n"));
    for prefix in [
        "/srv/flame'quote",
        "/srv/flame\"quote",
        "/srv/flame\\slash",
        "/srv/flame ",
    ] {
        let mut invalid = settings.clone();
        invalid.prefix = prefix.into();
        assert!(flmlet::desired(&invalid, &worker(), &cluster()).is_err());
    }
    let mut worker = worker();
    worker.spec.cluster_ref = "../flame".into();
    assert!(flmlet::desired(&settings, &worker, &cluster()).is_err());
}

#[test]
fn explicit_credential_revocation_stops_only_owned_services() {
    let mut host = FakeHost::healthy();
    run(&mut host, &settings(), &worker(), &cluster()).unwrap();
    assert!(matches!(
        reconcile(&mut host, &settings(), Input::Unauthorized),
        Err(Error::Unauthorized)
    ));
    assert!(host.active.is_empty() && host.enabled.is_empty());
    assert!(
        host.files
            .contains_key(&settings().prefix.join("conf/flame-cluster.yaml"))
    );
}

#[test]
fn preflight_is_read_only_and_rejects_foreign_units_and_unowned_runtime() {
    let mut host = FakeHost::healthy();
    flmlet::preflight(&mut host, &settings(), &worker(), &cluster()).unwrap();
    assert!(host.writes.is_empty() && host.mutations().is_empty());
    host.files.remove(&drop_in_path(&settings(), FEM));
    host.files.insert(
        settings().systemd_dir.join(FEM),
        b"# foreign unit\n".to_vec(),
    );
    assert!(flmlet::preflight(&mut host, &settings(), &worker(), &cluster()).is_err());
    assert!(host.writes.is_empty() && host.mutations().is_empty());
    host.files.remove(&settings().systemd_dir.join(FEM));
    let mut cluster = cluster();
    cluster.spec.version = "v99.0.0".into();
    assert!(flmlet::preflight(&mut host, &settings(), &worker(), &cluster).is_err());
    assert!(host.writes.is_empty() && host.mutations().is_empty());
}

#[test]
fn configure_restarts_upstream_units_after_writing_configuration_once() {
    let mut host = FakeHost::healthy();
    assert!(flmlet::configure(&mut host, &settings(), &worker(), &cluster()).unwrap());
    assert!(host.active.contains(FEM) && host.active.contains(FOC));
    assert!(host.enabled.contains(FEM) && host.enabled.contains(FOC));
    assert!(
        host.files
            .contains_key(&settings().prefix.join("conf/flame-cluster.yaml"))
    );
    host.calls.clear();
    host.writes.clear();
    assert!(!flmlet::configure(&mut host, &settings(), &worker(), &cluster()).unwrap());
    assert!(host.mutations().is_empty() && host.writes.is_empty());
    assert!(
        run(&mut host, &settings(), &worker(), &cluster())
            .unwrap()
            .ready
    );
    assert!(host.mutations().is_empty());
}

#[test]
fn registration_checks_later_native_pages_and_matches_actual_hostname() {
    let mut host = FakeHost::healthy();
    host.node_pages.insert(1,json!({"items":(0..100).map(|i|json!({"hostname":format!("other-{i}"),"state":"Ready"})).collect::<Vec<_>>()}));
    host.node_pages.insert(
        2,
        json!({"items":[{"name":"node-id","hostname":"actual-host","state":"Ready", "cpu":16,"gpu":0,"memory":34359738368u64}]}),
    );
    let outcome = run(&mut host, &settings(), &worker(), &cluster()).unwrap();
    assert!(outcome.ready);
    assert_eq!(
        outcome.status.unwrap().capacity,
        Some(flame_api::NodeCapacity {
            cpu: Some(16),
            gpu: Some(0),
            memory: Some(34359738368),
        })
    );
    assert_eq!(
        host.calls
            .iter()
            .filter(|call| call[0].ends_with("flmctl"))
            .count(),
        2
    );
}

#[test]
fn reenabled_worker_reapplies_and_enables_services_after_explicit_revocation() {
    let mut host = FakeHost::healthy();
    run(&mut host, &settings(), &worker(), &cluster()).unwrap();
    assert!(matches!(
        reconcile(&mut host, &settings(), Input::Unauthorized),
        Err(Error::Unauthorized)
    ));
    assert!(host.enabled.is_empty());
    assert!(
        run(&mut host, &settings(), &worker(), &cluster())
            .unwrap()
            .ready
    );
    assert!(host.enabled.contains(FEM) && host.enabled.contains(FOC));
}

#[test]
fn control_plane_tls_uses_public_ca_without_distributing_server_identity() {
    let desired = desired(&settings(), &worker(), &cluster()).unwrap();
    let ca_path = PathBuf::from("/srv/flame/conf/flmlet/control-plane-ca.crt");
    assert!(desired.files[&ca_path].ends_with(PUBLIC_CA));
    let config: Value =
        serde_yaml::from_str(&desired.files[&PathBuf::from("/srv/flame/conf/flame-cluster.yaml")])
            .unwrap();
    assert_eq!(config["cluster"]["endpoint"], "https://192.168.3.1:30080");
    assert_eq!(
        config["cluster"]["tls"],
        json!({"ca_file":ca_path,"cert_file":"","key_file":""})
    );
    assert!(config["cache"].get("tls").is_none());
    assert_eq!(config["cache"]["endpoint"], "grpc://192.168.3.8:9090");
    let client: Value =
        serde_yaml::from_str(&desired.files[&PathBuf::from("/srv/flame/conf/flmlet-client.yaml")])
            .unwrap();
    assert_eq!(
        client["contexts"][0]["cluster"]["endpoint"],
        "https://192.168.3.1:30080"
    );
    assert_eq!(
        client["contexts"][0]["cluster"]["tls"],
        json!({"ca_file":ca_path})
    );
    assert!(client["contexts"][0]["cache"].get("tls").is_none());
    assert!(
        desired.files[&PathBuf::from("/srv/flame/conf/flmlet.env")]
            .contains("FLAME_CA_FILE=\"/srv/flame/conf/flmlet/control-plane-ca.crt\"")
    );
    for (path, contents) in &desired.files {
        assert!(!path.to_string_lossy().contains("key"));
        assert!(!contents.contains("PRIVATE KEY"));
        assert!(!contents.contains("tls.key"));
    }
}

#[test]
fn missing_or_invalid_control_plane_trust_fails_closed_without_leaking_input() {
    let mut cluster = cluster();
    let invalid_ca = format!(
        "{PUBLIC_CA}-----BEGIN PRIVATE KEY-----\nSYNTHETIC_SECRET\n-----END PRIVATE KEY-----"
    );
    for ca in [
        None,
        Some(""),
        Some("invalid CA"),
        Some("-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----"),
        Some(invalid_ca.as_str()),
    ] {
        cluster.status.as_mut().unwrap().control_plane_ca = ca.map(String::from);
        let error = desired(&settings(), &worker(), &cluster).unwrap_err();
        assert!(matches!(error, Error::ControlPlaneTrust));
        assert!(!error.to_string().contains("SYNTHETIC_SECRET"));
        let mut host = FakeHost::healthy();
        assert!(flmlet::preflight(&mut host, &settings(), &worker(), &cluster).is_err());
        assert!(host.writes.is_empty() && host.calls.is_empty());
        assert!(flmlet::configure(&mut host, &settings(), &worker(), &cluster).is_err());
        assert!(host.writes.is_empty() && host.calls.is_empty());
    }
    let mut host = FakeHost::healthy();
    run(&mut host, &settings(), &worker(), &self::cluster()).unwrap();
    assert!(host.active.contains(FEM));
    assert!(matches!(
        run(&mut host, &settings(), &worker(), &cluster),
        Err(Error::ControlPlaneTrust)
    ));
    assert!(host.active.is_empty() && host.enabled.is_empty());
}

#[test]
fn public_ca_rotation_restarts_services_once_then_is_idempotent() {
    let mut host = FakeHost::healthy();
    let mut cluster = cluster();
    run(&mut host, &settings(), &worker(), &cluster).unwrap();
    // A valid trust-bundle extension changes the configured public trust/hash.
    cluster.status.as_mut().unwrap().control_plane_ca = Some(format!("{PUBLIC_CA}{PUBLIC_CA}"));
    assert!(
        run(&mut host, &settings(), &worker(), &cluster)
            .unwrap()
            .changed
    );
    host.calls.clear();
    host.writes.clear();
    assert!(
        !run(&mut host, &settings(), &worker(), &cluster)
            .unwrap()
            .changed
    );
    assert!(host.writes.is_empty() && host.mutations().is_empty());
}

// Verify the actual generated units with Linux's parser, never the host manager.
// CI must provide systemd-analyze; a missing executable is a test failure.
#[cfg(target_os = "linux")]
#[test]
fn generated_units_pass_systemd_analyze_with_literal_paths() {
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::tempdir().unwrap();
    for name in [
        "normal",
        "space %specifier $variable",
        "space %Qliteral $variable",
    ] {
        let mut settings = settings();
        settings.prefix = temporary.path().join(name);
        settings.systemd_dir = temporary.path().join("units");
        std::fs::create_dir_all(settings.prefix.join("bin")).unwrap();
        std::fs::create_dir_all(&settings.systemd_dir).unwrap();
        for binary in ["flame-executor-manager", "flame-object-cache"] {
            let path = settings.prefix.join("bin").join(binary);
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let desired = desired(&settings, &worker(), &cluster()).unwrap();
        let mut host = empty_runtime_for(&settings);
        flmlet::configure(&mut host, &settings, &worker(), &cluster()).unwrap();
        for (path, contents) in &host.files {
            if path.starts_with(temporary.path()) {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, contents).unwrap();
            }
        }
        let mut command = std::process::Command::new("systemd-analyze");
        command.args(["verify", "--man=no"]);
        for unit in desired.units.keys() {
            command.arg(settings.systemd_dir.join(unit));
        }
        let output = command
            .output()
            .expect("Linux tests require systemd-analyze");
        assert!(
            output.status.success(),
            "systemd rejected generated units: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let diagnostics = String::from_utf8_lossy(&output.stderr);
        for error in [
            "not absolute",
            "Failed to parse",
            "Invalid",
            "ignoring",
            "not executable",
        ] {
            assert!(
                !diagnostics.contains(error),
                "systemd ignored invalid directive: {diagnostics}"
            );
        }
    }
}

fn empty_runtime() -> FakeHost {
    let mut host = FakeHost::healthy();
    host.files.remove(&PathBuf::from("/srv/flame/VERSION"));
    for unit in [FEM, FOC] {
        host.files.remove(&settings().systemd_dir.join(unit));
        host.files.remove(&drop_in_path(&settings(), unit));
    }
    for binary in ["flame-executor-manager", "flmctl", "flame-object-cache"] {
        host.missing
            .insert(PathBuf::from(format!("/srv/flame/bin/{binary}")));
    }
    host
}

#[test]
fn first_join_installs_systemd_then_writes_configuration_and_restarts_once() {
    let mut host = empty_runtime();
    flmlet::preflight(&mut host, &settings(), &worker(), &cluster()).unwrap();
    assert!(host.events.is_empty() && host.bootstraps.is_empty());
    assert!(flmlet::configure(&mut host, &settings(), &worker(), &cluster()).unwrap());
    assert_eq!(host.events[0], "bootstrap-marker");
    assert_eq!(host.events[1], "install");
    assert!(
        run(&mut host, &settings(), &worker(), &cluster())
            .unwrap()
            .ready
    );
    let configured = host
        .events
        .iter()
        .position(|event| event == "configure")
        .unwrap();
    let started = host
        .events
        .iter()
        .position(|event| event == "start")
        .unwrap();
    assert!(configured < started);
    for unit in [FEM, FOC] {
        let base = settings().systemd_dir.join(unit);
        assert_eq!(host.files[&base], upstream_unit(unit).as_bytes());
        assert!(
            !host.writes.contains(&base),
            "flmlet must retain the upstream base unit"
        );
        assert!(host.files[&drop_in_path(&settings(), unit)].starts_with(b"# Managed by flmlet\n"));
    }
    assert_eq!(host.bootstraps.len(), 1);
    assert!(!host.bootstraps[0].2);
    assert!(
        !run(&mut host, &settings(), &worker(), &cluster())
            .unwrap()
            .changed
    );
    assert_eq!(host.bootstraps.len(), 1);
}

#[test]
fn failed_bootstrap_never_writes_configuration_or_starts_services() {
    let mut host = empty_runtime();
    host.fail_bootstrap = true;
    assert!(flmlet::configure(&mut host, &settings(), &worker(), &cluster()).is_err());
    assert!(
        host.writes
            .iter()
            .all(|path| path.ends_with("bootstrap.json"))
            && host.mutations().is_empty()
    );
    assert!(run(&mut host, &settings(), &worker(), &cluster()).is_err());
    assert!(
        host.writes
            .iter()
            .all(|path| path.ends_with("bootstrap.json"))
            && host.mutations().is_empty()
    );
}

#[test]
fn bootstrap_checks_foreign_ownership_before_installing() {
    let mut host = empty_runtime();
    host.files
        .insert(settings().systemd_dir.join(FEM), b"# foreign".to_vec());
    assert!(flmlet::configure(&mut host, &settings(), &worker(), &cluster()).is_err());
    assert!(host.events.is_empty());
}

#[test]
fn read_only_preflight_accepts_managed_version_mismatch() {
    let mut host = FakeHost::healthy();
    flmlet::configure(&mut host, &settings(), &worker(), &cluster()).unwrap();
    let mut cluster = cluster();
    cluster.spec.version = "v0.9.0".into();
    host.events.clear();
    flmlet::preflight(&mut host, &settings(), &worker(), &cluster).unwrap();
    assert!(host.events.is_empty());
}

#[test]
fn prepared_binaries_without_units_require_adoption_then_install_upstream_systemd() {
    let mut host = FakeHost::healthy();
    for unit in [FEM, FOC] {
        host.files.remove(&settings().systemd_dir.join(unit));
        host.files.remove(&drop_in_path(&settings(), unit));
    }
    let mut cluster = cluster();
    cluster.spec.version = "latest".into();
    assert!(flmlet::preflight(&mut host, &settings(), &worker(), &cluster).is_err());
    assert!(host.writes.is_empty() && host.bootstraps.is_empty());
    let mut settings = settings();
    settings.adopt_existing = true;
    flmlet::configure(&mut host, &settings, &worker(), &cluster).unwrap();
    assert_eq!(host.bootstraps.len(), 1);
    assert!(host.bootstraps[0].2);
    for unit in [FEM, FOC] {
        assert_eq!(
            host.files[&settings.systemd_dir.join(unit)],
            upstream_unit(unit).as_bytes()
        );
    }
    assert!(!flmlet::configure(&mut host, &settings, &worker(), &cluster).unwrap());
    assert_eq!(host.bootstraps.len(), 1);
}

#[test]
fn legacy_owned_base_units_migrate_without_discarding_unrelated_configuration() {
    let mut host = FakeHost::healthy();
    for unit in [FEM, FOC] {
        host.files.insert(settings().systemd_dir.join(unit), b"# Managed by flmlet\n# Worker: flame-system/worker-one\n[Service]\nExecStart=/legacy\n".to_vec());
        host.files.remove(&drop_in_path(&settings(), unit));
    }
    let legacy = settings().prefix.join("conf/flmlet/flame-cluster.yaml");
    let legacy_contents =
        b"# Managed by flmlet\n# Worker: flame-system/worker-one\nlegacy: retained\n";
    host.files.insert(legacy.clone(), legacy_contents.to_vec());
    host.files.insert(settings().prefix.join("conf/flame-cluster.yaml"),
        b"# Flame cluster configuration, generated by setup.sh from FLAME_* settings\ncluster: {}\n".to_vec());
    flmlet::configure(&mut host, &settings(), &worker(), &cluster()).unwrap();
    assert_eq!(host.bootstraps.len(), 1);
    assert!(host.bootstraps[0].2);
    assert_eq!(host.files[&legacy], legacy_contents);
    assert!(
        host.files[&settings().prefix.join("conf/flame-cluster.yaml")]
            .starts_with(b"# Managed by flmlet\n")
    );
    for unit in [FEM, FOC] {
        let base = settings().systemd_dir.join(unit);
        assert_eq!(host.files[&base], upstream_unit(unit).as_bytes());
        assert!(!host.writes.contains(&base));
    }
}

#[test]
fn partial_upstream_installation_retries_only_with_its_worker_scoped_marker() {
    let mut host = empty_runtime();
    host.fail_bootstrap = true;
    assert!(flmlet::configure(&mut host, &settings(), &worker(), &cluster()).is_err());
    let marker = settings().prefix.join("conf/flmlet/bootstrap.json");
    assert!(!host.files[&marker].is_empty());
    assert!(host.writes.iter().all(|path| path == &marker));
    assert!(host.mutations().is_empty());
    let writes = host.writes.len();
    let mut other_settings = settings();
    other_settings.worker = "other-worker".into();
    let mut other_worker = worker();
    other_worker.metadata.name = Some("other-worker".into());
    assert!(flmlet::preflight(&mut host, &other_settings, &other_worker, &cluster()).is_err());
    other_settings.adopt_existing = true;
    flmlet::preflight(&mut host, &other_settings, &other_worker, &cluster()).unwrap();
    let mut other_version = cluster();
    other_version.spec.version = "v0.9.0".into();
    assert!(flmlet::preflight(&mut host, &settings(), &worker(), &other_version).is_err());
    let base = settings().systemd_dir.join(FOC);
    let upstream = host.files[&base].clone();
    host.files.insert(
        base.clone(),
        b"# unrelated service\n[Service]\nExecStart=/unrelated\n".to_vec(),
    );
    assert!(flmlet::preflight(&mut host, &settings(), &worker(), &cluster()).is_err());
    host.files.insert(base, upstream);
    let drop_in = drop_in_path(&settings(), FOC);
    host.files
        .insert(drop_in.clone(), b"# foreign override".to_vec());
    assert!(flmlet::preflight(&mut host, &settings(), &worker(), &cluster()).is_err());
    host.files.remove(&drop_in);
    assert_eq!(host.writes.len(), writes);
    assert_eq!(host.bootstraps.len(), 1);
    host.fail_bootstrap = false;
    flmlet::configure(&mut host, &settings(), &worker(), &cluster()).unwrap();
    assert_eq!(host.bootstraps.len(), 2);
    assert!(host.bootstraps[1].2);
    assert!(host.files[&marker].is_empty());
    assert!(host.active.contains(FEM));
}

#[test]
fn optional_cache_is_owned_but_disabled_until_requested_and_foreign_cache_is_protected() {
    let mut host = FakeHost::healthy();
    host.active.insert(FOC.into());
    host.enabled.insert(FOC.into());
    let mut worker = worker();
    worker.spec.manage_object_cache = false;
    flmlet::configure(&mut host, &settings(), &worker, &cluster()).unwrap();
    assert!(!host.active.contains(FOC) && !host.enabled.contains(FOC));
    assert!(host.files[&drop_in_path(&settings(), FOC)].starts_with(b"# Managed by flmlet\n"));
    worker.spec.manage_object_cache = true;
    flmlet::configure(&mut host, &settings(), &worker, &cluster()).unwrap();
    assert!(host.active.contains(FOC) && host.enabled.contains(FOC));
    let mut foreign = empty_runtime();
    foreign.files.insert(
        settings().systemd_dir.join(FOC),
        b"# foreign cache".to_vec(),
    );
    worker.spec.manage_object_cache = false;
    assert!(flmlet::configure(&mut foreign, &settings(), &worker, &cluster()).is_err());
    assert!(foreign.writes.is_empty() && foreign.bootstraps.is_empty());
}

#[test]
fn bootstrap_preflight_requires_logrotate_without_touching_the_host() {
    let mut host = empty_runtime();
    host.missing.insert("logrotate".into());
    assert!(
        flmlet::preflight(&mut host, &settings(), &worker(), &cluster())
            .unwrap_err()
            .to_string()
            .contains("logrotate")
    );
    assert!(host.events.is_empty());
}

#[test]
fn additional_foreign_drop_ins_are_checked_even_when_the_base_unit_is_owned() {
    let mut host = FakeHost::healthy();
    let foreign = settings()
        .systemd_dir
        .join(format!("{FOC}.d/operator.conf"));
    host.files.insert(
        foreign.clone(),
        b"[Service]\nExecStart=/another-service\n".to_vec(),
    );
    let mut worker = worker();
    worker.spec.manage_object_cache = false;
    assert!(flmlet::preflight(&mut host, &settings(), &worker, &cluster()).is_err());
    assert!(host.writes.is_empty() && host.mutations().is_empty());
    let mut settings = settings();
    settings.adopt_existing = true;
    flmlet::preflight(&mut host, &settings, &worker, &cluster()).unwrap();
    assert_eq!(
        host.files[&foreign],
        b"[Service]\nExecStart=/another-service\n"
    );
}

#[test]
fn failed_final_restart_retries_configuration_without_reinstalling_upstream() {
    let mut host = empty_runtime();
    host.fail_restart = true;
    assert!(flmlet::configure(&mut host, &settings(), &worker(), &cluster()).is_err());
    assert_eq!(host.bootstraps.len(), 1);
    let marker = settings().prefix.join("conf/flmlet/bootstrap.json");
    assert!(!host.files[&marker].is_empty());
    assert!(
        !host
            .files
            .contains_key(&settings().prefix.join("conf/flmlet-applied-hash"))
    );
    host.fail_restart = false;
    flmlet::configure(&mut host, &settings(), &worker(), &cluster()).unwrap();
    assert_eq!(host.bootstraps.len(), 1);
    assert!(host.files[&marker].is_empty());
    assert!(host.active.contains(FEM));
}

fn empty_runtime_for(settings: &Settings) -> FakeHost {
    let mut host = empty_runtime();
    for binary in ["flame-executor-manager", "flmctl", "flame-object-cache"] {
        host.missing
            .insert(settings.prefix.join("bin").join(binary));
    }
    host
}

#[test]
fn percent_prefix_normalization_preserves_vendor_definitions_and_is_not_repeated_after_restart_failure()
 {
    let mut settings = settings();
    settings.prefix = "/srv/flame %Qliteral$variable".into();
    let mut host = empty_runtime_for(&settings);
    host.fail_restart = true;
    assert!(flmlet::configure(&mut host, &settings, &worker(), &cluster()).is_err());
    let prefix = settings.prefix.to_str().unwrap();
    for unit in [FEM, FOC] {
        let path = settings.systemd_dir.join(unit);
        let expected = upstream_unit_at(unit, prefix).replace(prefix, &prefix.replace('%', "%%"));
        assert_eq!(host.files[&path], expected.as_bytes());
        assert_eq!(host.modes[&path], 0o644);
        assert_eq!(
            host.writes.iter().filter(|entry| *entry == &path).count(),
            1
        );
    }
    assert_eq!(host.bootstraps.len(), 1);
    host.fail_restart = false;
    flmlet::configure(&mut host, &settings, &worker(), &cluster()).unwrap();
    assert!(!flmlet::configure(&mut host, &settings, &worker(), &cluster()).unwrap());
    assert_eq!(host.bootstraps.len(), 1);
    for unit in [FEM, FOC] {
        let path = settings.systemd_dir.join(unit);
        assert_eq!(
            host.writes.iter().filter(|entry| *entry == &path).count(),
            1
        );
        assert_eq!(
            host.files[&path],
            upstream_unit_at(unit, prefix)
                .replace(prefix, &prefix.replace('%', "%%"))
                .as_bytes()
        );
    }
}

#[test]
fn recovery_accepts_one_normalized_vendor_unit_and_one_raw_vendor_unit() {
    let mut settings = settings();
    settings.prefix = "/srv/flame %Qliteral".into();
    let mut host = empty_runtime_for(&settings);
    let first = settings.systemd_dir.join(FEM);
    let second = settings.systemd_dir.join(FOC);
    host.fail_write = Some(second.clone());
    assert!(flmlet::configure(&mut host, &settings, &worker(), &cluster()).is_err());
    let prefix = settings.prefix.to_str().unwrap();
    assert_eq!(
        host.files[&first],
        upstream_unit_at(FEM, prefix)
            .replace(prefix, &prefix.replace('%', "%%"))
            .as_bytes()
    );
    assert_eq!(
        host.files[&second],
        upstream_unit_at(FOC, prefix).as_bytes()
    );
    assert!(host.mutations().is_empty());
    flmlet::preflight(&mut host, &settings, &worker(), &cluster()).unwrap();
    flmlet::configure(&mut host, &settings, &worker(), &cluster()).unwrap();
    assert_eq!(host.bootstraps.len(), 2);
    for unit in [FEM, FOC] {
        assert_eq!(
            host.files[&settings.systemd_dir.join(unit)],
            upstream_unit_at(unit, prefix)
                .replace(prefix, &prefix.replace('%', "%%"))
                .as_bytes()
        );
    }
    assert!(host.active.contains(FEM));
}

#[test]
fn partial_normalization_during_upgrade_reinstalls_even_when_owned_drop_ins_survive() {
    let mut settings = settings();
    settings.prefix = "/srv/flame %Qupgrade".into();
    let mut host = empty_runtime_for(&settings);
    let mut cluster = cluster();
    flmlet::configure(&mut host, &settings, &worker(), &cluster).unwrap();
    cluster.spec.version = "v0.9.0".into();
    host.fail_write = Some(settings.systemd_dir.join(FOC));
    assert!(flmlet::configure(&mut host, &settings, &worker(), &cluster).is_err());
    assert_eq!(host.bootstraps.len(), 2);
    assert_eq!(host.reported_version, "v0.9.0");
    for unit in [FEM, FOC] {
        assert!(host.files.contains_key(&drop_in_path(&settings, unit)));
    }
    flmlet::configure(&mut host, &settings, &worker(), &cluster).unwrap();
    assert_eq!(host.bootstraps.len(), 3);
    let prefix = settings.prefix.to_str().unwrap();
    for unit in [FEM, FOC] {
        assert_eq!(
            host.files[&settings.systemd_dir.join(unit)],
            upstream_unit_at(unit, prefix)
                .replace(prefix, &prefix.replace('%', "%%"))
                .as_bytes()
        );
    }
    assert!(!flmlet::configure(&mut host, &settings, &worker(), &cluster).unwrap());
    assert_eq!(host.bootstraps.len(), 3);
}

#[test]
fn capacity_uses_only_matching_ready_node_and_preserves_unknown_values() {
    for (capacity, expected) in [
        (
            json!({"cpu":8,"gpu":0,"memory":4096}),
            Some(json!({"cpu":8,"gpu":0,"memory":4096})),
        ),
        (json!({"cpu":-1,"gpu":"2","memory":1.5}), None),
        (
            json!({"cpu":8,"gpu":null,"memory":false}),
            Some(json!({"cpu":8,"gpu":null,"memory":null})),
        ),
        (json!({}), None),
        (Value::Null, None),
    ] {
        let mut host = FakeHost::healthy();
        host.registration = json!({"items":[
            {"hostname":"other-host","state":"Ready","capacity":{"cpu":999,"gpu":999,"memory":999}},
            {"spec":{"hostname":"actual-host"},"status":{"state":1,"capacity":capacity}}
        ]});
        let outcome = run(&mut host, &settings(), &worker(), &cluster()).unwrap();
        assert!(
            outcome.ready,
            "unknown capacity must not change registration"
        );
        assert_eq!(
            outcome
                .status
                .unwrap()
                .capacity
                .map(|value| serde_json::to_value(value).unwrap()),
            expected
        );
    }
}

#[test]
fn stale_capacity_is_cleared_when_registration_is_lost_disabled_or_reconcile_fails() {
    let mut worker = worker();
    worker.status = Some(flame_api::FlameWorkerStatus {
        capacity: Some(flame_api::NodeCapacity {
            cpu: Some(8),
            gpu: Some(0),
            memory: Some(4096),
        }),
        config_hash: "keep-existing-hash".into(),
        ..Default::default()
    });
    let mut host = FakeHost::healthy();
    host.registration = json!({"items":[
        {"hostname":"other-host","state":"Ready","capacity":{"cpu":999}},
        {"hostname":"actual-host","state":"NotReady","capacity":{"cpu":8}}
    ]});
    let status = run(&mut host, &settings(), &worker, &cluster())
        .unwrap()
        .status
        .unwrap();
    assert!(!status.registered);
    assert!(status.capacity.is_none());
    worker.spec.disabled = true;
    assert!(
        run(&mut host, &settings(), &worker, &cluster())
            .unwrap()
            .status
            .unwrap()
            .capacity
            .is_none()
    );
    let failed = flmlet::failed_status(&worker, "inventory unavailable");
    assert!(failed.capacity.is_none());
    assert_eq!(failed.config_hash, "keep-existing-hash");
}

#[test]
fn native_flat_node_capacity_matches_observed_flmctl_output() {
    let mut host = FakeHost::healthy();
    // Native list -n output uses top-level measurements, not a capacity object.
    host.registration = json!([{
        "name":"flm-worker-01", "hostname":"flm-worker-01", "state":"Ready",
        "cpu":8, "memory":16613277696u64, "gpu":0
    }]);
    // This entry belongs to another actual hostname and must never supply capacity.
    assert!(
        run(&mut host, &settings(), &worker(), &cluster())
            .unwrap()
            .status
            .unwrap()
            .capacity
            .is_none()
    );
    host.registration[0]["hostname"] = json!("actual-host");
    let outcome = run(&mut host, &settings(), &worker(), &cluster()).unwrap();
    assert!(outcome.ready);
    assert_eq!(
        outcome.status.unwrap().capacity,
        Some(flame_api::NodeCapacity {
            cpu: Some(8),
            gpu: Some(0),
            memory: Some(16613277696),
        })
    );
    // Wrapper values cannot override flat measurements, including malformed ones.
    host.registration[0]["capacity"] = json!({"cpu":999,"gpu":999,"memory":999});
    host.registration[0]["cpu"] = json!(-1);
    let capacity = run(&mut host, &settings(), &worker(), &cluster())
        .unwrap()
        .status
        .unwrap()
        .capacity
        .unwrap();
    assert_eq!(
        capacity,
        flame_api::NodeCapacity {
            cpu: None,
            gpu: Some(0),
            memory: Some(16613277696)
        }
    );
}

#[test]
fn pci_inventory_comes_from_local_sysfs_without_registration() {
    let mut host = FakeHost::healthy();
    host.registration = json!([{
        "hostname":"another-host", "state":"Ready",
        "pci_devices":[{"address":"0000:99:00.0"}]
    }]);
    let report = |host: &mut FakeHost| {
        let outcome = run(host, &settings(), &worker(), &cluster()).unwrap();
        assert!(!outcome.ready);
        outcome.status.unwrap().pci_devices
    };
    assert_eq!(report(&mut host), None);

    let directory = PathBuf::from("/sys/bus/pci/devices");
    host.files.insert(directory.clone(), vec![]);
    assert_eq!(report(&mut host), Some(vec![]));

    let device = directory.join("0000:03:00.0");
    host.files.insert(device.clone(), vec![]);
    for (name, value) in [
        ("vendor", "0x10DE"),
        ("device", "0x1234"),
        ("class", "0x030000"),
        ("uevent", "DRIVER=vfio-pci\nPCI_SLOT_NAME=0000:03:00.0\n"),
        ("numa_node", "0\n"),
    ] {
        host.files
            .insert(device.join(name), value.as_bytes().to_vec());
    }
    // A disappearing device cannot invalidate the rest of the inventory.
    host.files.insert(directory.join("0000:04:00.0"), vec![]);
    assert_eq!(
        report(&mut host),
        Some(vec![flame_api::NodePciDevice {
            address: "0000:03:00.0".into(),
            vendor_id: "10de".into(),
            device_id: "1234".into(),
            class_id: "030000".into(),
            driver: Some("vfio-pci".into()),
            numa_node: Some(0),
        }])
    );
    host.files
        .insert(device.join("numa_node"), b"-1\n".to_vec());
    assert_eq!(report(&mut host).unwrap()[0].numa_node, None);
}

#[test]
fn procfs_usage_reports_memory_before_cpu_and_recovers_after_missing_sample() {
    let mut host = FakeHost::healthy();
    let stat = PathBuf::from("/proc/stat");
    host.files
        .insert(stat.clone(), b"cpu 10 0 10 80 0 0 0 0".to_vec());
    host.files.insert(
        "/proc/meminfo".into(),
        b"MemTotal: 100 kB\nMemAvailable: 75 kB".to_vec(),
    );
    let mut sampler = flmlet::usage::UsageSampler::default();

    let first = sampler.sample(&mut host).unwrap();
    assert_eq!(first.cpu_percent, None);
    assert_eq!(first.memory.unwrap().used_bytes, 25 * 1024);
    assert_eq!(first.swap, None);

    host.files
        .insert(stat.clone(), b"cpu 60 0 10 130 0 0 0 0".to_vec());
    assert_eq!(sampler.sample(&mut host).unwrap().cpu_percent, Some(50.0));
    host.files.remove(&stat);
    assert_eq!(sampler.sample(&mut host).unwrap().cpu_percent, None);
    host.files.insert(stat, b"cpu 90 0 10 160 0 0 0 0".to_vec());
    assert_eq!(sampler.sample(&mut host).unwrap().cpu_percent, None);
}
