{{- define "zccusan-chaos-toolbox.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "zccusan-chaos-toolbox.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "zccusan-chaos-toolbox.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "zccusan-chaos-toolbox.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "zccusan-chaos-toolbox.labels" -}}
app.kubernetes.io/name: {{ include "zccusan-chaos-toolbox.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | quote }}
{{- end -}}

{{- define "zccusan-chaos-toolbox.selectorLabels" -}}
app.kubernetes.io/name: {{ include "zccusan-chaos-toolbox.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "zccusan-chaos-toolbox.image" -}}
{{- if and .Values.image.digest (not (regexMatch "^sha256:[A-Fa-f0-9]{64}$" .Values.image.digest)) -}}
{{- fail "image.digest must be sha256: followed by exactly 64 hexadecimal characters" -}}
{{- end -}}
{{- if .Values.image.digest -}}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest -}}
{{- else -}}
{{- printf "%s:%s" .Values.image.repository (.Values.image.tag | default .Chart.AppVersion) -}}
{{- end -}}
{{- end -}}
