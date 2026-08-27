#!/usr/bin/env bash
set -euo pipefail

for command in gcloud jq kubectl; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

PROJECT_ID="${GCP_PROJECT_ID:-rob-adhoc-81326}"
GCP_ZONE="${GCP_ZONE:-us-central1-a}"
NAMESPACE="${NAMESPACE:-zccusan}"
IMAGE="${CSI_IMAGE_REPOSITORY:-docker.io/robjcaskey/zcblock-csi}:${CSI_IMAGE_TAG:-main}"
PROBE_IMAGE="${PROBE_IMAGE:-docker.io/robjcaskey/zccusan-storage-test:0.1.6}"
RESULT_DIR="${RESULT_DIR:?set RESULT_DIR to an evidence directory}"
LANES="${LANES:-16}"
WORKERS="${WORKERS:-16}"
BYTES_PER_CONNECTION="${BYTES_PER_CONNECTION:-1G}"
EXTENT_BYTES="${EXTENT_BYTES:-1M}"
REPEATS="${REPEATS:-3}"

if [[ ! "${LANES}" =~ ^[1-9][0-9]*$ ]] || (( LANES > 64 )); then
  echo "LANES must be in 1..64" >&2
  exit 2
fi
if [[ ! "${WORKERS}" =~ ^[1-9][0-9]*$ ]] || (( WORKERS > 32 )); then
  echo "WORKERS must be in 1..32" >&2
  exit 2
fi
if [[ ! "${REPEATS}" =~ ^[1-9][0-9]*$ ]] || (( REPEATS > 10 )); then
  echo "REPEATS must be in 1..10" >&2
  exit 2
fi

mkdir -p "${RESULT_DIR}"

