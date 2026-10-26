{{/* 通用命名与标签。 */}}
{{- define "houyidb.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "houyidb.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name (include "houyidb.name" .) | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}

{{- define "houyidb.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
app.kubernetes.io/name: {{ include "houyidb.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "houyidb.selectorLabels" -}}
app.kubernetes.io/name: {{ include "houyidb.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}
