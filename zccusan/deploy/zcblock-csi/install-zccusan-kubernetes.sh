#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/../../.." && pwd)"

telemetry_environment="${ZCCUSAN_TELEMETRY_ENV:-}"
case "${telemetry_environment}" in
  dev|prod) ;;
  *)
    echo "ZCCUSAN_TELEMETRY_ENV must be set to dev or prod" >&2
    exit 64
    ;;
esac

values_file="${ZCCUSAN_TELEMETRY_VALUES_FILE:-${repo_root}/zccusan/charts/zcblock-csi/values-telemetry-${telemetry_environment}.yaml}"
if [[ ! -r "${values_file}" ]]; then
  echo "telemetry values file is not readable: ${values_file}" >&2
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
  --values "${values_file}"
  --set-string "image.tag=${image_tag}"
  --wait
  --timeout "${ZCCUSAN_HELM_TIMEOUT:-10m}"
)

if [[ -n "${chart_version}" ]]; then
  helm_args+=(--version "${chart_version}")
fi

if [[ -n "${ZCCUSAN_SURVEY_BACKEND_URL:-}" ]]; then
  case "${ZCCUSAN_SURVEY_BACKEND_URL}" in
    https://*) ;;
    *)
      echo "ZCCUSAN_SURVEY_BACKEND_URL override must be an https URL" >&2
      exit 64
      ;;
  esac
  helm_args+=(
    --set-string "communitySurvey.backendUrl=${ZCCUSAN_SURVEY_BACKEND_URL}"
    --set-string "telemetryServer.upstreamUrl=${ZCCUSAN_SURVEY_BACKEND_URL}"
  )
fi

if [[ "${ZCCUSAN_HELM_DRY_RUN:-false}" == "true" ]]; then
  helm_args+=(--dry-run)
fi

echo "Installing ${chart_reference} with telemetry_environment=${telemetry_environment} values=${values_file} image_tag=${image_tag}" >&2
exec helm "${helm_args[@]}" "$@"
