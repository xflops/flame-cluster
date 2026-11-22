//! Host runtime installation and reconciliation with explicitly scoped worker credentials.
use flame_api::{
    Condition, FlameCluster, FlameWorker, FlameWorkerStatus, HostServiceStatus, NodeCapacity,
    validation,
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
mod pci;
pub mod usage;

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
    fn drop_in_path(&self, unit: &str) -> PathBuf {
        self.systemd_dir.join(format!("{unit}.d/flmlet.conf"))
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
    fn read_dir(&mut self, path: &Path) -> Result<Vec<PathBuf>>;
    fn write_atomic(&mut self, path: &Path, bytes: &[u8], mode: u32) -> Result<()>;
    fn executable(&mut self, path: &Path) -> bool;
    fn command(&mut self, program: &Path, args: &[String]) -> Result<CommandOutput>;
    fn bootstrap(
        &mut self,
        settings: &Settings,
        version: &str,
        python: &str,
        force: bool,
    ) -> Result<()>;
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
            settings.conf("flame-cluster.yaml"),
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
        let contents = format!(
            "{}[Service]\nEnvironmentFile={}\nWorkingDirectory={}\nExecStart=\nExecStart={} --config {}\nStandardOutput=append:{}\nStandardError=append:{}\n",
            settings.header(),
            systemd_value(&settings.conf("flmlet.env"), SystemdPath::Directive)?,
            systemd_value(&settings.prefix, SystemdPath::Directive)?,
            systemd_value(
                &settings.prefix.join("bin").join(binary),
                SystemdPath::Executable
            )?,
            systemd_value(&settings.conf("flame-cluster.yaml"), SystemdPath::Argument)?,
            systemd_value(
                &settings
                    .prefix
                    .join("logs")
                    .join(if unit == FEM { "fem.log" } else { "foc.log" }),
                SystemdPath::Directive
            )?,
            systemd_value(
                &settings
                    .prefix
                    .join("logs")
                    .join(if unit == FEM { "fem.log" } else { "foc.log" }),
                SystemdPath::Directive
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
fn owns_unit(host: &mut impl Host, settings: &Settings, unit: &str) -> Result<bool> {
    Ok(host
        .read(&settings.drop_in_path(unit))?
        .is_some_and(|bytes| settings.owns(&bytes))
        || host
            .read(&settings.unit_path(unit))?
            .is_some_and(|bytes| settings.owns(&bytes)))
}

pub fn stop_owned(host: &mut impl Host, settings: &Settings) -> Result<bool> {
    settings.validate()?;
    let mut changed = false;
    let mut owned = false;
    for unit in [FEM, FOC] {
        if owns_unit(host, settings, unit)? {
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
    if !host.executable(&settings.prefix.join("bin/flame-executor-manager")) {
        return Ok(None);
    }
    let result = command(
        host,
        &settings.prefix.join("bin/flame-executor-manager"),
        &["--version"],
    )?;
    let binary_version = result
        .success
        .then(|| {
            result
                .stdout
                .split_whitespace()
                .last()
                .unwrap_or_default()
                .to_string()
        })
        .filter(|value| !value.is_empty());
    if binary_version.is_some() {
        return Ok(binary_version);
    }
    Ok(host
        .read(&settings.prefix.join("VERSION"))?
        .and_then(|bytes| {
            let value = String::from_utf8_lossy(&bytes).trim().to_string();
            (!value.is_empty()).then_some(value)
        }))
}
fn version_matches(selected: &str, installed: Option<&str>) -> bool {
    selected == "latest"
        || installed.is_some_and(|version| {
            version.strip_prefix('v').unwrap_or(version)
                == selected.strip_prefix('v').unwrap_or(selected)
        })
}
fn registered_node<'a>(data: &'a Value, hostname: &str) -> Option<&'a Value> {
    let rows = data
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| data.as_array());
    rows.and_then(|rows| {
        rows.iter().find(|node| {
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
fn node_capacity(node: &Value) -> Option<NodeCapacity> {
    // Native flmctl inventory reports hardware fields directly on each node.
    // Keep nested capacity wrappers compatible without overriding native values.
    let nested = node
        .get("capacity")
        .or_else(|| node.pointer("/status/capacity"));
    let measurement = |field| {
        node.get(field)
            .or_else(|| nested.and_then(|capacity| capacity.get(field)))
            .and_then(Value::as_u64)
    };
    let result = NodeCapacity {
        cpu: measurement("cpu"),
        gpu: measurement("gpu"),
        memory: measurement("memory"),
    };
    (result.cpu.is_some() || result.gpu.is_some() || result.memory.is_some()).then_some(result)
}
fn registration(
    host: &mut impl Host,
    settings: &Settings,
    hostname: &str,
) -> Result<(bool, Option<NodeCapacity>)> {
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
            return Ok((false, None));
        }
        let data: Value = serde_json::from_str(&result.stdout)?;
        if let Some(node) = registered_node(&data, hostname) {
            return Ok((true, node_capacity(node)));
        }
        let rows = data
            .get("items")
            .and_then(Value::as_array)
            .or_else(|| data.as_array())
            .ok_or_else(|| {
                Error::Invalid("flmctl node listing must contain an inventory array".into())
            })?;
        if rows.len() < 100 {
            return Ok((false, None));
        }
    }
    Ok((false, None))
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
    status.capacity = None;
    status.usage = None;
    status.pci_devices = None;
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

fn bootstrap_identity(settings: &Settings, worker: &FlameWorker, desired: &Desired) -> Value {
    json!({"worker":settings.worker,"namespace":settings.namespace,"prefix":settings.prefix,"systemdDir":settings.systemd_dir,
        "version":desired.version,"python":worker.spec.python_version})
}

enum BootstrapPhase {
    Installing,
    Configuring,
}

fn bootstrap_phase(
    host: &mut impl Host,
    settings: &Settings,
    worker: &FlameWorker,
    desired: &Desired,
) -> Result<Option<BootstrapPhase>> {
    let Some(bytes) = host.read(&settings.conf("flmlet/bootstrap.json"))? else {
        return Ok(None);
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    let Some(data) = bytes.strip_prefix(settings.header().as_bytes()) else {
        if settings.adopt_existing {
            return Ok(None);
        }
        return Err(Error::Invalid(
            "refusing a foreign runtime bootstrap marker".into(),
        ));
    };
    let mut identity: Value = match serde_json::from_slice(data) {
        Ok(identity) => identity,
        Err(_) if settings.adopt_existing => return Ok(None),
        Err(_) => return Err(Error::Invalid("invalid runtime bootstrap marker".into())),
    };
    let phase = identity
        .as_object_mut()
        .and_then(|object| object.remove("phase"));
    if identity != bootstrap_identity(settings, worker, desired) {
        return Ok(None);
    }
    match phase.as_ref().and_then(Value::as_str) {
        None if phase.is_none() => Ok(Some(BootstrapPhase::Installing)),
        Some("installing") => Ok(Some(BootstrapPhase::Installing)),
        Some("configuring") => Ok(Some(BootstrapPhase::Configuring)),
        _ if settings.adopt_existing => Ok(None),
        _ => Err(Error::Invalid("invalid runtime bootstrap phase".into())),
    }
}

fn bootstrap_recovery(
    host: &mut impl Host,
    settings: &Settings,
    worker: &FlameWorker,
    desired: &Desired,
) -> Result<bool> {
    Ok(bootstrap_phase(host, settings, worker, desired)?.is_some())
}

fn write_bootstrap_marker(
    host: &mut impl Host,
    settings: &Settings,
    worker: &FlameWorker,
    desired: &Desired,
    phase: BootstrapPhase,
) -> Result<()> {
    let mut identity = bootstrap_identity(settings, worker, desired);
    identity["phase"] = json!(match phase {
        BootstrapPhase::Installing => "installing",
        BootstrapPhase::Configuring => "configuring",
    });
    let marker = settings.header() + &serde_json::to_string(&identity)?;
    host.write_atomic(
        &settings.conf("flmlet/bootstrap.json"),
        marker.as_bytes(),
        0o600,
    )
}

fn upstream_exec_start(unit: &str, prefix: &str) -> String {
    let binary = unit.strip_suffix(".service").unwrap_or(unit);
    format!("ExecStart={prefix}/bin/{binary} --config {prefix}/conf/flame-cluster.yaml\n")
}

fn upstream_unit(settings: &Settings, unit: &str, bytes: &[u8]) -> bool {
    let prefix = settings.prefix.to_string_lossy();
    let text = String::from_utf8_lossy(bytes);
    text.contains("Documentation=https://github.com/xflops-io/flame\n")
        && (text.contains(&upstream_exec_start(unit, &prefix))
            || text.contains(&upstream_exec_start(unit, &prefix.replace('%', "%%"))))
}

fn normalize_upstream_units(host: &mut impl Host, settings: &Settings) -> Result<()> {
    let prefix = settings.prefix.to_string_lossy();
    if !prefix.contains('%') {
        return Ok(());
    }
    let escaped = prefix.replace('%', "%%");
    for unit in [FEM, FOC] {
        let path = settings.unit_path(unit);
        let bytes = host
            .read(&path)?
            .ok_or_else(|| Error::Invalid(format!("upstream installer did not provide {unit}")))?;
        if !upstream_unit(settings, unit, &bytes) {
            return Err(Error::Invalid(format!(
                "refusing to normalize an unexpected upstream unit {unit}"
            )));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| Error::Invalid(format!("upstream unit {unit} is not UTF-8")))?;
        if text.contains(&upstream_exec_start(unit, &prefix)) {
            // The vendor unit has unescaped prefix paths. Preserve its definitions
            // while preventing systemd from expanding literal prefix percent signs.
            host.write_atomic(
                &path,
                text.replace(prefix.as_ref(), &escaped).as_bytes(),
                0o644,
            )?;
        }
    }
    Ok(())
}

fn validate_host(
    host: &mut impl Host,
    settings: &Settings,
    worker: &FlameWorker,
    desired: &Desired,
) -> Result<Option<String>> {
    let recovering = bootstrap_recovery(host, settings, worker, desired)?;
    let legacy_owned = host
        .read(&settings.unit_path(FEM))?
        .is_some_and(|bytes| settings.owns(&bytes))
        && host
            .read(&settings.conf("flmlet/flame-cluster.yaml"))?
            .is_some_and(|bytes| settings.owns(&bytes));
    // Check every unit before making any file or service changes.
    for unit in desired.units.keys() {
        for path in host.read_dir(&settings.systemd_dir.join(format!("{unit}.d")))? {
            if path
                .extension()
                .is_some_and(|extension| extension == "conf")
                && host
                    .read(&path)?
                    .is_some_and(|bytes| !settings.owns(&bytes))
                && !settings.adopt_existing
            {
                return Err(Error::Invalid(format!(
                    "refusing foreign drop-in {}; use --adopt-existing explicitly",
                    path.display()
                )));
            }
        }
        let drop_in = host.read(&settings.drop_in_path(unit))?;
        if drop_in.as_ref().is_some_and(|bytes| !settings.owns(bytes)) && !settings.adopt_existing {
            return Err(Error::Invalid(format!(
                "refusing foreign drop-in for {unit}; use --adopt-existing explicitly"
            )));
        }
        if let Some(bytes) = host.read(&settings.unit_path(unit))?
            && !settings.owns(&bytes)
            && !drop_in.as_ref().is_some_and(|bytes| settings.owns(bytes))
            && !(recovering && upstream_unit(settings, unit, &bytes))
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
            && !((recovering || legacy_owned)
                && path == &settings.conf("flame-cluster.yaml")
                && bytes.starts_with(
                    b"# Flame cluster configuration, generated by setup.sh from FLAME_* settings\n",
                ))
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

fn runtime_missing(host: &mut impl Host, settings: &Settings, _worker: &FlameWorker) -> bool {
    ["flame-executor-manager", "flmctl", "flame-object-cache"]
        .into_iter()
        .any(|binary| !host.executable(&settings.prefix.join("bin").join(binary)))
}

fn systemd_installation_needed(host: &mut impl Host, settings: &Settings) -> Result<bool> {
    for unit in [FEM, FOC] {
        let base = host.read(&settings.unit_path(unit))?;
        let drop_in = host.read(&settings.drop_in_path(unit))?;
        if base.is_none()
            || base.as_ref().is_some_and(|bytes| settings.owns(bytes))
            || !drop_in.as_ref().is_some_and(|bytes| settings.owns(bytes))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn bootstrap_needed(
    host: &mut impl Host,
    settings: &Settings,
    worker: &FlameWorker,
    desired: &Desired,
    version: Option<&str>,
) -> Result<bool> {
    let needed = runtime_missing(host, settings, worker)
        || !version_matches(&desired.version, version)
        || systemd_installation_needed(host, settings)?
        || matches!(
            bootstrap_phase(host, settings, worker, desired)?,
            Some(BootstrapPhase::Installing)
        );
    if needed {
        let existing = ["flame-executor-manager", "flmctl", "flame-object-cache"]
            .iter()
            .any(|binary| host.executable(&settings.prefix.join("bin").join(binary)));
        let owned =
            owns_unit(host, settings, FEM)? || bootstrap_recovery(host, settings, worker, desired)?;
        if existing && !owned && !settings.adopt_existing {
            return Err(Error::Invalid(
                "refusing to replace an unowned Flame runtime; use --adopt-existing explicitly"
                    .into(),
            ));
        }
        for tool in ["curl", "sh", "tar", "logrotate", "systemctl", "env"] {
            if !host.executable(Path::new(tool)) {
                return Err(Error::Invalid(format!("runtime bootstrap requires {tool}")));
            }
        }
    }
    Ok(needed)
}

fn ensure_runtime(
    host: &mut impl Host,
    settings: &Settings,
    worker: &FlameWorker,
    desired: &Desired,
) -> Result<(Option<String>, bool)> {
    let version = validate_host(host, settings, worker, desired)?;
    if bootstrap_needed(host, settings, worker, desired, version.as_deref())? {
        let force = ["flame-executor-manager", "flmctl", "flame-object-cache"]
            .iter()
            .any(|binary| host.executable(&settings.prefix.join("bin").join(binary)))
            || host.read(&settings.conf("flame-cluster.yaml"))?.is_some()
            || host.read(&settings.unit_path(FEM))?.is_some()
            || host.read(&settings.unit_path(FOC))?.is_some();
        write_bootstrap_marker(host, settings, worker, desired, BootstrapPhase::Installing)?;
        host.bootstrap(
            settings,
            &desired.version,
            &worker.spec.python_version,
            force,
        )?;
        normalize_upstream_units(host, settings)?;
        let installed = installed_version(host, settings)?;
        if runtime_missing(host, settings, worker)
            || !version_matches(&desired.version, installed.as_deref())
            || host.read(&settings.unit_path(FEM))?.is_none()
            || host.read(&settings.unit_path(FOC))?.is_none()
        {
            return Err(Error::Invalid(
                "Flame installer did not provide the selected runtime and systemd units".into(),
            ));
        }
        write_bootstrap_marker(host, settings, worker, desired, BootstrapPhase::Configuring)?;
        return Ok((installed, true));
    }
    Ok((version, false))
}

/// Read-only preflight. Missing runtimes are accepted when bootstrap can proceed safely.
pub fn preflight(
    host: &mut impl Host,
    settings: &Settings,
    worker: &FlameWorker,
    cluster: &FlameCluster,
) -> Result<Desired> {
    let desired = desired(settings, worker, cluster)?;
    let version = validate_host(host, settings, worker, &desired)?;
    bootstrap_needed(host, settings, worker, &desired, version.as_deref())?;
    Ok(desired)
}

fn write_desired(
    host: &mut impl Host,
    settings: &Settings,
    desired: &Desired,
    force: bool,
) -> Result<bool> {
    let mut changed = false;
    for (path, contents) in &desired.files {
        if force || host.read(path)?.as_deref() != Some(contents.as_bytes()) {
            host.write_atomic(path, contents.as_bytes(), 0o600)?;
            changed = true;
        }
    }
    for (unit, contents) in &desired.units {
        let path = settings.drop_in_path(unit);
        if force || host.read(&path)?.as_deref() != Some(contents.as_bytes()) {
            host.write_atomic(&path, contents.as_bytes(), 0o644)?;
            changed = true;
        }
    }
    Ok(changed)
}

fn apply_configuration(
    host: &mut impl Host,
    settings: &Settings,
    worker: &FlameWorker,
    desired: &Desired,
    version: Option<&str>,
    installed: bool,
) -> Result<(bool, String)> {
    let hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(
            &json!({"desired":desired,"installedVersion":version,"manageObjectCache":worker.spec.manage_object_cache})
        )?)
    );
    let applied = settings.conf("flmlet-applied-hash");
    let mut changed = host.read(&applied)?.as_deref() != Some(hash.as_bytes());
    changed |= write_desired(host, settings, desired, installed)?;
    disable_unrequested_cache(host, settings, worker)?;
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
    let marker = settings.conf("flmlet/bootstrap.json");
    if host.read(&marker)?.is_some_and(|bytes| !bytes.is_empty()) {
        host.write_atomic(&marker, b"", 0o600)?;
    }
    Ok((changed, hash))
}

fn disable_unrequested_cache(
    host: &mut impl Host,
    settings: &Settings,
    worker: &FlameWorker,
) -> Result<()> {
    if !worker.spec.manage_object_cache && owns_unit(host, settings, FOC)? {
        let state = service(host, FOC);
        if state.active_state != "inactive" {
            systemctl(host, &["stop", FOC])?;
        }
        if command(host, Path::new("systemctl"), &["is-enabled", FOC])?.success {
            systemctl(host, &["disable", FOC])?;
        }
    }
    Ok(())
}

/// Install upstream units, then apply worker configuration and restart the selected services.
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
    let (version, installed) = ensure_runtime(host, settings, worker, &desired)?;
    apply_configuration(
        host,
        settings,
        worker,
        &desired,
        version.as_deref(),
        installed,
    )
    .map(|(changed, _)| changed)
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
            pci_devices: pci::inventory(host),
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
    let (version, installed) = ensure_runtime(host, settings, worker, &desired)?;
    let (changed, hash) = apply_configuration(
        host,
        settings,
        worker,
        &desired,
        version.as_deref(),
        installed,
    )?;
    let object_cache = if worker.spec.manage_object_cache {
        Some(ensure_active(host, FOC)?)
    } else {
        None
    };
    let fem = ensure_active(host, FEM)?;
    let hostname = host.hostname()?;
    let (registered, capacity) = registration(host, settings, &hostname)?;
    let pci_devices = pci::inventory(host);
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
        capacity,
        usage: None,
        pci_devices,
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
