//! Host reconciliation for SRE-installed Flame binaries and explicitly scoped worker credentials.
use flame_api::{
    Condition, FlameCluster, FlameWorker, FlameWorkerStatus, HostServiceStatus, validation,
};
use kube::ResourceExt;
use rustls_pki_types::{CertificateDer, pem::PemObject};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub mod host;

pub const FEM: &str = "flame-executor-manager.service";
pub const FOC: &str = "flame-object-cache.service";
const MARKER: &str = "# Managed by flmlet\n";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    #[error(transparent)]
    Validation(#[from] validation::ValidationError),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Command(String),
    #[error("Kubernetes observation unavailable: {0}")]
    Unavailable(String),
    #[error("Kubernetes authorization was revoked; owned services are stopped")]
    Unauthorized,
    #[error("operator control-plane CA is missing or invalid; verified FLM access is required")]
    ControlPlaneTrust,
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug)]
pub struct Settings {
    pub worker: String,
    pub namespace: String,
    pub prefix: PathBuf,
    pub systemd_dir: PathBuf,
    pub adopt_existing: bool,
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        fn name(value: &str) -> bool {
            !value.is_empty()
                && value.len() <= 253
                && value.as_bytes()[0].is_ascii_alphanumeric()
                && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
                && value.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte)
                })
        }
        if !name(&self.worker)
            || !name(&self.namespace)
            || self.namespace.contains('.')
            || self.namespace.len() > 63
        {
            return Err(Error::Invalid(
                "worker and namespace must be valid Kubernetes resource names".into(),
            ));
        }
        for path in [&self.prefix, &self.systemd_dir] {
            if !path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                || path.to_str().is_none_or(|value| {
                    value
                        .chars()
                        .any(|character| character.is_control() || "\\\"'".contains(character))
                        || value.trim_end() != value
                        || value.ends_with('\\')
                })
            {
                return Err(Error::Invalid("prefix and systemd-dir must be absolute UTF-8 paths without parent traversal, control characters, quotes, backslashes, or trailing whitespace".into()));
            }
        }
        Ok(())
    }
    fn header(&self) -> String {
        format!("{MARKER}# Worker: {}/{}\n", self.namespace, self.worker)
    }
    fn unit_path(&self, unit: &str) -> PathBuf {
        self.systemd_dir.join(unit)
    }
    fn conf(&self, name: &str) -> PathBuf {
        self.prefix.join("conf").join(name)
    }
    fn owns(&self, bytes: &[u8]) -> bool {
        bytes.starts_with(self.header().as_bytes())
    }
}

#[derive(Clone, Debug, Default)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}
/// The only host effects, substituted by an in-memory host in unit tests.
pub trait Host {
    fn read(&mut self, path: &Path) -> Result<Option<Vec<u8>>>;
    fn write_atomic(&mut self, path: &Path, bytes: &[u8], mode: u32) -> Result<()>;
    fn executable(&mut self, path: &Path) -> bool;
    fn command(&mut self, program: &Path, args: &[String]) -> Result<CommandOutput>;
    fn hostname(&mut self) -> Result<String>;
    fn tcp_ready(&mut self, address: std::net::SocketAddrV4) -> bool;
}

#[derive(Clone, Debug, Serialize)]
pub struct Desired {
    pub files: BTreeMap<PathBuf, String>,
    pub units: BTreeMap<String, String>,
    pub version: String,
    pub rollout_revision: String,
}

fn env_value(value: &str) -> Result<String> {
    if value.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "environment values cannot contain control characters".into(),
        ));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}
enum SystemdPath {
    Directive,
    Executable,
    Argument,
}
fn systemd_value(path: &Path, syntax: SystemdPath) -> Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| Error::Invalid("paths must be UTF-8".into()))?;
    if value.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "paths cannot contain control characters".into(),
        ));
    }
    if matches!(syntax, SystemdPath::Directive) {
        // These directives consume a whole path, not an argv word: outer
        // quotes/backslash escapes become literal path characters. Only unit
        // specifiers need escaping (systemd load-fragment.c path parsers).
        return Ok(value.replace('%', "%%"));
    }
    let mut value = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    // systemd expands variables in argv arguments, never in the executable path.
    if matches!(syntax, SystemdPath::Argument) {
        value = value.replace('$', "$$");
    }
    Ok(format!("\"{value}\""))
}

