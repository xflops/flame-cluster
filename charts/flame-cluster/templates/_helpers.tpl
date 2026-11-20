{{- define "flame-cluster.api.fullname" -}}
{{- default "cluster" .Values.api.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- define "flame-cluster.api.labels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: api
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}
