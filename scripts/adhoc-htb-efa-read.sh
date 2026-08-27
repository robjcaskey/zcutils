#!/usr/bin/env bash
set -euo pipefail

# Two-node, topology-explicit block-edge -> userspace WAL leaf gate. The
# default is dual rail; RAIL_MODE=single is an explicit one-NIC topology and
# must never be reported as a dual-rail result.
# This script makes no placement decision in /dev/zcnblk0; the terminal leaf is
# a userspace stage and zcmem is deliberately volatile benchmark media.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROLE="${1:?usage: adhoc-htb-efa-read.sh leaf|client}"
LANES="${LANES:-64}"
HALF=$((LANES / 2))
RAIL_MODE="${RAIL_MODE:-dual}"
case "$RAIL_MODE" in
  single)
    [[ "$LANES" =~ ^[1-9][0-9]*$ ]] && [ "$LANES" -le 64 ] || {
      echo "LANES must be in 1..=64 for the single-rail topology" >&2
      exit 2
    }
    ;;
  dual)
    [[ "$LANES" =~ ^[1-9][0-9]*$ ]] && [ "$LANES" -le 64 ] && [ $((LANES % 2)) -eq 0 ] || {
      echo "LANES must be an even integer in 2..=64 for the dual-rail topology" >&2
      exit 2
    }
    ;;
  *) echo "RAIL_MODE must be single or dual" >&2; exit 2 ;;
esac
QD_PER_LANE="${QD_PER_LANE:-64}"
[[ "$QD_PER_LANE" =~ ^[1-9][0-9]*$ ]] && [ "$QD_PER_LANE" -le 1024 ] || {
  echo "QD_PER_LANE must be in 1..=1024" >&2
  exit 2
}
(( (QD_PER_LANE & (QD_PER_LANE - 1)) == 0 )) || {
  echo "QD_PER_LANE must be a power of two for the strict ring topology" >&2
  exit 2
}
RING_ENTRIES=$((QD_PER_LANE * 2))
RMA_COMPLETION_STRIDE="$QD_PER_LANE"
[ "$RMA_COMPLETION_STRIDE" -le 64 ] || RMA_COMPLETION_STRIDE=64
: "${RAIL0_IP:?set RAIL0_IP to the NUMA-0 private address}"
if [ "$RAIL_MODE" = dual ]; then
  : "${RAIL1_IP:?set RAIL1_IP to the NUMA-1 private address}"
else
  RAIL1_IP="$RAIL0_IP"
fi
RAIL0_NUMA_NODE="${RAIL0_NUMA_NODE:-0}"
RAIL1_NUMA_NODE="${RAIL1_NUMA_NODE:-1}"
DATA_TRANSPORT="${DATA_TRANSPORT:-ofi}"
case "$DATA_TRANSPORT" in ofi|tcp) ;; *) echo "DATA_TRANSPORT must be ofi or tcp" >&2; exit 2 ;; esac
BLOCK_MODE="${BLOCK_MODE:-read}"
case "$BLOCK_MODE" in
  read) BLOCK_READ_PERCENT="${BLOCK_READ_PERCENT:-100}" ;;
  write) BLOCK_READ_PERCENT="${BLOCK_READ_PERCENT:-0}" ;;
  rw) BLOCK_READ_PERCENT="${BLOCK_READ_PERCENT:-50}" ;;
  *) echo "BLOCK_MODE must be read, write, or rw" >&2; exit 2 ;;
