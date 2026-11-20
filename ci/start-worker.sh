#!/usr/bin/env bash
# The CI fixture supplies a prepared host; flmlet handles actual services and registration.
set -euo pipefail
source "$(dirname "$0")/worker-helpers.sh"
runtime="${1:?test result directory required}"
: "${E2E_ARTIFACTS:?}"
mkdir -p "$runtime"
artifacts="$(cd "$E2E_ARTIFACTS" && pwd)"
[[ -s "$artifacts/SHA256SUMS" ]]
node_ip="$(kubectl get nodes -o json | jq -r '[.items[0].status.addresses[]|select(.type=="InternalIP")|.address][0]')"
[[ -n "$node_ip" && "$node_ip" != null ]]
docker build -f ci/worker.Dockerfile -t flame-ci-systemd-worker:local ci
docker run -d --name "$worker" --hostname "$worker" --network kind \
  --privileged --cgroupns=private --tmpfs /run --tmpfs /run/lock --cpus=2 --memory=2g \
  --add-host "flame.e2e.test:$node_ip" \
  --mount "type=bind,src=$artifacts,dst=/ci-artifacts,readonly" \
  --mount "type=bind,src=$(pwd)/setup.sh,dst=/ci/setup.sh,readonly" flame-ci-systemd-worker:local >/dev/null
for attempt in {1..60}; do
  if worker_exec systemctl is-active dbus >/dev/null 2>&1; then break; fi
  [[ "$attempt" != 60 ]] || { docker logs "$worker" >&2; exit 1; }
  sleep 1
done
[[ "$(readlink /proc/self/ns/cgroup)" != "$(worker_exec readlink /proc/1/ns/cgroup)" ]]
worker_exec test -w /sys/fs/cgroup/cgroup.procs
worker_exec systemd-run --quiet --wait /bin/true
worker_ip="$(docker inspect "$worker" --format '{{(index .NetworkSettings.Networks "kind").IPAddress}}')"
# The installer must create the Worker through the rotating cluster join token.
if "${kube[@]}" get flameworker "$worker" >/dev/null 2>&1; then
  echo 'CI worker unexpectedly exists before setup.sh --join' >&2
  exit 1
fi
worker_startup_diagnostics() {
  worker_exec systemctl show flmlet.service flame-executor-manager.service flame-object-cache.service \
    --property=Id,LoadState,ActiveState,SubState,Result,ExecMainStatus >&2 || true
  worker_exec journalctl --no-pager -n 100 -u flmlet.service \
    -u flame-executor-manager.service -u flame-object-cache.service >&2 || true
}
if ! join_worker "$runtime"; then
  cat "$runtime/flmlet-join.log" >&2
  worker_startup_diagnostics
  exit 1
fi
if ! wait_worker_ready "$runtime/flmlet-worker-ready.json"; then
  worker_startup_diagnostics
  exit 1
fi
# Let the controller publish the host's cache only after its live status is Ready.
for attempt in {1..60}; do
  if "${kube[@]}" get endpointslices -o json \
    | jq -e --arg ip "$worker_ip" 'any(.items[]; .metadata.labels["kubernetes.io/service-name"]=="flame-1-cache-bootstrap" and any(.endpoints[]; .conditions.ready and any(.addresses[]; .==$ip)))' >/dev/null; then break; fi
  [[ "$attempt" != 60 ]] || exit 1
  sleep 2
done
worker_exec install -d -m 0700 /etc/flame-gateway /tmp/flame-client/.flame /tmp/flame-gateway/.flame
"${kube[@]}" get secret "$flame_name-client" -o json | jq -er '.data["flame.yaml"]|@base64d' \
  | worker_exec_stdin sh -ec 'umask 077; cat > /etc/flame-gateway/flame.yaml; cp /etc/flame-gateway/flame.yaml /tmp/flame-gateway/.flame/flame.yaml'
worker_exec cp /usr/local/flame/conf/flmlet-client.yaml /tmp/flame-client/.flame/flame.yaml
