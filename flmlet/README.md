# flmlet

`flmlet` reconciles one namespaced `FlameWorker` on an existing Linux systemd host.
SREs prepare workload runtimes, networking, and storage. Flmlet first installs
FEM through upstream `flame/setup.sh --join`, including its systemd units.
Flmlet defers service activation through the upstream `FLAME_SYSTEMCTL` hook,
then writes the cluster configuration and restarts FEM and the optional paired
object cache using systemctl. It stops and disables the object cache when it is
not requested.
Flmlet does not poll registries, install Kubernetes, or request Kubernetes CSRs.

During reconciliation, flmlet uses the existing FLM node-inventory check to report
`FlameWorker.status.capacity` for its actual hostname when that node is Ready.
CPU is a core count, GPU is a device count (including zero), and memory is bytes.
Missing or malformed measurements remain unknown; they do not prevent registration.
Capacity clears when registration is lost, the worker is disabled, or reconciliation
fails and its status can still be written. The UI reads this worker status.

The operator renews a cluster join token every fifteen minutes. Copy the current
join command from **Settings → Join a node** in the UI and run it on the prepared
worker host. The command fetches the hosted installer; no local `setup.sh` is
needed. The repository's
`setup.sh --join "$TOKEN"` uses that token to create its worker through the
operator, then installs flmlet with a durable, worker-scoped local kubeconfig.
Renewing the cluster bootstrap token does not disrupt already joined workers.
The selected namespace's token chooses the Flame, for example
`flame-1/flame-1`. Worker identity and status access remain in that namespace;
another namespace's objects and client Secrets are forbidden.
Keep tokens and the local kubeconfig private. The worker identity can read only
its own worker, read the selected FlameCluster, and patch its own status.

The generated FEM/FOC system services currently run as root. Both CRI and Host
shims are enabled by default (`cri,host`). The Host shim starts application
processes with FEM's OS identity, currently root. Select `--shims cri` at join
time or set `FlameWorker.spec.shims: [cri]` for CRI-only execution.
Therefore every principal allowed to submit Host-shim workloads, including
`flamepy.app` code, must be trusted as a host administrator. Such code can read
the root-owned worker kubeconfig and enrollment key, alter flmlet or systemd
units, and bypass later worker revocation. File modes and worker-scoped RBAC do
not isolate credentials from root workloads. Restrict FLM workload submission to
trusted administrators and isolate these hosts from untrusted workloads; this
release does not provide a tenant security boundary for Host-shim execution.

For slow GitHub release downloads, explicitly select an HTTPS accelerator for
the runtime archive and its checksum. The default remains the official GitHub
URLs. For example, replace `<token>` with the current token from Settings:

```sh
curl -fsSL https://xflops.io/cluster/setup.sh | sudo env FLAME_GITHUB_MIRROR=https://gh-proxy.com/ sh -s -- --join '<token>'
```

`sudo env` passes the setting to the root installer and its flmlet child;
assigning it only to `curl` or before `sudo` may not propagate it. A prefix such
as `https://gh-proxy.com/` or `https://ghproxy.net/` is prepended to the complete
official runtime archive and checksum URLs. Prefixes must use HTTPS and have no
credentials, query, or fragment; path prefixes are supported and a trailing slash is
normalized. This is a per-process opt-in, not a global proxy configuration.

Flmlet downloads the release checksum first, then verifies the downloaded archive
against it before extracting or executing the bundled upstream installer.
When a mirror is selected, both downloads use that mirror, which must be trusted
to supply the archive and checksum. The cluster join script still
comes from `https://xflops.io/cluster/setup.sh`; this option changes neither that
source nor SDK/Python download settings.

For manual diagnosis with already provisioned credentials:

```sh
flmlet --worker flm-worker-01 --namespace flame-1 \
  --kubeconfig /usr/local/flame/conf/flmlet/kubeconfig --check
flmlet --worker flm-worker-01 --namespace flame-1 \
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
resources and performs read-only host preflight, validating identity, trust,
and file/unit ownership before printing the desired configuration. A missing
runtime can be installed later; `--check` performs no installation.
The installer's `--configure` phase installs the runtime and upstream systemd
units if needed, then writes configuration and restarts the requested services.
Flmlet preserves upstream unit definitions, correcting literal percent escaping
in installation paths when needed, and writes worker-specific settings in
`<unit>.d/flmlet.conf` drop-ins.

Only `flame-executor-manager.service` and the optional
`flame-object-cache.service` are managed. Their drop-in ownership headers
identify the local namespace and worker. Foreign units or generated configuration are refused
unless `--adopt-existing` explicitly authorizes adoption. This flag is intended
for an SRE-controlled cutover and must not be set by tenants.

Private configuration is written atomically under `PREFIX/conf` as
`flame-cluster.yaml`, `flmlet-client.yaml`, and `flmlet.env`. Reconciliation
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
FEM's `--version`; a mismatch installs the selected version before configuration
and service restart. `latest` installs a missing runtime and otherwise retains
the existing runtime without looking for updates.
Readiness requires active services, the actual hostname Ready in FLM node
inventory, and TCP cache readiness when FOC is managed. Heartbeats and the
applied configuration hash are reported on `FlameWorker.status`.

When the installed Flame runtime reports PCI inventory, flmlet publishes it as
`FlameWorker.status.pciDevices` from the same Ready node matched by hostname.
An empty inventory is reported as an empty list. Older runtimes and unavailable
inventory leave the field absent.

Confirmed deletion/disable or explicit Kubernetes 401/403 revocation stops and
disables only this worker's owned FEM/FOC services. A missing referenced cluster
also stops them. Transport errors and server failures retain healthy services
and their last configuration. Revoked workers require a fresh operator-issued
join token to reconnect. Neither deletion nor revocation deletes local data.

Tests use an in-memory host/command runner and temporary files; they never invoke
host systemd. Run `cargo test -p flmlet` and `cargo clippy -p flmlet --all-targets`.

Runtime archive downloads allow up to 15 minutes, with a strict 15 minute
30 second process deadline covering retries. This accommodates slow links while
retaining TLS and release-checksum verification. The upstream installation step
has its own timeout.
