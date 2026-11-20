#!/usr/bin/env bash
# Functional assertions against real FLM/FEM/FOC and the deployed console/UI.
set -euo pipefail
runtime="${1:?test result directory required}"
base=http://127.0.0.1:18090
source "$(dirname "$0")/worker-helpers.sh"
get() { curl --fail-with-body --silent --show-error "$base$1"; }
wait_session_state() {
  local session="$1" state="$2"
  for attempt in {1..60}; do
    if get '/api/sessions?pageSize=100' > "$runtime/sessions.json" \
      && jq -e --arg id "$session" --argjson state "$state" 'any(.items[]; .metadata.id==$id and .status.state==$state)' "$runtime/sessions.json" >/dev/null; then return; fi
    sleep 1
  done
  cat "$runtime/sessions.json" >&2
  return 1
}
mutate() { curl --fail-with-body --silent --show-error -H 'Content-Type: application/json' -d '{}' "$base/api/sessions/$1/actions/$2"; }

# Install a real systemd host through the public join path and operator-issued token.
bash ci/start-worker.sh "$runtime"
node_ip="$(kubectl get nodes -o json | jq -r '[.items[0].status.addresses[]|select(.type=="InternalIP")|.address][0]')"
for attempt in {1..60}; do
  get '/api/nodes?pageSize=100&state=Ready' > "$runtime/nodes.json"
  if jq -e --arg worker "$worker" '.total>=1 and any(.items[]; (.metadata.name==$worker or .spec.hostname==$worker) and .status.state==1)' "$runtime/nodes.json" >/dev/null; then break; fi
  [[ "$attempt" != 60 ]] || { cat "$runtime/nodes.json" >&2; exit 1; }
  sleep 1
done

# Wait for the operator to finish reconciling the SRE cache endpoint change.
for attempt in {1..60}; do
  if "${kube[@]}" get flamecluster flame -o json | jq -e '.status.observedGeneration==.metadata.generation and any(.status.conditions[]; .type=="Ready" and .status=="True")' >/dev/null; then break; fi
  [[ "$attempt" != 60 ]] || exit 1
  sleep 1
done
# Native FLM protects both raw NodePorts with server-authenticated TLS. No
# client certificate is supplied here: native mTLS is intentionally not enabled.
"${kube[@]}" get flamecluster flame -o json \
  | jq -er '.status.controlPlaneCa | select(type=="string" and length>0)' > "$runtime/control-plane-ca.crt"
for port in 30080 30081; do
  timeout 20 openssl s_client -connect "$node_ip:$port" -verify_ip "$node_ip" \
    -CAfile "$runtime/control-plane-ca.crt" -verify_return_error -alpn h2 -brief \
    < /dev/null > "$runtime/native-tls-$port.log" 2>&1
  grep -Eq 'Verification: OK|Verify return code: 0 \(ok\)' "$runtime/native-tls-$port.log"
  # Send an actual HTTP/2 client preface and empty SETTINGS over plaintext.
  # A cleartext gRPC listener replies with SETTINGS; a TLS-only listener must
  # reject these bytes. Connect/refusal/timeout errors are not accepted evidence.
  python3 - "$node_ip" "$port" > "$runtime/native-plaintext-$port.log" <<'PYTLS'
import socket,sys
host,port=sys.argv[1],int(sys.argv[2])
with socket.create_connection((host,port),timeout=10) as connection:
    connection.settimeout(10)
    connection.sendall(b'PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n'+b'\x00\x00\x00\x04\x00\x00\x00\x00\x00')
    reply=b''
    try:
        while len(reply)<5:
            chunk=connection.recv(5-len(reply))
            if not chunk: break
            reply+=chunk
    except ConnectionResetError:
        pass
    if reply:
        # A TLS alert record is the only permissible nonempty rejection.
        assert len(reply)>=5 and reply[0]==21 and reply[1]==3, f'Native port {port} accepted/responded to plaintext HTTP/2: {reply[:16].hex()}'
    print(f'Native port {port} explicitly rejected plaintext HTTP/2')
