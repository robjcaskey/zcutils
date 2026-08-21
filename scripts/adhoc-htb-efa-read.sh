#!/usr/bin/env bash
set -euo pipefail

# Two-node, two-rail, topology-explicit block-edge -> userspace WAL leaf gate.
# This script makes no placement decision in /dev/zcnblk0; the terminal leaf is
# a userspace stage and zcmem is deliberately volatile benchmark media.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROLE="${1:?usage: adhoc-htb-efa-read.sh leaf|client}"
LANES="${LANES:-64}"
HALF=$((LANES / 2))
[ "$LANES" -eq 64 ] || { echo "this record topology currently requires LANES=64" >&2; exit 2; }
: "${RAIL0_IP:?set RAIL0_IP to the NUMA-0 private address}"
: "${RAIL1_IP:?set RAIL1_IP to the NUMA-1 private address}"

repeat_csv() {
  local value="$1" count="$2" result= i
  for ((i = 0; i < count; i++)); do
    [ "$i" -eq 0 ] || result+=,
    result+="$value"
  done
  printf '%s' "$result"
}

cpus="$(seq 96 3 189 | paste -sd,)"
cpus="$cpus,$(seq 0 3 93 | paste -sd,)"
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
domain0="$(domain_for_numa 0)"
domain1="$(domain_for_numa 1)"
iface0="${domain0%-rdm}"
iface1="${domain1%-rdm}"
domains="$(repeat_csv "$domain1" "$HALF"),$(repeat_csv "$domain0" "$HALF")"

case "$ROLE" in
leaf)
  exec sudo -n prlimit --memlock=unlimited -- env \
    URING_PLAY_TOPOLOGY_STRICT=1 \
    URING_PLAY_PIN_CPU_LIST="$cpus" \
    URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
    URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT=ofi \
    URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER=efa \
    URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT=rdm \
    URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ADDRS="$(repeat_csv "$RAIL1_IP" "$HALF"),$(repeat_csv "$RAIL0_IP" "$HALF")" \
    URING_PLAY_ZCNBLK_WAL_LEAF_OFI_DOMAINS="$domains" \
    URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MIN=256 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MAX=65536 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_WAIT_NS=50000 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_HYSTERESIS_NS=10000000 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
    URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1 \
    URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB=1 \
    URING_PLAY_OFI_CQ_SLEEP_NS=0 \
    URING_PLAY_OFI_RMA_READ_QD=64 \
    URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=65536 \
    URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 \
    URING_PLAY_OFI_EFA_FABRIC=efa-direct \
    FI_EFA_IFACE="$iface1,$iface0" FI_EFA_USE_DEVICE_RDMA=1 \
    "$ROOT/target/release/zcnblk-wal-leaf" zcmem:4096M "$RAIL0_IP" 43000 \
    "$LANES" 1 4096 "$LANES" true blocking
  ;;
client)
  : "${LEAF_RAIL0_IP:?set LEAF_RAIL0_IP}"
  : "${LEAF_RAIL1_IP:?set LEAF_RAIL1_IP}"
  topo="${EXTERNAL_LEAF_TOPOLOGY_ARTIFACT:-/tmp/zc-htb-leaf-topology.log}"
  cpu_map="$(for i in $(seq 0 31); do printf '%s:%s,' "$i" "$((96+i*3))"; done; for i in $(seq 32 63); do [ "$i" = 63 ] && s= || s=,; printf '%s:%s%s' "$i" "$(((i-32)*3))" "$s"; done)"
  nic_map="$(for i in $(seq 0 31); do printf '%s:%s/%s,' "$i" "$iface1" "$domain1"; done; for i in $(seq 32 63); do [ "$i" = 63 ] && s= || s=,; printf '%s:%s/%s%s' "$i" "$iface0" "$domain0" "$s"; done)"
  printf 'lane_to_worker_cpu=%s\nlane_to_nic=%s\n' "$cpu_map" "$nic_map" >"$topo"
  leaf_addrs="$(repeat_csv "$LEAF_RAIL1_IP:43000" "$HALF"),$(repeat_csv "$LEAF_RAIL0_IP:43000" "$HALF")"
  exec env COORDINATION_SCOPE=dedicated-adhoc BUILD=0 TARGET_MEMLOCK_UNLIMITED=1 \
    PERF_STAT="${PERF_STAT:-0}" \
    OUTDIR="${OUTDIR:-/tmp/zc-htb-efa-read}" LANES="$LANES" REPEATS="${REPEATS:-3}" \
    OPS_PER_WORKER="${OPS_PER_WORKER:-1500000}" IODEPTH=64 RING_ENTRIES=128 \
    KERNEL_QUEUE_DEPTH=64 KERNEL_PIPELINE_DEPTH=64 SHM_RING_ENTRIES=128 \
    SHM_PAYLOAD_ENTRIES=4096 SIZE_MIB=4096 MODE=read READ_PERCENT=100 \
    BACKEND=wal-tcp START_LOCAL_LEAF=0 LEAF_ADDR="$LEAF_RAIL0_IP" LEAF_PORT=43000 \
    LEAF_ADDRS="$leaf_addrs" EXTERNAL_LEAF_TOPOLOGY_ARTIFACT="$topo" \
    REPRESENTATIVE=1 BUFFER_MODE=hugetlb URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED=1 \
    URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 \
    URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=ofi URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER=efa \
    URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT=rdm URING_PLAY_ZCNBLK_SHM_OFI_DOMAINS="$domains" \
    URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS=1 URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD=64 \
    URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1 URING_PLAY_OFI_SELECTIVE_COMPLETION=1 \
    URING_PLAY_OFI_RMA_READ_COMPLETION_STRIDE=64 URING_PLAY_OFI_RMA_DEFER_TAIL_COMPLETION=1 \
    URING_PLAY_OFI_RMA_READ_MORE=1 URING_PLAY_OFI_CQ_SLEEP_NS=0 \
    URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=65536 URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 \
    URING_PLAY_OFI_EFA_FABRIC=efa-direct FI_EFA_IFACE="$iface1,$iface0" FI_EFA_USE_DEVICE_RDMA=1 \
    URING_PLAY_ZCNBLK_SHM_ARENA_BACKING=hugetlb URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW=64 \
    URING_PLAY_ZCNBLK_SHM_READ_BATCH=1 URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_MIN=1 \
    URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH=32768 \
    URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_RECORDS=128 URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_MIN_BATCH_RECORDS=64 \
    URING_PLAY_ZCNBLK_SHM_WAL_TRANSPORT_GREEDY=1 URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS=64 \
    URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE=0 \
    "$ROOT/scripts/zcnblk-shm-block-bench.sh"
  ;;
*) echo "ROLE must be leaf or client" >&2; exit 2 ;;
esac
