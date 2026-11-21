# Deployment and namespace isolation

The charts install Flame-owned resources on existing SRE-managed infrastructure.
They do not install Kubernetes, cert-manager, Envoy Gateway, databases, registries,
CRI/CNI, or the issuer/trust-distribution controllers. See [security](security.md)
for the boundaries of worker transport and workload execution.

## Prerequisites

SREs provide Kubernetes storage, cert-manager, Envoy Gateway with its extension
CRDs and Gateway API `BackendTLSPolicy` v1, a GatewayClass (`flame-envoy` by
default), and a ready ClusterIssuer (`flame-client-ca`). The management namespace
must contain a ConfigMap named for every configured issuer, with its public
CA PEM in `data["ca.crt"]`. Private CA keys remain with the issuer.

Worker hosts need Linux/systemd, Python 3, logrotate, and prepared workload
runtimes, networking, and storage. Flmlet uses upstream setup to install FEM
and its systemd units, then writes configuration and restarts FEM. SREs provide
and secure CRI/CNI. Workers enable both CRI and Host shims by default
(`cri,host`); Host workloads run as root. Use `setup.sh --join TOKEN --shims cri`
for CRI-only workers, or `--shims host` for Host-only workers.

## Install

Use a kubeconfig whose server address is reachable from clients and workers:

```sh
curl -fsSL https://xflops.io/cluster/setup.sh | sh
```

`--values FILE` is optional. `--kubeconfig FILE` selects a different kubeconfig;
otherwise kubectl's current context is used. The installer derives the UI external
IPv4, gateway host, enrollment API address, and worker frontend address from that
server endpoint. It removes the HTTPS scheme and API port for the UI address.
There is no separate UI address option or port-forward requirement.

The current native worker protocol requires an RFC 1918 frontend IPv4. If the
kubeconfig uses Tailscale or an API-only load balancer, use `--endpoint` for the
worker-reachable LAN frontend and, when necessary, `--kubernetes-endpoint` for
enrollment. These overrides do not change the UI's kubeconfig-derived address.
The API certificate must validate against Kubernetes CA trust.

The installer preflights every chart and configured GatewayClass/issuer. It
creates the managed Flame namespaces and copies only the issuers' public CA
ConfigMap data into them before installing workloads. No client private keys or
join Secrets are copied across namespaces. Readiness is checked for each Flame.

## Chart configuration

| Chart | Responsibility |
| --- | --- |
| `charts/flame-operator` | Namespaced Flame resources, certificates, gateways, and worker enrollment. |
| `charts/flame-cluster` | The always-enabled management service and an ordered list of Flame configurations. |
| `charts/flame-ui` | The UI and same-origin API proxy. |

The management Service and Deployment are named `cluster`; its binary and image
are `flame-cluster` and `docker.io/xflops/flame-cluster`. Backend settings live
under `cluster`. The previous `api` group and backend enable/disable switch are
removed. Flame configurations live in the `flames` list:

```yaml
cluster:
  image:
    repository: docker.io/xflops/flame-cluster
    tag: latest
  security:
    allowedHosts: [localhost, 127.0.0.1, "::1", "100.117.27.41"]
flames:
  - name: flame
    namespace: flame-ns-1
    gateway:
      hostname: 192.168.3.149
  - name: flame
    namespace: flame-ns-2
```

Names receive a one-based index: this example creates `flame-ns-1/flame-1` and
`flame-ns-2/flame-2`. Omitting namespace derives `<name>-ns-<index>`; duplicate
namespaces are rejected. Partial entries inherit resource defaults. Additional
entries inherit omitted gateway host, frontend host, and Kubernetes endpoint
from the first entry; explicit per-entry values are retained.

Each entry supports `version`, `image`, `endpoint`, `endpointHost`,
`kubernetesEndpoint`, `rolloutRevision`, `nodePort`, `storageSize`, `storageClass`,
`existingClaim`, `resources`, `nodeSelector`, `cacheEndpoints`, and `gateway`.
`endpointHost` combines a frontend IPv4 with `nodePort`; explicit `endpoint`
takes precedence. `cacheEndpoints` lists SRE-managed `{name,address,cachePort,ready}`
locations in addition to healthy, current same-Flame worker caches.