PYTLS
done
jq -n --arg ip "$node_ip" '{serverIp:$ip,ports:[30080,30081],serverCertificateVerified:true,
  clientCertificateRequired:false,plaintextGrpcRejected:true}' > "$runtime/native-tls-results.json"

# A working generated client identity must reach the public Gateway endpoint.
timeout 30 docker exec "$worker" /usr/local/flame/bin/flmctl --config /etc/flame-gateway/flame.yaml list -a > "$runtime/gateway-applications.txt"
"${kube[@]}" get secret flame-client -o json | jq -er '.data["ca.crt"] | @base64d' > "$runtime/gateway-ca.crt"
# Inspect the TLS alert itself; an SDK's generic transport error is not proof.
negative=0
printf 'GET / HTTP/1.1\r\nHost: flame.e2e.test\r\nConnection: close\r\n\r\n' \
  | timeout 20 openssl s_client -connect "$node_ip:30443" -servername flame.e2e.test \
      -verify_hostname flame.e2e.test -CAfile "$runtime/gateway-ca.crt" \
      -verify_return_error -tls1_3 -quiet > "$runtime/no-client-identity.log" 2>&1 || negative=$?
if [[ "$negative" == 0 || "$negative" == 124 || "$negative" == 137 ]]; then
  echo "Expected an explicit mTLS rejection, got exit $negative" >&2
  cat "$runtime/no-client-identity.log" >&2
  exit 1
fi
grep -Eqi 'alert.*certificate required|certificate required' "$runtime/no-client-identity.log"
timeout 30 docker exec "$worker" /usr/local/flame/bin/flmctl --config /etc/flame-gateway/flame.yaml list -a > /dev/null

# Registration must create an application that was not a seeded default.
worker_exec_stdin sh -c 'cat > /tmp/flame-ci-ping.yaml' <<'APP'
metadata:
  name: flame-ci-ping
spec:
  shim: host
  url: file:///usr/local/flame/bin/flmping-service
  command: /usr/local/flame/bin/flmping-service
  schema: {}
APP
worker_cli register --file /tmp/flame-ci-ping.yaml
worker_cli update --application /tmp/flame-ci-ping.yaml
get '/api/applications?pageSize=100' > "$runtime/applications.json"
jq -e '.total>=1 and any(.items[]; .metadata.name=="flame-ci-ping" and .status.state==0)' "$runtime/applications.json" >/dev/null
# flmping targets its built-in application. Pin its executable to the worker install.
worker_exec sh -ec 'sed s/flame-ci-ping/flmping/ /tmp/flame-ci-ping.yaml > /tmp/flmping.yaml; mkdir -p /tmp/flame-client/.flame /tmp/flame-gateway/.flame; cp /usr/local/flame/conf/flmlet-client.yaml /tmp/flame-client/.flame/flame.yaml; cp /etc/flame-gateway/flame.yaml /tmp/flame-gateway/.flame/flame.yaml'
worker_cli update --application /tmp/flmping.yaml

# Observe executors while work is running, then verify all four returned outputs.
timeout 180 docker exec "$worker" env HOME=/tmp/flame-gateway \
  /usr/local/flame/bin/flmping --task-num 4 --common-data functional-e2e --duration 3000 --resreq cpu=1,mem=128m \
  > "$runtime/flmping.log" 2>&1 &
workload=$!
for attempt in {1..60}; do
  get '/api/executors?pageSize=100' > "$runtime/executors-running.json"
  if jq -e '.total>=1 and any(.items[]; .spec.application=="flmping" and (.status.state==3 or .status.state==4))' "$runtime/executors-running.json" >/dev/null; then break; fi
  [[ "$attempt" != 60 ]] || { cat "$runtime/flmping.log" >&2; exit 1; }
  sleep 1