fn validate_public_ca(ca: &str) -> Result<()> {
    let invalid = || Error::ControlPlaneTrust;
    if ca.len() > 65536 || ca.trim().is_empty() {
        return Err(invalid());
    }
    let mut remaining = ca.trim();
    let mut roots = rustls::RootCertStore::empty();
    while !remaining.is_empty() {
        let tail = remaining
            .strip_prefix("-----BEGIN CERTIFICATE-----")
            .ok_or_else(invalid)?;
        let (_, rest) = tail
            .split_once("-----END CERTIFICATE-----")
            .ok_or_else(invalid)?;
        let certificate =
            CertificateDer::from_pem_slice(&remaining.as_bytes()[..remaining.len() - rest.len()])
                .map_err(|_| invalid())?;
        roots.add(certificate).map_err(|_| invalid())?;
        remaining = rest.trim();
    }
    Ok(())
}

pub fn desired(
    settings: &Settings,
    worker: &FlameWorker,
    cluster: &FlameCluster,
) -> Result<Desired> {
    settings.validate()?;
    validation::worker_spec(&worker.spec)?;
    if worker.name_any() != settings.worker
        || worker.namespace().as_deref() != Some(&settings.namespace)
        || cluster.namespace() != worker.namespace()
        || cluster.name_any() != worker.spec.cluster_ref
    {
        return Err(Error::Invalid(
            "worker/cluster identity does not match the locally selected worker".into(),
        ));
    }
    let spec = &worker.spec;
    let endpoint = validation::worker_endpoint(spec, &cluster.spec)?;
    let ca = cluster
        .status
        .as_ref()
        .and_then(|status| status.control_plane_ca.as_deref())
        .ok_or(Error::ControlPlaneTrust)?;
    validate_public_ca(ca)?;
    let ca_path = settings.conf("flmlet/control-plane-ca.crt");
    let frontend = format!("https://{endpoint}");
    let cache = format!("grpc://{}:{}", spec.address, spec.cache_port);
    let prefix = settings.prefix.to_string_lossy();
    // Engine FlameTlsYaml requires both server identity fields to be present,
    // but FEM's client_tls_config reads only ca_file. Never distribute server keys.
    let configuration = serde_yaml::to_string(&json!({
        "cluster":{"name":cluster.name_any(),"endpoint":frontend,"resreq":cluster.spec.resource_request,
            "tls":{"ca_file":ca_path,"cert_file":"","key_file":""},
            "policies":cluster.spec.policies,"storage":format!("fs://{prefix}/data"),
            "executors":{"shims":spec.shims},"limits":{"node":{"max_executors":spec.max_executors}}},
        "cache":{"endpoint":cache,"advertise":cache,"storage":format!("{prefix}/data/objects"),"pool":{"size":"64M","policy":"lru"}}
    }))?;
    let client = serde_yaml::to_string(
        &json!({"current-context":"flmlet", "contexts":[{"name":"flmlet",
        "cluster":{"endpoint":frontend,"tls":{"ca_file":ca_path}},"cache":{"endpoint":cache}}]}),
    )?;
    let environment = BTreeMap::from([
        ("FLAME_HOME", prefix.to_string()),
        ("FLAME_PYTHON_VERSION", spec.python_version.clone()),
        (
            "PYTHONPATH",
            format!(
                "{prefix}/lib/python{}/site-packages:{prefix}/lib64/python{}/site-packages",
                spec.python_version, spec.python_version
            ),
        ),
        ("FLAME_ENDPOINT", frontend),
        ("FLAME_CA_FILE", ca_path.to_string_lossy().into_owned()),
        ("FLAME_CACHE_ENDPOINT", cache.clone()),
        ("FLAME_CACHE_ADVERTISE", cache),
    ])
    .into_iter()
    .map(|(name, value)| Ok(format!("{name}={}\n", env_value(&value)?)))
    .collect::<Result<String>>()?;
    let files = BTreeMap::from([
        (ca_path, settings.header() + ca.trim() + "\n"),
        (
            settings.conf("flmlet/flame-cluster.yaml"),
            settings.header() + &configuration,
        ),
        (
            settings.conf("flmlet-client.yaml"),
            settings.header() + &client,
        ),
        (
            settings.conf("flmlet.env"),
            settings.header() + &environment,
        ),
    ]);
    let mut units = BTreeMap::new();
    for (unit, binary) in [(FEM, "flame-executor-manager"), (FOC, "flame-object-cache")] {
        if unit == FOC && !spec.manage_object_cache {
            continue;
        }
        let contents = format!(
            "{}[Unit]\nDescription=Flame host service managed by flmlet\nWants=network-online.target\nAfter=network-online.target\n\n[Service]\nType=simple\nEnvironmentFile={}\nWorkingDirectory={}\nExecStart={} --config {}\nRestart=on-failure\nRestartSec=5s\nTimeoutStopSec=60s\nLimitNOFILE=65536\n\n[Install]\nWantedBy=multi-user.target\n",
            settings.header(),
            systemd_value(&settings.conf("flmlet.env"), SystemdPath::Directive)?,
            systemd_value(&settings.prefix, SystemdPath::Directive)?,
            systemd_value(
                &settings.prefix.join("bin").join(binary),
                SystemdPath::Executable
            )?,
            systemd_value(
                &settings.conf("flmlet/flame-cluster.yaml"),
                SystemdPath::Argument
            )?
        );
        units.insert(unit.into(), contents);
    }
    // Sorted maps give a stable hash, independent of map insertion or serialization order.
    Ok(Desired {
        files,
        units,
        version: cluster.spec.version.clone(),
        rollout_revision: spec.rollout_revision.clone(),
    })
}

