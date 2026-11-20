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
- Host Flame binaries and any external object-cache endpoints.

The operator requires access to its Flame resources and objects it owns. Its Helm
chart installs the FlameCluster/FlameWorker CRDs and controller RBAC. The
controller creates worker-scoped service accounts and Roles for enrollment;
it does not install infrastructure or dependency controllers.

## Charts

| Chart | Important values |
| --- | --- |
| `charts/flame-operator` | `image.repository`, `image.tag`, `serviceAccount`, `rbac.create` |
| `charts/flame-cluster` | `nameOverride`, `version`, `image`, `endpointHost`, `endpoint`, `kubernetesEndpoint`, `nodePort`, `gateway`, `storageClass`, `cacheEndpoints` |
| `charts/flame-worker` | `clusterRef`, `address`, `endpoint`, `shims`, `maxExecutors`, `pythonVersion`, `manageObjectCache` |
| `charts/flame-console` | `flame.endpoint` (required), `flame.name`, `flame.tls`, `join.secretName`, `image` |
| `charts/flame-ui` | `consoleUpstream`, `image`, `service` |

Set `fullnameOverride` to change a workload's Service name. Update
`flame-ui.consoleUpstream` accordingly if the console's Service name changes.
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
`FlameCluster.status.controlPlaneCa`. The console mounts only the issuer's
public CA ConfigMap. Server private keys stay in the control-plane Secret.
Certificate renewal restarts the session manager with the new identity.
Native client-certificate authentication is future Flame work; network isolation
and access controls remain necessary.

SREs install Flame binaries and Python on a Linux host with systemd. Setup
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
`manageObjectCache` defaults to false. A selected release mismatch reports
NotReady until the SRE installs that Flame version; no registry polling or
automatic binary upgrades occur. Changing worker `rolloutRevision` restarts
the services; unchanged configuration does not restart healthy processes.

## Direct console TLS

The console supports HTTP for trusted in-cluster FLM connections and HTTPS/mTLS
for protected endpoints. Credentials come from an existing SRE-managed Secret;
the chart does not create credentials.

```sh
helm upgrade --install flame-console charts/flame-console --namespace flame-system \
  --set flame.endpoint=https://flame.example.com:30443 \
  --set join.secretName=flame-join \
  --set flame.tls.secretName=flame-console-client \
  --set flame.tls.caKey=ca.crt \
  --set flame.tls.certKey=tls.crt --set flame.tls.keyKey=tls.key
```

Only endpoint/display information is exposed through `/api/connection`.
Settings requests `/api/join` to obtain the current join command and expiration.
The optional `join.secretName` mounts the operator-issued `<cluster>-join` Secret
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

Uninstall console/UI independently with Helm. Uninstalling the operator leaves
FlameCluster CRDs/resources for SRE-managed cleanup. Deleting a FlameCluster
removes controller-owned resources; retain or back up persistent data according
to the StorageClass/PV reclaim policy before deleting a cluster or its Helm release.