done
wait "$workload"
python3 - "$runtime/flmping.log" "$runtime/session-id" <<'PY'
import re,sys
from pathlib import Path
log=Path(sys.argv[1]).read_text()
session=re.search(r'Session <([^>]+)> was created',log)
assert session,log
rows=re.findall(r'^\s*'+re.escape(session[1])+r'\s+(\d+)\s+Succeed\s+Completed on <flame-ci-worker>.*$',log,re.M)
assert len(rows)==4 and len(set(rows))==4,log
Path(sys.argv[2]).write_text(session[1])
print('Verified four distinct successful task outputs from the SRE worker.')
PY
session="$(cat "$runtime/session-id")"
wait_session_state "$session" 1
jq -e --arg id "$session" 'any(.items[]; .metadata.id==$id and .spec.application=="flmping" and .status.succeed==4 and .status.failed==0 and .status.pending==0 and .status.running==0)' "$runtime/sessions.json" >/dev/null
for page in 1 2; do
  get "/api/sessions/$session/tasks?page=$page&pageSize=2" > "$runtime/tasks-$page.json"
  jq -e --argjson page "$page" '.page==$page and .pageSize==2 and .total==4 and (.items|length)==2 and .hasNext==($page==1) and all(.items[]; .status.state==2 and .status.completion_time>0 and .spec.input==null and .spec.output==null)' "$runtime/tasks-$page.json" >/dev/null
done
jq -es '[.[].items[].metadata.id] | length==4 and (unique|length)==4' "$runtime/tasks-1.json" "$runtime/tasks-2.json" >/dev/null

# Separate fresh sessions exercise real mutations and preserve browser isolation.
new_session() { worker_cli create --app flmping --resreq cpu=1,mem=128m | sed -n 's/^Session <\([^>]*\)> was created\.$/\1/p'; }
http_session="$(new_session)"
ui_session="$(new_session)"
spare_session="$(new_session)"
[[ -n "$http_session" && -n "$ui_session" && -n "$spare_session" ]]
for page in 1 2; do
  get "/api/sessions?page=$page&pageSize=2" > "$runtime/sessions-page-$page.json"
  jq -e --argjson page "$page" '.page==$page and .pageSize==2 and .total==4 and (.items|length)==2 and .hasNext==($page==1)' "$runtime/sessions-page-$page.json" >/dev/null
done
jq -es '[.[].items[].metadata.id] | length==4 and (unique|length)==4' "$runtime/sessions-page-1.json" "$runtime/sessions-page-2.json" >/dev/null
mutate "$http_session" suspend > "$runtime/action-suspend.json"
wait_session_state "$http_session" 3
mutate "$http_session" resume > "$runtime/action-resume.json"
wait_session_state "$http_session" 0
mutate "$http_session" close > "$runtime/action-close.json"
wait_session_state "$http_session" 1
mutate "$http_session" delete > "$runtime/action-delete.json"
status="$(curl -sS -o "$runtime/deleted-session-action.json" -w '%{http_code}' -H 'Content-Type: application/json' -d '{}' "$base/api/sessions/$http_session/actions/close")"
[[ "$status" == 404 ]]
get '/api/sessions?state=Closed&pageSize=100' > "$runtime/closed-sessions.json"
jq -e --arg id "$session" '.total==1 and (.items|length)==1 and .items[0].metadata.id==$id and .items[0].status.state==1' "$runtime/closed-sessions.json" >/dev/null

# Keep one executor busy while the live browser checks its reported node association.
timeout 180 docker exec "$worker" env HOME=/tmp/flame-client \
  FLAME_ENDPOINT="https://$node_ip:30080" \
  FLAME_CA_FILE=/usr/local/flame/conf/flmlet/control-plane-ca.crt \
  /usr/local/flame/bin/flmping --task-num 1 --duration 30000 --perf --resreq cpu=1,mem=128m \
  > "$runtime/executor-probe.log" 2>&1 &
