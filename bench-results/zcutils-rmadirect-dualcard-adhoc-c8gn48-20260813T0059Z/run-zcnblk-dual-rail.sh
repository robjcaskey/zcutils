#!/usr/bin/env bash
set -euo pipefail

ROOT=/home/rob/zcutils
RUN=zcutils-rmadirect-dualcard-adhoc-c8gn48-20260813T0059Z
KEY=/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519
CLIENT=ubuntu@3.151.54.65
LEAF=ubuntu@18.225.65.76
REMOTE_ROOT=/home/ubuntu/zcutils
REMOTE_RUN="$REMOTE_ROOT/bench-results/$RUN"
LOCAL_RUN="$ROOT/bench-results/$RUN"

label="${1:-dual-l32-q512-w64}"
qd="${2:-512}"
wait_min="${3:-64}"
ops="${4:-200000}"
repeats="${5:-1}"
representative="${6:-0}"
placement_mode="${7:-owner-dispatch}"
lanes="${LANES_OVERRIDE:-32}"
port=42000
control_port=$((port + 1000))
control_prefix="${control_port%???}"
case "$placement_mode" in
	owner-dispatch) owner_dispatch=1; owner_ingress=0; owner_pipeline_batches=16 ;;
	lane-inline) owner_dispatch=0; owner_ingress=0; owner_pipeline_batches=16 ;;
	stable-owner) owner_dispatch=0; owner_ingress=1; owner_pipeline_batches=1 ;;
	*) echo "placement mode must be owner-dispatch, lane-inline, or stable-owner" >&2; exit 2 ;;
esac
owner_fragment_records="${OWNER_FRAGMENT_RECORDS_OVERRIDE:-16}"
owner_fragment_fill_us="${OWNER_FRAGMENT_FILL_US_OVERRIDE:-500}"
owner_debounce_us="${OWNER_DEBOUNCE_US_OVERRIDE:-2}"

client_cpus="${CLIENT_CPU_LIST_OVERRIDE:-0,6,12,18,24,30,36,42,48,54,60,66,72,78,84,90,96,102,108,114,120,126,132,138,144,150,156,162,168,174,180,186}"
target_cpus="${TARGET_CPU_LIST_OVERRIDE:-1,7,13,19,25,31,37,43,49,55,61,67,73,79,85,91,97,103,109,115,121,127,133,139,145,151,157,163,169,175,181,187}"
kernel_cpus="${KERNEL_CPU_LIST_OVERRIDE:-2,8,14,20,26,32,38,44,50,56,62,68,74,80,86,92,98,104,110,116,122,128,134,140,146,152,158,164,170,176,182,188}"
hctx_numa_node="${HCTX_NUMA_NODE_OVERRIDE:--2}"
owner_cpus=3,9,15,21,27,33,39,45,51,57,63,69,75,81,87,93,99,105,111,117,123,129,135,141,147,153,159,165,171,177,183,189
leaf_cpus=4,10,16,22,28,34,40,46,52,58,64,70,76,82,88,94,100,106,112,118,124,130,136,142,148,154,160,166,172,178,184,190
topology="$REMOTE_RUN/dual-leaf-topology.log"
if (( lanes == 64 )); then
	client_cpus="${CLIENT_CPU_LIST_OVERRIDE:-$(seq -s, 0 3 189)}"
	target_cpus="${TARGET_CPU_LIST_OVERRIDE:-$(seq -s, 1 3 190)}"
	kernel_cpus="${KERNEL_CPU_LIST_OVERRIDE:-$(seq -s, 2 3 191)}"
	owner_cpus="$(seq -s, 0 3 189)"
	leaf_cpus="$(seq -s, 0 3 189)"
	topology="$REMOTE_RUN/dual-leaf-topology-l64.log"
fi

domain_csv=""
leaf_addrs=""
for ((lane = 0; lane < lanes; lane++)); do
	if (( lane < lanes / 2 )); then
		domain=efa_0-rdm
		addr=172.31.39.134:42000
	else
		domain=efa_1-rdm
		addr=172.31.47.13:42000
	fi
	[ -z "$domain_csv" ] || domain_csv+=,
	[ -z "$leaf_addrs" ] || leaf_addrs+=,
	domain_csv+="$domain"
	leaf_addrs+="$addr"
done

ring_entries=$((qd * 2))
payload_entries=4096
if (( payload_entries < qd * 8 )); then
	payload_entries=$((qd * 8))
fi
leaf_log="$LOCAL_RUN/zcnblk0-dual/${label}.leaf.log"
client_log="$LOCAL_RUN/zcnblk0-dual/${label}.client.console.log"
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
cd '$REMOTE_ROOT'
exec env \\
URING_PLAY_PIN_CPU_LIST='$leaf_cpus' \\
URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 \\
URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \\
URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT=ofi \\
URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER=efa \\
URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT=rdm \\
URING_PLAY_ZCNBLK_WAL_LEAF_OFI_DOMAINS='$domain_csv' \\
URING_PLAY_ZCNBLK_WAL_LEAF_SPIN_READS=1 \\
URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 \\
URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \\
URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1 \\
URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB=1 \\
URING_PLAY_OFI_DOMAIN='' URING_PLAY_OFI_CONTROL_PORT_OFFSET=1000 \\
URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_RMA_READ_QD='$qd' \\
URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=65536 \\
URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 \\
FI_EFA_IFACE='efa_0,efa_1' FI_EFA_USE_DEVICE_RDMA=1 \\
URING_PLAY_OFI_EFA_FABRIC=efa-direct \\
target/release/zcnblk-wal-leaf zcmem:4G any '$port' '$lanes' 1 4096 '$lanes' true blocking
" >"$leaf_log" 2>&1 &
leaf_ssh_pid=$!

