#!/usr/bin/env bash
set -euo pipefail

SSH_KEY="${SSH_KEY:?set SSH_KEY to the ad-hoc instance key}"
CLIENT_HOST="${CLIENT_HOST:-18.227.100.7}"
LEAF_HOST="${LEAF_HOST:-18.222.231.47}"
CLIENT_PRIVATE="${CLIENT_PRIVATE:-172.31.37.157}"
LEAF_PRIVATE="${LEAF_PRIVATE:-172.31.44.118}"
REMOTE_ROOT="${REMOTE_ROOT:-/home/ubuntu/zcutils}"
ARTIFACT="${ARTIFACT:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
LANES="${LANES:-16}"
REPEATS="${REPEATS:-3}"
OPS_PER_WORKER="${OPS_PER_WORKER:-200000}"
QD_LIST="${QD_LIST:-1 2 4 8 16}"

client_cpus=0,4,8,12,16,20,24,28,32,36,40,44,48,52,56,60
target_cpus=1,5,9,13,17,21,25,29,33,37,41,45,49,53,57,61
kernel_cpus=2,6,10,14,18,22,26,30,34,38,42,46,50,54,58,62
sqpoll_cpus=3,7,11,15,19,23,27,31,35,39,43,47,51,55,59,63
leaf_cpus=32-47
ssh_opts=(-o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 -i "$SSH_KEY")
leaf_pid=""

cleanup_leaf() {
	[ -n "$leaf_pid" ] || return 0
	ssh "${ssh_opts[@]}" "ubuntu@$LEAF_HOST" bash -s -- "$leaf_pid" <<'REMOTE' || true
set -euo pipefail
pid="$1"
if kill -0 "$pid" 2>/dev/null; then
	command_line="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)"
	case "$command_line" in
		*zcnblk-wal-leaf*) kill "$pid" ;;
		*) printf 'refusing to stop unexpected pid=%s command=%s\n' "$pid" "$command_line" >&2 ;;
	esac
fi
REMOTE
	leaf_pid=""
}
trap cleanup_leaf EXIT

mkdir -p "$ARTIFACT/block"
qindex=0
for qd in $QD_LIST; do
	case "$qd" in
		1|2|4|8|16) ;;
		*) printf 'unsupported QD: %s\n' "$qd" >&2; exit 2 ;;
	esac
	base_port=$((29200 + qindex * 40))
	remote_leaf_log="/tmp/zcutils-rmaasync-ladder/block-qd${qd}-leaf.log"
	remote_out="/tmp/zcutils-rmaasync-ladder/block-qd${qd}"
	local_qd="$ARTIFACT/block/qd$qd"
	mkdir -p "$local_qd"
	printf 'block-start qd=%s repeats=%s ops_per_worker=%s base_port=%s\n' \
		"$qd" "$REPEATS" "$OPS_PER_WORKER" "$base_port"

	leaf_pid="$(ssh "${ssh_opts[@]}" "ubuntu@$LEAF_HOST" bash -s -- \
		"$REMOTE_ROOT" "$LEAF_PRIVATE" "$base_port" "$LANES" "$leaf_cpus" "$remote_leaf_log" <<'REMOTE'
set -euo pipefail
root="$1"
bind_addr="$2"
base_port="$3"
lanes="$4"
cpus="$5"
log="$6"
mkdir -p "$(dirname "$log")"
cd "$root"
nohup timeout --signal=TERM --kill-after=5s 600s \
	env FI_EFA_USE_DEVICE_RDMA=1 \
	URING_PLAY_OFI_DOMAIN=efa_0-rdm \
	URING_PLAY_OFI_EFA_FABRIC=efa \
	URING_PLAY_OFI_CQ_SLEEP_NS=0 \
	URING_PLAY_OFI_BUSY_POLL_ITERS=1000000 \
	URING_PLAY_OFI_TIMEOUT_MS=60000 \
	URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 \
	URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT=ofi \
	URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER=efa \
	URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT=rdm \
	URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1 \
	URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB=1 \
	URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
	URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
	URING_PLAY_PIN_CPUS=1 \
	URING_PLAY_PIN_CPU_LIST="$cpus" \
	target/release/zcnblk-wal-leaf zcmem:1G "$bind_addr" "$base_port" \
	"$lanes" 1 4K "$lanes" true blocking \
	>"$log" 2>&1 < /dev/null &
printf '%s\n' "$!"
REMOTE
)"
	control_port=$((base_port + 1000 + LANES - 1))
	ssh "${ssh_opts[@]}" "ubuntu@$LEAF_HOST" bash -s -- \
		"$leaf_pid" "$control_port" <<'REMOTE'
