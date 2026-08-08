#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
COORDINATION_SCOPE="${COORDINATION_SCOPE:-shared-host}"
BOOTSTRAP_MANIFEST="${ZCUTILS_BOOTSTRAP_MANIFEST:-$HOME/.local/state/zcutils/adhoc-bootstrap.env}"
MODULE="${MODULE:-$ROOT/kmods/zcnblk_client_mod.ko}"
TARGET_BIN="${TARGET_BIN:-$ROOT/target/release/zcnblk-shm-target}"
BENCH_BIN="${BENCH_BIN:-$ROOT/target/release/zcblockbench}"
ORDER_BIN="${ORDER_BIN:-$ROOT/target/release/zcnblk-order-smoke}"
ORDER_SMOKE_PAIRS="${ORDER_SMOKE_PAIRS:-0}"
LEAF_BIN="${LEAF_BIN:-$ROOT/target/release/zcnblk-wal-leaf}"
LANES="${LANES:-4}"
REPEATS="${REPEATS:-3}"
OPS_PER_WORKER="${OPS_PER_WORKER:-2000000}"
IODEPTH="${IODEPTH:-128}"
RING_ENTRIES="${RING_ENTRIES:-256}"
BLOCK_RING_MODE="${BLOCK_RING_MODE:-normal}"
BLOCK_ENGINE="${BLOCK_ENGINE:-uring-fixed}"
SQPOLL_CPU_LIST="${SQPOLL_CPU_LIST:-}"
SQPOLL_IDLE_MS="${SQPOLL_IDLE_MS:-1000}"
LATENCY_SAMPLE_RATE="${URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE:-0}"
BLOCK_RING_STATS="${URING_PLAY_BLOCKBENCH_RING_STATS:-1}"
if [ -n "${URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS+x}" ]; then
	BLOCK_WAIT_MIN_COMPLETIONS="$URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS"
elif [ "$IODEPTH" -ge 128 ]; then
	BLOCK_WAIT_MIN_COMPLETIONS=16
else
	BLOCK_WAIT_MIN_COMPLETIONS=1
fi
BLOCK_CQE_SPIN="${URING_PLAY_BLOCKBENCH_CQE_SPIN:-0}"
BLOCK_CQE_ADAPTIVE_SPIN="${URING_PLAY_BLOCKBENCH_CQE_ADAPTIVE_SPIN:-0}"
BLOCK_CQE_ADAPTIVE_SPIN_MIN="${URING_PLAY_BLOCKBENCH_CQE_ADAPTIVE_SPIN_MIN:-0}"
BLOCK_CQE_ADAPTIVE_SPIN_MAX="${URING_PLAY_BLOCKBENCH_CQE_ADAPTIVE_SPIN_MAX:-4096}"
BLOCK_CQE_ADAPTIVE_WAIT_NS="${URING_PLAY_BLOCKBENCH_CQE_ADAPTIVE_WAIT_NS:-50000}"
BLOCK_CQE_HOT_POLL="${URING_PLAY_BLOCKBENCH_CQE_HOT_POLL:-0}"
if [ -n "${URING_PLAY_BLOCKBENCH_CQE_HOT_POLL_PROGRESS_SPINS+x}" ]; then
	BLOCK_CQE_HOT_POLL_PROGRESS_SPINS="$URING_PLAY_BLOCKBENCH_CQE_HOT_POLL_PROGRESS_SPINS"
elif [ "$IODEPTH" -le 4 ]; then
	BLOCK_CQE_HOT_POLL_PROGRESS_SPINS=256
else
	BLOCK_CQE_HOT_POLL_PROGRESS_SPINS=4096
fi
TOPOLOGY_CPU_LIST="${TOPOLOGY_CPU_LIST:-}"
CLIENT_CPU_LIST="${CLIENT_CPU_LIST:-}"
TARGET_CPU_LIST="${TARGET_CPU_LIST:-}"
KERNEL_CPU_LIST="${KERNEL_CPU_LIST:-}"
LEAF_CPU_LIST="${LEAF_CPU_LIST:-}"
SHM_RING_ENTRIES="${SHM_RING_ENTRIES:-128}"
KERNEL_QUEUE_DEPTH="${KERNEL_QUEUE_DEPTH:-$IODEPTH}"
KERNEL_PIPELINE_DEPTH="${KERNEL_PIPELINE_DEPTH:-$SHM_RING_ENTRIES}"
SECTOR_ORDER_SLOTS="${URING_PLAY_ZCNBLK_SHM_SECTOR_ORDER_SLOTS:-65536}"
BACKEND="${BACKEND:-memory}"
if [ -z "${SHM_PAYLOAD_ENTRIES+x}" ]; then
	case "$BACKEND" in
		wal-memory|wal-tcp|tcp-leaf|fan-tcp) SHM_PAYLOAD_ENTRIES=4096 ;;
		*) SHM_PAYLOAD_ENTRIES="$SHM_RING_ENTRIES" ;;
	esac
fi
if [ -n "${URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH+x}" ]; then
	WRITEBACK_BATCH="$URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH"
else
	case "$BACKEND" in
		wal-tcp|tcp-leaf|fan-tcp) WRITEBACK_BATCH=$((2048 * LANES)) ;;
		*) WRITEBACK_BATCH=2048 ;;
	esac
fi
REQUEST_BATCH="${URING_PLAY_ZCNBLK_SHM_READ_BATCH:-$SHM_RING_ENTRIES}"
REQUEST_BATCH_FILL_US="${URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_US:-0}"
REQUEST_BATCH_FILL_MIN="${URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_MIN:-32}"
KICK_BATCH="${KICK_BATCH:-128}"
REPRESENTATIVE="${REPRESENTATIVE:-0}"
if [ -z "${POLL_US+x}" ]; then
	if [ "$REPRESENTATIVE" = 1 ]; then
		POLL_US=1000
	else
		POLL_US=50
	fi
fi
BUSY_POLL_US="${BUSY_POLL_US:-1000}"
BUSY_HYSTERESIS_US="${BUSY_HYSTERESIS_US:-10000}"
POLL_CLOCK_CHECK_SPINS="${URING_PLAY_ZCNBLK_SHM_POLL_CLOCK_CHECK_SPINS:-64}"
KERNEL_POLL_US="${KERNEL_POLL_US:-$POLL_US}"
LEASE_RELEASE_BATCH="${LEASE_RELEASE_BATCH:-1}"
SIZE_MIB="${SIZE_MIB:-$((LANES * 128))}"
REGION_BYTES_PER_WORKER="${REGION_BYTES_PER_WORKER:-67108864}"
MAX_FRAME_BYTES="${MAX_FRAME_BYTES:-4096}"
BUFFER_MODE="${BUFFER_MODE:-small-pages}"
LEAF_ADDR="${LEAF_ADDR:-127.0.0.1}"
LEAF_PORT="${LEAF_PORT:-29000}"
LEAF_SOURCE_ADDR="${LEAF_SOURCE_ADDR:-}"
LEAF_ADDRS="${LEAF_ADDRS:-}"
LEAF_SOURCE_ADDRS="${LEAF_SOURCE_ADDRS:-}"
LEAF_SUBMIT_MODE="${LEAF_SUBMIT_MODE:-blocking}"
LEAF_SPIN_READS="${URING_PLAY_ZCNBLK_WAL_LEAF_SPIN_READS:-0}"
if [ -n "${URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN+x}" ]; then
	LEAF_ADAPTIVE_SPIN="$URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN"
elif [ "$BACKEND" = wal-tcp ]; then
	LEAF_ADAPTIVE_SPIN=1
else
	LEAF_ADAPTIVE_SPIN=0
fi
LEAF_ADAPTIVE_SPIN_MIN="${URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MIN:-256}"
LEAF_ADAPTIVE_SPIN_MAX="${URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MAX:-65536}"
LEAF_ADAPTIVE_WAIT_NS="${URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_WAIT_NS:-50000}"
LEAF_ADAPTIVE_HYSTERESIS_NS="${URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_HYSTERESIS_NS:-10000000}"
if [ -n "${URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH+x}" ]; then
	WAL_LANE_BATCH="$URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH"
elif [ "$BACKEND" = wal-tcp ]; then
	WAL_LANE_BATCH=1
else
	WAL_LANE_BATCH=0
fi
WAL_SPLIT_TRANSPORT="${URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_TRANSPORT:-0}"
WAL_OWNER_DISPATCH="${URING_PLAY_ZCNBLK_SHM_WAL_OWNER_DISPATCH:-0}"
WAL_OWNER_INGRESS="${URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS:-0}"
WAL_OWNER_COUNT="${URING_PLAY_ZCNBLK_SHM_OWNER_COUNT:-$LANES}"
WAL_OWNER_CPU_LIST="${URING_PLAY_ZCNBLK_SHM_OWNER_CPU_LIST:-}"
WAL_OWNER_EXTENT_RECORDS="${URING_PLAY_ZCNBLK_SHM_OWNER_EXTENT_RECORDS:-256}"
WAL_OWNER_WORKER_SPINS="${URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_SPINS:-65536}"
WAL_OWNER_WORKER_ADAPTIVE_SPIN="${URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_ADAPTIVE_SPIN:-1}"
WAL_OWNER_WORKER_SPIN_MIN="${URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_SPIN_MIN:-4096}"
WAL_OWNER_WORKER_ADAPTIVE_WAIT_NS="${URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_ADAPTIVE_WAIT_NS:-50000}"
WAL_OWNER_MIXED_HYSTERESIS_US="${URING_PLAY_ZCNBLK_SHM_OWNER_MIXED_HYSTERESIS_US:-10000}"
WAL_OWNER_WRITE_FILL_US="${URING_PLAY_ZCNBLK_SHM_OWNER_WRITE_FILL_US:-0}"
WAL_OWNER_WRITE_FILL_MIN="${URING_PLAY_ZCNBLK_SHM_OWNER_WRITE_FILL_MIN:-256}"
WAL_OWNER_DEBOUNCE_US="${URING_PLAY_ZCNBLK_SHM_OWNER_DEBOUNCE_US:-2}"
WAL_OWNER_BACKLOG_HIGH_RECORDS="${URING_PLAY_ZCNBLK_SHM_OWNER_BACKLOG_HIGH_RECORDS:-$WAL_OWNER_WRITE_FILL_MIN}"
WAL_OWNER_BACKLOG_LOW_RECORDS="${URING_PLAY_ZCNBLK_SHM_OWNER_BACKLOG_LOW_RECORDS:-16}"
WAL_OWNER_PIPELINE_BATCHES="${URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_BATCHES:-16}"
WAL_OWNER_PIPELINE_REFILL_SPINS="${URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_REFILL_SPINS:-256}"
WAL_OWNER_BATCH_RECORDS="${URING_PLAY_ZCNBLK_SHM_OWNER_BATCH_RECORDS:-2048}"
if [ -n "${URING_PLAY_ZCNBLK_SHM_OWNER_FRAGMENT_RECORDS+x}" ]; then
	WAL_OWNER_FRAGMENT_RECORDS="$URING_PLAY_ZCNBLK_SHM_OWNER_FRAGMENT_RECORDS"
