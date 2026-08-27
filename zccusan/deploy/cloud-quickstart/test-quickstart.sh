#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
CHART_VERSION="${CHART_VERSION:-0.1.6}"
NAMESPACE="${NAMESPACE:-zccusan}"
RUN_ID="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
RESULT_DIR="${RESULT_DIR:-${ROOT_DIR}/bench-results/managed-k8s-quickstart-${RUN_ID}}"
KEEP_TEST_RESOURCES="${KEEP_TEST_RESOURCES:-0}"

QUICKSTART_DIR="${ROOT_DIR}/zccusan/deploy/zcblock-csi/getting-started"
NAMESPACE_FILE="${QUICKSTART_DIR}/namespace.yaml"
MEDIA_GRANT_FILE="${QUICKSTART_DIR}/media-grant.yaml"
STORAGE_PROFILE_FILE="${QUICKSTART_DIR}/storage-profile.yaml"
PVC_FILE="${QUICKSTART_DIR}/mirror-pvc.yaml"
FIO_FILE="${QUICKSTART_DIR}/mirror-fio.yaml"
PGBENCH_FILE="${QUICKSTART_DIR}/mirror-pgbench.yaml"

mkdir -p "${RESULT_DIR}"

cleanup() {
  local rc=$?
  set +e
  kubectl -n "${NAMESPACE}" get pods,pvc -o wide >"${RESULT_DIR}/objects-final.log" 2>&1
  kubectl -n "${NAMESPACE}" describe daemonset zccusan-zcblock-csi-node >"${RESULT_DIR}/csi-daemonset-describe.log" 2>&1
  kubectl -n "${NAMESPACE}" logs -l app.kubernetes.io/name=zcblock-csi \
    --all-containers=true --prefix=true >"${RESULT_DIR}/csi-all-containers.log" 2>&1
  kubectl get zcvolumes.storage.zcutils.io -o yaml >"${RESULT_DIR}/zcvolumes-final.yaml" 2>&1
  if [[ "${KEEP_TEST_RESOURCES}" != "1" ]]; then
    kubectl delete -f "${PGBENCH_FILE}" --ignore-not-found --wait=false >/dev/null 2>&1
    kubectl delete -f "${FIO_FILE}" --ignore-not-found --wait=false >/dev/null 2>&1
    kubectl delete -f "${PVC_FILE}" --ignore-not-found --wait=false >/dev/null 2>&1
    kubectl delete -f "${STORAGE_PROFILE_FILE}" --ignore-not-found --wait=false >/dev/null 2>&1
    kubectl delete -f "${MEDIA_GRANT_FILE}" --ignore-not-found --wait=false >/dev/null 2>&1
  fi
  exit "${rc}"
}
trap cleanup EXIT

for command in kubectl helm jq; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

kubectl version -o yaml >"${RESULT_DIR}/kubernetes-version.yaml"
CAN_GET_PODS="$(kubectl auth can-i get pods --namespace "${NAMESPACE}")"
CAN_GET_POD_LOGS="$(kubectl auth can-i get pods/log --namespace "${NAMESPACE}")"
{
  printf 'get pods: %s\n' "${CAN_GET_PODS}"
  printf 'get pods/log: %s\n' "${CAN_GET_POD_LOGS}"
} >"${RESULT_DIR}/kubernetes-auth.log"
if [[ "${CAN_GET_PODS}" != "yes" ]] || [[ "${CAN_GET_POD_LOGS}" != "yes" ]]; then
  echo "Kubernetes identity cannot read pod status and logs; see ${RESULT_DIR}/kubernetes-auth.log" >&2
  exit 1
fi
kubectl get nodes -o wide >"${RESULT_DIR}/nodes-before.log"

mapfile -t READY_NODES < <(
  kubectl get nodes -o json | jq -r '
    .items[]
    | select(any(.status.conditions[]; .type == "Ready" and .status == "True"))
    | .metadata.name
  ' | sort
)

