#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "print-exec-credential" ]]; then
  if [[ $# -ne 2 ]] || ! command -v gcloud >/dev/null 2>&1 ||
     ! command -v jq >/dev/null 2>&1; then
    echo "usage: $0 print-exec-credential GCP_ACCOUNT (requires gcloud and jq)" >&2
    exit 2
  fi
  gcloud config config-helper --account="$2" --format=json |
    jq -ce '{
      apiVersion: "client.authentication.k8s.io/v1",
      kind: "ExecCredential",
      status: {
        expirationTimestamp: .credential.token_expiry,
        token: .credential.access_token
      }
    }'
  exit 0
fi

if [[ "${ALLOW_CLOUD_TEST:-0}" != "1" ]]; then
  echo "set ALLOW_CLOUD_TEST=1 to create the disposable billed GKE resources" >&2
  exit 2
fi

for command in terraform gcloud kubectl helm jq curl base64; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

ARCHITECTURE="${1:-}"
case "${ARCHITECTURE}" in
  amd64)
    CLUSTER_NAME="zccusan-gke-amd64"
    ;;
  arm64)
    CLUSTER_NAME="zccusan-gke-axion"
    ;;
  *)
    echo "usage: $0 amd64|arm64" >&2
    exit 2
    ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/../../../.." && pwd)"
PROJECT_ID="${GCP_PROJECT_ID:-rob-adhoc-81326}"
GCP_ACCOUNT="${GCP_ACCOUNT:-rob@caskey.org}"
GCP_REGION="${GCP_REGION:-us-central1}"
GCP_ZONE="${GCP_ZONE:-us-central1-a}"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
RESULT_DIR="${RESULT_DIR:-${ROOT_DIR}/bench-results/gke-quickstart-${ARCHITECTURE}-${RUN_ID}}"
POST_TEST_HOLD_SECONDS="${POST_TEST_HOLD_SECONDS:-0}"
TF_DATA_DIR="${TF_DATA_DIR:-/mnt/bulk_data/zcutils-cloud-quickstart-terraform/gke/data}"
STATE_DIR="${STATE_DIR:-/mnt/bulk_data/zcutils-cloud-quickstart-terraform/gke/live-state}"
STATE_FILE="${STATE_DIR}/${CLUSTER_NAME}.tfstate"
KUBECONFIG_FILE="$(mktemp /tmp/zccusan-gke-kubeconfig.XXXXXX)"
CLUSTER_CA_FILE="$(mktemp /tmp/zccusan-gke-ca.XXXXXX)"
if [[ -n "${GCP_OPERATOR_CIDR:-}" ]]; then
  OPERATOR_CIDR="${GCP_OPERATOR_CIDR}"
else
  PUBLIC_IP="$(curl -fsS --max-time 10 https://checkip.amazonaws.com | tr -d '\r\n')"
  OPERATOR_CIDR="${PUBLIC_IP}/32"
fi
EXPIRY_EPOCH="$(date -u -d '+2 hours' +%s)"
DESTROYED=0

if [[ ! "${POST_TEST_HOLD_SECONDS}" =~ ^[0-9]+$ ]] ||
   (( POST_TEST_HOLD_SECONDS > 3600 )); then
  echo "POST_TEST_HOLD_SECONDS must be an integer from 0 through 3600" >&2
  exit 2
fi

mkdir -p "${RESULT_DIR}" "${TF_DATA_DIR}" "${STATE_DIR}"
chmod 600 "${KUBECONFIG_FILE}"
export TF_DATA_DIR KUBECONFIG="${KUBECONFIG_FILE}"

refresh_google_token() {
  GOOGLE_OAUTH_ACCESS_TOKEN="$(gcloud auth print-access-token --account="${GCP_ACCOUNT}")"
  export GOOGLE_OAUTH_ACCESS_TOKEN
}

terraform_destroy_with_reauth() {
  local log_file="$1"
  local attempt destroy_rc=1
  : >"${log_file}"
  for attempt in 1 2; do
    refresh_google_token
    if terraform -chdir="${SCRIPT_DIR}" destroy -auto-approve -input=false \
      "${TF_VARS[@]}" >>"${log_file}" 2>&1; then
      return 0
    else
      destroy_rc=$?
    fi
    printf 'terraform destroy attempt %s failed; refreshing GCP token and retrying\n' \
      "${attempt}" >>"${log_file}"
  done
  return "${destroy_rc}"
}

TF_VARS=(
  -var="project_id=${PROJECT_ID}"
  -var="name=${CLUSTER_NAME}"
  -var="region=${GCP_REGION}"
  -var="zone=${GCP_ZONE}"
  -var="operator_cidr=${OPERATOR_CIDR}"
  -var="expiry_epoch=${EXPIRY_EPOCH}"
  -var="worker_architecture=${ARCHITECTURE}"
)
if [[ -n "${GCP_WORKER_MACHINE_TYPE:-}" ]]; then
  TF_VARS+=(-var="worker_machine_type=${GCP_WORKER_MACHINE_TYPE}")
fi
if [[ -n "${GCP_WORKER_DISK_TYPE:-}" ]]; then
  TF_VARS+=(-var="worker_disk_type=${GCP_WORKER_DISK_TYPE}")
fi
if [[ -n "${GCP_WORKER_IMAGE_TYPE:-}" ]]; then
  TF_VARS+=(-var="worker_image_type=${GCP_WORKER_IMAGE_TYPE}")
fi
if [[ -n "${GCP_WORKER_GVNIC:-}" ]]; then
  TF_VARS+=(-var="worker_gvnic=${GCP_WORKER_GVNIC}")