elif [ "$LANES" -lt 16 ]; then
	WAL_OWNER_FRAGMENT_RECORDS="$LANES"
else
	WAL_OWNER_FRAGMENT_RECORDS=16
fi
WAL_OWNER_FRAGMENT_FILL_US="${URING_PLAY_ZCNBLK_SHM_OWNER_FRAGMENT_FILL_US:-500}"
WAL_OWNER_FOREGROUND_IMMEDIATE_LIMIT="${URING_PLAY_ZCNBLK_SHM_OWNER_FOREGROUND_IMMEDIATE_LIMIT:-1}"
WAL_OWNER_QUEUE_DEPTH="${URING_PLAY_ZCNBLK_SHM_OWNER_QUEUE_DEPTH:-128}"
WAL_OWNER_MAX_TX_IOVECS="${URING_PLAY_ZCNBLK_SHM_OWNER_MAX_TX_IOVECS:-960}"
if [ -n "${URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW+x}" ]; then
	WAL_LANE_WINDOW="$URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW"
elif [ "$WAL_OWNER_INGRESS" = 1 ]; then
	WAL_LANE_WINDOW=16
else
	WAL_LANE_WINDOW=4
fi
WAL_TRANSPORT_CPU_LIST="${URING_PLAY_ZCNBLK_SHM_WAL_TRANSPORT_CPU_LIST:-}"
WAL_TRANSPORT_GREEDY="${URING_PLAY_ZCNBLK_SHM_WAL_TRANSPORT_GREEDY:-1}"
TRANSFER_SLOTS="${URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS:-1}"
REMOTE_RECV_SPINS="${URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_SPINS:-0}"
REMOTE_RECV_POLICY="${URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_POLICY:-adaptive}"
REMOTE_RECV_ADAPTIVE_SPIN_MIN="${URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MIN:-0}"
REMOTE_RECV_ADAPTIVE_SPIN_MAX="${URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MAX:-65536}"
REMOTE_RECV_ADAPTIVE_WAIT_NS="${URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_WAIT_NS:-50000}"
REMOTE_RECV_ADAPTIVE_HYSTERESIS_NS="${URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_HYSTERESIS_NS:-10000000}"
REMOTE_SEND_MODE="${URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE:-blocking}"
REMOTE_SEND_RING_ENTRIES="${URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_RING_ENTRIES:-256}"
REMOTE_SEND_ZC_REQUIRED="${URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_ZC_REQUIRED:-1}"
ALLOW_UNSAFE_SEND_ZC="${URING_PLAY_ALLOW_UNSAFE_SEND_ZC:-0}"
WAL_EXTENT_RECORDS="${URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_RECORDS:-$SHM_RING_ENTRIES}"
if [ -n "${URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_FILL_US+x}" ]; then
	WAL_EXTENT_FILL_US="$URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_FILL_US"
elif [ "$WAL_SPLIT_TRANSPORT" = 1 ]; then
	WAL_EXTENT_FILL_US=50
elif [ "$WAL_LANE_BATCH" = 1 ]; then
	WAL_EXTENT_FILL_US=20
else
	WAL_EXTENT_FILL_US=0
fi
WAL_SPLIT_MIN_BATCH_RECORDS="${URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_MIN_BATCH_RECORDS:-64}"
if [ -n "${URING_PLAY_ZCNBLK_SHM_WAL_COMPACT_WRITES+x}" ]; then
	WAL_COMPACT_WRITES="$URING_PLAY_ZCNBLK_SHM_WAL_COMPACT_WRITES"
elif [ "$BACKEND" = wal-tcp ]; then
	WAL_COMPACT_WRITES=1
else
	WAL_COMPACT_WRITES=0
fi
DIRTY_PRESSURE_RESERVE="${URING_PLAY_ZCNBLK_SHM_DIRTY_PRESSURE_RESERVE:-0}"
WAL_DEBUG_STATE="${URING_PLAY_ZCNBLK_SHM_WAL_DEBUG_STATE:-0}"
KERNEL_STATE_INTERVAL_MS="${URING_PLAY_ZCNBLK_SHM_KERNEL_STATE_INTERVAL_MS:-$([ "$WAL_DEBUG_STATE" = 1 ] && printf 1000 || printf 0)}"
LEAF_TARGET="${LEAF_TARGET:-zcmem:${SIZE_MIB}M}"
START_LOCAL_LEAF="${START_LOCAL_LEAF:-$([ "$BACKEND" = wal-tcp ] && printf 1 || printf 0)}"
MODE="${MODE:-rw}"
READ_PERCENT="${READ_PERCENT:-50}"
PERF_STAT="${PERF_STAT:-1}"
BUILD="${BUILD:-0}"
SET_GOVERNOR="${SET_GOVERNOR:-}"
OUTDIR="${OUTDIR:-$ROOT/bench-results/local-zcnblk-shm-$(date -u +%Y%m%dT%H%M%SZ)}"

block_lease=""
perf_lease=""
target_pid=""
target_job_pid=""
leaf_pid=""
kernel_state_pid=""
declare -a tracked_pids=()
declare -a kthread_pids=()
pid_file="$OUTDIR/target.pid"
governors_file="$OUTDIR/governors.before"

log() {
	printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"
}

die() {
	printf 'zcnblk-shm-block-bench: ERROR: %s\n' "$*" >&2
	exit 1
}

token_from_result() {
	sed -n 's/.* token=\([^ ]*\).*/\1/p' <<<"$1"
}

honored_from_result() {
	sed -n 's/.* honored=\([^ ]*\).*/\1/p' <<<"$1"
}

expand_cpu_list() {
	local part start end cpu
	tr ',' '\n' <<<"$1" | while IFS= read -r part; do
		part="${part//[[:space:]]/}"
		if [[ "$part" == *-* ]]; then
			start="${part%-*}"
			end="${part#*-}"
			for ((cpu = start; cpu <= end; cpu++)); do
				printf '%s\n' "$cpu"
			done
		elif [ -n "$part" ]; then
			printf '%s\n' "$part"
		fi
	done
}

join_comma() {
	local IFS=,
	printf '%s' "$*"
}

cpu_numa_node() {
	local cpu="$1" node_path
	for node_path in "/sys/devices/system/cpu/cpu$cpu"/node[0-9]*; do
		[ -e "$node_path" ] || continue
		printf '%s' "${node_path##*node}"
		return 0
	done
	printf unknown
}

snapshot_contexts() {
	local output="$1" pid
	: >"$output"
	for pid in "${tracked_pids[@]}"; do
		[ -r "/proc/$pid/status" ] || continue
		awk -v pid="$pid" '
			/^Name:/ { name=$2 }
			/^Cpus_allowed_list:/ { cpus=$2 }
			/^voluntary_ctxt_switches:/ { voluntary=$2 }
			/^nonvoluntary_ctxt_switches:/ { involuntary=$2 }
			END { printf "%s %s %s %s %s\n", pid, name, cpus, voluntary+0, involuntary+0 }
		' "/proc/$pid/status" >>"$output"
	done
}

safe_stop_target() {
	[ -n "$target_pid" ] || return 0
	[ -r "/proc/$target_pid/comm" ] || return 0
	local comm
	comm="$(cat "/proc/$target_pid/comm")"
	[ "$comm" = "zcnblk-shm-targ" ] || {
		printf 'refusing to signal pid=%s comm=%s\n' "$target_pid" "$comm" >&2
		return 1
	}
	sudo -n kill -INT "$target_pid" 2>/dev/null || true
	for _ in $(seq 1 100); do
		[ ! -e "/proc/$target_pid" ] && break
		sleep 0.05
	done
}

safe_stop_leaf() {
	[ -n "$leaf_pid" ] || return 0
	[ -r "/proc/$leaf_pid/comm" ] || return 0
	local comm
	comm="$(cat "/proc/$leaf_pid/comm")"
	[ "$comm" = "zcnblk-wal-lea" ] || {
		printf 'refusing to signal leaf pid=%s comm=%s\n' "$leaf_pid" "$comm" >&2
		return 1
	}
	kill -TERM "$leaf_pid" 2>/dev/null || true
	wait "$leaf_pid" 2>/dev/null || true
	leaf_pid=""
}

restore_governors() {
	[ -s "$governors_file" ] || return 0
	while read -r path governor; do
		[ -w "$path" ] && printf '%s' "$governor" >"$path" || \
			sudo -n sh -c 'printf "%s" "$1" > "$2"' sh "$governor" "$path" || true
	done <"$governors_file"
}

cleanup() {
	local status=$?
	set +e
	if [ -n "$kernel_state_pid" ] && kill -0 "$kernel_state_pid" 2>/dev/null; then
		kill "$kernel_state_pid" 2>/dev/null
		wait "$kernel_state_pid" 2>/dev/null
	fi
	safe_stop_target
	if [ -n "$target_job_pid" ]; then
		wait "$target_job_pid" 2>/dev/null
	fi
	safe_stop_leaf
	if grep -q '^zcnblk_client_mod ' /proc/modules 2>/dev/null; then
		sudo -n rmmod zcnblk_client_mod
	fi
	restore_governors
	[ -n "$perf_lease" ] && "$COORD_BIN" release "$perf_lease" >>"$OUTDIR/coordination.log" 2>&1
	[ -n "$block_lease" ] && "$COORD_BIN" release "$block_lease" >>"$OUTDIR/coordination.log" 2>&1
	exit "$status"
}

trap cleanup EXIT INT TERM

[ "$LANES" -gt 0 ] || die "LANES must be positive"
[ "$REPEATS" -gt 0 ] || die "REPEATS must be positive"
[[ "$SHM_RING_ENTRIES" =~ ^[0-9]+$ ]] && [ "$SHM_RING_ENTRIES" -gt 0 ] || \
	die "SHM_RING_ENTRIES must be a positive integer"
[[ "$KERNEL_QUEUE_DEPTH" =~ ^[0-9]+$ ]] && [ "$KERNEL_QUEUE_DEPTH" -gt 0 ] || \
	die "KERNEL_QUEUE_DEPTH must be a positive integer"
[[ "$KERNEL_PIPELINE_DEPTH" =~ ^[0-9]+$ ]] && [ "$KERNEL_PIPELINE_DEPTH" -gt 0 ] || \
	die "KERNEL_PIPELINE_DEPTH must be a positive integer"
[ "$KERNEL_PIPELINE_DEPTH" -le "$SHM_RING_ENTRIES" ] || \
	die "KERNEL_PIPELINE_DEPTH must not exceed SHM_RING_ENTRIES"
