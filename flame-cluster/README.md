# Flame Cluster management service

`flame-cluster` serves the REST API used by `flame-ui`. Service and Deployment
names are `cluster`. It connects directly to configured FLMs, keeping each
namespace's client, resource pages, actions, and enrollment credentials separate.
The combined chart always deploys this service; settings live under `cluster`.
The `flames` list generates indexed namespaced Flame resources and connections.

## Connections

The chart mounts an ordered JSON catalog and sets `FLAME_CONNECTIONS_FILE`:

```json
[
  {
    "id": "flame-1",
    "namespace": "flame-ns-1",
    "displayName": "flame-ns-1/flame-1",
    "endpoint": "https://flame-1-session-manager.flame-ns-1.svc:8080",
    "caFile": "/etc/flame/connections/flame-1/ca.crt",
    "joinNamespace": "flame-ns-1",
    "joinSecret": "flame-1-join"
  }
]
```

IDs must be unique DNS labels; order chooses the default connection. The catalog
must not be empty. Metadata responses omit credential files and Secret names.
The UI labels the selector with the namespace and sends every operation to the
selected connection. The backend does not discover namespaces or FLM resources
through Kubernetes.

Managed enrollment reads use the mounted ServiceAccount identity with only
`get` on the configured exact join Secret in each Flame namespace. No Secret
listing, client private-key access, or writes are granted. TLS CA mounts contain
public trust from the management namespace. The matching public trust is also
provided in each Flame namespace for its operator-managed gateway.

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
mixed for one connection. Standalone mode requires no Kubernetes credentials.

## API

- `GET /api/connections`: ordered public connection metadata, including namespace.
- `GET /api/connections/{id}/connection`: selected endpoint/display metadata.
- `GET /api/connections/{id}/join`: selected join command and expiration.
- `GET /api/connections/{id}/{nodes|applications|sessions|executors}`.
- `GET /api/connections/{id}/sessions/{session}/tasks`.
- `POST /api/connections/{id}/sessions/{session}/actions/{suspend|resume|close|delete}` with JSON `{}`.
- `GET /healthz`: process health, independent of FLM availability.

Existing `/api/connection`, `/api/join`, and resource/action paths remain aliases
for the first configured connection. Unknown connection IDs return 404. Each
route is bound to that connection's own FLM client and enrollment source.

Lists forward one native FLM page/filter, without local sorting or full-inventory
downloads. Responses include `items`, `page`, `pageSize`, `total`, `hasNext`, and
`observedAt`. `pageSize` is 1–100, default 12. Invalid filters/pagination are
rejected; upstream failures return HTTP 502, never a fake healthy empty list.
Application responses omit credentials, environment variables, and task payloads.

Join responses are generated on demand. Missing, denied, malformed, or expired
credentials return HTTP 503 without a command. All API responses use
`Cache-Control: no-store`. The copied command uses local `setup.sh`; prepare that
installer on the host. Flmlet installs FEM, writes configuration, and starts it.

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

`cargo test -p flame-cluster` uses mock FLMs, mock Kubernetes Secret endpoints,
and temporary OpenSSL certificates to verify connection and credential isolation.