fi
if [[ -n "${GCP_WORKER_ADDITIONAL_NIC:-}" ]]; then
  TF_VARS+=(-var="worker_additional_nic=${GCP_WORKER_ADDITIONAL_NIC}")
fi
if [[ -n "${GCP_WORKER_TIER_1_NETWORKING:-}" ]]; then
  TF_VARS+=(-var="worker_tier_1_networking=${GCP_WORKER_TIER_1_NETWORKING}")
fi

destroy_cluster() {
  local rc=$?
  trap - EXIT INT TERM
  set +e
  if [[ "${DESTROYED}" != "1" ]]; then
    terraform_destroy_with_reauth "${RESULT_DIR}/terraform-destroy.log"
    destroy_rc=$?
    if [[ ${destroy_rc} -eq 0 ]]; then
      DESTROYED=1
    else
      echo "terraform destroy failed; see ${RESULT_DIR}/terraform-destroy.log" >&2
      rc=${destroy_rc}
    fi
  fi
  gcloud container clusters list --project="${PROJECT_ID}" --account="${GCP_ACCOUNT}" \
    --filter="name=${CLUSTER_NAME}" --format=json >"${RESULT_DIR}/postflight-clusters.json" 2>&1
  gcloud compute networks list --project="${PROJECT_ID}" --account="${GCP_ACCOUNT}" \
    --filter="name=${CLUSTER_NAME}" --format=json >"${RESULT_DIR}/postflight-networks.json" 2>&1
  rm -f "${KUBECONFIG_FILE}"
  rm -f "${CLUSTER_CA_FILE}"
  exit "${rc}"
}
trap destroy_cluster EXIT INT TERM

refresh_google_token
terraform -chdir="${SCRIPT_DIR}" init -reconfigure -input=false \
  -backend-config="path=${STATE_FILE}" \
  >"${RESULT_DIR}/terraform-init.log" 2>&1
terraform -chdir="${SCRIPT_DIR}" apply -auto-approve -input=false \
  "${TF_VARS[@]}" \
  >"${RESULT_DIR}/terraform-apply.log" 2>&1
terraform -chdir="${SCRIPT_DIR}" output -json \
  >"${RESULT_DIR}/terraform-outputs.json"

refresh_google_token
CLUSTER_ENDPOINT="$(gcloud container clusters describe "${CLUSTER_NAME}" \
  --zone="${GCP_ZONE}" --project="${PROJECT_ID}" --account="${GCP_ACCOUNT}" \
  --format='value(endpoint)')"
gcloud container clusters describe "${CLUSTER_NAME}" \
  --zone="${GCP_ZONE}" --project="${PROJECT_ID}" --account="${GCP_ACCOUNT}" \
  --format='value(masterAuth.clusterCaCertificate)' | base64 --decode >"${CLUSTER_CA_FILE}"
kubectl config set-cluster "${CLUSTER_NAME}" \
  --server="https://${CLUSTER_ENDPOINT}" \
  --certificate-authority="${CLUSTER_CA_FILE}" --embed-certs=true >/dev/null
kubectl config set-credentials "${CLUSTER_NAME}" \
  --exec-command="${SCRIPT_DIR}/run-live-test.sh" \
  --exec-api-version=client.authentication.k8s.io/v1 \
  --exec-interactive-mode=Never \
  --exec-arg=print-exec-credential \
  --exec-arg="${GCP_ACCOUNT}" >/dev/null
kubectl config set-context "${CLUSTER_NAME}" \
  --cluster="${CLUSTER_NAME}" --user="${CLUSTER_NAME}" >/dev/null
kubectl config use-context "${CLUSTER_NAME}" >/dev/null

RUN_ID="gke-${ARCHITECTURE}-${RUN_ID}" \
RESULT_DIR="${RESULT_DIR}/quickstart" \
CSI_DAEMONSET_PRIORITY_CLASS="" \
CSI_IMAGE_REPOSITORY="${CSI_IMAGE_REPOSITORY:-docker.io/robjcaskey/zcblock-csi}" \
CSI_IMAGE_TAG="${CSI_IMAGE_TAG:-main}" \
CSI_NODE_SETUP_BUILD="${CSI_NODE_SETUP_BUILD:-1}" \
  "${ROOT_DIR}/zccusan/deploy/cloud-quickstart/test-quickstart.sh"

if (( POST_TEST_HOLD_SECONDS > 0 )); then
  printf 'quickstart passed; holding GKE cluster for %s seconds before teardown\n' \
    "${POST_TEST_HOLD_SECONDS}"
  sleep "${POST_TEST_HOLD_SECONDS}"
fi

terraform_destroy_with_reauth "${RESULT_DIR}/terraform-destroy.log"
DESTROYED=1

gcloud container clusters list --project="${PROJECT_ID}" --account="${GCP_ACCOUNT}" \
  --filter="name=${CLUSTER_NAME}" --format=json >"${RESULT_DIR}/postflight-clusters.json"
gcloud compute networks list --project="${PROJECT_ID}" --account="${GCP_ACCOUNT}" \
  --filter="name=${CLUSTER_NAME}" --format=json >"${RESULT_DIR}/postflight-networks.json"

if [[ "$(jq length "${RESULT_DIR}/postflight-clusters.json")" != "0" ]] ||
   [[ "$(jq length "${RESULT_DIR}/postflight-networks.json")" != "0" ]]; then
  echo "postflight found residual GKE or network resources" >&2
  exit 1
fi

echo "${ARCHITECTURE} GKE quickstart passed and was fully destroyed: ${RESULT_DIR}"