esac
[[ "$BLOCK_READ_PERCENT" =~ ^[0-9]+$ ]] && [ "$BLOCK_READ_PERCENT" -le 100 ] || {
  echo "BLOCK_READ_PERCENT must be in 0..=100" >&2
  exit 2
}
[ "$BLOCK_MODE" != read ] || [ "$BLOCK_READ_PERCENT" -eq 100 ] || {
  echo "BLOCK_MODE=read requires BLOCK_READ_PERCENT=100" >&2
  exit 2
}
[ "$BLOCK_MODE" != write ] || [ "$BLOCK_READ_PERCENT" -eq 0 ] || {
  echo "BLOCK_MODE=write requires BLOCK_READ_PERCENT=0" >&2
  exit 2
}
LANE_RAIL_ORDER="${LANE_RAIL_ORDER:-rail1-first}"
case "$LANE_RAIL_ORDER" in
  rail0-first)
    first_rail=0
    second_rail=1
    ;;
  rail1-first)
    first_rail=1
    second_rail=0
    ;;
  *) echo "LANE_RAIL_ORDER must be rail0-first or rail1-first" >&2; exit 2 ;;
esac
BLOCK_HCTX_NUMA_NODE="${BLOCK_HCTX_NUMA_NODE:--1}"
[[ "$BLOCK_HCTX_NUMA_NODE" =~ ^-?[0-9]+$ ]] || {
  echo "BLOCK_HCTX_NUMA_NODE must be an integer" >&2
  exit 2
}

repeat_csv() {
  local value="$1" count="$2" result= i
  for ((i = 0; i < count; i++)); do
    [ "$i" -eq 0 ] || result+=,
    result+="$value"
  done
  printf '%s' "$result"
}

expand_cpu_list() {
  local part first last cpu
  IFS=, read -r -a parts <<<"$1"
  for part in "${parts[@]}"; do
    case "$part" in
      *-*)
        first="${part%-*}"
        last="${part#*-}"
        [[ "$first" =~ ^[0-9]+$ && "$last" =~ ^[0-9]+$ && "$first" -le "$last" ]] || return 2
        for ((cpu = first; cpu <= last; cpu++)); do printf '%s\n' "$cpu"; done
        ;;
      *) [[ "$part" =~ ^[0-9]+$ ]] || return 2; printf '%s\n' "$part" ;;
    esac
  done
}

if [ -n "${LEAF_WORKER_CPU_LIST:-}" ]; then
  leaf_cpu_list="$LEAF_WORKER_CPU_LIST"
elif [ "$LANES" -ne 64 ]; then
  echo "LEAF_WORKER_CPU_LIST is required when LANES is not the historical 64-lane topology" >&2
  exit 2
else
  # Historical c8gn.48xlarge record map. Other shapes must pass an explicit
  # list derived from the two EFA-local NUMA nodes.
  leaf_cpu_list="$(seq 96 3 189 | paste -sd,)"
  leaf_cpu_list="$leaf_cpu_list,$(seq 0 3 93 | paste -sd,)"
fi
mapfile -t leaf_cpus < <(expand_cpu_list "$leaf_cpu_list")
[ "${#leaf_cpus[@]}" -eq "$LANES" ] || {
  echo "LEAF_WORKER_CPU_LIST must supply exactly $LANES CPUs" >&2
  exit 2
}
for ((i = 0; i < LANES; i++)); do
  if [ "$RAIL_MODE" = single ]; then
    wanted_node_var=RAIL0_NUMA_NODE
  elif [ "$i" -lt "$HALF" ]; then
    wanted_node_var="RAIL${first_rail}_NUMA_NODE"
  else
    wanted_node_var="RAIL${second_rail}_NUMA_NODE"
  fi
  wanted_node="${!wanted_node_var}"
  cpu_node=unknown
  for node_path in "/sys/devices/system/cpu/cpu${leaf_cpus[$i]}"/node[0-9]*; do
    [ -e "$node_path" ] || continue
    cpu_node="${node_path##*node}"
    break
  done
  [ "$cpu_node" = "$wanted_node" ] || {
    echo "lane $i leaf CPU ${leaf_cpus[$i]} is NUMA $cpu_node; expected EFA-local NUMA $wanted_node" >&2
    exit 2
  }
