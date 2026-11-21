# Security boundaries

## Worker transport

Envoy session/cache NodePorts `30443`/`31443` use TLS and require the configured
cluster client identity when `gateway.clientAuth` is enabled (the default).
The session-manager ports `30080`/`30081` use native server-authenticated TLS.
Reaching them still bypasses gateway client-certificate authentication: native
Flame mTLS is future work. Their NodePorts are available on every node,
including addresses other than the advertised private `spec.endpoint`.

CNI/VNI isolation is deferred. Until it is enforced, this deployment assumes
trusted worker and Kubernetes networks. TLS encrypts these connections but does
not authorize their callers. SREs must prevent untrusted clients and
workloads from reaching the raw session-manager ports, cache endpoints and
internal management Services. Advertising a private IP does not enforce this
boundary. This release does not provide tenant network isolation.

Both FLM listeners verify as the selected service DNS name or declared worker
endpoint IP. Envoy also verifies its TLS upstream through BackendTLSPolicy.
Only certificate-only public CA trust is distributed through cluster status;
the FLM server private key stays in its Kubernetes Secret. Missing or invalid
worker trust fails closed. The optional worker object cache remains a private
`grpc://` endpoint in this change.

## Workload runtimes

New workers enable both CRI and Host shims by default (`cri,host`), including
Host execution as root. The SRE supplies and secures the CRI runtime and its
network configuration; setup does not install the CRI runtime. Flmlet installs
FEM through the trusted upstream Flame installer, including its systemd units,
then writes configuration and restarts its services. Users select shims with
`setup.sh --join TOKEN --shims cri`, `--shims host`, or
`--shims cri,host`, or configure `FlameWorker.spec.shims` directly.

flmlet manages host services as root. Host-shim tasks inherit the FEM service
identity, currently root, so Host mode requires workload authors to be trusted
host administrators. Such tasks can read local worker credentials, change host
services and bypass subsequent revocation. File permissions do not isolate
credentials from root workloads. CRI isolation depends on runtime and application
configuration; allowing privileged containers or host credential mounts defeats
that isolation.

## Console access

The console/UI Services default to ClusterIP. The console provides administrative
FLM reads and session mutations; it does not authenticate users or implement
tenant authorization. Expose it only through trusted access or an SRE-managed
authentication proxy. Backend mTLS authenticates the console to FLM, not users
to the console.

`CONSOLE_ALLOWED_HOSTS` and Helm `cluster.security.allowedHosts` restrict API Host headers
to explicit hostnames or IPs. Loopback names are allowed by default for local
port forwarding. Add the actual console hostname when configuring an ingress.
This protects against DNS rebinding; it is not user authentication. Session/task
metadata can include application-provided messages, commands and URLs and should
be treated as administrative data.

## Namespace boundaries

Each managed Flame uses its own namespace for its FlameCluster, FlameWorkers,
certificates, client bundle, and join Secret. Worker Roles reference only the
named worker/FlameCluster in that namespace. Cross-namespace worker reads and
client Secret reads are forbidden. Gateway client URI SANs bind namespace, Flame
name, and UID, preventing a client identity from authenticating to another Flame.

The central management ServiceAccount receives `get`/`list` on FlameWorkers in
each configured namespace, and `get` on that namespace's exact join and
client-bundle Secrets. It cannot write workers or list them cluster-wide. The
Nodes endpoint filters workers to the selected Flame's `spec.clusterRef` and
does not fall back to FLM registration inventory. Settings intentionally exposes
the selected `flame.yaml` bundle, including its TLS client private key, to trusted
administrators. It cannot list or write Secrets, read unrelated Secrets, or
retrieve FLM server private keys. Downloads return only the bundle entry, with
attachment and `no-store` headers; Secret names and contents are not included in
public connection metadata. Connection metadata is an explicit ordered catalog,
and the UI shows namespace labels; switching namespaces changes the FLM client,
join source, and download source together. Administrative UI/API access still
spans all configured Flames; namespace scoping is not an end-user login/authorization system.

These resource and credential boundaries do not change the native worker-port
network boundary described above. SREs must still restrict raw NodePort and
worker-cache traffic appropriately.

## Enrollment credentials

The `flame.yaml` client bundle embeds a TLS client certificate and private key.
The operator requests a one-year (`8760h`) client certificate and renewal thirty
days (`720h`) before expiry. The SRE-managed issuer must permit that lifetime.
Mounted bundles follow certificate rotation; copies exported to a host must be
refreshed separately before their embedded certificate expires. The FLM server
certificate keeps its 24-hour lifetime and renews eight hours before expiry.

The operator renews cluster join tokens every fifteen minutes. The token grants
access to enrollment through the Kubernetes HTTPS Service proxy and is exchanged
for a worker-specific identity. That identity can read its named worker and
cluster and patch its own worker status; it cannot read Secrets. Re-enrollment
also requires the existing host's private enrollment key. Disabled/deleting
worker credentials revoke independently of unrelated cluster errors.

Join tokens, enrollment keys and worker kubeconfigs are credentials. Restrict
Secret access and keep host state private. Settings' join command fetches
`https://xflops.io/cluster/setup.sh` and pipes it to `sudo sh -s -- --join TOKEN`;
no pre-downloaded script is required. `--join TOKEN` places the token in
the process argument list, where local users may be able to read it. On shared
hosts use `--join --token-file FILE` with a private token file to avoid that
argument exposure. Installer tracing and output do not intentionally print
credentials. The API-server-to-operator proxy hop uses the trusted cluster
network; the operator also validates the current opaque token and expiry.

## Publishing

Publishing credentials are encrypted Actions secrets and are used only by main
publication. PRs use read-only repository permissions and internal artifacts.
Release checksums detect corrupted artifact downloads; trust in the release
publisher and delivery of setup.sh remains necessary.
