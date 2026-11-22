# Flame Cluster management service

`flame-cluster` serves the REST API used by `flame-ui`. Service and Deployment
names are `cluster`. Nodes reads configured namespaced FlameWorkers; workload
resources and session actions use the selected FLM. Each namespace's client,
resource pages, actions, and enrollment credentials remain separate.
The combined chart always deploys this service; settings live under `cluster`.
The `flames` list generates indexed namespaced Flame resources and connections.

## Connections

The chart mounts an ordered JSON catalog and sets `FLAME_CONNECTIONS_FILE`:

```json
[
  {
    "id": "flame-1",
    "namespace": "flame-1",
    "name": "flame-1",
    "displayName": "flame-1/flame-1",
    "endpoint": "https://flame-1-session-manager.flame-1.svc:8080",
    "caFile": "/etc/flame/connections/flame-1/ca.crt",
    "joinNamespace": "flame-1",
    "joinSecret": "flame-1-join",
    "clientConfigSecret": "flame-1-client"
  }
]
```

IDs must be unique DNS labels; order chooses the default connection. The catalog
must not be empty. Metadata responses omit credential files and Secret names,
but include `clientConfigAvailable` to indicate whether a client bundle source
is configured. `clientConfigSecret` requires an explicit valid `namespace` and
always reads from that namespace; it has no separate namespace override.
The UI labels the selector with the namespace and sends every operation to the
selected connection. The backend does not discover namespaces. Nodes lists
FlameWorkers through Kubernetes in the explicitly configured namespace, filtered
by the configured Flame resource `name` (`spec.clusterRef`). Other workload
resources use FLM.

The mounted ServiceAccount identity has `get`/`list` on FlameWorkers in each
configured namespace and `get` on the configured exact join and client-config
Secrets. No worker writes or cluster-wide worker listing are granted. No Secret
listing or writes are granted. Client downloads return only the client Secret's
`flame.yaml` entry, never its other data keys. TLS CA mounts contain public trust
from the management namespace. The matching public trust is also
provided in each Flame namespace for its operator-managed gateway.

The selected `/connection` response also includes `configuration`: the actual
session-manager `flame-cluster.yaml` from that Flame's `<name>-config` ConfigMap,
parsed as a complete JSON object for Settings. The namespace Role grants only
`get` on that exact ConfigMap; no ConfigMap listing or writes are granted. This
generated configuration contains TLS file paths, not Secret contents. Reads
time out after twelve seconds and reject configurations larger than 1 MiB or
non-object YAML. Missing, denied, or invalid configuration returns a generic
`configurationError` while preserving HTTP 200 connection metadata. Standalone
connections without managed namespace/name settings omit both fields and make
no Kubernetes configuration request.

For a standalone process, the existing single-endpoint environment mode remains:

```sh
FLAME_ENDPOINT=http://flame-session-manager:8080 cargo run -p flame-cluster
```

Without a catalog, `FLAME_ENDPOINT` is required, `FLAME_NAME` defaults to `Flame`,
and `CONSOLE_BIND` defaults to `127.0.0.1:3000`. HTTPS uses native roots unless
`FLAME_CA_FILE` provides public CA trust. `FLAME_CERT_FILE` and `FLAME_KEY_FILE`
together configure mTLS; `FLAME_TLS_DOMAIN` overrides the server name. Partial
client identities and TLS settings on an HTTP endpoint are rejected.

Standalone join files use `FLAME_JOIN_TOKEN_FILE` and `FLAME_JOIN_EXPIRY_FILE`
together. File catalog entries can use corresponding `joinTokenFile` and
`joinExpiryFile` fields. File and managed Kubernetes join sources cannot be
mixed for one connection. This environment/file-token mode requires no
Kubernetes credentials for FLM operations. Nodes requires a configured namespace
and Flame resource name, plus the scoped FlameWorker read identity. Without it,
Nodes returns an unavailable error; it never substitutes FLM node registration.
Catalog connections with managed join
or client-config Secrets require the corresponding scoped Kubernetes identity.

## API

