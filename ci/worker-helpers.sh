#!/usr/bin/env bash
# A real Linux systemd host on the disposable Kind network.
worker=flame-ci-worker
flame_namespace=flame-1
flame_name=flame-1
kube=(kubectl -n "$flame_namespace")
management_kube=(kubectl -n flame-system)
worker_exec() { docker exec "$worker" "$@"; }
worker_exec_stdin() { docker exec -i "$worker" "$@"; }
worker_cli() {
  timeout 60 docker exec "$worker" /usr/local/flame/bin/flmctl \
    --config /usr/local/flame/conf/flmlet-client.yaml "$@"
}
wait_worker_ready() {
  local output="${1:?status output path required}" previous="${2:-}"
  for attempt in {1..90}; do
    if "${kube[@]}" get flameworker "$worker" -o json > "$output" \
      && jq -e --arg worker "$worker" --arg previous "$previous" '.metadata.generation as $generation | .status.observedGeneration==$generation
        and .status.hostname==$worker and .status.registered and .status.cacheReady
        and (.status.configHash|length)==64 and .status.fem.activeState=="active"
        and .status.objectCache.activeState=="active" and .status.lastHeartbeatTime!=$previous
        and any(.status.conditions[]; .type=="Ready" and .status=="True" and .observedGeneration==$generation)' "$output" >/dev/null; then return; fi
    sleep 2
  done
  cat "$output" >&2
  return 1
}
wait_cluster_ready() {
  for attempt in {1..120}; do
    if "${kube[@]}" get flamecluster "$flame_name" -o json \
      | jq -e '.metadata.generation as $generation | .status.observedGeneration==$generation
        and (.status.controlPlaneCa | type=="string" and length>0)
        and any(.status.conditions[]; .type=="Ready" and .status=="True" and .observedGeneration==$generation)' >/dev/null; then return; fi
    sleep 2
  done
  echo 'Cluster did not return to Ready with published control-plane TLS trust' >&2
  return 1
}
bootstrap_token() {
  local output="${1:?token output path required}"
  for attempt in {1..90}; do
    if "${kube[@]}" get secret "$flame_name-join" -o json \
      | jq -er '.data.token | @base64d' > "$output" 2>/dev/null; then return; fi
    sleep 2
  done
  echo 'Operator did not publish the cluster join token' >&2
  return 1
}
join_worker() (
  set -euo pipefail
  umask 077
  local runtime="${1:?runtime required}" label="${2:-join}" token_file address
  mkdir -p "$runtime/secrets"
  token_file="$(mktemp "$runtime/secrets/flmlet-token.XXXXXX")"
  trap 'rm -f "$token_file"' EXIT
  bootstrap_token "$token_file"
  chmod 0600 "$token_file"
  address="$(docker inspect "$worker" --format '{{(index .NetworkSettings.Networks "kind").IPAddress}}')"
  docker cp "$token_file" "$worker:/tmp/flmlet-join.token"
  # No Worker exists before the first join. The operator creates its scoped identity.
  # Explicitly adopt the fixture's prepared upstream runtime and vendor units.
  # Allow the upstream installer its bounded runtime before configuration/restart.
  # Keep the inline token out of Docker argv, command traces, and result artifacts.
  timeout 2100 docker exec "$worker" sh -ec 'set +x; trap '\''rm -f /tmp/flmlet-join.token'\'' EXIT; sh /ci/setup.sh --join "$(cat /tmp/flmlet-join.token)" --artifacts-dir /ci-artifacts --prefix /usr/local/flame --adopt-existing --worker flame-ci-worker --address "$1" --manage-object-cache --max-executors 2 --shims host' join "$address" \
    > "$runtime/flmlet-$label.log" 2>&1
)