set -euo pipefail
pid="$1"
control_port="$2"
for _ in $(seq 1 1200); do
	if ss -H -ltn "sport = :$control_port" | grep -q .; then
		exit 0
	fi
	kill -0 "$pid" 2>/dev/null || exit 1
	sleep 0.05
done
printf 'leaf pid=%s did not open control port=%s\n' "$pid" "$control_port" >&2
exit 1
REMOTE

	ssh "${ssh_opts[@]}" "ubuntu@$CLIENT_HOST" \
		"cd '$REMOTE_ROOT' && rm -rf '$remote_out' && env \
		BACKEND=wal-tcp START_LOCAL_LEAF=0 MODE=read READ_PERCENT=100 \
		LANES='$LANES' REPEATS='$REPEATS' OPS_PER_WORKER='$OPS_PER_WORKER' \
		IODEPTH='$qd' RING_ENTRIES=64 BLOCK_RING_MODE=sqpoll-no-sqarray \
		SQPOLL_CPU_LIST='$sqpoll_cpus' SQPOLL_IDLE_MS=10000 BUFFER_MODE=hugetlb \
		SHM_RING_ENTRIES=64 SHM_PAYLOAD_ENTRIES=512 KERNEL_QUEUES='$LANES' \
		KERNEL_QUEUE_DEPTH='$qd' KERNEL_PIPELINE_DEPTH='$qd' SIZE_MIB=1024 \
		REGION_BYTES_PER_WORKER=67108864 TOPOLOGY_CPU_LIST=0-63 \
		CLIENT_CPU_LIST='$client_cpus' TARGET_CPU_LIST='$target_cpus' \
		KERNEL_CPU_LIST='$kernel_cpus' POLL_US=50 KERNEL_POLL_US=50 \
		URING_PLAY_BLOCKBENCH_CQE_HOT_POLL=1 \
		URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE=1 \
		URING_PLAY_BLOCKBENCH_RING_STATS=1 PERF_STAT=0 BUILD=0 REPRESENTATIVE=1 \
		COORDINATION_SCOPE=dedicated-adhoc \
		URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED=1 \
		URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 \
		URING_PLAY_ROUTE_PROBE=1 URING_PLAY_EXPECT_ROUTE_DEV=ens50 \
		URING_PLAY_EXPECT_ROUTE_SRC='$CLIENT_PRIVATE' FI_EFA_USE_DEVICE_RDMA=1 \
		URING_PLAY_OFI_DOMAIN=efa_0-rdm URING_PLAY_OFI_EFA_FABRIC=efa \
		URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_BUSY_POLL_ITERS=1000000 \
		URING_PLAY_OFI_TIMEOUT_MS=60000 URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 \
		URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=ofi \
		URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER=efa \
		URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT=rdm \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS=1 \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD='$qd' \
		URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS=0 \
		URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW='$qd' \
		URING_PLAY_ZCNBLK_SHM_READ_BATCH=1 \
		URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_US=0 \
		URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_MIN=1 \
		URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1 \
		URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_TRANSPORT=0 \
		LEAF_ADDR='$LEAF_PRIVATE' LEAF_PORT='$base_port' LEAF_TARGET=zcmem:1G \
		OUTDIR='$remote_out' scripts/zcnblk-shm-block-bench.sh" \
		>"$local_qd/harness.log" 2>&1

	rsync -az -e "ssh ${ssh_opts[*]}" \
		"ubuntu@$CLIENT_HOST:$remote_out/" "$local_qd/client/"
	ssh "${ssh_opts[@]}" "ubuntu@$LEAF_HOST" "cat '$remote_leaf_log'" \
		>"$local_qd/leaf.log"
	ssh "${ssh_opts[@]}" "ubuntu@$LEAF_HOST" bash -s -- "$leaf_pid" <<'REMOTE'
set -euo pipefail
pid="$1"
for _ in $(seq 1 1200); do
	kill -0 "$pid" 2>/dev/null || exit 0
	sleep 0.05
done
printf 'leaf pid=%s did not exit after target EOF\n' "$pid" >&2
exit 1
REMOTE
	leaf_pid=""
	grep -m1 '^runs=' "$local_qd/client/summary.log"
	grep -m1 '^zcnblk-shm-target-summary:' "$local_qd/client/summary.log"
	qindex=$((qindex + 1))
done

printf 'block-complete artifact=%s/block\n' "$ARTIFACT"