fn command(host: &mut impl Host, program: &Path, args: &[&str]) -> Result<CommandOutput> {
    host.command(
        program,
        &args.iter().map(|value| (*value).into()).collect::<Vec<_>>(),
    )
}
fn systemctl(host: &mut impl Host, args: &[&str]) -> Result<()> {
    let result = command(host, Path::new("systemctl"), args)?;
    if !result.success {
        return Err(Error::Command(format!(
            "systemctl {}: {}",
            args.join(" "),
            result.stderr.trim()
        )));
    }
    Ok(())
}
fn service(host: &mut impl Host, unit: &str) -> HostServiceStatus {
    match command(
        host,
        Path::new("systemctl"),
        &["show", unit, "--property=ActiveState,SubState,Result"],
    ) {
        Ok(output) if output.success => {
            let fields: BTreeMap<_, _> = output
                .stdout
                .lines()
                .filter_map(|line| line.split_once('='))
                .collect();
            HostServiceStatus {
                active_state: fields
                    .get("ActiveState")
                    .copied()
                    .unwrap_or("unknown")
                    .into(),
                sub_state: fields.get("SubState").copied().unwrap_or("unknown").into(),
                message: fields.get("Result").copied().unwrap_or_default().into(),
            }
        }
        Ok(output) => HostServiceStatus {
            active_state: "unknown".into(),
            sub_state: String::new(),
            message: output.stderr,
        },
        Err(error) => HostServiceStatus {
            active_state: "unknown".into(),
            sub_state: String::new(),
            message: error.to_string(),
        },
    }
}
fn ensure_active(host: &mut impl Host, unit: &str) -> Result<HostServiceStatus> {
    let mut status = service(host, unit);
    if status.active_state != "active" {
        systemctl(host, &["start", unit])?;
        status = service(host, unit);
    }
    Ok(status)
}

/// Stop only the local worker's units. Foreign services/configuration are retained.
pub fn stop_owned(host: &mut impl Host, settings: &Settings) -> Result<bool> {
    settings.validate()?;
    let mut changed = false;
    let mut owned = false;
    for unit in [FEM, FOC] {
        if host
            .read(&settings.unit_path(unit))?
            .is_some_and(|bytes| settings.owns(&bytes))
        {
            owned = true;
            let state = service(host, unit);
            if !matches!(state.active_state.as_str(), "inactive" | "failed") {
                systemctl(host, &["stop", unit])?;
                changed = true;
            }
            if command(host, Path::new("systemctl"), &["is-enabled", unit])?.success {
                systemctl(host, &["disable", unit])?;
                changed = true;
            }
        }
    }
    let applied = settings.conf("flmlet-applied-hash");
    if owned && host.read(&applied)?.is_some_and(|bytes| !bytes.is_empty()) {
        host.write_atomic(&applied, b"", 0o600)?;
        changed = true;
    }
    Ok(changed)
}