[[ "$POLL_CLOCK_CHECK_SPINS" =~ ^[0-9]+$ ]] && [ "$POLL_CLOCK_CHECK_SPINS" -gt 0 ] || \
	die "POLL_CLOCK_CHECK_SPINS must be a positive integer"
[[ "$KERNEL_STATE_INTERVAL_MS" =~ ^[0-9]+$ ]] || \
	die "KERNEL_STATE_INTERVAL_MS must be an integer"
[[ "$SECTOR_ORDER_SLOTS" =~ ^[0-9]+$ ]] || die "SECTOR_ORDER_SLOTS must be an integer"
[ "$SECTOR_ORDER_SLOTS" -gt 0 ] || die "SECTOR_ORDER_SLOTS must be positive"
(( (SECTOR_ORDER_SLOTS & (SECTOR_ORDER_SLOTS - 1)) == 0 )) || \
	die "SECTOR_ORDER_SLOTS must be a power of two"
sector_order_floor=$((SIZE_MIB * 256))
[ "$sector_order_floor" -le 65536 ] || sector_order_floor=65536
if [ "$WAL_OWNER_INGRESS" = 1 ]; then
	active_order_pages=$((LANES * REGION_BYTES_PER_WORKER / 4096))
	active_order_target=$((active_order_pages * 2))
	sector_order_floor=1
	while [ "$sector_order_floor" -lt "$active_order_target" ]; do
		sector_order_floor=$((sector_order_floor * 2))
	done
fi
if [ "$SECTOR_ORDER_SLOTS" -lt "$sector_order_floor" ]; then
	printf 'PERF WARNING: shm_sector_order_slots=%s is below the measured floor=%s for size_mib=%s; false sector dependencies can serialize otherwise independent lanes\n' \
		"$SECTOR_ORDER_SLOTS" "$sector_order_floor" "$SIZE_MIB" >&2
	[ "$REPRESENTATIVE" != 1 ] || die "representative WAL runs require at least $sector_order_floor sector-order slots"
fi
[[ "$ORDER_SMOKE_PAIRS" =~ ^[0-9]+$ ]] || die "ORDER_SMOKE_PAIRS must be an integer"
[ "$ORDER_SMOKE_PAIRS" -le 64 ] || die "ORDER_SMOKE_PAIRS must not exceed 64"
[[ "$LEAF_ADAPTIVE_HYSTERESIS_NS" =~ ^[0-9]+$ ]] || \
	die "leaf adaptive hysteresis nanoseconds must be an integer"
[[ "$REMOTE_RECV_ADAPTIVE_HYSTERESIS_NS" =~ ^[0-9]+$ ]] || \
	die "remote adaptive receive hysteresis nanoseconds must be an integer"
[[ "$BLOCK_CQE_HOT_POLL_PROGRESS_SPINS" =~ ^[0-9]+$ ]] && \
	[ "$BLOCK_CQE_HOT_POLL_PROGRESS_SPINS" -gt 0 ] || \
	die "CQ hot-poll progress spins must be a positive integer"
if [ "$IODEPTH" -ge 128 ] && [ "$BLOCK_WAIT_MIN_COMPLETIONS" -lt 8 ]; then
	printf 'PERF WARNING: wait_min_completions=%s at iodepth=%s causes excessive io_uring enter/wait churn; use at least 8 (16 measured best locally) for high-IOPS throughput controls\n' \
		"$BLOCK_WAIT_MIN_COMPLETIONS" "$IODEPTH" >&2
	[ "$REPRESENTATIVE" != 1 ] || die "representative deep-queue runs require BLOCK_WAIT_MIN_COMPLETIONS >= 8"
fi
if [ "$BACKEND" = wal-tcp ] && [ "$WAL_LANE_BATCH" != 1 ] && [ "$WAL_OWNER_DISPATCH" != 1 ]; then
	printf 'PERF WARNING: WAL lane batching is disabled; the legacy per-command worker path cannot represent high-IOPS WAL TCP performance\n' >&2
	[ "$REPRESENTATIVE" != 1 ] || die "representative WAL TCP runs require URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1"
fi
if [ "$WAL_OWNER_DISPATCH" = 1 ] && [ "$WAL_SPLIT_TRANSPORT" = 1 ]; then
	die "owner dispatch uses its own remote workers and is incompatible with split lane transport"
fi
if [ "$WAL_OWNER_INGRESS" = 1 ]; then
	[ "$WAL_OWNER_DISPATCH" != 1 ] || die "legacy owner dispatch and stable owner ingress are mutually exclusive"
	[ "$WAL_LANE_BATCH" = 1 ] || die "stable owner ingress requires lane-local WAL batching"
	[ "$WAL_SPLIT_TRANSPORT" != 1 ] || die "stable owner ingress owns separate transport workers"
	[ "$START_LOCAL_LEAF" != 1 ] || die "stable owner ingress currently requires an external userspace leaf"
fi
[[ "$WAL_OWNER_COUNT" =~ ^[0-9]+$ ]] && [ "$WAL_OWNER_COUNT" -gt 0 ] || \
	die "WAL owner count must be a positive integer"
[ "$WAL_OWNER_COUNT" -le "$LANES" ] || \
	die "WAL owner count must not exceed LANES"
[[ "$WAL_OWNER_EXTENT_RECORDS" =~ ^[0-9]+$ ]] && [ "$WAL_OWNER_EXTENT_RECORDS" -gt 0 ] || \
	die "WAL owner extent records must be a positive integer"
[[ "$WAL_OWNER_WORKER_SPINS" =~ ^[0-9]+$ ]] || die "WAL owner worker spins must be an integer"
[[ "$WAL_OWNER_WORKER_ADAPTIVE_SPIN" =~ ^[01]$ ]] || \
	die "WAL owner adaptive spin must be zero or one"
[[ "$WAL_OWNER_WORKER_SPIN_MIN" =~ ^[0-9]+$ ]] || \
	die "WAL owner worker minimum spins must be an integer"
[ "$WAL_OWNER_WORKER_SPIN_MIN" -le "$WAL_OWNER_WORKER_SPINS" ] || \
	die "WAL owner worker minimum spins must not exceed maximum spins"
[[ "$WAL_OWNER_WORKER_ADAPTIVE_WAIT_NS" =~ ^[0-9]+$ ]] || \
	die "WAL owner adaptive wait nanoseconds must be an integer"
[[ "$WAL_OWNER_MIXED_HYSTERESIS_US" =~ ^[0-9]+$ ]] || \
	die "WAL owner mixed hysteresis microseconds must be an integer"
[[ "$WAL_OWNER_WRITE_FILL_US" =~ ^[0-9]+$ ]] || die "WAL owner write fill usec must be an integer"
[[ "$WAL_OWNER_WRITE_FILL_MIN" =~ ^[0-9]+$ ]] && [ "$WAL_OWNER_WRITE_FILL_MIN" -gt 0 ] || \
	die "WAL owner write fill minimum must be a positive integer"
[[ "$WAL_OWNER_DEBOUNCE_US" =~ ^[0-9]+$ ]] || \
	die "WAL owner debounce usec must be an integer"
[[ "$WAL_OWNER_BACKLOG_HIGH_RECORDS" =~ ^[0-9]+$ ]] && [ "$WAL_OWNER_BACKLOG_HIGH_RECORDS" -gt 0 ] || \
	die "WAL owner backlog high watermark must be a positive integer"
[[ "$WAL_OWNER_BACKLOG_LOW_RECORDS" =~ ^[0-9]+$ ]] || \
	die "WAL owner backlog low watermark must be an integer"
[ "$WAL_OWNER_BACKLOG_LOW_RECORDS" -lt "$WAL_OWNER_BACKLOG_HIGH_RECORDS" ] || \
	die "WAL owner backlog low watermark must be below the high watermark"
[[ "$WAL_OWNER_PIPELINE_BATCHES" =~ ^[0-9]+$ ]] && [ "$WAL_OWNER_PIPELINE_BATCHES" -gt 0 ] || \
	die "WAL owner pipeline batches must be a positive integer"
[[ "$WAL_OWNER_PIPELINE_REFILL_SPINS" =~ ^[0-9]+$ ]] || \
	die "WAL owner pipeline refill spins must be an integer"
[[ "$WAL_OWNER_BATCH_RECORDS" =~ ^[0-9]+$ ]] && [ "$WAL_OWNER_BATCH_RECORDS" -gt 0 ] || \
	die "WAL owner batch records must be a positive integer"
[[ "$WAL_OWNER_FRAGMENT_RECORDS" =~ ^[0-9]+$ ]] && [ "$WAL_OWNER_FRAGMENT_RECORDS" -gt 0 ] || \
	die "WAL owner fragment records must be a positive integer"
[[ "$WAL_OWNER_FRAGMENT_FILL_US" =~ ^[0-9]+$ ]] || \
	die "WAL owner fragment fill usec must be an integer"
[[ "$WAL_OWNER_FOREGROUND_IMMEDIATE_LIMIT" =~ ^[0-9]+$ ]] || \
	die "WAL owner foreground immediate limit must be an integer"
[[ "$WAL_OWNER_QUEUE_DEPTH" =~ ^[0-9]+$ ]] && [ "$WAL_OWNER_QUEUE_DEPTH" -gt 1 ] || \
	die "WAL owner queue depth must be at least two"
[[ "$WAL_OWNER_MAX_TX_IOVECS" =~ ^[0-9]+$ ]] && \
	[ "$WAL_OWNER_MAX_TX_IOVECS" -gt 0 ] && [ "$WAL_OWNER_MAX_TX_IOVECS" -le 1022 ] || \
	die "WAL owner max tx iovecs must be in 1..=1022"
[[ "$WAL_LANE_WINDOW" =~ ^[0-9]+$ ]] && [ "$WAL_LANE_WINDOW" -gt 0 ] || \
	die "WAL lane window must be a positive integer"
if [ "$START_LOCAL_LEAF" = 1 ] && [ "$LEAF_SPIN_READS" != 1 ] && [ "$LEAF_ADAPTIVE_SPIN" != 1 ]; then
	printf 'PERF WARNING: WAL leaf receive spinning is disabled; blocking once or more per network batch adds avoidable context switches. Enable URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 for high-IOPS controls\n' >&2
	[ "$REPRESENTATIVE" != 1 ] || die "representative local WAL leaf runs require adaptive or fixed receive spinning"
fi
[ "$SHM_PAYLOAD_ENTRIES" -gt "$SHM_RING_ENTRIES" ] || {
	printf 'PERF WARNING: payload entries (%s) do not exceed descriptor entries (%s); WAL writeback batches will collapse to one\n' \
		"$SHM_PAYLOAD_ENTRIES" "$SHM_RING_ENTRIES" >&2
	case "$BACKEND" in
		wal-memory|wal-tcp|tcp-leaf|fan-tcp)
			[ "$REPRESENTATIVE" != 1 ] || die "representative WAL runs require payload entries greater than descriptor entries"
			;;
	esac
}
command -v sudo >/dev/null || die "sudo is required"
sudo -n true || die "passwordless sudo is required for the block client edge"
[ ! -e /dev/zcnblk0 ] || die "/dev/zcnblk0 already exists; another owner may be using it"
mkdir -p "$OUTDIR"

