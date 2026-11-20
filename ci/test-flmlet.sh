#!/usr/bin/env bash
# Real host lifecycle checks after workloads/browser/SDK assertions are finished.
set -euo pipefail
source "$(dirname "$0")/worker-helpers.sh"
runtime="${1:?test result directory required}"
wait_stopped() {
  local fem cache
  for attempt in {1..60}; do
    fem="$(worker_exec systemctl show flame-executor-manager.service --property=MainPID --value)" || return 1
    cache="$(worker_exec systemctl show flame-object-cache.service --property=MainPID --value)" || return 1
    if [[ "$fem" == 0 && "$cache" == 0 ]] && worker_exec systemctl is-active --quiet flmlet.service; then return; fi
    sleep 1
  done
  worker_exec systemctl status flame-executor-manager.service flame-object-cache.service --no-pager >&2 || true
  return 1
}
wait_credentials_removed() {
  local uid="$1"
  for attempt in {1..60}; do
    if "${kube[@]}" get secrets -l "flame.xflops.io/owner-uid=$uid,flame.xflops.io/credential=api-token" -o json \
      | jq -e '.items|length==0' >/dev/null; then return; fi
    sleep 1
  done
  return 1
}
worker_pid() { worker_exec systemctl show "$1" --property=MainPID --value; }
worker_once() {
  timeout 75 docker exec "$worker" /usr/local/flame/bin/flmlet --worker "$worker" \
    --namespace flame-system --prefix /usr/local/flame \
    --kubeconfig /usr/local/flame/conf/flmlet/kubeconfig "$@"
}
uid="$("${kube[@]}" get flameworker "$worker" -o jsonpath='{.metadata.uid}')"
"${kube[@]}" get secrets -l "flame.xflops.io/owner-uid=$uid,flame.xflops.io/credential=api-token" -o json | jq -e '.items|length==1' >/dev/null
wait_cluster_ready
wait_worker_ready "$runtime/flmlet-before.json"
before_fem="$(worker_pid flame-executor-manager.service)"
before_cache="$(worker_pid flame-object-cache.service)"
[[ "$before_fem" -gt 0 && "$before_cache" -gt 0 ]]
worker_exec sha256sum /usr/local/flame/conf/flmlet/flame-cluster.yaml /usr/local/flame/conf/flmlet-client.yaml /usr/local/flame/conf/flmlet.env /usr/local/flame/conf/flmlet/control-plane-ca.crt > "$runtime/flmlet-config-before.txt"
worker_once --check > "$runtime/flmlet-preview.json"
worker_once --once > "$runtime/flmlet-once.log" 2>&1
worker_exec sha256sum /usr/local/flame/conf/flmlet/flame-cluster.yaml /usr/local/flame/conf/flmlet-client.yaml /usr/local/flame/conf/flmlet.env /usr/local/flame/conf/flmlet/control-plane-ca.crt > "$runtime/flmlet-config-after.txt"
cmp "$runtime/flmlet-config-before.txt" "$runtime/flmlet-config-after.txt"
[[ "$(worker_pid flame-executor-manager.service)" == "$before_fem" ]]
[[ "$(worker_pid flame-object-cache.service)" == "$before_cache" ]]

# A transport outage is not an authorization/deletion signal and must not stop work.
heartbeat="$(jq -r .status.lastHeartbeatTime "$runtime/flmlet-before.json")"
worker_exec python3 -c 'import json,os; from pathlib import Path; p=Path("/usr/local/flame/conf/flmlet/kubeconfig"); old=p.read_bytes(); b=Path("/tmp/flmlet-kubeconfig.restore"); b.write_bytes(old); b.chmod(0o600); data=json.loads(old); data["clusters"][0]["cluster"]["server"]="https://127.0.0.1:1"; p.write_text(json.dumps(data)); p.chmod(0o600)'
restore_config() {
  worker_exec sh -ec 'if [ -f /tmp/flmlet-kubeconfig.restore ]; then mv /tmp/flmlet-kubeconfig.restore /usr/local/flame/conf/flmlet/kubeconfig; chmod 0600 /usr/local/flame/conf/flmlet/kubeconfig; systemctl restart flmlet.service; fi' || true
}
trap restore_config EXIT
worker_exec systemctl restart flmlet.service
# Observe at least two daemon polls; connection refusal is immediate.
sleep 22
[[ "$(worker_pid flame-executor-manager.service)" == "$before_fem" ]]
[[ "$(worker_pid flame-object-cache.service)" == "$before_cache" ]]
worker_exec systemctl is-active --quiet flame-executor-manager.service
worker_exec systemctl is-active --quiet flame-object-cache.service
restore_config
trap - EXIT
wait_worker_ready "$runtime/flmlet-after-outage.json" "$heartbeat"

"${kube[@]}" patch flameworker "$worker" --type merge -p '{"spec":{"rolloutRevision":"live-flmlet-restart"}}'
wait_worker_ready "$runtime/flmlet-after-restart.json"
after_fem="$(worker_pid flame-executor-manager.service)"
after_cache="$(worker_pid flame-object-cache.service)"
[[ "$after_fem" -gt 0 && "$after_cache" -gt 0 && "$after_fem" != "$before_fem" && "$after_cache" != "$before_cache" ]]
[[ "$(jq -r .status.configHash "$runtime/flmlet-before.json")" != "$(jq -r .status.configHash "$runtime/flmlet-after-restart.json")" ]]

