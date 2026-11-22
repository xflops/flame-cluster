# Flame UI

The root URL `/` opens the first configured Flame's Dashboard
(`#/dashboard`). The selector lists configured clusters only, such as
`flame-1` and `flame-2`; there is no All clusters option or cross-cluster summary.
Switching clusters preserves the resource view while clearing pages, filters,
drawers, pending requests, and credentials from the previous Flame.

Navigation is Dashboard, Nodes, Executors, Applications, Sessions, Settings.
Dashboard reads the complete selected cluster node inventory in bounded pages.
Overview shows a circular Ready/not-ready node chart alongside the reported GPU,
CPU, and memory capacity totals. The Current usage section below it summarizes
the latest reports for the cluster. Use a monitoring system such as Prometheus
for historical views and averages over time. The Nodes table shows capacity per
worker. Incomplete inventories cannot replace the last complete dashboard data.

Current usage comes only from `FlameWorker.status.usage` with a heartbeat aged
0–120 seconds, including reports from nodes that are not Ready. CPU averages the
latest samples across reporting workers. Memory shows combined used bytes as a
percentage of combined total bytes. GPU averages reported device utilization
across reporting GPUs. Coverage counts identify partial results. Missing or
stale readings display `-`, and genuine zero readings stay zero.

Nodes lists
enrolled nodes in the selected cluster, including Pending workers that
have not registered with FLM yet. The table shows overall readiness, a dedicated Capacity column, address,
runtimes, executor limit, and heartbeat. FEM and cache service states are
available in node details as Executor manager and Object cache. Node details
also list PCI devices detected and reported by flmlet. Unreported values display `-`.

Settings separates full session-manager configuration, the client download,
and node enrollment into three cards. The configuration card displays
`flame-cluster.yaml` as pretty-printed JSON when connection configuration or a
per-cluster runtime review snapshot is available; it reports unavailability
instead of fabricating configuration from connection metadata. The other cards
provide Download flame.yaml and the current join command/expiration. Downloads
contain the selected cluster's client configuration and are never rendered in
the page. The copyable join command downloads the hosted installer, so the worker
does not need a local setup.sh. Join tokens are loaded only in Settings and are
never cached across cluster changes. Expired or unavailable tokens do not
produce a copyable command.

The browser first reads `/api/connections`, then scopes reads, session actions,
and task pages to `/api/connections/{id}/...`. The management service reads Nodes
from namespaced FlameWorkers filtered to the selected Flame. Applications,
sessions, tasks, and executors come from that Flame's configured FLM. See
[backend configuration](../flame-cluster/README.md).

The UI calls the namespace-backed identity **Cluster** and keeps Kubernetes
namespace and object-generation concepts out of visible labels. Internal
namespace fields still scope requests and enforce isolation.

Compact density is the default for the header, sidebar, table rows, controls,
and Settings. A full 12-row resource page fits a 1440×900 viewport.

The browser calls same-origin `/api` routes through Nginx. Set
`CONSOLE_API_UPSTREAM=cluster:3000` when running the image; the Helm chart
sets this to its configured console service. For direct Tailscale access, set
`service.externalIPs` to the manager's Tailscale IPv4 and include that address
in the API's allowed hosts; see [deployment](../docs/deployment.md#ui-and-direct-access). Configure the FLM endpoint and any
TLS credentials on `flame-cluster`, never in browser assets.

```sh
npm ci
npm test
CONSOLE_API_TARGET=http://127.0.0.1:3000 npm run dev
```

Open `http://127.0.0.1:4173/`. The development server proxies `/api` to the console
backend and serves the same root console as production.

```sh
npx playwright install chromium
npm run test:browser
```

The production build emits one compressed document to reduce asset requests.
No endpoint credentials or private keys are embedded in that document.

## Live browser checks

The regular browser suite uses deterministic API fixtures. To verify a deployed
console against a real FLM, run the live suite after an SRE worker has completed
four `flmping` tasks. Create a separate empty Open session for mutations and
keep an executor-probe session running during the first browser check:

```sh
FLAME_E2E_URL=http://127.0.0.1:18090 \
FLAME_E2E_APPLICATION=flmping \
FLAME_E2E_SESSION='completed-session-id' \
FLAME_E2E_OPEN_SESSION='fresh-open-session-id' \
FLAME_E2E_EXECUTOR_SESSION='running-probe-session-id' \
FLAME_E2E_WORKER=flame-ci-worker \
npx playwright test tests/live.spec.js
```

The live suite starts no development server and mocks no API requests. It checks
the actual application, worker, an executor Bound to the specified running probe
session, and four successful task records, then
suspends, resumes, closes, and deletes the separate Open session through the UI.
Without `FLAME_E2E_URL`, these live checks are skipped.