case "$COORDINATION_SCOPE" in
	shared-host)
		[ -x "$COORD_BIN" ] || die "agent-coord not found at $COORD_BIN"
		block_result="$($COORD_BIN request --owner "codex:zcutils-zcnblk-shm" \
			--mode exclusive --sensitivity high --priority 50 --ttl 1800 \
			--resource 'block=zcnblk0' --note 'zcnblk shared-memory block edge lifecycle')" || die "could not reserve /dev/zcnblk0"
		printf '%s\n' "$block_result" | tee -a "$OUTDIR/coordination.log"
		block_lease="$(token_from_result "$block_result")"
		[ -n "$block_lease" ] || die "agent-coord returned no block lease token"
		;;
	dedicated-adhoc)
		[ -r "$BOOTSTRAP_MANIFEST" ] || die "dedicated adhoc coordination requires bootstrap manifest: $BOOTSTRAP_MANIFEST"
		grep -qx 'coordination_scope=dedicated-adhoc-instance' "$BOOTSTRAP_MANIFEST" || \
			die "bootstrap manifest does not prove dedicated adhoc ownership"
		grep -qx 'coordination_honored=true' "$BOOTSTRAP_MANIFEST" || \
			die "bootstrap manifest does not honor dedicated coordination"
		grep -Eq '^instance_id=i-[0-9a-f]+$' "$BOOTSTRAP_MANIFEST" || \
			die "bootstrap manifest does not identify an EC2 instance"
		printf 'scope=dedicated-adhoc honored=true manifest=%s\n' "$BOOTSTRAP_MANIFEST" | \
			tee -a "$OUTDIR/coordination.log"
		;;
	*)
		die "COORDINATION_SCOPE must be shared-host or dedicated-adhoc"
		;;
esac

if [ "$BUILD" = 1 ]; then
	log "building release benchmark binaries and kernel modules"
	(cd "$ROOT" && cargo build --release --bin zcnblk-shm-target --bin zcblockbench \
		--bin zcnblk-order-smoke --bin zcnblk-wal-leaf)
	make -C "$ROOT/kmods" all
	sign_file="/usr/src/linux-headers-$(uname -r)/scripts/sign-file"
	[ -x "$sign_file" ] || die "module signing helper is missing: $sign_file"
	sudo -n "$sign_file" sha256 /root/mok/MOK.priv /root/mok/MOK.pem "$MODULE"
fi

[ -x "$TARGET_BIN" ] || die "target binary is missing: $TARGET_BIN (set BUILD=1)"
[ -x "$BENCH_BIN" ] || die "benchmark binary is missing: $BENCH_BIN (set BUILD=1)"
[ "$ORDER_SMOKE_PAIRS" = 0 ] || [ -x "$ORDER_BIN" ] || \
	die "order smoke binary is missing: $ORDER_BIN (set BUILD=1)"
[ "$START_LOCAL_LEAF" != 1 ] || [ -x "$LEAF_BIN" ] || die "leaf binary is missing: $LEAF_BIN (set BUILD=1)"
[ -r "$MODULE" ] || die "kernel module is missing: $MODULE (set BUILD=1)"

log "loading placement-free shared-memory client edge"
sudo -n insmod "$MODULE" transport=shm lanes="$LANES" connections_per_lane=1 \
	size_mib="$SIZE_MIB" queues="$LANES" queue_depth="$KERNEL_QUEUE_DEPTH" \
	shm_sector_order_slots="$SECTOR_ORDER_SLOTS" \
	max_frame_bytes="$MAX_FRAME_BYTES" \
	pipeline_depth="$KERNEL_PIPELINE_DEPTH" shm_ring_entries="$SHM_RING_ENTRIES" \
	shm_payload_entries="$SHM_PAYLOAD_ENTRIES" shm_poll_us="$KERNEL_POLL_US" pin_threads=0
for _ in $(seq 1 100); do
	[ -e /dev/zcnblk0 ] && [ -e /dev/zcnblk-shmctl ] && break
	sleep 0.05
done
[ -e /dev/zcnblk0 ] && [ -e /dev/zcnblk-shmctl ] || die "shared block edge did not appear"

declare -a client_cpus=() target_cpus=() kernel_cpus=() leaf_cpus=() transport_cpus=() owner_cpus=() all_cpus=()
declare -A used_cores=()
declare -A allowed_cpus=()
if [ -n "$TOPOLOGY_CPU_LIST" ]; then
	while IFS= read -r cpu; do
		allowed_cpus[$cpu]=1
	done < <(expand_cpu_list "$TOPOLOGY_CPU_LIST")
fi
roles_per_lane=3
[ "$START_LOCAL_LEAF" != 1 ] || roles_per_lane=4
[ "$WAL_SPLIT_TRANSPORT" != 1 ] || [ "$START_LOCAL_LEAF" = 1 ] || roles_per_lane=4
for ((lane = 0; lane < LANES; lane++)); do
	hctx="/sys/block/zcnblk0/mq/$lane/cpu_list"
	[ -r "$hctx" ] || die "missing hctx CPU map: $hctx"
	mapfile -t candidates < <(expand_cpu_list "$(cat "$hctx")")
	declare -a selected=()
	for cpu in "${candidates[@]}"; do
		[ -z "$TOPOLOGY_CPU_LIST" ] || [ "${allowed_cpus[$cpu]:-}" = 1 ] || continue
		package="$(cat "/sys/devices/system/cpu/cpu$cpu/topology/physical_package_id")"
		core="$(cat "/sys/devices/system/cpu/cpu$cpu/topology/core_id")"
		key="$package:$core"
		[ -z "${used_cores[$key]:-}" ] || continue
		used_cores[$key]=1
		selected+=("$cpu")
		[ "${#selected[@]}" -eq "$roles_per_lane" ] && break
	done
	[ "${#selected[@]}" -eq "$roles_per_lane" ] || die "hctx$lane cannot supply $roles_per_lane unused physical cores; reduce LANES or provide a larger host"
	client_cpus+=("${selected[0]}")
	target_cpus+=("${selected[1]}")
	kernel_cpus+=("${selected[2]}")
	if [ "$START_LOCAL_LEAF" = 1 ]; then
		leaf_cpus+=("${selected[3]}")
	elif [ "$WAL_SPLIT_TRANSPORT" = 1 ]; then
		transport_cpus+=("${selected[3]}")
	fi
	all_cpus+=("${selected[@]}")
done
if [ -n "$CLIENT_CPU_LIST$TARGET_CPU_LIST$KERNEL_CPU_LIST$LEAF_CPU_LIST" ]; then
	[ -n "$CLIENT_CPU_LIST" ] && [ -n "$TARGET_CPU_LIST" ] && [ -n "$KERNEL_CPU_LIST" ] || \
		die "explicit topology requires CLIENT_CPU_LIST, TARGET_CPU_LIST, and KERNEL_CPU_LIST"
	[ "$START_LOCAL_LEAF" != 1 ] || [ -n "$LEAF_CPU_LIST" ] || \
		die "explicit local-leaf topology requires LEAF_CPU_LIST"
	mapfile -t client_cpus < <(expand_cpu_list "$CLIENT_CPU_LIST")
	mapfile -t target_cpus < <(expand_cpu_list "$TARGET_CPU_LIST")
	mapfile -t kernel_cpus < <(expand_cpu_list "$KERNEL_CPU_LIST")
	transport_cpus=()
	if [ "$START_LOCAL_LEAF" = 1 ]; then
		mapfile -t leaf_cpus < <(expand_cpu_list "$LEAF_CPU_LIST")
	else
		leaf_cpus=()
	fi
	[ "${#client_cpus[@]}" -eq "$LANES" ] || die "CLIENT_CPU_LIST must provide one CPU per lane"
	[ "${#target_cpus[@]}" -eq "$LANES" ] || die "TARGET_CPU_LIST must provide one CPU per lane"
	[ "${#kernel_cpus[@]}" -eq "$LANES" ] || die "KERNEL_CPU_LIST must provide one CPU per lane"
	[ "$START_LOCAL_LEAF" != 1 ] || [ "${#leaf_cpus[@]}" -eq "$LANES" ] || \
		die "LEAF_CPU_LIST must provide one CPU per lane"
	all_cpus=("${client_cpus[@]}" "${target_cpus[@]}" "${kernel_cpus[@]}" "${leaf_cpus[@]}")
	for ((lane = 0; lane < LANES; lane++)); do
		hctx="$(cat "/sys/block/zcnblk0/mq/$lane/cpu_list")"
		for cpu in "${client_cpus[$lane]}" "${target_cpus[$lane]}" "${kernel_cpus[$lane]}"; do
			cpu_allowed=false
			while IFS= read -r allowed_cpu; do
				if [ "$allowed_cpu" = "$cpu" ]; then
					cpu_allowed=true
					break
				fi
			done < <(expand_cpu_list "$hctx")
			[ "$cpu_allowed" = true ] || die "explicit lane $lane CPU $cpu is outside hctx map $hctx"
		done
	done
fi
declare -a owner_hctx_lanes=()
if [ "$WAL_OWNER_INGRESS" = 1 ]; then
	owner_cpus=()
	if [ -n "$WAL_OWNER_CPU_LIST" ]; then
		mapfile -t owner_cpus < <(expand_cpu_list "$WAL_OWNER_CPU_LIST")
		[ "${#owner_cpus[@]}" -eq "$WAL_OWNER_COUNT" ] || \
			die "WAL_OWNER_CPU_LIST must provide one CPU per configured owner"
		for ((owner = 0; owner < WAL_OWNER_COUNT; owner++)); do
			owner_hctx_lanes+=(explicit)
		done
	else
		for ((owner = 0; owner < WAL_OWNER_COUNT; owner++)); do
			owner_lane=$((owner * LANES / WAL_OWNER_COUNT))
			hctx="/sys/block/zcnblk0/mq/$owner_lane/cpu_list"
			owner_cpu=""
			while IFS= read -r cpu; do
				[ -z "$TOPOLOGY_CPU_LIST" ] || [ "${allowed_cpus[$cpu]:-}" = 1 ] || continue
				package="$(cat "/sys/devices/system/cpu/cpu$cpu/topology/physical_package_id")"
				core="$(cat "/sys/devices/system/cpu/cpu$cpu/topology/core_id")"
				key="$package:$core"
				[ -z "${used_cores[$key]:-}" ] || continue
				used_cores[$key]=1
				owner_cpu="$cpu"
				break
			done < <(expand_cpu_list "$(cat "$hctx")")
			[ -n "$owner_cpu" ] || \
				die "owner $owner cannot find an unused CPU near hctx$owner_lane"
			owner_cpus+=("$owner_cpu")
			owner_hctx_lanes+=("$owner_lane")
		done
	fi
	for ((owner = 0; owner < WAL_OWNER_COUNT; owner++)); do
		cpu="${owner_cpus[$owner]}"
		[ -z "$TOPOLOGY_CPU_LIST" ] || [ "${allowed_cpus[$cpu]:-}" = 1 ] || \
			die "owner $owner CPU $cpu is outside TOPOLOGY_CPU_LIST"
		package="$(cat "/sys/devices/system/cpu/cpu$cpu/topology/physical_package_id")"
		core="$(cat "/sys/devices/system/cpu/cpu$cpu/topology/core_id")"
		key="$package:$core"
		if [ "${owner_hctx_lanes[$owner]}" = explicit ]; then
			[ -z "${used_cores[$key]:-}" ] || \
				die "owner $owner CPU $cpu overlaps an ingress role core"
			used_cores[$key]=1
		fi
	done
	all_cpus+=("${owner_cpus[@]}")