ready=0
for _ in $(seq 1 160); do
	if ! kill -0 "$leaf_ssh_pid" 2>/dev/null; then
		wait "$leaf_ssh_pid" || true
		echo "dual-rail leaf exited before client startup; see $leaf_log" >&2
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
(( ready == 1 )) || { echo "dual-rail leaf listeners were not ready" >&2; exit 1; }

outdir="$REMOTE_RUN/zcnblk0-dual/$label"
ssh "${ssh_opts[@]}" "$CLIENT" "
cd '$REMOTE_ROOT'
exec env \\
OUTDIR='$outdir' COORDINATION_SCOPE=dedicated-adhoc \\
TARGET_READY_ATTEMPTS=600 TARGET_MEMLOCK_UNLIMITED=1 \\
LANES='$lanes' REPEATS='$repeats' OPS_PER_WORKER='$ops' IODEPTH='$qd' RING_ENTRIES='$ring_entries' \\
CLIENT_CPU_LIST='$client_cpus' TARGET_CPU_LIST='$target_cpus' KERNEL_CPU_LIST='$kernel_cpus' \\
HCTX_NUMA_NODE='$hctx_numa_node' BLOCK_RING_MODE=normal BLOCK_ENGINE=uring-fixed \\
URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE=0 \\
URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS='$wait_min' \\
URING_PLAY_BLOCKBENCH_CQE_HOT_POLL=0 \\
SHM_RING_ENTRIES='$ring_entries' KERNEL_QUEUE_DEPTH='$qd' KERNEL_PIPELINE_DEPTH='$qd' KERNEL_QUEUES='$lanes' \\
SIZE_MIB=4096 REGION_BYTES_PER_WORKER=67108864 \\
BACKEND=wal-tcp START_LOCAL_LEAF=0 MODE=read READ_PERCENT=100 \\
SHM_PAYLOAD_ENTRIES='$payload_entries' KICK_BATCH=128 REPRESENTATIVE='$representative' \\
URING_PLAY_ZCNBLK_SHM_READ_BATCH=1 \\
URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_US=0 \\
URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_MIN=1 \\
POLL_US=1000 BUSY_POLL_US=1000 BUSY_HYSTERESIS_US=10000 KERNEL_POLL_US=1000 \\
BUFFER_MODE=hugetlb LEAF_ADDR=172.31.39.134 LEAF_PORT='$port' LEAF_ADDRS='$leaf_addrs' \\
URING_PLAY_ZCNBLK_SHM_ARENA_CPU_LIST='$target_cpus' \\
EXTERNAL_LEAF_TOPOLOGY_ARTIFACT='$topology' \\
URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED=1 \\
URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=ofi \\
URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER=efa \\
URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT=rdm \\
URING_PLAY_ZCNBLK_SHM_OFI_DOMAINS='$domain_csv' \\
URING_PLAY_OFI_DOMAIN=efa_0-rdm URING_PLAY_OFI_CQ_SLEEP_NS=0 \\
URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=65536 \\
URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 \\
FI_EFA_IFACE='efa_0,efa_1' FI_EFA_USE_DEVICE_RDMA=1 URING_PLAY_OFI_EFA_FABRIC=efa-direct \\
URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS=1 \\
URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD='$qd' \\
URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1 \\
URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1 \\
URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW='$qd' \\
URING_PLAY_ZCNBLK_SHM_WAL_OWNER_DISPATCH='$owner_dispatch' \\
URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS='$owner_ingress' \\
URING_PLAY_ZCNBLK_SHM_OWNER_CPU_LIST='$owner_cpus' \\
URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_BATCHES='$owner_pipeline_batches' \\
URING_PLAY_ZCNBLK_SHM_OWNER_FRAGMENT_RECORDS='$owner_fragment_records' \\
URING_PLAY_ZCNBLK_SHM_OWNER_FRAGMENT_FILL_US='$owner_fragment_fill_us' \\
URING_PLAY_ZCNBLK_SHM_OWNER_DEBOUNCE_US='$owner_debounce_us' \\
URING_PLAY_ZCNBLK_SHM_OWNER_COUNT='$lanes' \\
URING_PLAY_ZCNBLK_SHM_OWNER_EXTENT_RECORDS=1 \\
URING_PLAY_ZCNBLK_SHM_WAL_FOREGROUND_READ_IMMEDIATE=1 \\
URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_RECORDS='$ring_entries' \\
URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_FILL_US=20 \\
URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_MIN_BATCH_RECORDS=64 \\
URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 \\
scripts/zcnblk-shm-block-bench.sh
" 2>&1 | tee "$client_log"

wait "$leaf_ssh_pid"
leaf_ssh_pid=""
