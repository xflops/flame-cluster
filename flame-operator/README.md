# flame-operator

The Rust controller reconciles namespaced `flame.xflops.io/v1alpha1`
`FlameCluster` resources. Install it with the `charts/flame-operator` Helm chart.
It does not install Kubernetes, dependency controllers, registries, or worker binaries.

SREs prepare cert-manager, Envoy Gateway with the Gateway API and Envoy CRDs,
a GatewayClass, a ClusterIssuer, storage, and ingress networking. With client
mTLS enabled, each FlameCluster namespace also needs a ConfigMap named after
`gateway.issuerName` containing the issuer trust bundle in `ca.crt`. These
resources are referenced by the controller and remain under SRE ownership.
The default GatewayClass is `flame-envoy`, issuer is `flame-client-ca`, and Envoy
controller namespace is `envoy-gateway-system`; set the gateway fields to match
the installation.

For each cluster, the operator manages one session-manager Deployment, a
retained PVC, configuration, a NodePort Service, external cache Services and
EndpointSlices, cert-manager Certificates, EnvoyProxy, Gateway, HTTPRoutes, and
client identity policies. It registers hosts through rotating join tokens and
issues worker credentials. SREs prepare the host runtime; `setup.sh --join TOKEN`
creates a namespaced `FlameWorker` and generates FEM configuration. Copy the
current command and expiration from the UI's **Settings → Join a node** section.
The local
`flmlet` reconciles host systemd services and reports actual service and Flame
registration health.

`spec.version` selects the session-manager release and defaults to `latest`.
An explicit `spec.image` overrides the default image. References are used
directly: reconciliation never polls registries or advances mutable image tags.
Change the version, image, or `rolloutRevision` to select an upgrade/restart.
The current selection is reported in `status.resolvedVersions`.

`spec.cacheEndpoints` explicitly lists externally managed cache locations:

```yaml
cacheEndpoints:
  - name: worker-01
    address: 192.168.3.8
    cachePort: 9090
    ready: true
```

The list defaults to empty. Each address/port must be a unique RFC 1918 IPv4
endpoint. Ready endpoints serve bootstrap uploads; exact
`x-flame-object-cache` header routes reach the named cache for subsequent reads.
SREs update the list and readiness flags when worker topology changes.
The controller also merges matching `FlameWorker` addresses from the same
namespace. Disabled/deleting workers are excluded. Worker-derived endpoints are
ready only when cache health and the current-generation Ready condition are true
and the status heartbeat is at most 120 seconds old. Missing/stale reports leave
the endpoint unready; they never stop a host service. Names and address/port pairs
must be unique across both sources.
Cluster readiness concerns the managed Deployment, Certificates, Gateway and
identity policies, and does not wait for external worker heartbeats.

The frontend NodePort defaults to 30080; worker backend traffic uses NodePort + 1.
Both ports use native server-authenticated TLS. The controller issues the server
certificate and distributes only validated public CA certificates through
`status.controlPlaneCa`. Native Flame mTLS remains future work.
An optional `spec.endpoint: ip:port` selects an RFC 1918 LAN address and NodePort
and publishes frontend/backend endpoints in status. SREs choose which routable
Kubernetes node workers connect to; the operator does not discover or manage nodes.
The default `sh setup.sh` flow derives this address and `spec.kubernetesEndpoint`
from the active kubeconfig, without requiring a values file. The Helm chart's
`endpointHost` combines that IPv4 address with `nodePort` unless `endpoint` is
explicitly supplied.

`gateway.hostname` accepts a DNS hostname or IPv4 address; rendering and
reconciliation do not resolve it through DNS. DNS hostnames use DNS certificate
SANs and hostname-constrained listeners/routes. IPv4 addresses use IP SANs and
listeners/routes without hostname constraints. Clients must reach the selected
Kubernetes node address. Optional `gateway.ingressIp` records an advertised IPv4 address
in status and never binds Envoy to a host interface. Envoy uses a NodePort
Service with `externalTrafficPolicy: Cluster`, so any reachable node can forward
to Envoy. `nodeSelector` is inherited from the session manager.
`gateway.sessionPort` and `gateway.cachePort` default to 30443 and 31443 and
select both the TLS listener and its public NodePort. They must be distinct
values within 30000..32767 and must not overlap either session-manager NodePort.
All four ports must be unique across FlameClusters, regardless of hostname/IP.
`gateway.serviceAnnotations` applies to the NodePort Service.

The controller generates `<cluster>-client` with a standalone `flame.yaml` and
CA/certificate/private-key files after cert-manager issues the client identity.
The Secret name is reported in `status.clientSecret` and refreshed on renewal.
The client certificate lasts one year (`8760h`), renewing 30 days (`720h`) before
expiry, subject to the issuer honoring that lifetime. Exported `flame.yaml`
copies must be refreshed separately. The native FLM server certificate retains
its 24-hour lifetime and renews eight hours before expiry.
Treat it as a credential and distribute it through SRE-managed access controls.
Client URI SANs bind namespace, name and object UID; a certificate for a different
FlameCluster cannot authenticate even when both use the same trusted issuer.

Set `spec.kubernetesEndpoint` to the externally reachable HTTPS Kubernetes API
address to enable worker enrollment. The operator uses the mounted API CA and
rotates `<cluster>-join` every 15 minutes using Kubernetes TokenRequest. Its
`token` key is an opaque installer credential, and `expiresAt` records its
expiry. The bootstrap identity can only POST to this operator's named Service
proxy through Kubernetes HTTPS; it cannot read Secrets or create resources.
The join handler creates a FlameWorker and binds it to a host-generated private
enrollment key. Retries must present the same key to retrieve that worker's
credentials. Existing workers cannot be claimed with a fresh cluster token.

Joined workers receive a durable service-account identity with only `get` on
their named FlameWorker and FlameCluster, and `patch` on their named worker
status. No kubeconfig supplied by an SRE is needed. Cluster-token renewal does
not replace those credentials or restart FEM. Disabling/deleting a Worker
revokes its identity; flmlet stops its owned services on explicit authorization
failure and preserves healthy services through transient API/network errors.
The operator's `OPERATOR_NAMESPACE` and `OPERATOR_SERVICE_NAME` identify the
join Service. Cross-namespace bootstrap proxy Roles/RoleBindings use owner-UID
labels and the cluster finalizer for cleanup.

The controller runs one replica with Recreate strategy and has no leader election.
Unmanaged resources require explicit `adoptExisting: true`; foreign controller
UIDs are never taken over. PVCs are retained on cluster deletion. Cross-namespace
Envoy egress policies use owner-UID labels and a finalizer instead of invalid
cross-namespace owner references.

The shared `flame-api` crate owns resource contracts, validation, and CRD
generation. The controller owns resource rendering and reconciliation.

Generate CRDs or preview managed resources without installing them:

```sh
cargo run -p flame-api --bin crdgen -- flame
cargo run -p flame-api --bin crdgen -- worker
cargo run -p flame-operator --bin render -- examples/flamecluster.yaml
```

Rendering works offline without resolving the ingress hostname. The example
UID in a preview is replaced with the real API object UID when the controller
reconciles it.
