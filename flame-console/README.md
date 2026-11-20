# Flame Console

`flame-console` serves the REST API used by `flame-ui`. It connects directly to one
configured Flame Frontend (FLM). It requires no Kubernetes credentials, resource
access, service discovery, node agent, or logical manager.

```sh
FLAME_ENDPOINT=http://flame-session-manager:8080 cargo run -p flame-console
```

`FLAME_ENDPOINT` is required. `FLAME_NAME` optionally sets the display name (default
`Flame`), and `CONSOLE_BIND` sets the HTTP listener (default `127.0.0.1:3000`).
Use `http://` for explicitly chosen cleartext internal gRPC. For `https://`, native
system CA roots are used unless `FLAME_CA_FILE` supplies a PEM CA bundle.
`FLAME_CERT_FILE` and `FLAME_KEY_FILE` together supply a PEM client identity for
mTLS. `FLAME_TLS_DOMAIN` optionally overrides the certificate server name. TLS
settings are rejected for an HTTP endpoint, and a partial client identity is
rejected at startup. SREs mount and maintain the credential files.

`CONSOLE_ALLOWED_HOSTS` is a comma-separated list of exact trusted UI/API
hostnames or IP addresses, without ports, wildcards, or credentials. It defaults
to `localhost,127.0.0.1,::1`. Every `/api/` request requires one matching Host
header; `/healthz` remains available for Kubernetes probes. Configure the exact
public UI/SSO ingress hostname before exposing the UI, using Helm
`security.allowedHosts` for the console chart. The UI proxy must preserve Host.
Session mutations also require any supplied Origin to match the full Host
including its port. This protects against DNS rebinding; access authentication
and authorization remain the SRE's responsibility.

The API exposes:

- `GET /api/connection`: endpoint and display name, without credentials.
- `GET /api/join`: current node join command and expiration, when configured.
- `GET /api/nodes`, `/api/applications`, `/api/sessions`, `/api/executors`.
- `GET /api/sessions/{id}/tasks`.
- `POST /api/sessions/{id}/actions/{suspend|resume|close|delete}` with JSON `{}`.
- `GET /healthz`: process health, independently of upstream availability.

Lists forward one native FLM page and state filter without local sorting or full
inventory downloads. Responses contain `items`, `page`, `pageSize`, `total`,
`hasNext`, and `observedAt`. `pageSize` is 1–100 (default 12); obsolete search and
invalid pagination are rejected. Upstream listing failures return HTTP 502 with a
generic error; upstream error text is excluded from responses and console logs.
Missing paging metadata never becomes an empty successful list. Mutation errors
retain meaningful HTTP status and reject cross-origin requests.

Application responses omit credentials, environment variables and task payloads.
The console does not expose a private-key download or Kubernetes settings API.
To show the node join command in Settings, set Helm `join.secretName` to the
operator's `<cluster>-join` Secret in the console namespace. The Secret is mounted
read-only; no Kubernetes client, service account token, or RBAC is added. Outside
Helm, configure `FLAME_JOIN_TOKEN_FILE` and `FLAME_JOIN_EXPIRY_FILE` together with
paths to its `token` and `expiresAt` contents. Each `/api/join` request reads the
current files so operator rotation appears without restarting the console.
Missing, malformed, or expired credentials return HTTP 503 without a command.
The join endpoint carries enrollment credentials and, like all API responses,
uses `Cache-Control: no-store`. The copied command uses a local `setup.sh`; prepare
that installer on the node before running it.
SREs must restrict access to the UI/API because session actions use the configured
FLM identity; this service does not add a separate user authorization system.

`cargo test -p flame-console` runs a local mock FLM and generates temporary TLS
credentials using OpenSSL; development and CI test hosts need the `openssl` CLI.