For an external FLM, set `flames: []` and provide `cluster.flame.endpoint` and
its TLS settings. The management service still runs. Optional standalone join
files come from `cluster.join.secretName`; this mode does not need Kubernetes
Secret access.

## Per-namespace resources and credentials

Each Flame namespace contains its FlameCluster, enrolled FlameWorkers, server
and client certificates, generated client bundle, join Secret, workload Services,
and enrollment RBAC. A default first worker joins `flame-ns-1/flame-1` using the
namespace encoded in its selected join token. Its Kubernetes identity can read
only its named worker and FlameCluster and patch its own status in that namespace.
It cannot read client Secrets or objects in another namespace.

Client URI SANs bind namespace, Flame name, and object UID. A client certificate
for one Flame cannot authenticate to another Flame's gateway, even when both
use the same issuer. The `<flame-name>-client` Secret contains standalone
`flame.yaml` plus CA, certificate, and private-key files. The requested client
certificate lifetime is one year (`8760h`), renewing 30 days (`720h`) before
expiry. The issuer must allow that lifetime. Mounted bundles follow renewal;
exported copies need a separate refresh before expiry.

The management API gets `get`/`list` on FlameWorkers in each configured
namespace, and `get` on that namespace's exact
`<flame-name>-join` and `<flame-name>-client` Secrets. The first supplies join
commands; the second supplies the selected `flame.yaml` download. It cannot list
or modify Secrets, or read unrelated TLS server credentials. Its ordered
connection catalog is explicit configuration, not namespace or workload discovery. The central management UI/API is trusted
administrative access across the configured Flames; it does not authenticate
end users or implement tenant authorization.

## UI and direct access

Open `http://<kubeconfig-server-ip>/`. The selector shows namespaces only, such
as `flame-ns-1` and `flame-ns-2`. `/` opens the first configured Flame directly.
Selecting another namespace switches resources, session actions, join
credentials, and client downloads together.

Nodes reads FlameWorker resources in the selected namespace, filtered by the
selected Flame's `spec.clusterRef`. It shows the worker's identity, declared
address, and reported status even before FLM registration. Readiness is Pending,
Ready, Not ready, Disabled, or Deleting; no FLM node fallback or invented machine
capacity is used. Applications, sessions, executors, and tasks still read the
selected FLM. Worker-list failures are reported as errors, not healthy empty
inventories. There is no aggregate page or All clusters option.

Compact density is the default. Navigation is Nodes → Executors → Applications
→ Sessions → Settings. Settings shows the selected connection and its current
join command and expiration, plus **Download flame.yaml** for the selected
namespace. The join command fetches the hosted installer and runs it directly:

```sh
curl -fsSL https://xflops.io/cluster/setup.sh | sudo sh -s -- --join '<token>'
```

Copy the actual command from Settings onto the prepared worker host; no local
installer or kubectl token lookup is needed. The download reads only that
connection's configured `<flame-name>-client` Secret in its namespace and returns
the `flame.yaml` entry as an attachment with `Cache-Control: no-store`. It
contains client credentials; store it privately and replace exported copies
when renewed. An unavailable bundle produces an error, without exposing other
Secret entries. The management API does not discover or accept arbitrary Secret
names from the download request.

The UI proxies to `cluster.<management-namespace>.svc:3000`. The backend talks
to each FLM through its in-cluster session-manager Service for workload
resources; Nodes uses the namespace-scoped Kubernetes FlameWorker API. Neither
path requires Tailscale or a NodePort for in-cluster reads. Tailscale may carry browser and administrative access to k3s.

Setup configures the UI Service's `service.externalIPs` and
`cluster.security.allowedHosts` from the kubeconfig endpoint together. For manual
Helm deployment, configure those fields with the node-owned address you intend
to expose. Add any UI/SSO hostname to the host allowlist and restrict access
through trusted tailnet policy or an authentication proxy. Change
`cluster.fullnameOverride` and `flame-ui.consoleUpstream` together if renaming
the management Service.

