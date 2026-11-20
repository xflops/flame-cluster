# Flame Cluster

Rust operator, host worker service, direct FLM console, and web UI for Flame clusters.
The Rust implementation replaces the earlier Go operator.

Read the [security boundaries](docs/security.md) before deployment. CRI runtime
selection is configurable; worker-network isolation remains an SRE responsibility.

| Component | Purpose |
| --- | --- |
| `flame-api` | Own FlameCluster/FlameWorker definitions, validation, and CRD generation. |
| [flame-operator](flame-operator/README.md) | Reconcile Flame session-manager storage, services, certificates and gateway routes. |
| [flmlet](flmlet/README.md) | Install FEM through upstream Flame setup, configure it, and reconcile host services. |
| [flame-cluster](flame-cluster/README.md) | Read nodes, applications, sessions, executors and tasks directly from a configured FLM; forward session actions. |
| [flame-ui](flame-ui/README.md) | Serve the Flame console at `/`. |

There is no platform operator, logical manager, Terraform, Ansible,
registry provisioning, or dependency installer. SREs provision Kubernetes, workers,
storage, cert-manager, Envoy Gateway/Gateway API, the GatewayClass and certificate
issuer/trust configuration separately. The Helm charts have no dependencies.

## Install

The installer uses Helm for the control plane in an existing Kubernetes cluster:

```sh
curl -fsSLo setup.sh https://xflops.io/cluster/setup.sh
sh setup.sh
```

Setup uses the active kubeconfig server for Kubernetes access and the
session-manager/cache NodePort host. Defaults require no values file; `--values`
is optional. SREs prepare the dependencies described below. The operator generates
a cluster join token and renews it every 15 minutes. Open the UI at `/`, select
**Settings**, and copy the **Join a node** command. Run it on the prepared worker
host with `setup.sh` available; no `kubectl` token lookup is needed. Flmlet runs
upstream `flame/setup.sh --join` with service startup suppressed, prepares the
cluster config, and starts FEM only after configuration succeeds.

The installer creates the FlameWorker and generates
`conf/flmlet/flame-cluster.yaml` for FEM. No worker CR is needed in advance.
Joined workers keep their own credentials when the join token renews.
See [deployment](docs/deployment.md) for host prerequisites and options.

Use a namespace and infrastructure prepared by your SRE. For the example below,
`flame-envoy` is an existing Envoy GatewayClass, `flame-client-ca` is an existing
ClusterIssuer, and a ConfigMap named `flame-client-ca` in `flame-system` contains
its trusted CA in `data["ca.crt"]`. The Envoy controller namespace is
`envoy-gateway-system`.

The Envoy gateway exposes session traffic on NodePort `30443` and cache traffic
on NodePort `31443` by default, reachable through any routable Kubernetes node.
Setup uses the kubeconfig server host for both endpoints. IPv4 hosts use IP
certificates and need no new DNS record; a DNS hostname must resolve to a
reachable node. Envoy uses a NodePort Service. Reserve unique ports across clusters, including the
session-manager pair `30080`/`30081`. All four ports are configurable in Helm
values; `gateway.ingressIp` is optional advertised-address metadata only.

For manual Helm installation, supply the addresses that setup would derive.
This example uses a Kubernetes node at `192.168.3.10`:

```sh
helm upgrade --install flame-operator charts/flame-operator \
  --namespace flame-system --create-namespace
helm upgrade --install flame charts/flame-cluster --namespace flame-system \
  --set gateway.hostname=192.168.3.10 --set endpointHost=192.168.3.10 \
  --set kubernetesEndpoint=https://192.168.3.10:6443
helm upgrade --install flame-ui charts/flame-ui --namespace flame-system
```

Setup defaults the UI external IP to the IPv4 address of the kubeconfig server
(without its API port) and adds it to the API host allowlist. To use the
manager's Tailscale IPv4 instead, pass `--ui-address <tailscale-ip>`. Open `http://<tailscale-ip>/` to view the configured cluster's
Applications page; see [direct access](docs/deployment.md#direct-tailscale-access)
for Helm commands.
The selector lists only that cluster. The Compact sidebar order is
Nodes → Executors → Applications → Sessions → Settings. Settings shows the
connection and the current node join command, including its expiration.

The generated `flame.yaml` bundle uses a one-year TLS client certificate,
renewed 30 days before expiry. Exported copies must be refreshed separately.
This lifetime is separate from the 15-minute node join token rotation.

The console talks directly to FLM and has no
Kubernetes client, service-account token or Kubernetes RBAC. For an existing FLM,
install `charts/flame-cluster` with `cluster.enabled=false` and set
`api.flame.endpoint` to that service, then install `charts/flame-ui`. The combined
cluster chart deploys the FlameCluster resource and its API by default;
`api.enabled=false` omits the API. Its image is `docker.io/xflops/flame-cluster`,
and its default Service name is `cluster`.
See [Helm values and SRE prerequisites](docs/deployment.md) for TLS, image selection,
external cache endpoints, CRD upgrades and uninstall behavior.

## Development

Rust 1.95+, Node.js LTS, and Helm are required.

```sh
bash ci/check.sh
(cd flame-ui && npm ci && npm test)
(cd flame-ui && npx playwright install chromium && npm run test:browser)
FLAME_ENDPOINT=https://192.168.10.10:30080 FLAME_CA_FILE=ca.crt cargo run -p flame-cluster
```

Build the `flame-operator`, `flame-cluster` and `flame-ui` targets in the root
Dockerfile. All image references use upstream registries; SREs choose the image
repositories/tags in Helm values and the FlameCluster resource.

CI runs unit/browser/chart checks, builds native amd64/arm64 images and worker
binaries once, then downloads those artifacts into a disposable Kind cluster
for E2E. E2E installs through `setup.sh`, including a host with systemd. Functional
checks register a worker with its operator-issued token, execute real tasks through mTLS, verify
pagination and session actions, and run live browser and `flamepy.app` tests.
Successful empty read responses are not execution coverage. CI acts as
the SRE for its own test dependencies; production charts never install them.
Only successful main runs publish the tested images, worker binaries, Helm charts
and installer. New merges cancel older
main runs. Publishing needs `DOCKER_HUB_USERNAME` and `DOCKER_HUB_PAT` configured
for the `xflops/flame-operator`, `xflops/flame-cluster`, and `xflops/flame-ui` repositories.
`XFLOPS_CLUSTER_TOKEN` is the website publishing Actions secret.
