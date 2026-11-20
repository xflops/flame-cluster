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
- Routable Kubernetes node addresses, DNS for the selected gateway hostname,
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
| `charts/flame-cluster` | `nameOverride`, `version`, `image`, `rolloutRevision`, `gateway`, `storageClass`, `cacheEndpoints` |
| `charts/flame-worker` | `clusterRef`, `address`, `endpoint`, `shims`, `maxExecutors`, `pythonVersion`, `manageObjectCache` |
| `charts/flame-console` | `flame.endpoint` (required), `flame.name`, `flame.tls`, `image` |
| `charts/flame-ui` | `consoleUpstream`, `image`, `service` |

Set `fullnameOverride` to change a workload's Service name. Update
`flame-ui.consoleUpstream` accordingly if the console's Service name changes.
Use SRE-managed ingress/gateway resources to expose the UI; it serves the console
at `/`, with no separate dashboard.

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

SREs map `gateway.hostname` in DNS to their chosen routable node address and
allow traffic to both NodePorts. The hostname remains the TLS/SNI identity; the
operator does not resolve it during rendering or reconciliation.
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

SREs install Flame binaries and Python on a Linux host with systemd. Set
`FlameCluster.spec.endpoint` to the worker-reachable frontend `LAN-IP:NodePort`
and `spec.kubernetesEndpoint` to the externally reachable Kubernetes HTTPS API
address. The API certificate must validate against the cluster CA. The operator
does not discover a public Kubernetes endpoint or disable TLS verification.

The operator issues a cluster join token immediately and renews it every 15
minutes. It exists before any FlameWorker does. Retrieve the current token and
run `setup.sh --join <token>` on the host:

```sh
umask 077
kubectl -n flame-system get secret flame-join \
  -o jsonpath='{.data.token}' | base64 --decode > join-token.txt
# Transfer this private token file to the host, then run there:
sudo sh setup.sh --join "$(cat join-token.txt)"
```

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
readiness. Wait for the join Secret before exporting it; the Secret also
records the token's expiration.

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
  --set flame.tls.secretName=flame-console-client \
  --set flame.tls.caKey=ca.crt \
  --set flame.tls.certKey=tls.crt --set flame.tls.keyKey=tls.key
```

Only endpoint/display information is exposed through `/api/connection`. Private
keys and Kubernetes Secret content are never returned. The console has no
Kubernetes access and does not discover clusters.

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