fi
if [ "$WAL_SPLIT_TRANSPORT" = 1 ]; then
	if [ -n "$WAL_TRANSPORT_CPU_LIST" ]; then
		mapfile -t transport_cpus < <(expand_cpu_list "$WAL_TRANSPORT_CPU_LIST")
		[ "${#transport_cpus[@]}" -eq "$LANES" ] || \
			die "WAL_TRANSPORT_CPU_LIST must provide exactly one CPU per lane"
	elif [ "${#transport_cpus[@]}" -eq "$LANES" ]; then
		:
	else
		for cpu in "${leaf_cpus[@]}"; do
			mapfile -t siblings < <(expand_cpu_list "$(cat "/sys/devices/system/cpu/cpu$cpu/topology/thread_siblings_list")")
			transport_cpu=""
			for sibling in "${siblings[@]}"; do
				[ "$sibling" = "$cpu" ] && continue
				[[ ",$(join_comma "${all_cpus[@]}")," != *",$sibling,"* ]] || continue
				transport_cpu="$sibling"
				break
			done
			[ -n "$transport_cpu" ] || die "leaf CPU $cpu has no free SMT sibling for split WAL transport"
			transport_cpus+=("$transport_cpu")
		done
	fi
	[ "${#transport_cpus[@]}" -eq "$LANES" ] || \
		die "split WAL transport requires one transport CPU per lane"
	transport_cpu_list="$(join_comma "${transport_cpus[@]}")"
	all_cpus+=("${transport_cpus[@]}")
else
	transport_cpu_list="inline"
fi
client_cpu_list="$(join_comma "${client_cpus[@]}")"
target_cpu_list="$(join_comma "${target_cpus[@]}")"
kernel_cpu_list="$(join_comma "${kernel_cpus[@]}")"
leaf_cpu_list="none"
[ "$START_LOCAL_LEAF" != 1 ] || leaf_cpu_list="$(join_comma "${leaf_cpus[@]}")"
coordinator_cpu="none"
case "$BACKEND:$LANES" in
	wal-tcp:1|tcp-leaf:1|fan-tcp:1) coordinator_cpu="${target_cpus[0]}" ;;
	wal-tcp:*|tcp-leaf:*|fan-tcp:*)
		if [ "$WAL_LANE_BATCH" != 1 ] || [ "$WAL_OWNER_DISPATCH" = 1 ]; then
			mapfile -t coordinator_candidates < <(expand_cpu_list "$(cat /sys/block/zcnblk0/mq/0/cpu_list)")
			for cpu in "${coordinator_candidates[@]}"; do
				package="$(cat "/sys/devices/system/cpu/cpu$cpu/topology/physical_package_id")"
				core="$(cat "/sys/devices/system/cpu/cpu$cpu/topology/core_id")"
				key="$package:$core"
				[ -z "${used_cores[$key]:-}" ] || continue
				used_cores[$key]=1
				coordinator_cpu="$cpu"
				all_cpus+=("$cpu")
				break
			done
			[ "$coordinator_cpu" != none ] || die "multi-lane WAL target needs a coordinator CPU distinct from lane workers"
		fi
		;;
esac
all_cpu_list="$(join_comma "${all_cpus[@]}")"

sqpoll_cpu_list="none"
case "$BLOCK_RING_MODE" in
	sqpoll|sqpoll-no-sqarray)
		declare -a sqpoll_cpus=()
		if [ -n "$SQPOLL_CPU_LIST" ]; then
			mapfile -t sqpoll_cpus < <(expand_cpu_list "$SQPOLL_CPU_LIST")
			[ "${#sqpoll_cpus[@]}" -eq "$LANES" ] || \
				die "SQPOLL_CPU_LIST must provide exactly one CPU per lane"
		else
			for cpu in "${client_cpus[@]}"; do
				mapfile -t siblings < <(expand_cpu_list "$(cat "/sys/devices/system/cpu/cpu$cpu/topology/thread_siblings_list")")
				sqpoll_cpu=""
				for sibling in "${siblings[@]}"; do
					[ "$sibling" = "$cpu" ] && continue
					sqpoll_cpu="$sibling"
					break
				done
				[ -n "$sqpoll_cpu" ] || die "client CPU $cpu has no SMT sibling for SQPOLL"
				sqpoll_cpus+=("$sqpoll_cpu")
			done
		fi
		sqpoll_cpu_list="$(join_comma "${sqpoll_cpus[@]}")"
		all_cpus+=("${sqpoll_cpus[@]}")
		all_cpu_list="$(join_comma "${all_cpus[@]}")"
		;;
	normal|no-sqarray|sq-rewind) ;;
	*) die "unsupported BLOCK_RING_MODE=$BLOCK_RING_MODE" ;;
esac

coord_resource="cpu=$all_cpu_list;memory-bandwidth=*"
[ "$START_LOCAL_LEAF" != 1 ] || coord_resource="$coord_resource;port=$LEAF_PORT"

coord_honored=true
if [ "$COORDINATION_SCOPE" = shared-host ]; then
	perf_result="$($COORD_BIN request --owner "codex:zcutils-zcnblk-shm" \
		--mode soft-exclusive --sensitivity critical --priority 50 --ttl 1800 \
		--resource "$coord_resource" \
		--note "zcnblk ${LANES}-lane mixed 4K control")"
	printf '%s\n' "$perf_result" | tee -a "$OUTDIR/coordination.log"
	perf_lease="$(token_from_result "$perf_result")"
	[ -n "$perf_lease" ] || die "agent-coord returned no performance lease token"
	coord_honored="$(honored_from_result "$perf_result")"
fi
if [ "$REPRESENTATIVE" = 1 ] && [ "$coord_honored" != true ]; then
	die "representative run refused because the soft-exclusive performance lease was not honored"
fi