probe=$!
probe_session=""
for attempt in {1..30}; do
  probe_session="$(sed -n 's/^Session <\([^>]*\)> was created.*/\1/p' "$runtime/executor-probe.log" | head -1)"
  if [[ -n "$probe_session" ]] && get '/api/executors?pageSize=100' \
    | jq -e --arg session "$probe_session" 'any(.items[]; .spec.application=="flmping" and .status.state==4 and .status.session_id==$session)' >/dev/null; then break; fi
  [[ "$attempt" != 30 ]] || { cat "$runtime/executor-probe.log" >&2; exit 1; }
  sleep 1
done
(
  cd flame-ui
  FLAME_E2E_URL="$base" FLAME_E2E_APPLICATION=flmping FLAME_E2E_SESSION="$session" \
    FLAME_E2E_OPEN_SESSION="$ui_session" FLAME_E2E_WORKER="$worker" FLAME_E2E_EXECUTOR_SESSION="$probe_session" \
    PLAYWRIGHT_JSON_OUTPUT_FILE="$runtime/live-browser-results.json" \
    npx playwright test tests/live.spec.js --workers=1 --reporter=line,json
)
jq -e '.stats.expected==3 and .stats.unexpected==0 and .stats.skipped==0 and .stats.flaky==0' "$runtime/live-browser-results.json" >/dev/null
wait "$probe"
grep -Eq 'Succeeded:[[:space:]]+1/1' "$runtime/executor-probe.log"
grep -Eq 'Failed:[[:space:]]+0' "$runtime/executor-probe.log"

# Real SDK App/cache execution, not local mocks or successful empty reads.
bash ci/test-flamepy.sh "$runtime"

# A real refused connection must be an error, never a successful empty list.
"${kube[@]}" set env deployment/flame-console FLAME_ENDPOINT=https://127.0.0.1:1
"${kube[@]}" rollout status deployment/flame-console --timeout=120s
for attempt in {1..30}; do
  status="$(curl -sS --max-time 20 -o "$runtime/upstream-unavailable.json" -w '%{http_code}' "$base/api/applications")"
  # The Service briefly has no console endpoint during its Recreate rollout.
  # Require the console's JSON error, rather than nginx's transient HTML 502.
  if [[ "$status" == 502 ]] && jq -e '.error|length>0' "$runtime/upstream-unavailable.json" >/dev/null 2>&1; then break; fi
  [[ "$attempt" != 30 ]] || { cat "$runtime/upstream-unavailable.json" >&2; exit 1; }
  sleep 1
done
"${kube[@]}" set env deployment/flame-console FLAME_ENDPOINT=https://flame-session-manager:8080
"${kube[@]}" rollout status deployment/flame-console --timeout=120s
for attempt in {1..30}; do
  if get '/api/applications?pageSize=100' > "$runtime/recovered-applications.json" && jq -e 'any(.items[]; .metadata.name=="flame-ci-ping")' "$runtime/recovered-applications.json" >/dev/null; then break; fi
  [[ "$attempt" != 30 ]] || exit 1
  sleep 1
done

# Exercise Helm upgrade -> CRD change -> operator rollout -> retained session data.
# The paging fixture deliberately left one session Open. Finish that owned
# fixture before startup reconciles the bundled flmping application manifest.
mutate "$spare_session" close > "$runtime/action-close-spare.json"
wait_session_state "$spare_session" 1
helm upgrade flame-cluster charts/flame-cluster --namespace flame-system --reuse-values --set rolloutRevision=functional-restart
for attempt in {1..60}; do
  if "${kube[@]}" get deployment flame-session-manager -o json \
    | jq -e '.spec.template.metadata.annotations["flame.xflops.io/rollout-revision"]=="functional-restart"' >/dev/null; then break; fi
  [[ "$attempt" != 60 ]] || exit 1
  sleep 1