mapfile -t NODES < <(kubectl get nodes -o name | sed 's#^node/##' | sort)
if (( ${#NODES[@]} != 3 )); then
  echo "dual-rail quickstart benchmark requires exactly three nodes; found ${#NODES[@]}" >&2
  exit 2
fi
TARGET_NODE="${NODES[0]}"
CLIENT_NODE="${NODES[2]}"

describe_node() {
  gcloud compute instances describe "$1" \
    --project="${PROJECT_ID}" --zone="${GCP_ZONE}" --format=json
}

TARGET_JSON="$(describe_node "${TARGET_NODE}")"
CLIENT_JSON="$(describe_node "${CLIENT_NODE}")"
for role_json in "${TARGET_JSON}" "${CLIENT_JSON}"; do
  if [[ "$(jq '.networkInterfaces | length' <<<"${role_json}")" != "2" ]] ||
     [[ "$(jq '[.networkInterfaces[].nicType == "GVNIC"] | all' <<<"${role_json}")" != "true" ]]; then
    echo "every benchmark node must expose exactly two gVNIC interfaces" >&2
    exit 1
  fi
done

TARGET_RAIL0_IP="$(jq -r '.networkInterfaces[0].networkIP' <<<"${TARGET_JSON}")"
TARGET_RAIL1_IP="$(jq -r '.networkInterfaces[1].networkIP' <<<"${TARGET_JSON}")"
CLIENT_RAIL0_IP="$(jq -r '.networkInterfaces[0].networkIP' <<<"${CLIENT_JSON}")"
CLIENT_RAIL1_IP="$(jq -r '.networkInterfaces[1].networkIP' <<<"${CLIENT_JSON}")"

cleanup_pods() {
  kubectl -n "${NAMESPACE}" delete pod \
    zc-net-probe-target zc-net-probe-client \
    zc-wal-target-r0 zc-wal-target-r1 zc-wal-client-r0 zc-wal-client-r1 \
    --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
trap cleanup_pods EXIT INT TERM

create_probe() {
  local name="$1"
  local node="$2"
  local overrides
  overrides="$(jq -cn --arg node "${node}" --arg image "${PROBE_IMAGE}" '{
    spec: {
      nodeName: $node,
      hostNetwork: true,
      dnsPolicy: "ClusterFirstWithHostNet",
      containers: [{name: "probe", image: $image}]
    }
  }')"
  kubectl -n "${NAMESPACE}" run "${name}" --restart=Never \
    --image="${PROBE_IMAGE}" --overrides="${overrides}" \
    --command -- sleep 600 >/dev/null
  kubectl -n "${NAMESPACE}" wait --for=condition=Ready "pod/${name}" --timeout=120s >/dev/null
}

probe_interfaces() {
  local pod="$1"
  # The program below is intentionally expanded only by the shell in the pod.
  # shellcheck disable=SC2016
  kubectl -n "${NAMESPACE}" exec "${pod}" -- sh -c '
    default_dev=$(awk '\''$2 == "00000000" { print $1; exit }'\'' /proc/net/route)
    printf "default=%s\n" "$default_dev"
    for path in /sys/class/net/*; do
      dev=${path##*/}
      driver=$(readlink -f "$path/device/driver" 2>/dev/null || true)
      case "$driver" in */gve) printf "gve=%s\n" "$dev";; esac
    done
  '
}

create_probe zc-net-probe-target "${TARGET_NODE}"
create_probe zc-net-probe-client "${CLIENT_NODE}"
TARGET_PROBE="$(probe_interfaces zc-net-probe-target)"
CLIENT_PROBE="$(probe_interfaces zc-net-probe-client)"
TARGET_RAIL0_DEV="$(sed -n 's/^default=//p' <<<"${TARGET_PROBE}")"
CLIENT_RAIL0_DEV="$(sed -n 's/^default=//p' <<<"${CLIENT_PROBE}")"
TARGET_RAIL1_DEV="$(sed -n 's/^gve=//p' <<<"${TARGET_PROBE}" | grep -Fxv "${TARGET_RAIL0_DEV}" | head -1)"
CLIENT_RAIL1_DEV="$(sed -n 's/^gve=//p' <<<"${CLIENT_PROBE}" | grep -Fxv "${CLIENT_RAIL0_DEV}" | head -1)"
if [[ -z "${TARGET_RAIL0_DEV}" || -z "${TARGET_RAIL1_DEV}" ||
      -z "${CLIENT_RAIL0_DEV}" || -z "${CLIENT_RAIL1_DEV}" ]]; then
  echo "failed to map both guest gVNIC devices" >&2
  exit 1
fi
cleanup_pods

jq -n \
  --arg target_node "${TARGET_NODE}" --arg client_node "${CLIENT_NODE}" \
  --arg target_r0_ip "${TARGET_RAIL0_IP}" --arg target_r1_ip "${TARGET_RAIL1_IP}" \
  --arg client_r0_ip "${CLIENT_RAIL0_IP}" --arg client_r1_ip "${CLIENT_RAIL1_IP}" \
  --arg target_r0_dev "${TARGET_RAIL0_DEV}" --arg target_r1_dev "${TARGET_RAIL1_DEV}" \
  --arg client_r0_dev "${CLIENT_RAIL0_DEV}" --arg client_r1_dev "${CLIENT_RAIL1_DEV}" \
  --argjson lanes "${LANES}" --argjson workers "${WORKERS}" \
  '{
    schema_version: 1,
    topology: "gke-c4-dual-gvnic-direct-userspace-wal",
    target: {node:$target_node, rails:[{index:0,ip:$target_r0_ip,dev:$target_r0_dev},{index:1,ip:$target_r1_ip,dev:$target_r1_dev}]},
    client: {node:$client_node, rails:[{index:0,ip:$client_r0_ip,dev:$client_r0_dev},{index:1,ip:$client_r1_ip,dev:$client_r1_dev}]},
    lane_to_worker: "lane modulo worker count within each rail process",
    target_cpu_map: {rail0:"0-15",rail1:"16-31"},
    client_cpu_map: {rail0:"0-15",rail1:"16-31"},
    lanes_per_rail:$lanes,
    workers_per_rail:$workers
  }' >"${RESULT_DIR}/topology.json"