{
	if [ "$COORDINATION_SCOPE" = dedicated-adhoc ]; then
		printf 'classification=dedicated-adhoc-control\n'
	else
		printf 'classification=%s\n' "$([ "$REPRESENTATIVE" = 1 ] && printf representative || printf noisy-local-control)"
	fi
	printf 'coordination_scope=%s\n' "$COORDINATION_SCOPE"
	printf 'bootstrap_manifest=%s\n' "$BOOTSTRAP_MANIFEST"
	printf 'lane_count=%s\n' "$LANES"
	printf 'topology_cpu_list=%s\n' "${TOPOLOGY_CPU_LIST:-unrestricted}"
	printf 'coordinator_cpu=%s\n' "$coordinator_cpu"
	for ((lane = 0; lane < LANES; lane++)); do
		lane_leaf_cpu=none
		[ "$START_LOCAL_LEAF" != 1 ] || lane_leaf_cpu="${leaf_cpus[$lane]}"
		printf 'lane=%s client_cpu=%s target_cpu=%s transport_cpu=%s kernel_cpu=%s leaf_cpu=%s hctx_cpus=%s\n' \
			"$lane" "${client_cpus[$lane]}" "${target_cpus[$lane]}" \
			"$([ "$WAL_SPLIT_TRANSPORT" = 1 ] && printf '%s' "${transport_cpus[$lane]}" || printf inline)" \
			"${kernel_cpus[$lane]}" "$lane_leaf_cpu" "$(cat "/sys/block/zcnblk0/mq/$lane/cpu_list")"
	done
	if [ "$WAL_OWNER_INGRESS" = 1 ]; then
		for ((owner = 0; owner < WAL_OWNER_COUNT; owner++)); do
			printf 'owner=%s owner_cpu=%s source_hctx_lane=%s numa_node=%s\n' \
				"$owner" "${owner_cpus[$owner]}" "${owner_hctx_lanes[$owner]}" \
				"$(cpu_numa_node "${owner_cpus[$owner]}")"
		done
	fi
	printf 'hugepages_total=%s\n' "$(awk '/HugePages_Total:/{print $2}' /proc/meminfo)"
	printf 'hugepages_free=%s\n' "$(awk '/HugePages_Free:/{print $2}' /proc/meminfo)"
	printf 'hugepage_size_kib=%s\n' "$(awk '/Hugepagesize:/{print $2}' /proc/meminfo)"
	printf 'memlock_kib=%s\n' "$(ulimit -l)"
	printf 'loadavg=%s\n' "$(cat /proc/loadavg)"
	printf 'coordination_honored=%s\n' "$coord_honored"
	printf 'target_poll_us=%s\n' "$POLL_US"
	printf 'target_busy_poll_us=%s\n' "$BUSY_POLL_US"
	printf 'target_busy_hysteresis_us=%s\n' "$BUSY_HYSTERESIS_US"
	printf 'target_poll_clock_check_spins=%s\n' "$POLL_CLOCK_CHECK_SPINS"
	printf 'kernel_completion_poll_us=%s\n' "$KERNEL_POLL_US"
	printf 'kernel_state_interval_ms=%s\n' "$KERNEL_STATE_INTERVAL_MS"
	printf 'block_ring_mode=%s sqpoll_cpu_list=%s sqpoll_idle_ms=%s\n' \
		"$BLOCK_RING_MODE" "$sqpoll_cpu_list" "$SQPOLL_IDLE_MS"
	printf 'block_engine=%s\n' "$BLOCK_ENGINE"
	printf 'block_latency_sample_rate=%s\n' "$LATENCY_SAMPLE_RATE"
	printf 'block_ring_stats=%s block_wait_min_completions=%s block_cqe_spin=%s block_cqe_adaptive_spin=%s block_cqe_adaptive_spin_min=%s block_cqe_adaptive_spin_max=%s block_cqe_adaptive_wait_ns=%s block_cqe_hot_poll=%s block_cqe_hot_poll_progress_spins=%s\n' \
		"$BLOCK_RING_STATS" "$BLOCK_WAIT_MIN_COMPLETIONS" "$BLOCK_CQE_SPIN" "$BLOCK_CQE_ADAPTIVE_SPIN" \
		"$BLOCK_CQE_ADAPTIVE_SPIN_MIN" "$BLOCK_CQE_ADAPTIVE_SPIN_MAX" \
		"$BLOCK_CQE_ADAPTIVE_WAIT_NS" "$BLOCK_CQE_HOT_POLL" "$BLOCK_CQE_HOT_POLL_PROGRESS_SPINS"
	printf 'shm_descriptor_entries_per_channel=%s\n' "$SHM_RING_ENTRIES"
	printf 'kernel_queue_depth=%s kernel_pipeline_depth=%s\n' \
		"$KERNEL_QUEUE_DEPTH" "$KERNEL_PIPELINE_DEPTH"
	printf 'shm_sector_order_slots=%s\n' "$SECTOR_ORDER_SLOTS"
	printf 'shm_payload_entries_per_channel=%s\n' "$SHM_PAYLOAD_ENTRIES"
	safe_writeback_limit=$((SHM_PAYLOAD_ENTRIES - SHM_RING_ENTRIES))
	[ "$safe_writeback_limit" -gt 0 ] || safe_writeback_limit=1
	case "$BACKEND" in
		wal-tcp|tcp-leaf|fan-tcp) safe_writeback_limit=$((safe_writeback_limit * LANES)) ;;
	esac
	effective_writeback_batch="$WRITEBACK_BATCH"
	[ "$effective_writeback_batch" -le "$safe_writeback_limit" ] || effective_writeback_batch="$safe_writeback_limit"
	printf 'writeback_batch_requested=%s writeback_batch_effective=%s\n' \
		"$WRITEBACK_BATCH" "$effective_writeback_batch"
	printf 'request_batch=%s request_batch_fill_us=%s request_batch_fill_min=%s\n' \
		"$REQUEST_BATCH" "$REQUEST_BATCH_FILL_US" "$REQUEST_BATCH_FILL_MIN"
	printf 'wal_lane_batch=%s representative_eligible=%s\n' \
		"$WAL_LANE_BATCH" true
	printf 'wal_owner_dispatch=%s wal_owner_extent_records=%s wal_owner_worker_spins=%s wal_owner_worker_adaptive_spin=%s wal_owner_worker_spin_min=%s wal_owner_worker_adaptive_wait_ns=%s\n' \
		"$WAL_OWNER_DISPATCH" "$WAL_OWNER_EXTENT_RECORDS" "$WAL_OWNER_WORKER_SPINS" \
		"$WAL_OWNER_WORKER_ADAPTIVE_SPIN" "$WAL_OWNER_WORKER_SPIN_MIN" \
		"$WAL_OWNER_WORKER_ADAPTIVE_WAIT_NS"
	printf 'wal_owner_ingress=%s wal_owner_count=%s wal_owner_cpu_list=%s\n' \
		"$WAL_OWNER_INGRESS" "$WAL_OWNER_COUNT" \
		"$([ "$WAL_OWNER_INGRESS" = 1 ] && join_comma "${owner_cpus[@]}" || printf none)"
	printf 'wal_owner_write_fill_us=%s wal_owner_write_fill_min=%s wal_owner_pipeline_batches=%s wal_owner_pipeline_refill_spins=%s wal_owner_mixed_hysteresis_us=%s\n' \
		"$WAL_OWNER_WRITE_FILL_US" "$WAL_OWNER_WRITE_FILL_MIN" "$WAL_OWNER_PIPELINE_BATCHES" \
		"$WAL_OWNER_PIPELINE_REFILL_SPINS" "$WAL_OWNER_MIXED_HYSTERESIS_US"
	printf 'wal_owner_debounce_us=%s wal_owner_backlog_low_records=%s wal_owner_backlog_high_records=%s\n' \
		"$WAL_OWNER_DEBOUNCE_US" "$WAL_OWNER_BACKLOG_LOW_RECORDS" "$WAL_OWNER_BACKLOG_HIGH_RECORDS"
	printf 'wal_owner_batch_records=%s wal_owner_fragment_records=%s wal_owner_fragment_fill_us=%s wal_owner_queue_depth=%s\n' \
		"$WAL_OWNER_BATCH_RECORDS" "$WAL_OWNER_FRAGMENT_RECORDS" "$WAL_OWNER_FRAGMENT_FILL_US" "$WAL_OWNER_QUEUE_DEPTH"
	printf 'wal_owner_foreground_immediate_limit=%s\n' "$WAL_OWNER_FOREGROUND_IMMEDIATE_LIMIT"
	printf 'wal_owner_max_tx_iovecs=%s\n' "$WAL_OWNER_MAX_TX_IOVECS"
	printf 'wal_lane_window=%s\n' "$WAL_LANE_WINDOW"
	printf 'wal_split_transport=%s wal_transport_cpu_list=%s\n' \
		"$WAL_SPLIT_TRANSPORT" "$transport_cpu_list"
	printf 'wal_transport_wait_policy=%s\n' \
		"$([ "$WAL_TRANSPORT_GREEDY" = 1 ] && printf greedy || printf adaptive)"
	effective_transfer_slots="$TRANSFER_SLOTS"
	[ "$WAL_OWNER_DISPATCH" != 1 ] || effective_transfer_slots=0
	printf 'transfer_payload_slots_requested=%s transfer_payload_slots_effective=%s\n' \
		"$TRANSFER_SLOTS" "$effective_transfer_slots"
	printf 'wal_extent_records=%s wal_extent_fill_us=%s wal_split_min_batch_records=%s\n' \
		"$WAL_EXTENT_RECORDS" "$WAL_EXTENT_FILL_US" "$WAL_SPLIT_MIN_BATCH_RECORDS"
	printf 'wal_compact_writes=%s\n' "$WAL_COMPACT_WRITES"
	printf 'dirty_pressure_reserve=%s\n' "$DIRTY_PRESSURE_RESERVE"
	printf 'remote_recv_spins=%s leaf_spin_reads=%s leaf_spin_policy=%s leaf_spin_budget=%s leaf_adaptive_spin_min=%s leaf_adaptive_spin_max=%s leaf_adaptive_wait_ns=%s leaf_adaptive_hysteresis_ns=%s\n' \
		"$REMOTE_RECV_SPINS" "$LEAF_SPIN_READS" \
		"$([ "$LEAF_ADAPTIVE_SPIN" = 1 ] && printf adaptive || { [ "$LEAF_SPIN_READS" = 1 ] && printf fixed || printf blocking; })" \
		"${URING_PLAY_ZCNBLK_WAL_LEAF_SPIN_BUDGET:-adaptive}" \
		"$LEAF_ADAPTIVE_SPIN_MIN" "$LEAF_ADAPTIVE_SPIN_MAX" "$LEAF_ADAPTIVE_WAIT_NS" \
		"$LEAF_ADAPTIVE_HYSTERESIS_NS"
	printf 'remote_recv_policy=%s remote_recv_adaptive_spin_min=%s remote_recv_adaptive_spin_max=%s remote_recv_adaptive_wait_ns=%s remote_recv_adaptive_hysteresis_ns=%s\n' \
		"$REMOTE_RECV_POLICY" "$REMOTE_RECV_ADAPTIVE_SPIN_MIN" \
		"$REMOTE_RECV_ADAPTIVE_SPIN_MAX" "$REMOTE_RECV_ADAPTIVE_WAIT_NS" \
		"$REMOTE_RECV_ADAPTIVE_HYSTERESIS_NS"
	printf 'remote_send_mode=%s remote_send_ring_entries=%s remote_send_zc_required=%s allow_unsafe_send_zc=%s\n' \
		"$REMOTE_SEND_MODE" "$REMOTE_SEND_RING_ENTRIES" "$REMOTE_SEND_ZC_REQUIRED" \
		"$ALLOW_UNSAFE_SEND_ZC"
	printf 'order_smoke_pairs=%s\n' "$ORDER_SMOKE_PAIRS"
	printf 'shm_payload_slot_bytes=%s\n' "$MAX_FRAME_BYTES"
	printf 'shm_lease_release_batch=%s\n' "$LEASE_RELEASE_BATCH"
	printf 'backend=%s local_leaf=%s leaf_target=%s leaf_addr=%s leaf_port=%s leaf_submit_mode=%s\n' \
		"$BACKEND" "$START_LOCAL_LEAF" "$LEAF_TARGET" "$LEAF_ADDR" "$LEAF_PORT" "$LEAF_SUBMIT_MODE"
	printf 'leaf_source_addr=%s\n' "${LEAF_SOURCE_ADDR:-kernel-route}"
	printf 'leaf_addrs=%s leaf_source_addrs=%s\n' \
		"${LEAF_ADDRS:-single-address}" "${LEAF_SOURCE_ADDRS:-single-source}"
} | tee "$OUTDIR/topology.log"
ps -eLo pid,tid,psr,pcpu,comm --sort=-pcpu | head -n 80 >"$OUTDIR/process-noise.before" || true

if [ "$BUFFER_MODE" != hugetlb ]; then
	printf 'PERF WARNING: BUFFER_MODE=%s; this is not a hugetlb representative run\n' "$BUFFER_MODE" | tee -a "$OUTDIR/preflight.log" >&2
	[ "$REPRESENTATIVE" != 1 ] || die "representative runs require BUFFER_MODE=hugetlb"
else
	hugepages_free="$(awk '/HugePages_Free:/{print $2}' /proc/meminfo)"
	required_hugepages=$((LANES * IODEPTH))
	printf 'hugetlb_preflight: free_pages=%s required_pages=%s reason=one-hugepage-per-registered-slot\n' \
		"$hugepages_free" "$required_hugepages" | tee -a "$OUTDIR/preflight.log"
	if [ "$hugepages_free" -lt "$required_hugepages" ]; then
		die "hugetlb needs at least $required_hugepages free pages for $LANES workers x iodepth $IODEPTH; found $hugepages_free"
	fi
fi
if [ "$SET_GOVERNOR" = performance ]; then
	governor_paths=(/sys/devices/system/cpu/cpu*/cpufreq/scaling_governor)
	if [ ! -e "${governor_paths[0]}" ]; then
		printf 'PERF WARNING: cpufreq governor controls are unavailable on this kernel; hardware-managed performance state is unchanged\n' | \
			tee -a "$OUTDIR/preflight.log" >&2
		governor_paths=()
	fi
	for path in "${governor_paths[@]}"; do
		printf '%s %s\n' "$path" "$(cat "$path")" >>"$governors_file"
		sudo -n sh -c 'printf performance > "$1"' sh "$path"
	done
else
	printf 'PERF WARNING: CPU governor is unchanged; record it before interpreting spread\n' | tee -a "$OUTDIR/preflight.log" >&2
fi

