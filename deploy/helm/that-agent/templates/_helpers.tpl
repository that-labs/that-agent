{{/*
Expand the name of the chart.
*/}}
{{- define "that-agent.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "that-agent.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "that-agent.labels" -}}
app.kubernetes.io/name: {{ include "that-agent.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ include "that-agent.imageTag" . | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: that-agent
that-agent/managed: "true"
that-agent/name: {{ .Values.agent.name | quote }}
that-agent/type: {{ if eq .Values.agent.role "ephemeral" }}ephemeral{{ else }}persistent{{ end }}
{{- if .Values.agent.parent }}
that-agent/parent: {{ .Values.agent.parent | quote }}
{{- end }}
{{- if .Values.agent.agentRole }}
that-agent/role: {{ .Values.agent.agentRole | quote }}
{{- end }}
{{- end }}

{{/*
Is this a root agent (full stack with infra services)?
*/}}
{{- define "that-agent.isRoot" -}}
{{- eq .Values.agent.role "root" }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "that-agent.selectorLabels" -}}
app.kubernetes.io/name: {{ include "that-agent.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Service account name
*/}}
{{- define "that-agent.serviceAccountName" -}}
{{ include "that-agent.fullname" . }}
{{- end }}

{{/*
Secret name — use existing or generate
*/}}
{{- define "that-agent.secretName" -}}
{{- if .Values.secrets.existingSecret }}
{{- .Values.secrets.existingSecret }}
{{- else }}
{{- include "that-agent.fullname" . }}-secrets
{{- end }}
{{- end }}

{{/*
Git server internal URL
*/}}
{{- define "that-agent.gitServerUrl" -}}
http://{{ include "that-agent.fullname" . }}-git-server.{{ .Release.Namespace }}.svc.cluster.local:9418
{{- end }}

{{/*
Cache proxy internal URL
*/}}
{{- define "that-agent.cacheProxyUrl" -}}
http://{{ include "that-agent.fullname" . }}-cache-proxy.{{ .Release.Namespace }}.svc.cluster.local:3128
{{- end }}

{{/*
BuildKit internal address
*/}}
{{- define "that-agent.buildkitHost" -}}
tcp://{{ include "that-agent.fullname" . }}-buildkit.{{ .Release.Namespace }}.svc.cluster.local:1234
{{- end }}

{{/*
Image tag — user override or "v" + appVersion (matches Docker tag format: v0.3.0)
*/}}
{{- define "that-agent.imageTag" -}}
{{- .Values.agent.image.tag | default (printf "v%s" .Chart.AppVersion) }}
{{- end }}

{{/*
Namespace role name based on access level
*/}}
{{- define "that-agent.roleName" -}}
{{- if eq .Values.accessLevel "readonly" }}
{{- include "that-agent.fullname" . }}-readonly
{{- else }}
{{- include "that-agent.fullname" . }}-runtime
{{- end }}
{{- end }}