pod_manifest() {
  local name="$1" node="$2" mode="$3" rail="$4" ip="$5" source_ip="$6" dev="$7" base_port="$8" cpu_list="$9"
  local command
  if [[ "${mode}" == "recv" ]]; then
    command="zcwal-extent-recv"
  else
    command="zcwal-extent-send"
  fi
  jq -n \
    --arg namespace "${NAMESPACE}" --arg name "${name}" --arg node "${node}" \
    --arg image "${IMAGE}" --arg command "${command}" --arg ip "${ip}" \
    --arg source_ip "${source_ip}" --arg dev "${dev}" --arg cpu_list "${cpu_list}" \
    --arg base_port "${base_port}" --arg lanes "${LANES}" --arg bytes "${BYTES_PER_CONNECTION}" \
    --arg extent "${EXTENT_BYTES}" --arg workers "${WORKERS}" --arg rail "${rail}" \
    '{
      apiVersion:"v1", kind:"Pod",
      metadata:{name:$name,namespace:$namespace,labels:{"app.kubernetes.io/name":"zc-wal-dual-rail-bench","zcutils.io/rail":$rail}},
      spec:{
        nodeName:$node, hostNetwork:true, dnsPolicy:"ClusterFirstWithHostNet", restartPolicy:"Never",
        terminationGracePeriodSeconds:1,
        containers:[{
          name:"bench", image:$image, imagePullPolicy:"IfNotPresent",
          command:["/usr/local/bin/zcutils"],
          args:[$command,$ip,$base_port,$lanes,"1",$bytes,$extent,$workers,"true","stream","uring"],
          env:[
            {name:"URING_PLAY_PIN_CPUS",value:"1"},
            {name:"URING_PLAY_PIN_CPU_LIST",value:$cpu_list},
            {name:"URING_PLAY_SOURCE_IP",value:$source_ip},
            {name:"URING_PLAY_ROUTE_PROBE",value:"1"},
            {name:"URING_PLAY_EXPECT_ROUTE_DEV",value:$dev},
            {name:"URING_PLAY_EXPECT_ROUTE_SRC",value:$source_ip},
            {name:"URING_PLAY_TOPOLOGY_FATAL",value:"1"},
            {name:"URING_PLAY_ZCWAL_FRAME_BYTES",value:"1M"},
            {name:"URING_PLAY_ZCWAL_URING_RECV_PIPELINE",value:"1"},
            {name:"ZCCUSAN_BENCHMARK_RUN_ID",value:("gke-dual-gvnic-"+$rail)},
            {name:"ZCCUSAN_BENCHMARK_RAIL_INDEX",value:$rail},
            {name:"ZCCUSAN_BENCHMARK_RAIL_COUNT",value:"2"},
            {name:"ZCCUSAN_TOPOLOGY_TRANSPORT",value:"tcp-dual-gvnic"},
            {name:"ZCCUSAN_TOPOLOGY_PATH_COUNT",value:"2"},
            {name:"ZCCUSAN_TOPOLOGY_CLASS",value:"direct"},
            {name:"ZCCUSAN_PLACEMENT_SCOPE",value:"same-zone"}
          ]
        }]
      }
    }'
}

for ((repeat = 1; repeat <= REPEATS; repeat++)); do
  repeat_dir="${RESULT_DIR}/repeat-${repeat}"
  mkdir -p "${repeat_dir}"
  cleanup_pods

  pod_manifest zc-wal-target-r0 "${TARGET_NODE}" recv 0 "${TARGET_RAIL0_IP}" "${TARGET_RAIL0_IP}" "${TARGET_RAIL0_DEV}" 26400 0-15 | kubectl apply -f - >/dev/null
  pod_manifest zc-wal-target-r1 "${TARGET_NODE}" recv 1 "${TARGET_RAIL1_IP}" "${TARGET_RAIL1_IP}" "${TARGET_RAIL1_DEV}" 27400 16-31 | kubectl apply -f - >/dev/null
  kubectl -n "${NAMESPACE}" wait --for=condition=Ready pod/zc-wal-target-r0 pod/zc-wal-target-r1 --timeout=120s >/dev/null

  pod_manifest zc-wal-client-r0 "${CLIENT_NODE}" send 0 "${TARGET_RAIL0_IP}" "${CLIENT_RAIL0_IP}" "${CLIENT_RAIL0_DEV}" 26400 0-15 | kubectl apply -f - >/dev/null
  pod_manifest zc-wal-client-r1 "${CLIENT_NODE}" send 1 "${TARGET_RAIL1_IP}" "${CLIENT_RAIL1_IP}" "${CLIENT_RAIL1_DEV}" 27400 16-31 | kubectl apply -f - >/dev/null

  kubectl -n "${NAMESPACE}" wait --for=jsonpath='{.status.phase}'=Succeeded pod/zc-wal-client-r0 pod/zc-wal-client-r1 --timeout=300s >/dev/null
  kubectl -n "${NAMESPACE}" wait --for=jsonpath='{.status.phase}'=Succeeded pod/zc-wal-target-r0 pod/zc-wal-target-r1 --timeout=300s >/dev/null
  for role in target client; do
    for rail in r0 r1; do
      kubectl -n "${NAMESPACE}" logs "zc-wal-${role}-${rail}" >"${repeat_dir}/${role}-${rail}.log"
    done
  done
done

cleanup_pods
trap - EXIT INT TERM

{
  for log in "${RESULT_DIR}"/repeat-*/client-r*.log; do
    repeat="$(basename "$(dirname "${log}")")"
    rail="$(basename "${log}" .log)"
    sed -n "s/^zcwal-extent-send-summary: /${repeat} ${rail} /p" "${log}"
  done
} >"${RESULT_DIR}/summaries.log"

echo "dual-rail WAL benchmark complete: ${RESULT_DIR}"
