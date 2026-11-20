# Helm deployment and SRE responsibilities

See [security boundaries](security.md) for raw worker-port exposure, CRI/Host
runtime trust, administrative console access and enrollment credentials.

The charts install only Flame-owned workloads and resources. They do not install
Kubernetes, workers, CRI/CNI, databases, registry infrastructure, cert-manager,
Envoy Gateway, Gateway API CRDs, a GatewayClass, an issuer, or a trust-distribution
controller.

## Prerequisites

SREs provide:

- Kubernetes with persistent storage for the session manager.
- cert-manager and a usable ClusterIssuer (default `flame-client-ca`).
- Envoy Gateway, its extension CRDs, standard Gateway API CRDs and a GatewayClass
  using `gateway.envoyproxy.io/gatewayclass-controller` (default `flame-envoy`).
  Gateway API must serve `BackendTLSPolicy` at `gateway.networking.k8s.io/v1`.
- A ConfigMap in each FlameCluster namespace named for the configured issuer,
  with the trusted CA PEM under `ca.crt`, for gateway client authentication.
- Routable Kubernetes node addresses, DNS when using a gateway DNS hostname,
  and firewall access to the configured NodePorts.
- Linux/systemd worker hosts, prepared CRI/networking, and any external cache endpoints.

The operator requires access to its Flame resources and objects it owns. Its Helm
chart installs the FlameCluster/FlameWorker CRDs and controller RBAC. The
controller creates worker-scoped service accounts and Roles for enrollment;
it does not install infrastructure or dependency controllers.

## Charts

| Chart | Important values |
| --- | --- |
| `charts/flame-operator` | `image.repository`, `image.tag`, `serviceAccount`, `rbac.create` |
| `charts/flame-cluster` | `nameOverride`, `version`, `image`, `endpointHost`, `endpoint`, `kubernetesEndpoint`, `nodePort`, `gateway`, `storageClass`, `cacheEndpoints`, `cluster.enabled`, `api` |
| `charts/flame-ui` | `consoleUpstream`, `image`, `service` |

The `flame-cluster` chart includes the FlameCluster resource and REST API in one
release. The API uses image `docker.io/xflops/flame-cluster` and Service
`cluster`; configure it under `api`, including `api.image`,
`api.security.allowedHosts`, `api.flame`, and `api.join`. Its default connection,
CA trust, and join Secret follow the configured cluster name, namespace, and
issuer. Set `api.enabled=false` for a resource-only installation, or
`cluster.enabled=false` and `api.flame.endpoint` for an API-only installation
against an existing FLM.

Set `api.fullnameOverride` to change the API Service name, and update
`flame-ui.consoleUpstream` accordingly. The UI chart's default upstream is
`cluster:3000`.
Use SRE-managed ingress/gateway resources to expose the UI; it serves the console
at `/`, opening the configured cluster's Applications page directly. The cluster
selector contains only the configured cluster, with no aggregate option.
Navigation is Nodes → Executors → Applications → Sessions → Settings, using
Compact density throughout. Settings shows connection details and the current
node join command with a copy button and expiration.

`FlameCluster.spec.version` selects the default session-manager image tag.
An explicit custom image tag/digest is used unchanged. No controller polls
registries to follow moving tags. Change `rolloutRevision` to explicitly restart
the session manager through a Helm upgrade. `cacheEndpoints` is an optional list of
`{name,address,cachePort,ready}` for externally managed IPv4 object caches.
The operator merges these with same-cluster FlameWorker cache addresses whose
status is current and healthy. Cluster readiness covers the control plane;
each worker reports its own readiness separately.

## Direct Tailscale access

Setup exposes the UI directly on the kubeconfig server's IPv4 by default,
stripping the Kubernetes API port. It sets `service.externalIPs` and the API host
allowlist together. If kubeconfig uses the Tailscale address, no override is
needed. If kubeconfig uses a LAN address, select Tailscale explicitly:

```sh
sh setup.sh --ui-address 100.117.27.41
```

For a manual Helm installation with manager address `100.117.27.41`:

```sh
helm upgrade flame-cluster charts/flame-cluster --namespace flame-system \
  --reuse-values \
  --set-json 'api.security.allowedHosts=["localhost","127.0.0.1","::1","100.117.27.41"]'
helm upgrade flame-ui charts/flame-ui --namespace flame-system --reuse-values \
  --set 'service.externalIPs[0]=100.117.27.41'
```

