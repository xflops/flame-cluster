{{- define "flame-cluster.fullname" -}}
{{- default "cluster" .Values.cluster.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- define "flame-cluster.labels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: cluster
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/* Keep partial flames entries consistent without introducing public defaults configuration. */}}
{{- define "flame-cluster.flame-defaults" -}}
name: flame
namespace: ""
version: latest
endpoint: ""
endpointHost: ""
kubernetesEndpoint: ""
rolloutRevision: ""
image: docker.io/xflops/flame-session-manager:latest
storageSize: 20Gi
storageClass: ""
existingClaim: ""
resources:
  requests: {cpu: "1", memory: 1Gi}
  limits: {cpu: "4", memory: 8Gi}
nodeSelector: {}
cacheEndpoints: []
gateway:
  hostname: ""
  className: flame-envoy
  envoyNamespace: envoy-gateway-system
  issuerName: flame-client-ca
  clientAuth: true
{{- end -}}

{{- define "flame-cluster.flames" -}}
{{- $flames := list -}}
{{- $ports := dict -}}
{{- $namespaces := dict -}}
{{- range $index, $entry := .Values.flames -}}
{{- $flame := mergeOverwrite (include "flame-cluster.flame-defaults" $ | fromYaml) (deepCopy $entry) -}}
{{- if gt $index 0 -}}
{{- $first := first $.Values.flames -}}
{{- if and (not $flame.gateway.hostname) $first.gateway $first.gateway.hostname -}}
{{- $_ := set $flame.gateway "hostname" $first.gateway.hostname -}}
{{- end -}}
{{- range $key := list "endpointHost" "kubernetesEndpoint" -}}
{{- if and (not (get $flame $key)) (get $first $key) -}}
{{- $_ := set $flame $key (get $first $key) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $namespace := default (printf "%s-%d" $flame.name (add $index 1)) $flame.namespace -}}
{{- if or (gt (len $namespace) 63) (not (regexMatch "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$" $namespace)) (hasKey $namespaces $namespace) -}}
{{- fail "each flames entry requires a distinct namespace that is a DNS label of at most 63 characters" -}}
{{- end -}}
{{- $_ := set $namespaces $namespace true -}}
{{- $_ := set $flame "namespace" $namespace -}}
{{- $name := printf "%s-%d" $flame.name (add $index 1) -}}
{{- if or (gt (len $name) 32) (not (regexMatch "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$" $name)) -}}
{{- fail "each indexed flames name must be a DNS label of at most 32 characters" -}}
{{- end -}}
{{- $_ := set $flame "name" $name -}}
{{- $_ := set $flame "nodePort" (int (default (add 30080 (mul $index 10)) $flame.nodePort)) -}}
{{- $_ := set $flame.gateway "sessionPort" (int (default (add 30443 (mul $index 10)) $flame.gateway.sessionPort)) -}}
{{- $_ := set $flame.gateway "cachePort" (int (default (add 31443 (mul $index 10)) $flame.gateway.cachePort)) -}}
{{- $_ := required (printf "flames[%d].gateway.hostname is required" $index) $flame.gateway.hostname -}}
{{- $frontend := $flame.nodePort -}}
{{- if $flame.endpoint -}}{{- $frontend = int (last (splitList ":" $flame.endpoint)) -}}{{- end -}}
{{- range $port := list $frontend (add $frontend 1) $flame.gateway.sessionPort $flame.gateway.cachePort -}}
{{- $key := printf "%v" $port -}}
{{- if or (lt (int $port) 30000) (gt (int $port) 32767) (hasKey $ports $key) -}}
{{- fail "flames must use distinct NodePorts within 30000..32767, including each frontend/backend pair" -}}
{{- end -}}
{{- $_ := set $ports $key true -}}
{{- end -}}
{{- $flames = append $flames $flame -}}
{{- end -}}
{{- toJson $flames -}}
{{- end -}}

{{- define "flame-cluster.connections" -}}
{{- $connections := list -}}
{{- $flames := include "flame-cluster.flames" . | fromJsonArray -}}
{{- range $flame := $flames -}}
{{- $directory := printf "/etc/flame/connections/%s" $flame.name -}}
{{- $endpoint := printf "https://%s-session-manager.%s.svc:8080" $flame.name $flame.namespace -}}
{{- if and (eq (len $flames) 1) $.Values.cluster.flame.endpoint -}}
{{- $endpoint = $.Values.cluster.flame.endpoint -}}
{{- end -}}
{{- $connection := dict "id" $flame.name "name" $flame.name "endpoint" $endpoint "displayName" (printf "%s/%s" $flame.namespace $flame.name) "namespace" $flame.namespace "caFile" (printf "%s/ca.crt" $directory) "joinNamespace" $flame.namespace "joinSecret" (printf "%s-join" $flame.name) "clientConfigSecret" (printf "%s-client" $flame.name) -}}
{{- $connections = append $connections $connection -}}
{{- end -}}
{{- toPrettyJson $connections -}}
{{- end -}}