done
domain_for_numa() {
  local wanted="$1" path name
  for path in /sys/class/infiniband/*; do
    [ -r "$path/device/numa_node" ] || continue
    [ "$(cat "$path/device/numa_node")" = "$wanted" ] || continue
    name="$(basename "$path")"
    printf '%s-rdm' "$name"
    return 0
  done
  echo "no EFA/RDMA device found on NUMA node $wanted" >&2
  return 1
}
domain0="$(domain_for_numa "$RAIL0_NUMA_NODE")"
if [ "$RAIL_MODE" = dual ]; then
  domain1="$(domain_for_numa "$RAIL1_NUMA_NODE")"
else
  domain1="$domain0"
fi
iface0="${domain0%-rdm}"
iface1="${domain1%-rdm}"
rail_iface0="${RAIL0_INTERFACE:-$iface0}"
rail_iface1="${RAIL1_INTERFACE:-$iface1}"
first_domain_var="domain${first_rail}"
second_domain_var="domain${second_rail}"
first_iface_var="iface${first_rail}"
second_iface_var="iface${second_rail}"
first_rail_iface_var="rail_iface${first_rail}"
second_rail_iface_var="rail_iface${second_rail}"
first_rail_ip_var="RAIL${first_rail}_IP"
second_rail_ip_var="RAIL${second_rail}_IP"
first_domain="${!first_domain_var}"
second_domain="${!second_domain_var}"
first_iface="${!first_iface_var}"
second_iface="${!second_iface_var}"
first_rail_iface="${!first_rail_iface_var}"
second_rail_iface="${!second_rail_iface_var}"
first_rail_ip="${!first_rail_ip_var}"
second_rail_ip="${!second_rail_ip_var}"
if [ "$RAIL_MODE" = single ]; then
  domains="$(repeat_csv "$domain0" "$LANES")"
  efa_ifaces="$iface0"
else
  domains="$(repeat_csv "$first_domain" "$HALF"),$(repeat_csv "$second_domain" "$HALF")"
  efa_ifaces="$first_iface,$second_iface"
fi

case "$ROLE" in
leaf)
  leaf_transport_env=(URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT="$DATA_TRANSPORT")
  leaf_bind="$RAIL0_IP"
  if [ "$DATA_TRANSPORT" = ofi ]; then
    leaf_transport_env+=(
      URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER=efa
      URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT=rdm
      URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ADDRS="$([ "$RAIL_MODE" = single ] && repeat_csv "$RAIL0_IP" "$LANES" || printf '%s,%s' "$(repeat_csv "$first_rail_ip" "$HALF")" "$(repeat_csv "$second_rail_ip" "$HALF")")"
      URING_PLAY_ZCNBLK_WAL_LEAF_OFI_DOMAINS="$domains"
      URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1
      URING_PLAY_OFI_CQ_SLEEP_NS=0
      URING_PLAY_OFI_RMA_READ_QD="$QD_PER_LANE"
      URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=65536
      URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1
      URING_PLAY_OFI_EFA_FABRIC=efa-direct
      FI_EFA_IFACE="$efa_ifaces"
      FI_EFA_USE_DEVICE_RDMA=1
    )
  else
    # One wildcard listener set lets source-bound client lanes select either
    # TCP rail while the userspace leaf remains a single placement-free stage.
    leaf_bind=0.0.0.0
  fi
  exec sudo -n prlimit --memlock=unlimited -- env \
    "${leaf_transport_env[@]}" \
    URING_PLAY_TOPOLOGY_STRICT=1 \
    URING_PLAY_PIN_CPU_LIST="$leaf_cpu_list" \
    URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MIN=256 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MAX=65536 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_WAIT_NS=50000 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_HYSTERESIS_NS=10000000 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB=1 \
    "$ROOT/target/release/zcnblk-wal-leaf" zcmem:4096M "$leaf_bind" 43000 \
    "$LANES" 1 4096 "$LANES" true blocking
  ;;
client)
  : "${LEAF_RAIL0_IP:?set LEAF_RAIL0_IP}"
  if [ "$RAIL_MODE" = dual ]; then
    : "${LEAF_RAIL1_IP:?set LEAF_RAIL1_IP}"
  else
    LEAF_RAIL1_IP="$LEAF_RAIL0_IP"
  fi
  topo="${EXTERNAL_LEAF_TOPOLOGY_ARTIFACT:-/tmp/zc-htb-leaf-topology.log}"
  mkdir -p "$(dirname "$topo")"
  cpu_map="$(for ((i = 0; i < LANES; i++)); do [ "$i" = "$((LANES - 1))" ] && s= || s=,; printf '%s:%s%s' "$i" "${leaf_cpus[$i]}" "$s"; done)"
  if [ "$DATA_TRANSPORT" = ofi ]; then
    first_rail_label="$first_iface/$first_domain"
    second_rail_label="$second_iface/$second_domain"
  else
    first_rail_label="$first_rail_iface/tcp"
    second_rail_label="$second_rail_iface/tcp"
  fi
  if [ "$RAIL_MODE" = single ]; then
    nic_map="$(for ((i = 0; i < LANES; i++)); do [ "$i" = "$((LANES - 1))" ] && s= || s=,; printf '%s:%s%s' "$i" "$first_rail_label" "$s"; done)"
  else
    nic_map="$(for ((i = 0; i < HALF; i++)); do printf '%s:%s,' "$i" "$first_rail_label"; done; for ((i = HALF; i < LANES; i++)); do [ "$i" = "$((LANES - 1))" ] && s= || s=,; printf '%s:%s%s' "$i" "$second_rail_label" "$s"; done)"
  fi
  reported_lane_order="$LANE_RAIL_ORDER"
  [ "$RAIL_MODE" != single ] || reported_lane_order=single-rail0
  printf 'lane_to_worker_cpu=%s\nlane_to_nic=%s\nrail_mode=%s lane_rail_order=%s\ncloud_region=%s availability_zone=%s instance_type=%s placement_group=%s\n' \
    "$cpu_map" "$nic_map" "$RAIL_MODE" "$reported_lane_order" "${CLOUD_REGION:-unreported}" "${AVAILABILITY_ZONE:-unreported}" \
    "${INSTANCE_TYPE:-unreported}" "${PLACEMENT_GROUP:-unconfirmed}" >"$topo"
  first_leaf_ip_var="LEAF_RAIL${first_rail}_IP"
  second_leaf_ip_var="LEAF_RAIL${second_rail}_IP"
  first_leaf_ip="${!first_leaf_ip_var}"
  second_leaf_ip="${!second_leaf_ip_var}"
  if [ "$RAIL_MODE" = single ]; then
    leaf_addrs="$(repeat_csv "$LEAF_RAIL0_IP:43000" "$LANES")"
  else
    leaf_addrs="$(repeat_csv "$first_leaf_ip:43000" "$HALF"),$(repeat_csv "$second_leaf_ip:43000" "$HALF")"
  fi
  leaf_source_addrs=
  transport_env=(URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT="$DATA_TRANSPORT")
  if [ "$DATA_TRANSPORT" = ofi ]; then
    transport_env+=(
      URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER=efa
      URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT=rdm
      URING_PLAY_ZCNBLK_SHM_OFI_DOMAINS="$domains"
      URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS=1
      URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD="$QD_PER_LANE"
      URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1
      URING_PLAY_OFI_SELECTIVE_COMPLETION=1
      URING_PLAY_OFI_RMA_READ_COMPLETION_STRIDE="$RMA_COMPLETION_STRIDE"
      URING_PLAY_OFI_RMA_DEFER_TAIL_COMPLETION=1
      URING_PLAY_OFI_RMA_READ_MORE=1
      URING_PLAY_OFI_CQ_SLEEP_NS=0
      URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=65536
      URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1
      URING_PLAY_OFI_EFA_FABRIC=efa-direct
      FI_EFA_IFACE="$efa_ifaces"
      FI_EFA_USE_DEVICE_RDMA=1
    )
  else
    : "${SOURCE_RAIL0_IP:?set SOURCE_RAIL0_IP for dual-rail TCP}"
    if [ "$RAIL_MODE" = dual ]; then
      : "${SOURCE_RAIL1_IP:?set SOURCE_RAIL1_IP for dual-rail TCP}"
    else
      SOURCE_RAIL1_IP="$SOURCE_RAIL0_IP"
    fi
    first_source_ip_var="SOURCE_RAIL${first_rail}_IP"
    second_source_ip_var="SOURCE_RAIL${second_rail}_IP"
    if [ "$RAIL_MODE" = single ]; then
      leaf_source_addrs="$(repeat_csv "$SOURCE_RAIL0_IP" "$LANES")"
    else
      leaf_source_addrs="$(repeat_csv "${!first_source_ip_var}" "$HALF"),$(repeat_csv "${!second_source_ip_var}" "$HALF")"
    fi
  fi
  exec env COORDINATION_SCOPE=dedicated-adhoc BUILD=0 TARGET_MEMLOCK_UNLIMITED=1 \
    "${transport_env[@]}" \
    PERF_STAT="${PERF_STAT:-0}" \
    OUTDIR="${OUTDIR:-/tmp/zc-htb-efa-read}" LANES="$LANES" REPEATS="${REPEATS:-3}" \
    OPS_PER_WORKER="${OPS_PER_WORKER:-1500000}" IODEPTH="$QD_PER_LANE" RING_ENTRIES="$RING_ENTRIES" \
    KERNEL_QUEUE_DEPTH="$QD_PER_LANE" KERNEL_PIPELINE_DEPTH="$QD_PER_LANE" SHM_RING_ENTRIES="$RING_ENTRIES" \
    HCTX_NUMA_NODE="$BLOCK_HCTX_NUMA_NODE" \
    SHM_PAYLOAD_ENTRIES=4096 SIZE_MIB=4096 MODE="$BLOCK_MODE" READ_PERCENT="$BLOCK_READ_PERCENT" \
    BACKEND=wal-tcp START_LOCAL_LEAF=0 LEAF_ADDR="$LEAF_RAIL0_IP" LEAF_PORT=43000 \
    LEAF_ADDRS="$leaf_addrs" LEAF_SOURCE_ADDRS="$leaf_source_addrs" EXTERNAL_LEAF_TOPOLOGY_ARTIFACT="$topo" \
    ZCCUSAN_TOPOLOGY_TRANSPORT="$DATA_TRANSPORT-$RAIL_MODE-rail" ZCCUSAN_TOPOLOGY_PATH_COUNT="$([ "$RAIL_MODE" = single ] && printf 1 || printf 2)" \
    REPRESENTATIVE=1 BUFFER_MODE=hugetlb URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED=1 \
    URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 \
    URING_PLAY_ZCNBLK_SHM_ARENA_BACKING=hugetlb URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW="$QD_PER_LANE" \
    URING_PLAY_ZCNBLK_SHM_READ_BATCH=1 URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_MIN=1 \
    URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH=32768 \
    URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_RECORDS=128 URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_MIN_BATCH_RECORDS=64 \
    URING_PLAY_ZCNBLK_SHM_WAL_TRANSPORT_GREEDY=1 \
    URING_PLAY_BLOCKBENCH_COMPLETION_BATCH="$QD_PER_LANE" \
    URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS="$QD_PER_LANE" \
    URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE=0 \
    "$ROOT/scripts/zcnblk-shm-block-bench.sh"
  ;;
*) echo "ROLE must be leaf or client" >&2; exit 2 ;;
esac
