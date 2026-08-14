#!/usr/bin/env bash
set -euo pipefail

ROOT=/home/rob/zcutils
RUN=zcutils-rmadirect-dualcard-adhoc-c8gn48-20260813T0059Z
KEY=/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519
CLIENT=ubuntu@3.151.54.65
LEAF=ubuntu@18.225.65.76
CLIENT_ROOT=/home/ubuntu/zcutils
REMOTE_RUN="$CLIENT_ROOT/bench-results/$RUN"
LOCAL_RUN="$ROOT/bench-results/$RUN"

label="${1:?label}"
qd="${2:?per-worker qd}"
wait_min="${3:?wait minimum}"
hot_poll="${4:?hot poll 0/1}"
progress_spins="${5:?progress spins}"
ops="${6:-300000}"
repeats="${7:-1}"
representative="${8:-0}"
lanes="${9:-16}"
domain="${10:-efa_0-rdm}"
iface="${11:-efa_0}"
leaf_ip="${12:-172.31.39.134}"
leaf_cpus="${13:-0-15}"
port="${14:-42000}"
read_immediate="${15:-1}"
extent_fill_us="${16:-20}"
minimum_batch_records="${17:-64}"
ring_mode="${18:-normal}"
control_port=$((port + 1000))
control_prefix="${control_port%???}"

client_cpus=""
target_cpus=""
kernel_cpus=""
hctx_node=-1
if [[ "$lanes" == 16 && "$iface" == efa_0 ]]; then
	hctx_node=0
	client_cpus=0,6,12,18,24,30,36,42,48,54,60,66,72,78,84,90
	target_cpus=1,7,13,19,25,31,37,43,49,55,61,67,73,79,85,91
	kernel_cpus=2,8,14,20,26,32,38,44,50,56,62,68,74,80,86,92
elif [[ "$lanes" == 32 && "$iface" == efa_0 ]]; then
	hctx_node=0
	client_cpus=0,3,6,9,12,15,18,21,24,27,30,33,36,39,42,45,48,51,54,57,60,63,66,69,72,75,78,81,84,87,90,93
	target_cpus=1,4,7,10,13,16,19,22,25,28,31,34,37,40,43,46,49,52,55,58,61,64,67,70,73,76,79,82,85,88,91,94
	kernel_cpus=2,5,8,11,14,17,20,23,26,29,32,35,38,41,44,47,50,53,56,59,62,65,68,71,74,77,80,83,86,89,92,95
elif [[ "$lanes" == 16 && "$iface" == efa_1 ]]; then
	hctx_node=1
	client_cpus=96,102,108,114,120,126,132,138,144,150,156,162,168,174,180,186
	target_cpus=97,103,109,115,121,127,133,139,145,151,157,163,169,175,181,187
	kernel_cpus=98,104,110,116,122,128,134,140,146,152,158,164,170,176,182,188
fi

ring_entries=$((qd * 2))
shm_entries="$ring_entries"
payload_entries=4096
if (( payload_entries < qd * 8 )); then
	payload_entries=$((qd * 8))
fi
leaf_log="$LOCAL_RUN/zcnblk0-tuning/${label}.leaf.log"
client_log="$LOCAL_RUN/zcnblk0-tuning/${label}.client.console.log"
mkdir -p "$(dirname "$leaf_log")"

ssh_opts=(-o StrictHostKeyChecking=no -o ServerAliveInterval=15 -i "$KEY")
leaf_ssh_pid=""
cleanup() {
	local status=$?
	if [[ -n "$leaf_ssh_pid" ]] && kill -0 "$leaf_ssh_pid" 2>/dev/null; then
		kill -TERM "$leaf_ssh_pid" 2>/dev/null || true
		wait "$leaf_ssh_pid" 2>/dev/null || true
	fi
	exit "$status"
}
trap cleanup EXIT INT TERM

ssh "${ssh_opts[@]}" "$LEAF" "
cd '$CLIENT_ROOT'
exec env \\
URING_PLAY_PIN_CPU_LIST='$leaf_cpus' \\
URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \\
URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT=ofi \\
URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER=efa \\
URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT=rdm \\
URING_PLAY_ZCNBLK_WAL_LEAF_SPIN_READS=1 \\
URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 \\
URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \\
URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1 \\
URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB=1 \\
URING_PLAY_OFI_DOMAIN='$domain' \\
URING_PLAY_OFI_CONTROL_PORT_OFFSET=1000 URING_PLAY_OFI_CQ_SLEEP_NS=0 \\
URING_PLAY_OFI_RMA_READ_QD='$qd' \\
URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=65536 \\
URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 \\
FI_EFA_IFACE='$iface' FI_EFA_USE_DEVICE_RDMA=1 \\
URING_PLAY_OFI_EFA_FABRIC=efa-direct \\
target/release/zcnblk-wal-leaf zcmem:4G '$leaf_ip' '$port' '$lanes' 1 4096 '$lanes' true blocking
" >"$leaf_log" 2>&1 &
leaf_ssh_pid=$!