Open `http://100.117.27.41/` from a device on the tailnet. The UI Service listens
on port 80 through Kubernetes Service routing; the address must belong to the
node and be reachable through Tailscale. No SSH tunnel or local listener is
required. Restrict access with your tailnet policy or authentication proxy.
This also works when k3s limits NodePort addresses to its primary LAN IP.

## Gateway NodePorts

The Envoy data-plane Service uses `NodePort` with `externalTrafficPolicy: Cluster`.
`gateway.sessionPort` defaults to `30443` and `gateway.cachePort` to `31443`;
each value is used for its Gateway listener, Service port and NodePort. Clients
can reach those ports on any routable Kubernetes node, including nodes without
an Envoy pod. The configured `nodeSelector` still controls Envoy pod placement.
No host networking, host ports, or binding to a particular node IP is required.

`sh setup.sh` uses the active kubeconfig server host for both gateway NodePorts
and its full HTTPS URL for worker enrollment. It needs no hostname flags or
values file. Session traffic defaults to port `30443`, cache traffic to `31443`,
and the native worker frontend/backend to `30080`/`30081`. `--kubeconfig FILE`
selects another kubeconfig; otherwise kubectl's current context is used.
The installer requires Helm, kubectl, and Python 3, along with the SRE
prerequisites above. A minimal installation is:

```sh
curl -fsSLo setup.sh https://xflops.io/cluster/setup.sh
sh setup.sh
```

For manual Helm installations, `endpointHost` combines a worker-reachable IPv4
address with the chart's `nodePort`; an explicit `endpoint` takes precedence.
Set `kubernetesEndpoint` to enable enrollment. Helm alone does not read kubeconfig
to populate these values.

An IPv4 gateway uses an IP certificate SAN and routes without DNS hostname
constraints. A DNS gateway uses its hostname for TLS/SNI; the operator does not
resolve it during rendering or reconciliation. Allow traffic to the NodePorts.
`--hostname`, `--endpoint`, and `--kubernetes-endpoint` remain optional overrides.
Use `--values FILE` only when overriding the default chart configuration.
`gateway.ingressIp` is optional advertised-address metadata only. It does not
select an Envoy node or bind an address, and can be omitted when SRE DNS provides
the mapping.

Every NodePort must be unique across the Kubernetes cluster and must lie in its
configured NodePort range. Reserve the session-manager frontend/backend pair
(`nodePort`, default `30080`, and `nodePort + 1`, default `30081`) as well as both
Envoy ports. Assign different values to additional FlameClusters and avoid
collisions with other Kubernetes Services:

```yaml
nodePort: 30080
gateway:
  hostname: flame.example.com
  sessionPort: 30443
  cachePort: 31443
```

## Host workers

The session manager serves native TLS on both its frontend and backend. The
operator issues a server certificate for its Service DNS names and declared
worker endpoint IPs. Envoy verifies the upstream through BackendTLSPolicy;
workers verify the advertised IP against validated public CA trust from
`FlameCluster.status.controlPlaneCa`. The API mounts the issuer's public CA ConfigMap and the optional join Secret. Server private keys stay in the control-plane Secret.
Certificate renewal restarts the session manager with the new identity.
Native client-certificate authentication is future Flame work; network isolation
and access controls remain necessary.

SREs prepare Linux/systemd hosts and the selected workload runtime/networking.
Flmlet installs FEM through upstream `flame/setup.sh --join --no-systemd`, then
writes cluster configuration and starts FEM. Setup
resolves the kubeconfig server host to an IPv4 address for the native worker
frontend and combines it with `nodePort` from the default or optional values file.
The current worker protocol requires an RFC 1918 address. If the kubeconfig
address is an API-only load balancer, use the optional endpoint overrides for
worker-reachable node addresses. The Kubernetes API certificate must validate
against the cluster CA; TLS verification stays enabled.

The generated `<cluster>-client` Secret contains a standalone `flame.yaml` with
an embedded TLS client certificate and private key for session/cache gateway
access. Its requested lifetime is one year (`8760h`), with renewal 30 days
(`720h`) before expiry. The issuer must allow that lifetime. Mounted bundles
follow renewal; refresh exported copies separately before expiration. This
client credential is separate from the short-lived node join token below.

