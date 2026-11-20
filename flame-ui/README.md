# Flame UI

The root URL `/` opens the configured cluster's Applications view (`#/applications`).
Applications, sessions, nodes, and
executors come from the console backend's direct FLM connection. There is no
Kubernetes inventory or separate dashboard URL. The cluster selector lists only
the configured cluster. Navigation is Nodes, Executors, Applications, Sessions,
Settings. Settings displays connection details and a copyable node join command
with its expiration, loaded from `/api/join` only while Settings is open.
The command uses the current rotating token; unavailable or expired tokens do
not produce a copyable command. Refresh Settings for the latest command.
The cluster installer configures the join Secret mount automatically. For a
manual console deployment, set `join.secretName` as described in the
[console configuration](../flame-console/README.md).

Compact density is the default for the header, sidebar, table rows, controls,
and Settings. A full 12-row resource page fits a 1440×900 viewport.

The browser calls same-origin `/api` routes through Nginx. Set
`CONSOLE_API_UPSTREAM=flame-console:3000` when running the image; the Helm chart
sets this to its configured console service. Configure the FLM endpoint and any
TLS credentials on `flame-console`, never in browser assets.

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
