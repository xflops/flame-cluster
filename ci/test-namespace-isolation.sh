#!/usr/bin/env bash
# Exercise namespace boundaries with real credentials and both deployed Flames.
set -euo pipefail
umask 077
runtime="${1:?test result directory required}"
source "$(dirname "$0")/worker-helpers.sh"
base=http://127.0.0.1:18090
mkdir -p "$runtime/secrets"

curl -fsS "$base/api/connections" > "$runtime/connections.json"
jq -e '[.items[]|{id,namespace}] == [{id:"flame-1",namespace:"flame-1"},{id:"flame-2",namespace:"flame-2"}]' "$runtime/connections.json" >/dev/null
curl -fsS "$base/api/connections/flame-2/nodes?pageSize=100" > "$runtime/second-flame-nodes.json"
jq -e '.total==0 and (.items|length)==0' "$runtime/second-flame-nodes.json" >/dev/null
for index in 1 2; do
  curl -fsS "$base/api/connections/flame-$index/join" > "$runtime/secrets/join-$index.json"
done
python3 - "$runtime/secrets" <<'PYJOIN'
import base64,json,shlex,sys
from pathlib import Path
root=Path(sys.argv[1])
for index in (1,2):
    result=json.loads((root/f'join-{index}.json').read_text())
    command=shlex.split(result['command'])
    assert command[:9]==['curl','-fsSL','https://xflops.io/cluster/setup.sh','|','sudo','sh','-s','--','--join']
    token=command[command.index('--join')+1]
    envelope=json.loads(base64.b64decode(token,validate=True))
    assert envelope['namespace']==f'flame-{index}'
    assert envelope['cluster']==f'flame-{index}'
print('Each join command enrolls only into its selected namespace and Flame.')
PYJOIN
rm -f "$runtime/secrets/join-1.json" "$runtime/secrets/join-2.json"

# Downloads must match the selected namespace's actual bundle, not another client.
jq -e 'all(.items[]; .clientConfigAvailable==true)' "$runtime/connections.json" >/dev/null
for index in 1 2; do
  curl -fsS -D "$runtime/secrets/client-headers-$index.txt" \
    "$base/api/connections/flame-$index/flame.yaml" > "$runtime/secrets/client-download-$index.yaml"
  kubectl -n "flame-$index" get secret "flame-$index-client" -o json \
    | jq -ej '.data["flame.yaml"]|@base64d' > "$runtime/secrets/client-expected-$index.yaml"
  cmp "$runtime/secrets/client-download-$index.yaml" "$runtime/secrets/client-expected-$index.yaml"
  grep -Eqi '^cache-control:.*no-store' "$runtime/secrets/client-headers-$index.txt"
  grep -Eqi '^content-disposition:.*attachment' "$runtime/secrets/client-headers-$index.txt"
done
! cmp -s "$runtime/secrets/client-download-1.yaml" "$runtime/secrets/client-download-2.yaml"
rm -f "$runtime"/secrets/client-download-*.yaml "$runtime"/secrets/client-expected-*.yaml "$runtime"/secrets/client-headers-*.txt


# Worker credentials may observe their own objects, never another namespace.
worker_exec_stdin python3 - <<'PYWORKER'
import base64,json,ssl
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request,urlopen
config=json.loads(Path('/usr/local/flame/conf/flmlet/kubeconfig').read_text())
cluster=config['clusters'][0]['cluster'];user=config['users'][0]['user']
context=ssl.create_default_context(cadata=base64.b64decode(cluster['certificate-authority-data']).decode())
def status(path):
    request=Request(cluster['server']+path,headers={'Authorization':'Bearer '+user['token']})
    try:
        with urlopen(request,context=context,timeout=15) as response:
            return response.status
    except HTTPError as error:
        return error.code
prefix='/apis/flame.xflops.io/v1alpha1/namespaces/'
assert status(prefix+'flame-1/flameworkers/flame-ci-worker')==200
assert status(prefix+'flame-2/flameworkers/flame-ci-worker')==403
assert status(prefix+'flame-2/flameclusters/flame-2')==403
assert status('/api/v1/namespaces/flame-1/secrets/flame-1-client')==403
print('Worker identity is restricted to its namespace and cannot read client Secrets.')
PYWORKER

