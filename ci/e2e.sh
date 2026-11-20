#!/usr/bin/env bash
# This disposable CI fixture acts as the SRE: dependencies are not chart dependencies.
set -euo pipefail
: "${GITHUB_SHA:?}" "${E2E_ARTIFACTS:?}"
runtime="${RUNNER_TEMP:-/tmp}/flame-ci"
mkdir -p "$runtime"
kind create cluster --name flame-ci --wait 120s
gzip -dc "$E2E_ARTIFACTS/flame-images-amd64.tar.gz" > "$runtime/images.tar"
kind load image-archive "$runtime/images.tar" --name flame-ci
rm "$runtime/images.tar"
helm upgrade --install cert-manager oci://quay.io/jetstack/charts/cert-manager \
  --namespace cert-manager --create-namespace --set crds.enabled=true --wait --timeout 5m
helm upgrade --install envoy-gateway oci://docker.io/envoyproxy/gateway-helm \
  --namespace envoy-gateway-system --create-namespace --wait --timeout 5m
kubectl apply -f ci/dependencies.yaml
kubectl -n cert-manager wait certificate/flame-client-ca --for=condition=Ready --timeout=120s
kubectl create namespace flame-system
kubectl -n cert-manager get secret flame-client-ca -o json \
  | jq '{apiVersion:"v1",kind:"ConfigMap",metadata:{name:"flame-client-ca",namespace:"flame-system"},data:{"ca.crt":(.data["ca.crt"]|@base64d)}}' \
  | kubectl apply -f -
node_ip="$(kubectl get nodes -o json | jq -r '.items[0].status.addresses[]|select(.type=="InternalIP")|.address')"
cat > "$runtime/install-values.yaml" <<VALUES
flames:
  - name: flame
    storageClass: standard
    endpoint: $node_ip:30080
  - name: flame
    storageClass: standard
    endpointHost: $node_ip
VALUES
# Use the node-reachable API endpoint in the installer kubeconfig. Kind's host
# kubeconfig points to loopback, which cannot be advertised to remote workers/UI.
install_kubeconfig="$runtime/install-kubeconfig"
(umask 077; kubectl config view --raw --minify --flatten > "$install_kubeconfig")
install_cluster="$(kubectl config view --minify -o 'jsonpath={.clusters[0].name}')"
kubectl --kubeconfig "$install_kubeconfig" config set-cluster "$install_cluster" \
  --server="https://$node_ip:6443" >/dev/null
# Exercise the public installer with this run's previously built artifacts.
sh setup.sh --artifacts-dir "$E2E_ARTIFACTS" --hostname flame.e2e.test \
  --values "$runtime/install-values.yaml" --image-tag "sha-$GITHUB_SHA-amd64" \
  --kubeconfig "$install_kubeconfig" --endpoint "$node_ip:30080" \
  --console-endpoint https://flame-1-session-manager.flame-ns-1.svc:8080 \
  --kubernetes-endpoint "https://$node_ip:6443"
kubectl -n flame-ns-1 wait flamecluster/flame-1 --for=condition=Ready --timeout=10m
kubectl -n flame-ns-2 wait flamecluster/flame-2 --for=condition=Ready --timeout=10m
# Envoy exposes matching listener/Service/NodePorts on every routable node.
envoy_selector='gateway.envoyproxy.io/owning-gateway-name=flame-1-gateway,gateway.envoyproxy.io/owning-gateway-namespace=flame-ns-1'
kubectl -n envoy-gateway-system get services -l "$envoy_selector" -o json > "$runtime/envoy-services.json"
jq -e '.items | length==1 and all(.[];
  .spec.type=="NodePort" and .spec.externalTrafficPolicy=="Cluster"
  and ([.spec.ports[] | {port,nodePort}] | sort_by(.port))==[{port:30443,nodePort:30443},{port:31443,nodePort:31443}])' "$runtime/envoy-services.json" >/dev/null
kubectl -n envoy-gateway-system get deployments -l "$envoy_selector" -o json > "$runtime/envoy-deployments.json"
jq -e '.items | length==1 and all(.[];
  (.spec.template.spec.hostNetwork // false)==false
  and all((.spec.template.spec.containers + (.spec.template.spec.initContainers // []))[];
    all(.ports[]?; (has("hostPort") | not) and (has("hostIP") | not))))' "$runtime/envoy-deployments.json" >/dev/null
kubectl -n flame-system port-forward service/flame-ui 18090:80 > "$runtime/forward.log" 2>&1 &
forward=$!
trap 'kill "$forward" 2>/dev/null || true' EXIT
for attempt in {1..30}; do
  if curl -fsS http://127.0.0.1:18090/healthz >/dev/null; then break; fi
  sleep 1
done
curl -fsS http://127.0.0.1:18090/ -o "$runtime/index.html"
test -s "$runtime/index.html"
curl -fsS http://127.0.0.1:18090/api/connection | jq -e '.endpoint=="https://flame-1-session-manager.flame-ns-1.svc:8080"'
# Read shapes alone are only deployment smoke coverage. Require real execution,
# persisted metadata, and browser-driven mutations against this deployed FLM.
bash ci/functional.sh "$runtime"