- `GET /api/connections`: ordered public connection metadata, including namespace.
- `GET /api/connections/{id}/connection`: selected endpoint/display metadata and managed session-manager configuration as JSON.
- `GET /api/connections/{id}/join`: selected join command and expiration.
- `GET /api/connections/{id}/flame.yaml`: selected client YAML as a `flame.yaml` attachment.
- `GET /api/connections/{id}/nodes`: namespace-scoped FlameWorkers filtered by the selected Flame, with raw resource fields plus `readiness` (Pending, Ready, Not ready, Disabled, or Deleting).
- `GET /api/connections/{id}/{applications|sessions|executors}`: selected FLM resources.
- `GET /api/connections/{id}/sessions/{session}?page=1&pageSize=12`: native `GetSession` snapshot and one bounded `ListTasks` metadata page, returned as `{session: {metadata, spec, status}, tasks: {items, page, pageSize, total, hasNext, observedAt}}`. Lifecycle events are in `session.status.events`. The wire subset excludes session common data, tokens, and task payloads. The session is fetched first so missing sessions return 404; upstream failures stay generic.
- `POST /api/connections/{id}/sessions/{session}/actions/{suspend|resume|close|delete}` with JSON `{}`.
- `GET /healthz`: process health, independent of FLM availability.

Existing `/api/connection`, `/api/join`, `/api/flame.yaml`, and resource/action paths remain aliases
for the first configured connection. Unknown connection IDs return 404. Each
route is bound to that connection's own FLM client and enrollment source.

FLM resource lists forward one native FLM page/filter, without local sorting or full-inventory
downloads. Responses include `items`, `page`, `pageSize`, `total`, `hasNext`, and
`observedAt`. `pageSize` is 1–100, default 12. Invalid filters/pagination are
rejected; upstream failures return HTTP 502, never a fake healthy empty list.
Application responses omit credentials, environment variables, and task payloads.

Nodes sorts workers by name, filters by readiness, then paginates. Enrolled
workers without a current status report appear as Pending. Ready requires a
current Ready condition, FEM registration and active service, a heartbeat no
older than 120 seconds, and active ready cache service when managed. Kubernetes
inventory reads are limited to 100 chunks of 500 workers and 12 seconds. Read
failures, invalid responses, or incomplete inventories return HTTP 503 without
partial results.

Join responses are generated on demand. Missing, denied, malformed, or expired
credentials return HTTP 503 without a command. All API responses use
`Cache-Control: no-store`. The copied join command downloads the cluster
installer from xflops.io and runs it on the worker host:

```sh
curl -fsSL https://xflops.io/cluster/setup.sh | sudo sh -s -- --join 'TOKEN'
```

Flmlet installs FEM, writes configuration, and activates the selected services.

Settings also downloads the selected Flame's `flame.yaml`. Each request reads
the current configured Secret; the response uses `application/yaml`, attachment
filename `flame.yaml`, `X-Content-Type-Options: nosniff`, and `Cache-Control:
no-store`. Missing, denied, invalid UTF-8, or empty bundles return a generic
HTTP 503. The download contains the selected Flame's embedded client certificate
and private key. Keep the exported file private and refresh it separately when
the client certificate renews; it is not included in metadata or join responses.

## Access

`CONSOLE_ALLOWED_HOSTS` is a comma-separated list of exact trusted UI/API hosts,
without ports or wildcards. It defaults to `localhost,127.0.0.1,::1`; the chart
uses `cluster.security.allowedHosts`. Setup adds the kubeconfig-derived UI host.
The UI proxy preserves Host. All API routes enforce this allowlist; session
mutations also require any supplied Origin to match the full Host and port.

These checks protect against DNS rebinding; they are not user authentication.
The central management UI/API is administrative access to all configured Flames.
Expose it through trusted access or an authentication proxy. Per-namespace worker
and gateway-client credentials remain separate. See [security](../docs/security.md).

`cargo test -p flame-cluster` uses mock FLMs, mock Kubernetes worker and Secret
endpoints, and temporary OpenSSL certificates to verify inventory, connection,
and credential isolation.