## Ports and TLS

For the first Flame, native frontend/backend NodePorts are `30080`/`30081`;
gateway session/cache ports are `30443`/`31443`. Later entries offset each base
by ten: the second uses `30090`/`30091` and `30453`/`31453`. Explicit per-entry
ports override these defaults. Rendering rejects collisions and ports outside
the NodePort range across all configurations.

Envoy uses a NodePort Service with `externalTrafficPolicy: Cluster`, without
host networking or host-port binding. A DNS gateway uses DNS certificate SANs
and hostname constraints; an IPv4 gateway uses IP SANs and routes without DNS
hostname constraints. Ensure the selected node addresses accept NodePorts;
some k3s configurations restrict them to the primary LAN address.

Native FLM frontend/backend use server-authenticated TLS. Workers verify the
advertised IP against validated public CA trust from FlameCluster status;
Envoy verifies its upstream through BackendTLSPolicy. Server private keys stay
in the Flame namespace. Server certificates retain their 24-hour lifetime and
renew eight hours before expiry. Native worker transport does not provide
client-certificate authentication; SRE network restrictions remain necessary.

The backend supports `cluster.flame.tls` for standalone HTTP/HTTPS/mTLS endpoints.
For managed Flames, it mounts only public CA trust and obtains join tokens using
its exact scoped Secret permissions. Kubernetes Secret updates are read on
demand, and credential responses use `Cache-Control: no-store`.

## Worker lifecycle

Join tokens renew every 15 minutes and are exchanged through the Kubernetes
HTTPS Service proxy for a durable worker-scoped identity. Re-enrollment also
requires the host's private enrollment key. The namespace in the token selects
the Flame; explicit worker namespace/name checks reject mismatches.

Flmlet installs FEM through the upstream checksum-verified package's bundled
`setup.sh --join`, with systemd installation enabled. It uses the upstream
`FLAME_SYSTEMCTL` hook to defer service activation until installation finishes.
Flmlet then writes `PREFIX/conf/flame-cluster.yaml` and restarts the requested
services with that configuration. It preserves the upstream unit definitions,
escaping literal percent signs in installation paths when needed, and manages
worker-specific overrides in systemd drop-ins.
The object cache is stopped and disabled when it is not requested.
`--check` remains read-only. `--configure` performs installation, configuration,
and the required service restart before returning.

A pinned version mismatch installs the selected runtime before reconfiguration.
`latest` installs a missing runtime but does not poll for newer releases.
`rolloutRevision` restarts managed services. Disable/deletion revokes the
worker's credentials and stops its owned services; transient observation errors
retain healthy services. Foreign units or configuration require explicit adoption.

## Upgrades and uninstall

Apply updated Flame CRDs before upgrading the operator chart; Helm does not
upgrade existing files in `crds/`:

```sh
kubectl apply --server-side -f charts/flame-operator/crds/
helm upgrade flame-operator charts/flame-operator --namespace flame-system
```

When updating a field inside `flames`, pass the complete configured list. Helm
replaces lists during values merging, so `--reuse-values` with only a partial
`--set flames[0]...` entry loses the other fields and Flames. For a rollout:

```sh
helm get values flame-cluster --namespace flame-system -o yaml > flame-values.yaml
helm upgrade flame-cluster charts/flame-cluster --namespace flame-system \
  --values flame-values.yaml --set 'flames[0].rolloutRevision=restart-1'
```

Changing a Flame's namespace creates a separate identity boundary. Existing
workers and exported clients must join/use the new namespace; their identities
are not migrated or shared automatically.

Namespaces have `helm.sh/resource-policy: keep`, and the operator retains session
PVCs. Removing a Flame removes its managed control-plane resources and revokes
its worker credentials, while preserving namespace/storage for SRE cleanup.
Uninstall the UI independently; the management service belongs to the combined
cluster release. Back up data before deleting namespaces or retained storage.