# Both legitimate clients work, but neither identity authenticates to the other Gateway.
cleanup_clients() {
  worker_exec rm -f /tmp/flame-1-client.yaml /tmp/flame-2-client.yaml /tmp/flame-cross-1.yaml /tmp/flame-cross-2.yaml
  rm -f "$runtime"/secrets/namespace-client-*.ca "$runtime"/secrets/namespace-client-*.crt "$runtime"/secrets/namespace-client-*.key
}
trap cleanup_clients EXIT
for index in 1 2; do
  kubectl -n "flame-$index" get secret "flame-$index-client" -o json \
    | jq -er '.data["flame.yaml"]|@base64d' \
    | worker_exec_stdin sh -c 'umask 077; cat > "$1"' copy "/tmp/flame-$index-client.yaml"
  timeout 30 docker exec "$worker" /usr/local/flame/bin/flmctl --config "/tmp/flame-$index-client.yaml" list -a >/dev/null
done
worker_exec sh -ec 'sed -e s/:30443/:30453/g -e s/:31443/:31453/g /tmp/flame-1-client.yaml > /tmp/flame-cross-1.yaml; sed -e s/:30453/:30443/g -e s/:31453/:31443/g /tmp/flame-2-client.yaml > /tmp/flame-cross-2.yaml; chmod 600 /tmp/flame-cross-1.yaml /tmp/flame-cross-2.yaml'
for index in 1 2; do
  rejected=0
  timeout 30 docker exec "$worker" /usr/local/flame/bin/flmctl --config "/tmp/flame-cross-$index.yaml" list -a > "$runtime/cross-namespace-$index.log" 2>&1 || rejected=$?
  [[ "$rejected" != 0 && "$rejected" != 124 && "$rejected" != 137 ]] || { echo 'Expected an explicit cross-namespace client rejection' >&2; exit 1; }
done
# A nonzero SDK exit alone could be a network failure. Require the TLS peer's
# explicit certificate rejection while validating its own CA and hostname.
node_ip="$(kubectl get nodes -o json | jq -r '[.items[0].status.addresses[]|select(.type=="InternalIP")|.address][0]')"
for index in 1 2; do
  for pair in 'ca.crt:ca' 'tls.crt:crt' 'tls.key:key'; do
    key="${pair%:*}"; extension="${pair#*:}"
    kubectl -n "flame-$index" get secret "flame-$index-client" -o json \
      | jq -er --arg key "$key" '.data[$key]|@base64d' > "$runtime/secrets/namespace-client-$index.$extension"
  done
  target=30453
  [[ "$index" == 1 ]] || target=30443
  rejected=0
  printf 'GET / HTTP/1.1\r\nHost: flame.e2e.test\r\nConnection: close\r\n\r\n' \
    | timeout 20 openssl s_client -connect "$node_ip:$target" -servername flame.e2e.test \
        -verify_hostname flame.e2e.test -verify_return_error -tls1_3 -quiet \
        -CAfile "$runtime/secrets/namespace-client-$index.ca" \
        -cert "$runtime/secrets/namespace-client-$index.crt" \
        -key "$runtime/secrets/namespace-client-$index.key" \
        > "$runtime/cross-namespace-tls-$index.log" 2>&1 || rejected=$?
  [[ "$rejected" != 0 && "$rejected" != 124 && "$rejected" != 137 ]] || { echo 'Expected an explicit cross-namespace TLS rejection' >&2; exit 1; }
  grep -Eqi 'alert.*(bad certificate|certificate unknown|access denied|unknown ca)|SSL alert number (42|46|48|49)' "$runtime/cross-namespace-tls-$index.log"
done
printf '%s\n' '{"namespacedJoinCommands":true,"workerNamespaceRestricted":true,"workerCannotReadClientSecrets":true,"selectedClientDownloadsMatch":true,"crossNamespaceClientsRejected":true}' > "$runtime/namespace-isolation.json"
