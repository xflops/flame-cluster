# Flame Cluster

Rust operator, host worker service, management API, and web UI for Flame clusters.
The Rust implementation replaces the earlier Go operator.

Read the [security boundaries](docs/security.md) before deployment. Workers enable
both CRI and Host shims by default; Host tasks run as root. Runtime selection is
configurable, and worker-network isolation remains an SRE responsibility.

| Component | Purpose |
| --- | --- |
| `flame-api` | Own FlameCluster/FlameWorker definitions, validation, and CRD generation. |
| [flame-operator](flame-operator/README.md) | Reconcile Flame session-manager storage, services, certificates and gateway routes. |
| [flmlet](flmlet/README.md) | Install FEM through upstream Flame setup, configure it, and reconcile host services. |
| [flame-cluster](flame-cluster/README.md) | List namespaced FlameWorkers as Nodes; read applications, sessions, executors and tasks from the selected FLM; forward session actions. |
| [flame-ui](flame-ui/README.md) | Serve the Flame console at `/`. |

There is no platform operator, logical manager, Terraform, Ansible,
registry provisioning, or dependency installer. SREs provision Kubernetes, workers,
storage, cert-manager, Envoy Gateway/Gateway API, the GatewayClass and certificate
issuer/trust configuration separately. The Helm charts have no dependencies.

## Install

The installer uses Helm for the control plane in an existing Kubernetes cluster:

```sh
curl -fsSL https://xflops.io/cluster/setup.sh | sh
```

Setup uses the active kubeconfig server for Kubernetes access and the
session-manager/cache NodePort host. Defaults require no values file; `--values`
is optional. SREs prepare the dependencies described below. The operator generates
a cluster join token and renews it every 15 minutes. Open the UI at `/`, select
a namespace in the selector, open **Settings**, and copy the **Join a node**
command. Run it on the prepared worker host; it fetches the hosted installer:

```sh
curl -fsSL https://xflops.io/cluster/setup.sh | sudo sh -s -- --join '<token>'
```

Use the actual command from Settings; no local installer or `kubectl` token
lookup is needed. Settings also offers **Download flame.yaml** for the selected
namespace. The downloaded bundle contains its client certificate and private
key; keep the file private. Flmlet runs
upstream `flame/setup.sh --join` to install FEM and its systemd units, then writes
the cluster config and restarts FEM with that configuration.

The installer creates the FlameWorker and generates
`conf/flame-cluster.yaml` for FEM. No worker CR is needed in advance.
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

The management Service and Deployment are `cluster`; its image is
`docker.io/xflops/flame-cluster`. The combined `flame-cluster` chart always runs
that service. Its settings live under `cluster`, and `flames` is an ordered list
of Flame configurations. The default first resource is `flame-1/flame-1`;
further entries receive indexed names and separate namespaces and ports.

Each namespace owns its FlameCluster, FlameWorkers, client credentials, and join
Secret. Worker permissions and gateway client identities are scoped to that
Flame. Nodes lists FlameWorker resources from the selected namespace and Flame,
including workers that have not registered with FLM yet. Applications, sessions,
executors, and tasks continue to come from the selected FLM. The management API
may get/list FlameWorkers in configured namespaces and read only the configured
exact join and client-bundle Secrets, enabling administrative join commands and
client downloads. It cannot list or modify Secrets, or read unrelated server
private keys. Restrict UI/API access to trusted administrators.

Open `http://<kubeconfig-server-ip>/`. Setup derives the UI external IP and host
allowlist directly from kubeconfig, without a UI-address flag or port-forward.
The selector shows namespaces only and opens the first configured Flame by
default; it has no All clusters option or cross-cluster summary. `/` opens that
cluster's Dashboard with node readiness and reported capacity totals. A separate
section shows current CPU, memory, and GPU usage for each worker. Navigation is
Dashboard → Nodes → Executors → Applications → Sessions → Settings, with Compact
density throughout.

The generated `flame.yaml` client credential lasts one year, with renewal 30 days
before expiry. Exported copies need a separate refresh. Node join tokens keep
their separate 15-minute rotation. The backend connects directly to in-cluster
FLM Services; only remote browser/administrative traffic uses Tailscale.
See [deployment and configuration](docs/deployment.md) for multiple Flames,
namespace isolation, TLS, external FLMs, CRD upgrades, and storage retention.

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