ready=0
for _ in $(seq 1 100); do
	if ! kill -0 "$leaf_ssh_pid" 2>/dev/null; then
		wait "$leaf_ssh_pid" || true
		echo "leaf exited before client startup; see $leaf_log" >&2
		exit 1
	fi
	listeners="$(ssh "${ssh_opts[@]}" "$LEAF" \
		"ss -ltnH | awk '\$4 ~ /:${control_prefix}[0-9][0-9][0-9]\$/ { n++ } END { print n+0 }'" 2>/dev/null || true)"
	if [[ "$listeners" =~ ^[0-9]+$ ]] && (( listeners >= lanes )); then
		ready=1
		break
	fi
	sleep 0.05
done
(( ready == 1 )) || { echo "leaf listeners were not ready" >&2; exit 1; }

topology="$REMOTE_RUN/zcnblk0-quick/leaf-topology.log"
if [[ "$lanes" == 32 ]]; then
	topology="$REMOTE_RUN/zcnblk0-sweep/leaf-topology-l32.log"
fi
outdir="$REMOTE_RUN/zcnblk0-tuning/$label"
ssh "${ssh_opts[@]}" "$CLIENT" "
cd '$CLIENT_ROOT'
exec env \\
OUTDIR='$outdir' \\
COORDINATION_SCOPE=dedicated-adhoc \\
LANES='$lanes' REPEATS='$repeats' OPS_PER_WORKER='$ops' IODEPTH='$qd' RING_ENTRIES='$ring_entries' \\
CLIENT_CPU_LIST='$client_cpus' TARGET_CPU_LIST='$target_cpus' KERNEL_CPU_LIST='$kernel_cpus' \\
HCTX_NUMA_NODE='$hctx_node' \\
BLOCK_RING_MODE='$ring_mode' BLOCK_ENGINE=uring-fixed \\
URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE=0 \\
URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS='$wait_min' \\
URING_PLAY_BLOCKBENCH_CQE_HOT_POLL='$hot_poll' \\
URING_PLAY_BLOCKBENCH_CQE_HOT_POLL_PROGRESS_SPINS='$progress_spins' \\
SHM_RING_ENTRIES='$shm_entries' KERNEL_QUEUE_DEPTH='$qd' \\
KERNEL_PIPELINE_DEPTH='$qd' KERNEL_QUEUES='$lanes' \\
SIZE_MIB=$((lanes * 128)) REGION_BYTES_PER_WORKER=67108864 \\
BACKEND=wal-tcp START_LOCAL_LEAF=0 MODE=read READ_PERCENT=100 \\
SHM_PAYLOAD_ENTRIES='$payload_entries' \\
URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH=$((2048 * lanes)) \\
URING_PLAY_ZCNBLK_SHM_READ_BATCH=1 \\
URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_US=0 \\
URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_MIN=1 \\
KICK_BATCH=128 REPRESENTATIVE='$representative' \\
POLL_US=1000 BUSY_POLL_US=1000 BUSY_HYSTERESIS_US=10000 KERNEL_POLL_US=1000 \\
BUFFER_MODE=hugetlb LEAF_ADDR='$leaf_ip' LEAF_PORT='$port' \\
EXTERNAL_LEAF_TOPOLOGY_ARTIFACT='$topology' \\
URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED=1 \\
URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=ofi \\
URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER=efa \\
URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT=rdm \\
URING_PLAY_OFI_DOMAIN='$domain' URING_PLAY_OFI_CQ_SLEEP_NS=0 \\
URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=65536 \\
URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 \\
FI_EFA_IFACE='$iface' FI_EFA_USE_DEVICE_RDMA=1 \\
URING_PLAY_OFI_EFA_FABRIC=efa-direct \\
URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS=1 \\
URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD='$qd' \\
URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1 \\
URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1 \\
URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW='$qd' \\
URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_RECORDS='$shm_entries' \\
URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_FILL_US='$extent_fill_us' \\
URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_MIN_BATCH_RECORDS='$minimum_batch_records' \\
URING_PLAY_ZCNBLK_SHM_WAL_FOREGROUND_READ_IMMEDIATE='$read_immediate' \\
URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 \\
scripts/zcnblk-shm-block-bench.sh
" 2>&1 | tee "$client_log"

wait "$leaf_ssh_pid"
leaf_ssh_pid=""
