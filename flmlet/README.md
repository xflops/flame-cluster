# flmlet

`flmlet` reconciles one namespaced `FlameWorker` on an existing Linux systemd host.
SREs install the public Flame runtime, Python SDK/interpreters, optional CRI
runtime, networking, and storage first. flmlet configures and starts the installed
FEM and, when `manageObjectCache: true`, its paired FOC. It never downloads Flame
releases, polls registries, installs Kubernetes, or requests Kubernetes CSRs.

The operator renews a cluster join token every fifteen minutes. Copy the current
join command from **Settings → Join a node** in the UI and run it on the prepared
worker host with `setup.sh` available. The repository's
`setup.sh --join "$TOKEN"` uses that token to create its worker through the
operator, then installs flmlet with a durable, worker-scoped local kubeconfig.
Renewing the cluster bootstrap token does not disrupt already joined workers.
Keep tokens and the local kubeconfig private. The worker identity can read only
its own worker, read the selected FlameCluster, and patch its own status.

The generated FEM/FOC system services currently run as root. The CRI shim is the
default. When explicitly enabled, the Host shim starts application processes
with FEM's OS identity.
Therefore every principal allowed to submit Host-shim workloads, including
`flamepy.app` code, must be trusted as a host administrator. Such code can read
the root-owned worker kubeconfig and enrollment key, alter flmlet or systemd
units, and bypass later worker revocation. File modes and worker-scoped RBAC do
not isolate credentials from root workloads. Restrict FLM workload submission to
trusted administrators and isolate these hosts from untrusted workloads; this
release does not provide a tenant security boundary for Host-shim execution.

For manual diagnosis with already provisioned credentials:

```sh
flmlet --worker flm-worker-01 --namespace flame-system \
  --kubeconfig /usr/local/flame/conf/flmlet/kubeconfig --check
flmlet --worker flm-worker-01 --namespace flame-system \
  --kubeconfig /usr/local/flame/conf/flmlet/kubeconfig --once
```

`--prefix` defaults to `/usr/local/flame`; `--systemd-dir` defaults to
`/etc/systemd/system`. Without `--kubeconfig`, normal Kubernetes configuration
loading honors `KUBECONFIG`. Host paths and unit destinations are local CLI
settings; a worker resource cannot select them. Paths may contain internal spaces,
percent signs, and dollar signs, but quotes, backslashes, control characters,
parent traversal, and trailing whitespace are rejected before host changes. The daemon polls every ten
seconds. `--once` reports failure unless the worker becomes Ready (or is
intentionally disabled/absent and cleanup completes). `--check` reads current
resources and performs read-only host preflight, validating binaries, selected
version, and file/unit ownership before printing the desired configuration.
The installer's `--configure` phase
performs the same preflight and writes configuration/owned units without
starting services; normal daemon reconciliation activates them afterward.

Only `flame-executor-manager.service` and the optional
`flame-object-cache.service` are managed. Their ownership headers identify the
local namespace and worker. Foreign units or generated configuration are refused
unless `--adopt-existing` explicitly authorizes adoption. This flag is intended
for an SRE-controlled cutover and must not be set by tenants.

Private configuration is written atomically under `PREFIX/conf` as
`flmlet/flame-cluster.yaml`, `flmlet-client.yaml`, and `flmlet.env`. Reconciliation
updates services only when configuration, selected binary version, or worker
`rolloutRevision` changes; it checks live service state and starts inactive
managed services every cycle. Other package configuration is retained. The
cache uses the worker's LAN address and a conservative 64 MiB LRU pool.

FLM frontend and backend connections use HTTPS with endpoint-IP certificate
verification. The operator publishes only its public control-plane CA in
`FlameCluster.status.controlPlaneCa`; flmlet validates that certificate bundle and
writes `PREFIX/conf/flmlet/control-plane-ca.crt`. FEM configuration, the local
flmctl context, and SDK environment reference this trust file. No FLM server
private key or client identity is downloaded. The FEM YAML's empty certificate
and key path fields satisfy the engine's configuration format; its client TLS
implementation uses only the CA. Native FLM client authentication is not provided
by this configuration. Missing or malformed trust rejects read-only preflight
and installation before writes, and stops owned runtime services during normal
reconciliation. A valid CA change updates configuration and restarts services.
The optional paired object cache continues using private-LAN `grpc://` with no
cache TLS; SREs must restrict that network separately.

A pinned cluster version must match local `PREFIX/VERSION` or the installed
FEM's `--version`; mismatches report NotReady and require SRE installation.
`latest` uses the existing selected runtime without looking for updates.
Readiness requires active services, the actual hostname Ready in FLM node
inventory, and TCP cache readiness when FOC is managed. Heartbeats and the
applied configuration hash are reported on `FlameWorker.status`.

Confirmed deletion/disable or explicit Kubernetes 401/403 revocation stops and
disables only this worker's owned FEM/FOC services. A missing referenced cluster
also stops them. Transport errors and server failures retain healthy services
and their last configuration. Revoked workers require a fresh operator-issued
join token to reconnect. Neither deletion nor revocation deletes local data.

Tests use an in-memory host/command runner and temporary files; they never invoke
host systemd. Run `cargo test -p flmlet` and `cargo clippy -p flmlet --all-targets`.
