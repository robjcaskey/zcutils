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
{{- if and .Values.image.digest (not (regexMatch "^sha256:[A-Fa-f0-9]{64}$" .Values.image.digest)) -}}
{{- fail "image.digest must be sha256: followed by exactly 64 hexadecimal characters" -}}
{{- end -}}
{{- if .Values.image.digest -}}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest -}}
{{- else -}}
{{- printf "%s:%s" .Values.image.repository (.Values.image.tag | default .Chart.AppVersion) -}}
{{- end -}}
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

{{- define "zcblock-csi.nodeSetupImage" -}}
{{- include "zcblock-csi.mainImage" . -}}
{{- end -}}

{{- define "zcblock-csi.nodeSetupImagePullPolicy" -}}
{{- .Values.image.pullPolicy -}}
{{- end -}}

{{- define "zcblock-csi.nodeSetupConfigMapName" -}}
{{- printf "%s-node-setup" (include "zcblock-csi.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "zcblock-csi.nodeSetupSourceConfigMapName" -}}
{{- if .Values.nodeSetup.developmentBuild.sourceConfigMap -}}
{{- .Values.nodeSetup.developmentBuild.sourceConfigMap -}}
{{- else -}}
{{- printf "%s-zcnblk-source" (include "zcblock-csi.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "zcblock-csi.nodeArtifactCacheName" -}}
{{- printf "%s-module-cache" (include "zcblock-csi.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "zcblock-csi.nodeSetupChecksum" -}}
{{- printf "%s\n%s\n%s\n%s\n%s" (toYaml .Values.nodeSetup) (.Files.Get "files/kmod/zcnblk_client_mod.c") (.Files.Get "files/kmod/zcnblk_shm_abi.h") (.Files.Get "files/kmod/Makefile") (.Files.Get "files/kmod/Kbuild") | sha256sum -}}
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
