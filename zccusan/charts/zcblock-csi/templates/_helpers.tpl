{{- define "zcblock-csi.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "zcblock-csi.fullname" -}}
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

{{- define "zcblock-csi.namespace" -}}
{{- default .Release.Namespace .Values.namespace.name -}}
{{- end -}}

{{- define "zcblock-csi.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "zcblock-csi.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "zcblock-csi.labels" -}}
app.kubernetes.io/name: {{ include "zcblock-csi.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | quote }}
{{- end -}}

{{- define "zcblock-csi.selectorLabels" -}}
app.kubernetes.io/name: {{ include "zcblock-csi.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "zcblock-csi.pluginDir" -}}
{{- if .Values.pluginDir -}}
{{- .Values.pluginDir -}}
{{- else -}}
{{- printf "%s/%s" .Values.kubeletDir (printf "plugins/%s" .Values.driverName) -}}
{{- end -}}
{{- end -}}

{{- define "zcblock-csi.mainImage" -}}
{{- printf "%s:%s" .Values.image.repository (.Values.image.tag | default .Chart.AppVersion) -}}
{{- end -}}

{{- define "zcblock-csi.sidecarImage" -}}
{{- printf "%s:%s" .repository .tag -}}
{{- end -}}
