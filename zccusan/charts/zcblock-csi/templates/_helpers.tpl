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

{{- define "zcblock-csi.telemetryServiceName" -}}
{{- printf "%s-telemetry" (include "zcblock-csi.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "zcblock-csi.telemetryApiEndpoint" -}}
{{- if .Values.telemetry.apiEndpoint -}}
{{- .Values.telemetry.apiEndpoint -}}
{{- else if .Values.telemetryServer.enabled -}}
{{- printf "http://%s:%v/v1/events" (include "zcblock-csi.telemetryServiceName" .) .Values.telemetryServer.port -}}
{{- end -}}
{{- end -}}

{{- define "zcblock-csi.sidecarImage" -}}
{{- printf "%s:%s" .repository .tag -}}
{{- end -}}

{{- define "zcblock-csi.installationSecretName" -}}
{{- printf "%s-zccusan-installation-secret" (include "zcblock-csi.fullname" .) -}}
{{- end -}}

{{- define "zcblock-csi.installationConfigMapName" -}}
{{- printf "%s-zccusan-installation-config" (include "zcblock-csi.fullname" .) -}}
{{- end -}}

{{- define "zcblock-csi.installationId" -}}
{{- if .Values.installation.id -}}
{{- .Values.installation.id -}}
{{- else -}}
{{- $secret_name := include "zcblock-csi.installationSecretName" . -}}
{{- $namespace := include "zcblock-csi.namespace" . -}}
{{- $secret := lookup "v1" "Secret" $namespace $secret_name -}}
{{- if and $secret (hasKey $secret "data") -}}
{{- $value := get $secret.data "ZCCUSAN_INSTALLATION_ID" -}}
{{- if $value -}}
{{- $value | b64dec -}}
{{- else -}}
{{- randAlphaNum 20 -}}
{{- end -}}
{{- else -}}
{{- randAlphaNum 20 -}}
{{- end -}}
{{- end -}}

{{- end -}}
