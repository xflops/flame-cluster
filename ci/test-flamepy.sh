#!/usr/bin/env bash
# Run the matching SDK on the CI host; execute services on the SRE worker.
set -euo pipefail
umask 077
runtime="${1:?test result directory required}"
mkdir -p "$runtime"
runtime="$(cd "$runtime" && pwd)"
kube=(kubectl -n flame-system)
worker=flame-ci-worker
hosts_marker="flame-app-ci-$$"
hosts_added=false
cleanup() {
  local status=$?
  trap - EXIT
  if [[ "$hosts_added" == true ]]; then
    sudo -n sed -i "/# $hosts_marker\$/d" /etc/hosts || status=1
  fi
  if [[ ! -s "$runtime/flamepy-app-results.json" ]]; then
    python3 - "$runtime/flamepy-app-results.json" "$status" <<'PYFAIL'
import json,sys
from pathlib import Path
Path(sys.argv[1]).write_text(json.dumps({'coverage':'flamepy.app','successful':False,'testsRun':0,
    'runnerExitStatus':int(sys.argv[2]),'error':'SDK bootstrap or test process failed before producing results; inspect flamepy-app.log.'},indent=2)+'\n')
PYFAIL
  fi
  return "$status"
}
trap cleanup EXIT
: > "$runtime/flamepy-app-results.json"
mkdir -p "$runtime/flamepy-wheels" "$runtime/flamepy-case" "$runtime/flamepy-home/.flame"
cp ci/flamepy-app.py "$runtime/flamepy-case/flamepy-app.py"
# Use exactly the SDK wheel delivered with the worker, not an unrelated PyPI SDK.
docker cp "$worker:/usr/local/flame/wheels/." "$runtime/flamepy-wheels/"
wheels=("$runtime/flamepy-wheels"/flamepy*.whl)
[[ "${#wheels[@]}" == 1 && -f "${wheels[0]}" ]]
python3.12 -m venv "$runtime/flamepy-venv"
"$runtime/flamepy-venv/bin/python" -m pip install "${wheels[0]}" > "$runtime/flamepy-sdk-install.log" 2>&1
# The private identity stays outside every directory packaged by flamepy.app.
"${kube[@]}" get secret flame-client -o json \
  | jq -er '.data["flame.yaml"] | @base64d' > "$runtime/flamepy-home/.flame/flame.yaml"
node_ip="$(kubectl get nodes -o json | jq -r '[.items[0].status.addresses[] | select(.type=="InternalIP") | .address][0]')"
[[ -n "$node_ip" && "$node_ip" != null ]]
printf '%s flame.e2e.test # %s\n' "$node_ip" "$hosts_marker" | sudo -n tee -a /etc/hosts >/dev/null
hosts_added=true
status=0
(
  cd "$runtime/flamepy-case"
  timeout 600 env -u FLAME_ENDPOINT -u FLAME_CACHE_ENDPOINT -u FLAME_CA_FILE -u FLAME_CERT_FILE -u FLAME_KEY_FILE -u PYTHONPATH -u GH_TOKEN -u GITHUB_TOKEN \
    HOME="$runtime/flamepy-home" PYTHONUNBUFFERED=1 \
    "$runtime/flamepy-venv/bin/python" flamepy-app.py \
    --worker "$worker" --results "$runtime/flamepy-app-results.json"
) 2>&1 | tee "$runtime/flamepy-app.log" || status=$?
[[ "$status" == 0 ]]
jq -e '.successful and .testsRun==3 and (.remoteCalls|length)==4 and .cache.bytes==131072 and .chainedResult==14 and .counter.result.total==6 and (.cleanup|length)==3' "$runtime/flamepy-app-results.json" >/dev/null
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  python3 - "$runtime/flamepy-app-results.json" >> "$GITHUB_STEP_SUMMARY" <<'PY'
import json,sys
r=json.load(open(sys.argv[1]))
print(f"\n## Flame App SDK\n\n- SDK {r['sdkVersion']}, Python {r['python']}: {r['testsRun']} live tests passed.")
print(f"- Client `{r['clientHost']}` called remote worker `{r['remoteCalls'][0]['worker']}`.")
print(f"- Cache round trip: {r['cache']['bytes']} bytes, SHA-256 `{r['cache']['sha256']}`.")
print('- Four remote argument/ObjectRef calls returned the expected worker, values and payload hashes.')
print(f"- Chained future: {r['chainedResult']}; stateful counter: {r['counter']['result']['total']}.")
print(f"- All {len(r['cleanup'])} unique App registrations retired after cleanup.")
PY
fi