if (( ${#READY_NODES[@]} < 3 )); then
  echo "quickstart requires three Ready Kubernetes nodes; found ${#READY_NODES[@]}" >&2
  exit 2
fi

SERVER_A="${READY_NODES[0]}"
SERVER_B="${READY_NODES[1]}"
CLIENT="${READY_NODES[2]}"

kubectl label node "${SERVER_A}" storage.zcutils.io/example-server=true --overwrite
kubectl label node "${SERVER_B}" storage.zcutils.io/example-server=true --overwrite
kubectl label node "${CLIENT}" storage.zcutils.io/example-client=true --overwrite

{
  printf 'server_a=%s\n' "${SERVER_A}"
  printf 'server_b=%s\n' "${SERVER_B}"
  printf 'client=%s\n' "${CLIENT}"
  kubectl get nodes "${SERVER_A}" "${SERVER_B}" "${CLIENT}" \
    -o 'custom-columns=NAME:.metadata.name,ZONE:.metadata.labels.topology\.kubernetes\.io/zone,ARCH:.status.nodeInfo.architecture,KERNEL:.status.nodeInfo.kernelVersion,OS:.status.nodeInfo.osImage,INTERNAL_IP:.status.addresses[?(@.type=="InternalIP")].address'
} >"${RESULT_DIR}/selected-topology.log"

kubectl apply -f "${NAMESPACE_FILE}"
helm repo add zcutils https://robjcaskey.github.io/zcutils --force-update
helm repo update zcutils
HELM_INSTALL_ARGS=(
  upgrade --install zccusan zcutils/zcblock-csi
  --version "${CHART_VERSION}"
  --namespace "${NAMESPACE}"
  --wait
  --timeout 300s
)
if [[ -n "${CSI_DAEMONSET_PRIORITY_CLASS+x}" ]]; then
  HELM_INSTALL_ARGS+=(--set-string "daemonset.priorityClassName=${CSI_DAEMONSET_PRIORITY_CLASS}")
fi
if [[ -n "${CSI_IMAGE_REPOSITORY:-}" ]]; then
  HELM_INSTALL_ARGS+=(--set-string "image.repository=${CSI_IMAGE_REPOSITORY}")
fi
if [[ -n "${CSI_IMAGE_TAG:-}" ]]; then
  HELM_INSTALL_ARGS+=(--set-string "image.tag=${CSI_IMAGE_TAG}")
fi
if [[ "${CSI_NODE_SETUP_BUILD:-0}" == "1" ]]; then
  HELM_INSTALL_ARGS+=(
    --set-string "nodeSetup.moduleSource.type=build"
    --set "nodeSetup.developmentBuild.enabled=true"
    --set "nodeSetup.developmentBuild.installHostDependencies=true"
  )
fi
helm "${HELM_INSTALL_ARGS[@]}"

kubectl -n "${NAMESPACE}" rollout status daemonset/zccusan-zcblock-csi-node --timeout=300s
kubectl -n "${NAMESPACE}" get pods -o wide >"${RESULT_DIR}/csi-pods.log"
kubectl -n "${NAMESPACE}" get pods -l app.kubernetes.io/name=zcblock-csi -o json \
  >"${RESULT_DIR}/csi-pods.json"

kubectl apply -f "${MEDIA_GRANT_FILE}"
kubectl apply -f "${STORAGE_PROFILE_FILE}"
kubectl wait --for=jsonpath='{.status.phase}'=Ready \
  storageprofile/getting-started-mirror-ram --timeout=180s 2>/dev/null || true
kubectl apply -f "${PVC_FILE}"
kubectl apply -f "${FIO_FILE}"

if ! kubectl -n "${NAMESPACE}" wait --for=jsonpath='{.status.phase}'=Succeeded \
  pod/zc-mirror-fio --timeout=360s; then
  kubectl -n "${NAMESPACE}" describe pod zc-mirror-fio >"${RESULT_DIR}/fio-describe.log" 2>&1
  kubectl -n "${NAMESPACE}" logs zc-mirror-fio --all-containers=true \
    >"${RESULT_DIR}/fio.log" 2>&1 || true
  exit 1
fi
kubectl -n "${NAMESPACE}" logs zc-mirror-fio >"${RESULT_DIR}/fio.log"
kubectl -n "${NAMESPACE}" get pod zc-mirror-fio -o yaml >"${RESULT_DIR}/fio-pod.yaml"
kubectl delete -f "${FIO_FILE}" --wait=true

kubectl apply -f "${PGBENCH_FILE}"
if ! kubectl -n "${NAMESPACE}" wait --for=jsonpath='{.status.phase}'=Succeeded \
  pod/zc-mirror-pgbench --timeout=360s; then
  kubectl -n "${NAMESPACE}" describe pod zc-mirror-pgbench >"${RESULT_DIR}/pgbench-describe.log" 2>&1
  kubectl -n "${NAMESPACE}" logs zc-mirror-pgbench --all-containers=true \
    >"${RESULT_DIR}/pgbench.log" 2>&1 || true
  exit 1
fi
kubectl -n "${NAMESPACE}" logs zc-mirror-pgbench >"${RESULT_DIR}/pgbench.log"
kubectl -n "${NAMESPACE}" get pod zc-mirror-pgbench -o yaml >"${RESULT_DIR}/pgbench-pod.yaml"

kubectl -n "${NAMESPACE}" get pods -o wide >"${RESULT_DIR}/objects-success.log"
echo "quickstart passed; evidence: ${RESULT_DIR}"