fn installed_version(host: &mut impl Host, settings: &Settings) -> Result<Option<String>> {
    if let Some(bytes) = host.read(&settings.prefix.join("VERSION"))? {
        let value = String::from_utf8_lossy(&bytes).trim().to_string();
        if !value.is_empty() {
            return Ok(Some(value));
        }
    }
    let result = command(
        host,
        &settings.prefix.join("bin/flame-executor-manager"),
        &["--version"],
    )?;
    Ok(result
        .success
        .then(|| {
            result
                .stdout
                .split_whitespace()
                .last()
                .unwrap_or_default()
                .to_string()
        })
        .filter(|value| !value.is_empty()))
}
fn version_matches(selected: &str, installed: Option<&str>) -> bool {
    selected == "latest"
        || installed.is_some_and(|version| {
            version.strip_prefix('v').unwrap_or(version)
                == selected.strip_prefix('v').unwrap_or(selected)
        })
}
fn node_registered(data: &Value, hostname: &str) -> bool {
    let rows = data
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| data.as_array());
    rows.is_some_and(|rows| {
        rows.iter().any(|node| {
            let name = node
                .get("hostname")
                .or_else(|| node.pointer("/spec/hostname"))
                .or_else(|| node.get("name"))
                .or_else(|| node.pointer("/metadata/name"))
                .and_then(Value::as_str);
            let state = node.get("state").or_else(|| node.pointer("/status/state"));
            name == Some(hostname)
                && state.is_some_and(|state| {
                    state.as_i64() == Some(1)
                        || state
                            .as_str()
                            .is_some_and(|value| value.eq_ignore_ascii_case("ready"))
                })
        })
    })
}
fn registration(host: &mut impl Host, settings: &Settings, hostname: &str) -> Result<bool> {
    let config = settings.conf("flmlet-client.yaml");
    let config = config
        .to_str()
        .ok_or_else(|| Error::Invalid("client config path must be UTF-8".into()))?;
    // Bound external inventory work while checking more than the CLI's default page.
    for page in 1..=100 {
        let number = page.to_string();
        let result = command(
            host,
            &settings.prefix.join("bin/flmctl"),
            &[
                "--config", config, "list", "-n", "-o", "json", "--limit", "100", "--number",
                &number,
            ],
        )?;
        if !result.success {
            return Ok(false);
        }
        let data: Value = serde_json::from_str(&result.stdout)?;
        if node_registered(&data, hostname) {
            return Ok(true);
        }
        let rows = data
            .get("items")
            .and_then(Value::as_array)
            .or_else(|| data.as_array())
            .ok_or_else(|| {
                Error::Invalid("flmctl node listing must contain an inventory array".into())
            })?;
        if rows.len() < 100 {
            return Ok(false);
        }
    }
    Ok(false)
}

#[derive(Debug)]
pub enum Input<'a> {
    Present {
        worker: &'a FlameWorker,
        cluster: Option<&'a FlameCluster>,
    },
    Missing,
    Unauthorized,
    Unavailable(&'a str),
}
#[derive(Clone, Debug)]
pub struct Outcome {
    pub status: Option<FlameWorkerStatus>,
    pub changed: bool,
    pub ready: bool,
}

