#!/usr/bin/env bash
set -euo pipefail

values_file="${ZCCUSAN_HELM_VALUES_FILE:-}"
if [[ -n "${values_file}" && ! -r "${values_file}" ]]; then
  echo "Helm values file is not readable: ${values_file}" >&2
  exit 66
fi

helm_repository_name="${ZCCUSAN_HELM_REPOSITORY_NAME:-zccusan}"
helm_repository_url="${ZCCUSAN_HELM_REPOSITORY_URL:-https://robjcaskey.github.io/zcutils}"
chart_reference="${ZCCUSAN_HELM_CHART:-${helm_repository_name}/zcblock-csi}"
release_name="${ZCCUSAN_HELM_RELEASE:-zccusan}"
namespace="${ZCCUSAN_HELM_NAMESPACE:-zccusan}"
image_tag="${ZCCUSAN_IMAGE_TAG:-0.1.2}"
chart_version="${ZCCUSAN_CHART_VERSION:-0.1.2}"

helm repo add "${helm_repository_name}" "${helm_repository_url}" --force-update >/dev/null
helm repo update "${helm_repository_name}" >/dev/null

helm_args=(
  upgrade --install "${release_name}" "${chart_reference}"
  --namespace "${namespace}"
  --create-namespace
  --set-string "image.tag=${image_tag}"
  --wait
  --timeout "${ZCCUSAN_HELM_TIMEOUT:-10m}"
)

if [[ -n "${values_file}" ]]; then
  helm_args+=(--values "${values_file}")
fi

if [[ -n "${chart_version}" ]]; then
  helm_args+=(--version "${chart_version}")
fi

if [[ -n "${ZCCUSAN_TELEMETRY_API_ENDPOINT:-}" ]]; then
  case "${ZCCUSAN_TELEMETRY_API_ENDPOINT}" in
    http://*|https://*) ;;
    *)
      echo "ZCCUSAN_TELEMETRY_API_ENDPOINT must be an HTTP(S) URL" >&2
      exit 64
      ;;
  esac
  helm_args+=(--set-string "telemetry.apiEndpoint=${ZCCUSAN_TELEMETRY_API_ENDPOINT}")
fi

if [[ -n "${ZCCUSAN_COMMUNITY_SURVEY_API_ENDPOINT:-}" ]]; then
  case "${ZCCUSAN_COMMUNITY_SURVEY_API_ENDPOINT}" in
    https://*) ;;
    *)
      echo "ZCCUSAN_COMMUNITY_SURVEY_API_ENDPOINT must be an HTTPS URL" >&2
      exit 64
      ;;
  esac
  helm_args+=(--set-string "communitySurvey.apiEndpoint=${ZCCUSAN_COMMUNITY_SURVEY_API_ENDPOINT}")
fi

if [[ "${ZCCUSAN_HELM_DRY_RUN:-false}" == "true" ]]; then
  helm_args+=(--dry-run)
fi

echo "Installing ${chart_reference} values=${values_file:-chart-defaults} image_tag=${image_tag}" >&2
exec helm "${helm_args[@]}" "$@"