if [ "$START_LOCAL_LEAF" = 1 ]; then
	command -v ss >/dev/null || die "ss is required to validate the local WAL leaf listener"
	ss -H -ltn | awk -v port=":$LEAF_PORT" '$4 ~ port "$" { found=1 } END { exit found ? 0 : 1 }' && \
		die "TCP port $LEAF_PORT is already listening"
	log "starting terminal userspace WAL leaf on cpu $leaf_cpu_list"
	env URING_PLAY_PIN_CPU_LIST="$leaf_cpu_list" URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_SPIN_READS="$LEAF_SPIN_READS" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN="$LEAF_ADAPTIVE_SPIN" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MIN="$LEAF_ADAPTIVE_SPIN_MIN" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MAX="$LEAF_ADAPTIVE_SPIN_MAX" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_WAIT_NS="$LEAF_ADAPTIVE_WAIT_NS" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_HYSTERESIS_NS="$LEAF_ADAPTIVE_HYSTERESIS_NS" \
		"$LEAF_BIN" "$LEAF_TARGET" "$LEAF_ADDR" "$LEAF_PORT" "$LANES" 1 4096 "$LANES" true "$LEAF_SUBMIT_MODE" \
		>"$OUTDIR/leaf.log" 2>&1 &
	leaf_pid=$!
	for _ in $(seq 1 100); do
		ss -H -ltn | awk -v port=":$LEAF_PORT" '$4 ~ port "$" { found=1 } END { exit !found }' && break
		[ -e "/proc/$leaf_pid" ] || die "WAL leaf exited before listening; see $OUTDIR/leaf.log"
		sleep 0.05
	done
	ss -H -ltn | awk -v port=":$LEAF_PORT" '$4 ~ port "$" { found=1 } END { exit !found }' || \
		die "WAL leaf did not listen on $LEAF_ADDR:$LEAF_PORT"
fi

log "starting userspace shared target/fan; no placement decision exists in the kernel edge"
sudo -n env URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE="$pid_file" \
	URING_PLAY_TOPOLOGY_REPRESENTATIVE="$REPRESENTATIVE" \
	URING_PLAY_ZCNBLK_SHM_POLL_CLOCK_CHECK_SPINS="$POLL_CLOCK_CHECK_SPINS" \
	URING_PLAY_ZCNBLK_SHM_COORDINATOR_CPU="$coordinator_cpu" \
	URING_PLAY_ZCNBLK_SHM_LEASE_RELEASE_BATCH="$LEASE_RELEASE_BATCH" \
	URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH="$WRITEBACK_BATCH" \
	URING_PLAY_ZCNBLK_SHM_READ_BATCH="$REQUEST_BATCH" \
	URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_US="$REQUEST_BATCH_FILL_US" \
	URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_MIN="$REQUEST_BATCH_FILL_MIN" \
	URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH="$WAL_LANE_BATCH" \
	URING_PLAY_ZCNBLK_SHM_WAL_OWNER_DISPATCH="$WAL_OWNER_DISPATCH" \
	URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS="$WAL_OWNER_INGRESS" \
	URING_PLAY_ZCNBLK_SHM_OWNER_COUNT="$WAL_OWNER_COUNT" \
	URING_PLAY_ZCNBLK_SHM_OWNER_CPU_LIST="$([ "$WAL_OWNER_INGRESS" = 1 ] && join_comma "${owner_cpus[@]}" || printf '')" \
	URING_PLAY_ZCNBLK_SHM_OWNER_EXTENT_RECORDS="$WAL_OWNER_EXTENT_RECORDS" \
	URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_SPINS="$WAL_OWNER_WORKER_SPINS" \
	URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_ADAPTIVE_SPIN="$WAL_OWNER_WORKER_ADAPTIVE_SPIN" \
	URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_SPIN_MIN="$WAL_OWNER_WORKER_SPIN_MIN" \
	URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_ADAPTIVE_WAIT_NS="$WAL_OWNER_WORKER_ADAPTIVE_WAIT_NS" \
	URING_PLAY_ZCNBLK_SHM_OWNER_MIXED_HYSTERESIS_US="$WAL_OWNER_MIXED_HYSTERESIS_US" \
	URING_PLAY_ZCNBLK_SHM_OWNER_WRITE_FILL_US="$WAL_OWNER_WRITE_FILL_US" \
		URING_PLAY_ZCNBLK_SHM_OWNER_WRITE_FILL_MIN="$WAL_OWNER_WRITE_FILL_MIN" \
		URING_PLAY_ZCNBLK_SHM_OWNER_DEBOUNCE_US="$WAL_OWNER_DEBOUNCE_US" \
		URING_PLAY_ZCNBLK_SHM_OWNER_BACKLOG_HIGH_RECORDS="$WAL_OWNER_BACKLOG_HIGH_RECORDS" \
		URING_PLAY_ZCNBLK_SHM_OWNER_BACKLOG_LOW_RECORDS="$WAL_OWNER_BACKLOG_LOW_RECORDS" \
	URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_BATCHES="$WAL_OWNER_PIPELINE_BATCHES" \
	URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_REFILL_SPINS="$WAL_OWNER_PIPELINE_REFILL_SPINS" \
	URING_PLAY_ZCNBLK_SHM_OWNER_BATCH_RECORDS="$WAL_OWNER_BATCH_RECORDS" \
	URING_PLAY_ZCNBLK_SHM_OWNER_FRAGMENT_RECORDS="$WAL_OWNER_FRAGMENT_RECORDS" \
		URING_PLAY_ZCNBLK_SHM_OWNER_FRAGMENT_FILL_US="$WAL_OWNER_FRAGMENT_FILL_US" \
		URING_PLAY_ZCNBLK_SHM_OWNER_FOREGROUND_IMMEDIATE_LIMIT="$WAL_OWNER_FOREGROUND_IMMEDIATE_LIMIT" \
		URING_PLAY_ZCNBLK_SHM_OWNER_QUEUE_DEPTH="$WAL_OWNER_QUEUE_DEPTH" \
	URING_PLAY_ZCNBLK_SHM_OWNER_MAX_TX_IOVECS="$WAL_OWNER_MAX_TX_IOVECS" \
	URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW="$WAL_LANE_WINDOW" \
	URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_TRANSPORT="$WAL_SPLIT_TRANSPORT" \
	URING_PLAY_ZCNBLK_SHM_WAL_TRANSPORT_CPU_LIST="$([ "$WAL_SPLIT_TRANSPORT" = 1 ] && printf '%s' "$transport_cpu_list" || printf '')" \
	URING_PLAY_ZCNBLK_SHM_WAL_TRANSPORT_GREEDY="$WAL_TRANSPORT_GREEDY" \
	URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS="$TRANSFER_SLOTS" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_SPINS="$REMOTE_RECV_SPINS" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_POLICY="$REMOTE_RECV_POLICY" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MIN="$REMOTE_RECV_ADAPTIVE_SPIN_MIN" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MAX="$REMOTE_RECV_ADAPTIVE_SPIN_MAX" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_WAIT_NS="$REMOTE_RECV_ADAPTIVE_WAIT_NS" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_HYSTERESIS_NS="$REMOTE_RECV_ADAPTIVE_HYSTERESIS_NS" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE="$REMOTE_SEND_MODE" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_RING_ENTRIES="$REMOTE_SEND_RING_ENTRIES" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_ZC_REQUIRED="$REMOTE_SEND_ZC_REQUIRED" \
	URING_PLAY_ALLOW_UNSAFE_SEND_ZC="$ALLOW_UNSAFE_SEND_ZC" \
	URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_RECORDS="$WAL_EXTENT_RECORDS" \
	URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_FILL_US="$WAL_EXTENT_FILL_US" \
	URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_MIN_BATCH_RECORDS="$WAL_SPLIT_MIN_BATCH_RECORDS" \
	URING_PLAY_ZCNBLK_SHM_WAL_COMPACT_WRITES="$WAL_COMPACT_WRITES" \
	URING_PLAY_ZCNBLK_SHM_DIRTY_PRESSURE_RESERVE="$DIRTY_PRESSURE_RESERVE" \
	URING_PLAY_ZCNBLK_SHM_WAL_DEBUG_STATE="$WAL_DEBUG_STATE" \
	URING_PLAY_ROUTE_PROBE="${URING_PLAY_ROUTE_PROBE:-0}" \
	URING_PLAY_EXPECT_ROUTE_DEV="${URING_PLAY_EXPECT_ROUTE_DEV:-}" \
	URING_PLAY_EXPECT_ROUTE_SRC="${URING_PLAY_EXPECT_ROUTE_SRC:-}" \
	URING_PLAY_TOPOLOGY_STRICT="${URING_PLAY_TOPOLOGY_STRICT:-0}" \
	URING_PLAY_TOPOLOGY_FATAL="${URING_PLAY_TOPOLOGY_FATAL:-0}" \
	URING_PLAY_ZCNBLK_SHM_LEAF_ADDR="$LEAF_ADDR:$LEAF_PORT" \
	URING_PLAY_ZCNBLK_SHM_LEAF_SOURCE_ADDR="$LEAF_SOURCE_ADDR" \
	URING_PLAY_ZCNBLK_SHM_LEAF_ADDRS="$LEAF_ADDRS" \
	URING_PLAY_ZCNBLK_SHM_LEAF_SOURCE_ADDRS="$LEAF_SOURCE_ADDRS" \
	"$TARGET_BIN" /dev/zcnblk-shmctl "$BACKEND" "$KICK_BATCH" \
	"$target_cpu_list" "$POLL_US" "$BUSY_POLL_US" "$BUSY_HYSTERESIS_US" \
	>"$OUTDIR/target.log" 2>&1 &
target_job_pid=$!
for _ in $(seq 1 100); do
	[ -s "$pid_file" ] && break
	sleep 0.05
done
[ -s "$pid_file" ] || die "target did not publish its PID file"
target_pid="$(cat "$pid_file")"
[[ "$target_pid" =~ ^[0-9]+$ ]] || die "invalid target PID: $target_pid"

declare -A kthread_pid_by_name=()
for _ in $(seq 1 100); do
	kthread_pid_by_name=()
	while read -r kthread_pid name; do
		[[ "$name" == zcnblk-shm-*-0 ]] || continue
		[ -z "${kthread_pid_by_name[$name]:-}" ] || \
			die "multiple kernel threads have exact name $name"
		kthread_pid_by_name[$name]="$kthread_pid"
	done < <(ps -e -o pid=,comm=)
	[ "${#kthread_pid_by_name[@]}" -ge "$LANES" ] && break
	sleep 0.05
done
for ((lane = 0; lane < LANES; lane++)); do
	name="zcnblk-shm-$lane-0"
	kthread_pid="${kthread_pid_by_name[$name]:-}"
	[ -n "$kthread_pid" ] || die "could not find exact kthread $name"
	kthread_pids+=("$kthread_pid")
	sudo -n taskset -pc "${kernel_cpus[$lane]}" "$kthread_pid" >>"$OUTDIR/kthreads.log"
	printf 'lane=%s pid=%s cpu=%s name=%s\n' "$lane" "$kthread_pid" "${kernel_cpus[$lane]}" "$name" >>"$OUTDIR/kthreads.log"
done