pub fn failed_status(worker: &FlameWorker, error: &str) -> FlameWorkerStatus {
    let mut status = worker.status.clone().unwrap_or_default();
    status.observed_generation = worker.metadata.generation.unwrap_or(0);
    status.last_heartbeat_time = chrono::Utc::now().to_rfc3339();
    status.conditions = conditions(worker, false, false, "ReconcileFailed", error);
    status
}
fn conditions(
    worker: &FlameWorker,
    configured: bool,
    ready: bool,
    reason: &str,
    message: &str,
) -> Vec<Condition> {
    [("Configured", configured), ("Ready", ready)]
        .into_iter()
        .map(|(kind, value)| {
            let state = if value { "True" } else { "False" };
            let previous = worker.status.as_ref().and_then(|status| {
                status
                    .conditions
                    .iter()
                    .find(|condition| condition.r#type == kind && condition.status == state)
            });
            Condition {
                r#type: kind.into(),
                status: state.into(),
                reason: reason.into(),
                message: message.into(),
                observed_generation: worker.metadata.generation.unwrap_or(0),
                last_transition_time: previous
                    .map(|condition| condition.last_transition_time.clone())
                    .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
            }
        })
        .collect()
}

fn validate_host(
    host: &mut impl Host,
    settings: &Settings,
    worker: &FlameWorker,
    desired: &Desired,
) -> Result<Option<String>> {
    for binary in ["flame-executor-manager", "flmctl"].into_iter().chain(
        worker
            .spec
            .manage_object_cache
            .then_some("flame-object-cache"),
    ) {
        if !host.executable(&settings.prefix.join("bin").join(binary)) {
            return Err(Error::Invalid(format!(
                "SRE-installed binary {binary} is missing or not executable"
            )));
        }
    }
    // Check every unit before making any file or service changes.
    for unit in desired.units.keys() {
        if let Some(bytes) = host.read(&settings.unit_path(unit))?
            && !settings.owns(&bytes)
            && !settings.adopt_existing
        {
            return Err(Error::Invalid(format!(
                "refusing foreign unit {unit}; use --adopt-existing explicitly"
            )));
        }
    }
    for path in desired.files.keys() {
        if let Some(bytes) = host.read(path)?
            && !settings.owns(&bytes)
            && !settings.adopt_existing
        {
            return Err(Error::Invalid(format!(
                "refusing foreign configuration {}; use --adopt-existing explicitly",
                path.display()
            )));
        }
    }
    installed_version(host, settings)
}

/// Read-only installation preflight, including local executable/version and ownership checks.
pub fn preflight(
    host: &mut impl Host,
    settings: &Settings,
    worker: &FlameWorker,
    cluster: &FlameCluster,
) -> Result<Desired> {
    let desired = desired(settings, worker, cluster)?;
    let version = validate_host(host, settings, worker, &desired)?;
    if !version_matches(&desired.version, version.as_deref()) {
        return Err(Error::Invalid(format!(
            "installed Flame version {:?} does not match selected {}; SRE installation is required",
            version, desired.version
        )));
    }
    Ok(desired)
}

fn write_desired(host: &mut impl Host, settings: &Settings, desired: &Desired) -> Result<bool> {
    let mut changed = false;
    for (path, contents) in &desired.files {
        if host.read(path)?.as_deref() != Some(contents.as_bytes()) {
            host.write_atomic(path, contents.as_bytes(), 0o600)?;
            changed = true;
        }
    }
    for (unit, contents) in &desired.units {
        let path = settings.unit_path(unit);
        if host.read(&path)?.as_deref() != Some(contents.as_bytes()) {
            host.write_atomic(&path, contents.as_bytes(), 0o644)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Installer phase: validate and write local configuration/units without starting services.
pub fn configure(
    host: &mut impl Host,
    settings: &Settings,
    worker: &FlameWorker,
    cluster: &FlameCluster,
) -> Result<bool> {
    if worker.spec.disabled || worker.metadata.deletion_timestamp.is_some() {
        return Err(Error::Invalid(
            "cannot configure a disabled or deleting worker".into(),
        ));
    }
    let desired = preflight(host, settings, worker, cluster)?;
    let changed = write_desired(host, settings, &desired)?;
    if changed {
        host.write_atomic(&settings.conf("flmlet-applied-hash"), b"", 0o600)?;
    }
    Ok(changed)
}

pub fn reconcile(host: &mut impl Host, settings: &Settings, input: Input<'_>) -> Result<Outcome> {
    settings.validate()?;
    let (worker, cluster) = match input {
        Input::Unavailable(error) => return Err(Error::Unavailable(error.into())),
        Input::Unauthorized => {
            stop_owned(host, settings)?;
            return Err(Error::Unauthorized);
        }
        Input::Missing => {
            return Ok(Outcome {
                status: None,
                changed: stop_owned(host, settings)?,
                ready: false,
            });
        }
        Input::Present { worker, cluster } => (worker, cluster),
    };
    if worker.name_any() != settings.worker
        || worker.namespace().as_deref() != Some(&settings.namespace)
    {
        return Err(Error::Invalid(
            "observed worker does not match local identity".into(),
        ));
    }
    if worker.spec.disabled || worker.metadata.deletion_timestamp.is_some() {
        let changed = stop_owned(host, settings)?;
        let status = FlameWorkerStatus {
            observed_generation: worker.metadata.generation.unwrap_or(0),
            last_heartbeat_time: chrono::Utc::now().to_rfc3339(),
            conditions: conditions(
                worker,
                false,
                false,
                "Disabled",
                "Worker is disabled or deleting; owned services are stopped",
            ),
            fem: service(host, FEM),
            object_cache: worker.spec.manage_object_cache.then(|| service(host, FOC)),
            ..Default::default()
        };
        return Ok(Outcome {
            status: Some(status),
            changed,
            ready: false,
        });
    }
    let cluster = cluster
        .ok_or_else(|| Error::Unavailable("FlameCluster observation is unavailable".into()))?;
    let desired = match desired(settings, worker, cluster) {
        Ok(desired) => desired,
        Err(Error::ControlPlaneTrust) => {
            stop_owned(host, settings)?;
            return Err(Error::ControlPlaneTrust);
        }
        Err(error) => return Err(error),
    };
    let version = validate_host(host, settings, worker, &desired)?;
    if !version_matches(&desired.version, version.as_deref()) {
        let status = FlameWorkerStatus {
            observed_generation: worker.metadata.generation.unwrap_or(0),
            last_heartbeat_time: chrono::Utc::now().to_rfc3339(),
            installed_version: version,
            conditions: conditions(
                worker,
                false,
                false,
                "VersionMismatch",
                "Installed Flame version does not match the selected cluster version; SRE installation is required",
            ),
            fem: service(host, FEM),
            object_cache: worker.spec.manage_object_cache.then(|| service(host, FOC)),
            ..Default::default()
        };
        return Ok(Outcome {
            status: Some(status),
            changed: false,
            ready: false,
        });
    }
    let hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(
            &json!({"desired":desired,"installedVersion":version})
        )?)
    );
    let applied = settings.conf("flmlet-applied-hash");
    let previous = host.read(&applied)?;
    let mut changed = previous.as_deref() != Some(hash.as_bytes());
    changed |= write_desired(host, settings, &desired)?;
    if !worker.spec.manage_object_cache
        && host
            .read(&settings.unit_path(FOC))?
            .is_some_and(|bytes| settings.owns(&bytes))
    {
        let state = service(host, FOC);
        if state.active_state != "inactive" {
            systemctl(host, &["stop", FOC])?;
        }
        if command(host, Path::new("systemctl"), &["is-enabled", FOC])?.success {
            systemctl(host, &["disable", FOC])?;
        }
    }
    if changed {
        systemctl(host, &["daemon-reload"])?;
        for unit in [worker.spec.manage_object_cache.then_some(FOC), Some(FEM)]
            .into_iter()
            .flatten()
        {
            systemctl(host, &["enable", unit])?;
            systemctl(host, &["restart", unit])?;
        }
        host.write_atomic(&applied, hash.as_bytes(), 0o600)?;
    }
    let object_cache = if worker.spec.manage_object_cache {
        Some(ensure_active(host, FOC)?)
    } else {
        None
    };
    let fem = ensure_active(host, FEM)?;
    let hostname = host.hostname()?;
    let registered = registration(host, settings, &hostname)?;
    let address = std::net::SocketAddrV4::new(
        validation::lan_ipv4(&worker.spec.address)?,
        worker.spec.cache_port,
    );
    let cache_ready = host.tcp_ready(address);
    let services_active = fem.active_state == "active"
        && object_cache
            .as_ref()
            .is_none_or(|status| status.active_state == "active");
    let ready = services_active && registered && (!worker.spec.manage_object_cache || cache_ready);
    let (reason, message) = if !services_active {
        ("ServiceInactive", "A managed systemd service is not active")
    } else if !registered {
        (
            "NotRegistered",
            "The actual hostname is not Ready in FLM node inventory",
        )
    } else if worker.spec.manage_object_cache && !cache_ready {
        (
            "CacheUnreachable",
            "The managed object cache is not accepting TCP connections",
        )
    } else {
        (
            "Reconciled",
            "Configured services are active and the actual host is registered with FLM",
        )
    };
    let status = FlameWorkerStatus {
        observed_generation: worker.metadata.generation.unwrap_or(0),
        conditions: conditions(worker, true, ready, reason, message),
        last_heartbeat_time: chrono::Utc::now().to_rfc3339(),
        config_hash: hash,
        installed_version: version,
        hostname: Some(hostname),
        registered,
        cache_ready,
        fem,
        object_cache,
    };
    Ok(Outcome {
        status: Some(status),
        changed,
        ready,
    })
}