The operator issues a cluster join token immediately and renews it every 15
minutes. It exists before any FlameWorker does. Open **Settings → Join a node**
in the UI, copy the command, and run it on the prepared host with `setup.sh`
available. Settings shows the token expiration; refresh to obtain the latest
command before joining. The cluster installer configures this automatically.
Worker enrollment creates the FlameWorker resource; no worker Helm chart or
separate worker release is required.

The installer registers the host through the operator, creates its FlameWorker,
and generates `conf/flmlet/flame-cluster.yaml` for FEM. Hostname and LAN address
are detected automatically; `--worker` and `--address` override them. Use
`--manage-object-cache` when flmlet should also manage the installed object cache.
`--join --token-file FILE` is an alternate way to supply the token.

Bootstrap requests use the existing Kubernetes HTTPS API and its Service proxy
to reach the operator. The bootstrap credential grants enrollment access only.
The operator exchanges it for a separate, persistent worker credential that
can read that worker and its referenced cluster and patch only its own status.
Renewing the join token does not interrupt joined workers. Rejoining the same
host uses its private enrollment key, so a join token alone cannot claim an
existing worker. Deleting or disabling a worker revokes its credential.
Explicit authorization rejection stops its owned services; temporary network
failures retain the last applied configuration.

An omitted `kubernetesEndpoint` skips enrollment without changing control-plane
readiness. Settings shows the join command once the operator has issued the
join Secret.

`flmlet` writes dedicated configuration and manages only its own FEM and optional
object-cache units. Foreign services require an explicit adoption flag.
`manageObjectCache` defaults to false. A selected release mismatch triggers
an installation of the selected version before configuration is applied and
services are started. The `latest` selection does not poll for newer builds.
Changing worker `rolloutRevision` restarts
the services; unchanged configuration does not restart healthy processes.

## Direct console TLS

The console supports HTTP for trusted in-cluster FLM connections and HTTPS/mTLS
for protected endpoints. Credentials come from an existing SRE-managed Secret;
the chart does not create credentials.

```sh
helm upgrade --install flame-cluster charts/flame-cluster --namespace flame-system \
  --set cluster.enabled=false \
  --set api.flame.endpoint=https://flame.example.com:30443 \
  --set api.join.secretName=flame-join \
  --set api.flame.tls.secretName=flame-cluster-client \
  --set api.flame.tls.caKey=ca.crt \
  --set api.flame.tls.certKey=tls.crt --set api.flame.tls.keyKey=tls.key
```

Only endpoint/display information is exposed through `/api/connection`.
Settings requests `/api/join` to obtain the current join command and expiration.
The optional `api.join.secretName` mounts the operator-issued `<cluster>-join` Secret
from the console namespace, reading its `token` and `expiresAt` keys on each
request so projected Secret updates take effect without restarting. An absent
or expired token is unavailable until a current token is projected. Standalone
console processes can set `FLAME_JOIN_TOKEN_FILE` and `FLAME_JOIN_EXPIRY_FILE`
to equivalent files. No Kubernetes API client or RBAC is needed.

Serve the UI and API behind your trusted SSO ingress: anyone authorized to read
Settings can copy the enrollment credential. TLS private keys are never
returned. The console does not discover clusters.

## Upgrades and uninstall

Helm installs files in `crds/` on first install but does not upgrade existing
CRDs. Apply the generated schema before upgrading the operator chart:

```sh
kubectl apply --server-side -f charts/flame-operator/crds/
helm upgrade flame-operator charts/flame-operator --namespace flame-system
```

The Rust FlameCluster API differs from the earlier Go controller's nested
session/executor-manager configuration. Convert existing Go resources to the
flat schema shown in `examples/flamecluster.yaml` before applying the new CRD.
Existing installations are managed separately by SREs.

Uninstall the UI independently with Helm. The API belongs to the cluster chart
release; disable it with `api.enabled=false` to remove it without uninstalling
the FlameCluster resource. Uninstalling the operator leaves
FlameCluster CRDs/resources for SRE-managed cleanup. Deleting a FlameCluster
removes controller-owned resources; retain or back up persistent data according
to the StorageClass/PV reclaim policy before deleting a cluster or its Helm release.