case "$BACKEND" in
	wal-memory) expected_target_tasks=1 ;;
	*) expected_target_tasks=$((LANES > 1 ? LANES + 1 : 1)) ;;
esac
[ "$WAL_SPLIT_TRANSPORT" != 1 ] || expected_target_tasks=$((LANES * 2 + 1))
[ "$WAL_OWNER_INGRESS" != 1 ] || expected_target_tasks=$((LANES + WAL_OWNER_COUNT + 1))
for _ in $(seq 1 100); do
	mapfile -t target_tasks < <(find "/proc/$target_pid/task" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' 2>/dev/null | sort -n)
	[ "${#target_tasks[@]}" -ge "$expected_target_tasks" ] && break
	sleep 0.05
done
[ "${#target_tasks[@]}" -ge "$expected_target_tasks" ] || die "target worker threads did not appear"
tracked_pids=("${target_tasks[@]}" "${kthread_pids[@]}")
if [ "$START_LOCAL_LEAF" = 1 ]; then
	for _ in $(seq 1 100); do
		mapfile -t leaf_tasks < <(find "/proc/$leaf_pid/task" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' 2>/dev/null | sort -n)
		[ "${#leaf_tasks[@]}" -ge 2 ] && break
		sleep 0.05
	done
	[ "${#leaf_tasks[@]}" -ge 2 ] || die "leaf stream worker thread did not appear"
	tracked_pids+=("${leaf_tasks[@]}")
fi
snapshot_contexts "$OUTDIR/hot-contexts.initial"

if [ "$KERNEL_STATE_INTERVAL_MS" -gt 0 ]; then
	debug_state=/sys/kernel/debug/zcnblk/state
	sudo -n test -r "$debug_state" || \
		die "kernel SHM state sampling requested but $debug_state is unavailable"
	state_interval="$(awk -v ms="$KERNEL_STATE_INTERVAL_MS" 'BEGIN { printf "%.3f", ms / 1000 }')"
	(
		while grep -q '^zcnblk_client_mod ' /proc/modules 2>/dev/null; do
			printf 'timestamp_ns=%s\n' "$(date +%s%N)"
			sudo -n cat "$debug_state"
			sleep "$state_interval"
		done
	) >>"$OUTDIR/kernel-shm-state.log" 2>&1 &
	kernel_state_pid=$!
fi

if [ "$ORDER_SMOKE_PAIRS" -gt 0 ]; then
	log "proving same-sector ordering and sync across the live $LANES-lane path"
	sudo -n env "URING_PLAY_PIN_CPU_LIST=$client_cpu_list" \
		"$ORDER_BIN" /dev/zcnblk0 "$ORDER_SMOKE_PAIRS" | tee "$OUTDIR/order-smoke.log"
	grep -q 'sync_terminal_state=true' "$OUTDIR/order-smoke.log" || \
		die "multi-lane order smoke did not prove terminal sync state"
fi

log "running $REPEATS repeated $MODE controls on the shared host"
for ((rep = 1; rep <= REPEATS; rep++)); do
	result_log="$OUTDIR/rep$rep.log"
	perf_log="$OUTDIR/rep$rep.perf"
	context_before="$OUTDIR/rep$rep.context.before"
	context_after="$OUTDIR/rep$rep.context.after"
	snapshot_contexts "$context_before"
	bench=(env "URING_PLAY_PIN_CPU_LIST=$client_cpu_list"
		"URING_PLAY_TOPOLOGY_STRICT=$REPRESENTATIVE"
		"URING_PLAY_BLOCKBENCH_RING_STATS=$BLOCK_RING_STATS"
		"URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS=$BLOCK_WAIT_MIN_COMPLETIONS"
		"URING_PLAY_CQE_SPIN=$BLOCK_CQE_SPIN"
		"URING_PLAY_CQE_ADAPTIVE_SPIN=$BLOCK_CQE_ADAPTIVE_SPIN"
		"URING_PLAY_CQE_ADAPTIVE_SPIN_MIN=$BLOCK_CQE_ADAPTIVE_SPIN_MIN"
		"URING_PLAY_CQE_ADAPTIVE_SPIN_MAX=$BLOCK_CQE_ADAPTIVE_SPIN_MAX"
		"URING_PLAY_CQE_ADAPTIVE_WAIT_NS=$BLOCK_CQE_ADAPTIVE_WAIT_NS"
		"URING_PLAY_CQE_HOT_POLL=$BLOCK_CQE_HOT_POLL"
		"URING_PLAY_CQE_HOT_POLL_PROGRESS_SPINS=$BLOCK_CQE_HOT_POLL_PROGRESS_SPINS"
		"$BENCH_BIN" /dev/zcnblk0
		--engine "$BLOCK_ENGINE" --mode "$MODE" --workers "$LANES"
		--ops-per-worker "$OPS_PER_WORKER" --bs 4096 --iodepth "$IODEPTH"
		--region-bytes-per-worker "$REGION_BYTES_PER_WORKER"
		--read-percent "$READ_PERCENT" --ring-entries "$RING_ENTRIES"
		--ring-mode "$BLOCK_RING_MODE" --sqpoll-idle-ms "$SQPOLL_IDLE_MS"
		--buffer-mode "$BUFFER_MODE" --pin true)
	if [ "$LATENCY_SAMPLE_RATE" -gt 0 ]; then
		bench+=(--latency-sample-rate "$LATENCY_SAMPLE_RATE")
	fi
	if [ "$sqpoll_cpu_list" != none ]; then
		bench+=(--sqpoll-cpus "$sqpoll_cpu_list")
	fi
	if [ "$PERF_STAT" = 1 ] && command -v perf >/dev/null 2>&1; then
		sudo -n perf stat -o "$perf_log" \
			-e task-clock,context-switches,cpu-migrations,cycles,instructions,cache-misses \
			-- "${bench[@]}" >"$result_log" 2>&1
	else
		sudo -n "${bench[@]}" >"$result_log" 2>&1
	fi
	snapshot_contexts "$context_after"
	awk -v logical_ops="$((LANES * OPS_PER_WORKER))" '
		NR == FNR { voluntary[$1]=$4; involuntary[$1]=$5; next }
		{
			v=$4-voluntary[$1]; iv=$5-involuntary[$1]; total=v+iv;
			printf "pid=%s name=%s cpus=%s voluntary=%d involuntary=%d total=%d per_1k_logical_io=%.3f\n",
				$1, $2, $3, v, iv, total, total*1000/logical_ops;
		}
	' "$context_before" "$context_after" >"$OUTDIR/rep$rep.context.delta"
	awk -v rep="$rep" -v logical_ops="$((LANES * OPS_PER_WORKER))" '
		{
			split($2, name, "="); split($6, total, "=");
			if (name[2] == "zcnblk-shm-targ" || index(name[2], "zcwal-lane-") == 1 || index(name[2], "zcwal-tx-") == 1 || index(name[2], "zcwal-owner-") == 1) target += total[2];
			else if (index(name[2], "zcnblk-wal-lea") == 1) leaf += total[2];
			else kernel += total[2];
		}
		END {
			printf "repeat=%d target_context_switches=%d target_per_1k_logical_io=%.3f leaf_context_switches=%d leaf_per_1k_logical_io=%.3f kernel_context_switches=%d kernel_per_1k_logical_io=%.3f\n",
				rep, target, target*1000/logical_ops, leaf, leaf*1000/logical_ops, kernel, kernel*1000/logical_ops;
		}
	' "$OUTDIR/rep$rep.context.delta" | tee -a "$OUTDIR/context-results.log"
	if [ -s "$perf_log" ]; then
		awk -v rep="$rep" -v logical_ops="$((LANES * OPS_PER_WORKER))" '
			/context-switches/ { gsub(/,/, "", $1); switches=$1+0 }
			/cpu-migrations/ { gsub(/,/, "", $1); migrations=$1+0 }
			END {
				printf "repeat=%d client_context_switches=%d client_per_1k_logical_io=%.3f client_migrations=%d\n",
					rep, switches, switches*1000/logical_ops, migrations;
			}
		' "$perf_log" | tee -a "$OUTDIR/context-results.log"
	fi
	line="$(grep 'zcblockbench-result:' "$result_log" | tail -n 1)"
	[ -n "$line" ] || die "repeat $rep produced no result"
	printf 'repeat=%s %s\n' "$rep" "$line" | tee -a "$OUTDIR/results.log"
	latency_line="$(grep 'zcblockbench-latency:' "$result_log" | tail -n 1 || true)"
	[ -z "$latency_line" ] || printf 'repeat=%s %s\n' "$rep" "$latency_line" | tee -a "$OUTDIR/results.log"
	ring_line="$(grep 'zcblockbench-ring:' "$result_log" | tail -n 1 || true)"
	[ -z "$ring_line" ] || printf 'repeat=%s %s\n' "$rep" "$ring_line" | tee -a "$OUTDIR/results.log"
done

safe_stop_target
target_pid=""
wait "$target_job_pid" || true
target_job_pid=""
if [ "$START_LOCAL_LEAF" = 1 ]; then
	for _ in $(seq 1 100); do
		[ ! -e "/proc/$leaf_pid" ] && break
		[[ "$(awk '{print $3}' "/proc/$leaf_pid/stat" 2>/dev/null || true)" == Z ]] && break
		sleep 0.05
	done
	if [ -e "/proc/$leaf_pid" ] && [[ "$(awk '{print $3}' "/proc/$leaf_pid/stat")" != Z ]]; then
		die "WAL leaf did not exit after target EOF"
	fi
	wait "$leaf_pid" || true
	leaf_pid=""
fi
ps -eLo pid,tid,psr,pcpu,comm --sort=-pcpu | head -n 80 >"$OUTDIR/process-noise.after" || true

awk '
  /zcblockbench-result:/ {
    for (i = 1; i <= NF; i++) if ($i ~ /^ops_per_sec=/) {
      split($i, a, "="); value = a[2] + 0;
      if (count == 0 || value < min) min = value;
      if (count == 0 || value > max) max = value;
      total += value; count++;
    }
  }
  END {
    if (count) printf "runs=%d min_iops=%.0f mean_iops=%.0f max_iops=%.0f spread_pct=%.2f\n", count, min, total/count, max, (max-min)/(total/count)*100;
  }
' "$OUTDIR/results.log" | tee "$OUTDIR/summary.log"
grep 'zcnblk-shm-target-summary:' "$OUTDIR/target.log" | tee -a "$OUTDIR/summary.log"
if [ "$START_LOCAL_LEAF" = 1 ]; then
	grep 'zcnblk-shm-target-remote-leaf-summary:' "$OUTDIR/target.log" | tee -a "$OUTDIR/summary.log"
	grep 'zcnblk-wal-leaf-summary:' "$OUTDIR/leaf.log" | tee -a "$OUTDIR/summary.log"
fi
printf 'artifact=%s\n' "$OUTDIR" | tee -a "$OUTDIR/summary.log"