# Rotate the short-lived bootstrap identity without disturbing durable worker credentials.
umask 077
mkdir -p "$runtime/secrets"
bootstrap_token "$runtime/secrets/bootstrap-before.token"
identity_before="$(worker_exec sha256sum /usr/local/flame/conf/flmlet/kubeconfig | cut -d' ' -f1)"
"${kube[@]}" patch secret flame-join --type merge -p '{"stringData":{"expiresAt":"2000-01-01T00:00:00Z"}}' >/dev/null
for attempt in {1..90}; do
  if "${kube[@]}" get secret flame-join -o json | jq -er '.data.token|@base64d' > "$runtime/secrets/bootstrap-after.token" \
    && ! cmp -s "$runtime/secrets/bootstrap-before.token" "$runtime/secrets/bootstrap-after.token"; then break; fi
  [[ "$attempt" != 90 ]] || exit 1
  sleep 2
done
[[ "$(worker_pid flame-executor-manager.service)" == "$after_fem" ]]
[[ "$(worker_pid flame-object-cache.service)" == "$after_cache" ]]
[[ "$(worker_exec sha256sum /usr/local/flame/conf/flmlet/kubeconfig | cut -d' ' -f1)" == "$identity_before" ]]
wait_worker_ready "$runtime/flmlet-after-token-rotation.json"
# A retired token must not authorize creation of another worker, even though the
# original worker continues with its durable scoped identity.
docker cp "$runtime/secrets/bootstrap-before.token" "$worker:/tmp/flmlet-expired.token"
worker_exec sh -ec 'mkdir -p /tmp/flmlet-expired/bin /tmp/flmlet-expired-units; ln -s /usr/local/flame/bin/flame-executor-manager /tmp/flmlet-expired/bin/flame-executor-manager'
address=192.168.250.250
rejected=0
timeout 90 docker exec "$worker" sh -ec 'set +x; trap '\''rm -f /tmp/flmlet-expired.token'\'' EXIT; sh /ci/setup.sh --join "$(cat /tmp/flmlet-expired.token)" --artifacts-dir /ci-artifacts --prefix /tmp/flmlet-expired --systemd-dir /tmp/flmlet-expired-units --worker flame-expired-probe --address "$1"' expired "$address" \
  > "$runtime/flmlet-old-token.log" 2>&1 || rejected=$?
[[ "$rejected" != 0 && "$rejected" != 124 && "$rejected" != 137 ]]
grep -Eq 'HTTP (401|403)' "$runtime/flmlet-old-token.log"
probe="$("${kube[@]}" get flameworker flame-expired-probe --ignore-not-found -o name)"
[[ -z "$probe" ]] || { echo 'Rotated bootstrap token incorrectly created a new worker' >&2; exit 1; }
[[ "$(worker_pid flame-executor-manager.service)" == "$after_fem" ]]
[[ "$(worker_pid flame-object-cache.service)" == "$after_cache" ]]
rm -f "$runtime/secrets/bootstrap-before.token" "$runtime/secrets/bootstrap-after.token"

# Revocation may precede the next spec poll. Explicit 401/403 is authoritative.
"${kube[@]}" patch flameworker "$worker" --type merge -p '{"spec":{"disabled":true}}'
wait_stopped
wait_credentials_removed "$uid"
worker_exec systemctl is-active --quiet flmlet.service
"${kube[@]}" patch flameworker "$worker" --type merge -p '{"spec":{"disabled":false}}'
join_worker "$runtime" rejoin
wait_worker_ready "$runtime/flmlet-after-rejoin.json"
worker_exec systemctl is-enabled --quiet flame-executor-manager.service
worker_exec systemctl is-enabled --quiet flame-object-cache.service

# Deletion revokes the scoped token and shuts down only this host's owned services.
"${kube[@]}" delete flameworker "$worker" --wait=true
wait_stopped
wait_credentials_removed "$uid"
worker_exec systemctl is-active --quiet flmlet.service
worker_exec test -s /usr/local/flame/conf/flmlet/flame-cluster.yaml
python3 - "$runtime" "$before_fem" "$after_fem" "$before_cache" "$after_cache" <<'PY'
import json,sys
from pathlib import Path
r=Path(sys.argv[1]);before=json.loads((r/'flmlet-before.json').read_text());after=json.loads((r/'flmlet-after-restart.json').read_text());rejoin=json.loads((r/'flmlet-after-rejoin.json').read_text())
result={'coverage':'flmlet-host','hostname':rejoin['status']['hostname'],'registered':rejoin['status']['registered'],
        'cacheReady':rejoin['status']['cacheReady'],'configHashBefore':before['status']['configHash'],
        'configHashAfter':after['status']['configHash'],'femPidBefore':int(sys.argv[2]),'femPidAfter':int(sys.argv[3]),
        'cachePidBefore':int(sys.argv[4]),'cachePidAfter':int(sys.argv[5]),
        'sameSpecPreservedPids':True,'transportOutagePreservedPids':True,'disableStoppedServices':True,
        'bootstrapRotationPreservedWorkerPidsAndCredentials':True,'rotatedTokenRejectedForNewWorker':True,
        'newJoinRecoveredWorker':True,'deletionRevokedCredentialsAndStoppedServices':True}
(r/'flmlet-results.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result,indent=2))
PY