done
"${kube[@]}" rollout status deployment/flame-session-manager --timeout=120s
# TLS trust/status may disappear briefly during control-plane rollout. Require
# fresh worker readiness rather than assuming its old registration/PIDs survived.
wait_cluster_ready
wait_worker_ready "$runtime/flmlet-after-control-plane-rollout.json"
wait_session_state "$session" 1
get "/api/sessions/$session/tasks?page=1&pageSize=100" > "$runtime/tasks-after-restart.json"
jq -e '.total==4 and (.items|length)==4 and all(.items[]; .status.state==2)' "$runtime/tasks-after-restart.json" >/dev/null
get '/api/applications?pageSize=100' | jq -e 'any(.items[]; .metadata.name=="flame-ci-ping")' >/dev/null
# Verify real host idempotence, rolling configuration and credential revocation.
bash ci/test-flmlet.sh "$runtime"

# Reads and mutations succeeded with no Kubernetes token in the console.
"${kube[@]}" get deployment flame-console -o json | jq -e '.spec.template.spec.automountServiceAccountToken==false' >/dev/null
python3 - "$runtime" <<'PY'
import json,sys
from pathlib import Path
r=Path(sys.argv[1])
nodes=json.loads((r/'nodes.json').read_text())
tasks=json.loads((r/'tasks-after-restart.json').read_text())
flamepy=json.loads((r/'flamepy-app-results.json').read_text())
flmlet=json.loads((r/'flmlet-results.json').read_text())
browser=json.loads((r/'live-browser-results.json').read_text())['stats']
assert flamepy['successful'] and flamepy['testsRun']==3
result={'coverage':'functional','readyWorkers':nodes['total'],'completedTasks':len(tasks['items']),
        'taskStates':[task['status']['state'] for task in tasks['items']],
        'taskPages':2,'uniqueTaskIds':len({task['metadata']['id'] for task in tasks['items']}),
        'httpSessionActions':['suspend','resume','close','delete'],'liveBrowserTests':browser['expected'],'liveBrowserSkipped':browser['skipped'],
        'gatewayMtls':True,'identityWithoutCertificateRejected':True,
        'nativeControlPlaneTls':json.loads((r/'native-tls-results.json').read_text()),
        'upstreamFailureStatus':502,'persistenceAfterRestart':True,
        'flamepyAppTests':flamepy['testsRun'],'flamepyRemoteResults':flamepy['remoteCalls'],
        'flamepyChainedResult':flamepy['chainedResult'],'flamepyCounter':flamepy['counter']['result'],
        'hostWorker':flmlet}
(r/'coverage.json').write_text(json.dumps(result,indent=2)+'\n')
(r/'coverage.md').write_text('## Functional E2E\n\n'+
    f"- Ready SRE workers: {result['readyWorkers']}\n- Successful workload tasks: {result['completedTasks']}/4\n"+
    '- Native FLM frontend/backend verified server certificates without client certificates and rejected plaintext gRPC.\n'+
    '- Gateway mTLS accepted the generated identity and rejected a CA-only client.\n- Two task pages contain four unique successful records.\n- Four HTTP session actions and three live browser tests passed.\n'+
    f"- Flame App SDK: {flamepy['testsRun']} real tests; chained result {flamepy['chainedResult']}, counter {flamepy['counter']['result']['total']}.\n"+
    '- Unreachable upstream returned 502; Helm upgrade retained the four successful tasks.\n'+
    '- setup.sh joined a systemd host without a pre-existing Worker CR and generated FEM configuration.\n'+
    '- Bootstrap rotation preserved worker processes and credentials; the retired token could not enroll another worker.\n'+
    '- Worker configuration rollout, disable/rejoin and deletion were verified against real services.\n')
PY
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then cat "$runtime/coverage.md" >> "$GITHUB_STEP_SUMMARY"; fi
cat "$runtime/coverage.md"
