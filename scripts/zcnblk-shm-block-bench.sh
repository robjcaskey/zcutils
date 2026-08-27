#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
COORDINATION_SCOPE="${COORDINATION_SCOPE:-shared-host}"
BOOTSTRAP_MANIFEST="${ZCUTILS_BOOTSTRAP_MANIFEST:-$HOME/.local/state/zcutils/adhoc-bootstrap.env}"
MODULE="${MODULE:-$ROOT/kmods/zcnblk_client_mod.ko}"
TARGET_BIN="${TARGET_BIN:-$ROOT/target/release/zcnblk-shm-target}"
BENCH_BIN="${BENCH_BIN:-$ROOT/target/release/zcblockbench}"
EDGE_SYNC_BIN="${EDGE_SYNC_BIN:-$ROOT/target/release/zcnblk-edge-sync}"
EDGE_CONTINUITY_BIN="${EDGE_CONTINUITY_BIN:-$ROOT/target/release/zcnblk-edge-continuity}"
DIRECT_MIGRATECTL_BIN="${DIRECT_MIGRATECTL_BIN:-$ROOT/target/release/zcnblk-direct-migratectl}"
ORDER_BIN="${ORDER_BIN:-$ROOT/target/release/zcnblk-order-smoke}"
ORDER_SMOKE_PAIRS="${ORDER_SMOKE_PAIRS:-0}"
CONTRACT_BIN="${CONTRACT_BIN:-$ROOT/target/release/zcnblk-contract-smoke}"
CONTRACT_SMOKE_BLOCK="${CONTRACT_SMOKE_BLOCK:-}"
LEAF_BIN="${LEAF_BIN:-$ROOT/target/release/zcnblk-wal-leaf}"
LANES="${LANES:-4}"
REPEATS="${REPEATS:-3}"
MIN_IOPS_PER_REP="${MIN_IOPS_PER_REP:-0}"
MIN_MEAN_IOPS="${MIN_MEAN_IOPS:-0}"
OPS_PER_WORKER="${OPS_PER_WORKER:-2000000}"
IODEPTH="${IODEPTH:-128}"
RING_ENTRIES="${RING_ENTRIES:-256}"
BACKEND="${BACKEND:-memory}"
BLOCK_RING_MODE="${BLOCK_RING_MODE:-normal}"
BLOCK_REGISTERED_RING="${URING_PLAY_BLOCKBENCH_REGISTERED_RING:-0}"
BLOCK_ENGINE="${BLOCK_ENGINE:-uring-fixed}"
BLOCK_SIZE="${BLOCK_SIZE:-4096}"
BLOCK_FUA_WRITES="${URING_PLAY_BLOCKBENCH_FUA_WRITES:-0}"
BLOCK_NOATIME="${URING_PLAY_BLOCKBENCH_NOATIME:-1}"
SQPOLL_CPU_LIST="${SQPOLL_CPU_LIST:-}"
SQPOLL_IDLE_MS="${SQPOLL_IDLE_MS:-1000}"
LATENCY_SAMPLE_RATE="${URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE:-0}"
ZCCUSAN_PLACEMENT_SCOPE="${ZCCUSAN_PLACEMENT_SCOPE:-unknown}"
ZCCUSAN_TOPOLOGY_CLASS="${ZCCUSAN_TOPOLOGY_CLASS:-client-leaf}"
ZCCUSAN_TOPOLOGY_PATH_COUNT="${ZCCUSAN_TOPOLOGY_PATH_COUNT:-1}"
ZCCUSAN_TOPOLOGY_TRANSPORT="${ZCCUSAN_TOPOLOGY_TRANSPORT:-unknown}"
ZCCUSAN_TOPOLOGY_NUMA_NODE_COUNT="${ZCCUSAN_TOPOLOGY_NUMA_NODE_COUNT:-}"
ZCCUSAN_TOPOLOGY_NUMA_LOCAL="${ZCCUSAN_TOPOLOGY_NUMA_LOCAL:-0}"
BLOCK_RING_STATS="${URING_PLAY_BLOCKBENCH_RING_STATS:-1}"
if [ -n "${URING_PLAY_BLOCKBENCH_COMPLETION_BATCH+x}" ]; then
	BLOCK_COMPLETION_BATCH="$URING_PLAY_BLOCKBENCH_COMPLETION_BATCH"
elif [ "$IODEPTH" -ge 128 ] && [ "$BACKEND" = memory ]; then
	# Saturation controls amortize io_uring entry cost. QD128 measurements put
	# 128/32 ahead of 64/16 for read, write, and mixed exact-alias workloads.
	BLOCK_COMPLETION_BATCH=128
elif [ "$IODEPTH" -lt 64 ]; then
	BLOCK_COMPLETION_BATCH="$IODEPTH"
else
	BLOCK_COMPLETION_BATCH=64
fi
if [ -n "${URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS+x}" ]; then
	BLOCK_WAIT_MIN_COMPLETIONS="$URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS"
elif [ "$IODEPTH" -ge 128 ] && [ "$BACKEND" = memory ]; then
	BLOCK_WAIT_MIN_COMPLETIONS=32
elif [ "$IODEPTH" -ge 128 ]; then
	# Preserve the measured remote WAL default until the 128/32 candidate is
	# repeated on a dedicated cross-node transport topology.
	BLOCK_WAIT_MIN_COMPLETIONS=16
else
	BLOCK_WAIT_MIN_COMPLETIONS=1
fi
BLOCK_FUSED_SUBMIT_WAIT="${URING_PLAY_BLOCKBENCH_FUSED_SUBMIT_WAIT:-0}"
BLOCK_CQE_SPIN="${URING_PLAY_BLOCKBENCH_CQE_SPIN:-0}"
BLOCK_CQE_ADAPTIVE_SPIN="${URING_PLAY_BLOCKBENCH_CQE_ADAPTIVE_SPIN:-0}"
BLOCK_CQE_ADAPTIVE_SPIN_MIN="${URING_PLAY_BLOCKBENCH_CQE_ADAPTIVE_SPIN_MIN:-0}"
BLOCK_CQE_ADAPTIVE_SPIN_MAX="${URING_PLAY_BLOCKBENCH_CQE_ADAPTIVE_SPIN_MAX:-4096}"
BLOCK_CQE_ADAPTIVE_WAIT_NS="${URING_PLAY_BLOCKBENCH_CQE_ADAPTIVE_WAIT_NS:-50000}"
BLOCK_CQE_HOT_POLL="${URING_PLAY_BLOCKBENCH_CQE_HOT_POLL:-0}"
BLOCK_WBT_LAT_USEC="${URING_PLAY_ZCNBLK_WBT_LAT_USEC:-}"
if [ -n "${URING_PLAY_BLOCKBENCH_CQE_HOT_POLL_PROGRESS_SPINS+x}" ]; then
	BLOCK_CQE_HOT_POLL_PROGRESS_SPINS="$URING_PLAY_BLOCKBENCH_CQE_HOT_POLL_PROGRESS_SPINS"
# A short progress window wins through the latency/efficiency curve.  A 4,096
# spin window at QD8/QD16 delayed io_uring re-entry enough to create a large,
# transport-independent throughput notch; reserve it for saturation depths.
elif [ "$IODEPTH" -le 16 ]; then
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
KERNEL_QUEUES="${KERNEL_QUEUES:-$LANES}"
KERNEL_WORKER_BATCH_DEQUEUE="${KERNEL_WORKER_BATCH_DEQUEUE:-1}"
KERNEL_DISABLE_MERGES="${KERNEL_DISABLE_MERGES:-1}"
KERNEL_SEQUENCE_TELEMETRY_INTERVAL="${KERNEL_SEQUENCE_TELEMETRY_INTERVAL:-256}"
KERNEL_COMPLETION_BATCH="${KERNEL_COMPLETION_BATCH:-256}"
# Keep SMT siblings in the same blk-mq hardware context.  Generic blk-mq
# mapping can split sibling threads between hctxs, which prevents the strict
# topology planner from assigning distinct client, target, and completion
# cores at saturation.  The whole-core map is topology-time only and remains
# explicitly overridable with HCTX_NUMA_NODE=-1 for kernel-default controls.
HCTX_NUMA_NODE="${HCTX_NUMA_NODE:--3}"
SIZE_MIB="${SIZE_MIB:-$((LANES * 128))}"
REGION_BYTES_PER_WORKER="${REGION_BYTES_PER_WORKER:-67108864}"
case "$BACKEND" in
	memory|wal-tcp) lane_local_sequences_default=1 ;;
	*) lane_local_sequences_default=0 ;;
esac
LANE_LOCAL_SEQUENCES="${URING_PLAY_ZCNBLK_SHM_LANE_LOCAL_SEQUENCES:-$lane_local_sequences_default}"
APP_ARENA_BUFFERS="${URING_PLAY_ZCNBLK_SHM_APP_ARENA_BUFFERS:-0}"
START_LOCAL_LEAF="${START_LOCAL_LEAF:-$([ "$BACKEND" = wal-tcp ] && printf 1 || printf 0)}"
EXTERNAL_LEAF_TOPOLOGY_ARTIFACT="${EXTERNAL_LEAF_TOPOLOGY_ARTIFACT:-}"
MODE="${MODE:-rw}"
READ_PERCENT="${READ_PERCENT:-50}"
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
EXTERNAL_NIC_LOW_LATENCY_CONFIRMED="${URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED:-0}"
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
HTB_SUSTAINED_IOPS="${URING_PLAY_ZCNBLK_SHM_HTB_SUSTAINED_IOPS:-}"
HTB_PEAK_IOPS="${URING_PLAY_ZCNBLK_SHM_HTB_PEAK_IOPS:-}"
HTB_QUANTUM_OPS="${URING_PLAY_ZCNBLK_SHM_HTB_QUANTUM_OPS:-}"
HTB_BURST_SECONDS="${URING_PLAY_ZCNBLK_SHM_HTB_BURST_SECONDS:-}"
HTB_CONTROL_FILE="${URING_PLAY_ZCNBLK_SHM_HTB_CONTROL_FILE:-}"
KERNEL_POLL_US="${KERNEL_POLL_US:-$POLL_US}"
LEASE_RELEASE_BATCH="${LEASE_RELEASE_BATCH:-1}"
MAX_FRAME_BYTES="${MAX_FRAME_BYTES:-4096}"
BUFFER_MODE="${BUFFER_MODE:-small-pages}"
if [ -n "${URING_PLAY_ZCNBLK_SHM_ARENA_BACKING+x}" ]; then
SHM_ARENA_BACKING="$URING_PLAY_ZCNBLK_SHM_ARENA_BACKING"
elif [ "$BUFFER_MODE" = hugetlb ]; then
	SHM_ARENA_BACKING=hugetlb
else
	SHM_ARENA_BACKING=vmalloc
fi
SHM_ARENA_CPU_LIST="${URING_PLAY_ZCNBLK_SHM_ARENA_CPU_LIST:-}"
SHM_ARENA_CPU_LIST_SOURCE="$([ -n "$SHM_ARENA_CPU_LIST" ] && printf explicit || printf unassigned)"
SHM_ARENA_LOCALITY=inactive
LEAF_ADDR="${LEAF_ADDR:-127.0.0.1}"
LEAF_PORT="${LEAF_PORT:-29000}"
LEAF_SOURCE_ADDR="${LEAF_SOURCE_ADDR:-}"
LEAF_ADDRS="${LEAF_ADDRS:-}"
LEAF_SOURCE_ADDRS="${LEAF_SOURCE_ADDRS:-}"
LEAF_SUBMIT_MODE="${LEAF_SUBMIT_MODE:-blocking}"
REMOTE_TRANSPORT="${URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT:-tcp}"
REMOTE_OFI_PROVIDER="${URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER:-efa}"
REMOTE_OFI_ENDPOINT="${URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT:-rdm}"
OFI_THREADING="${URING_PLAY_OFI_THREADING:-unspec}"
OFI_SELECTIVE_COMPLETION="${URING_PLAY_OFI_SELECTIVE_COMPLETION:-0}"
OFI_RMA_READ_COMPLETION_STRIDE="${URING_PLAY_OFI_RMA_READ_COMPLETION_STRIDE:-1}"
OFI_RMA_DEFER_TAIL_COMPLETION="${URING_PLAY_OFI_RMA_DEFER_TAIL_COMPLETION:-1}"
if [ -n "${URING_PLAY_OFI_RMA_READ_MORE+x}" ]; then
	OFI_RMA_READ_MORE="$URING_PLAY_OFI_RMA_READ_MORE"
elif [ "$REMOTE_OFI_PROVIDER" = efa ]; then
	OFI_RMA_READ_MORE=1
else
	OFI_RMA_READ_MORE=0
fi
SHM_OFI_DOMAINS="${URING_PLAY_ZCNBLK_SHM_OFI_DOMAINS:-}"
OFI_DOMAIN="${URING_PLAY_OFI_DOMAIN:-}"
OFI_CQ_SLEEP_NS="${URING_PLAY_OFI_CQ_SLEEP_NS:-50000}"
WAL_OFI_MESSAGE_BYTES="${URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES:-1048576}"
WAL_OFI_HUGETLB_CONFIRMED="${URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED:-0}"
EFA_USE_DEVICE_RDMA="${FI_EFA_USE_DEVICE_RDMA:-0}"
EFA_IFACE="${FI_EFA_IFACE:-}"
OFI_EFA_FABRIC="${URING_PLAY_OFI_EFA_FABRIC:-}"
SHM_OFI_RMA_READS="${URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS:-0}"
SHM_OFI_RMA_READ_QD="${URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD:-1}"
LEAF_OFI_RMA_READS="${URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS:-$SHM_OFI_RMA_READS}"
SHM_OFI_RMA_WRITES="${URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES:-0}"
SHM_OFI_RMA_WRITES_REQUIRED="${URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES_REQUIRED:-$SHM_OFI_RMA_WRITES}"
SHM_OFI_RMA_WRITE_OWNER_MODE="${URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_OWNER_MODE:-placement}"
SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED="${URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED:-0}"
if [ -n "${URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_QD+x}" ]; then
	SHM_OFI_RMA_WRITE_QD="$URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_QD"
	SHM_OFI_RMA_WRITE_QD_SOURCE=explicit-wal-payload
elif [ -n "${URING_PLAY_OFI_RMA_WRITE_QD+x}" ]; then
	SHM_OFI_RMA_WRITE_QD="$URING_PLAY_OFI_RMA_WRITE_QD"
	SHM_OFI_RMA_WRITE_QD_SOURCE=generic-ofi-compat
elif [ "$SHM_OFI_RMA_WRITES" = 1 ] && \
	[ "$SHM_OFI_RMA_WRITE_OWNER_MODE" = single-domain-fan-in ]; then
	# A single EFA endpoint needs enough delivery-complete operations in flight
	# to expose the device's high-PPS path. This remains independent of block QD.
	SHM_OFI_RMA_WRITE_QD=64
	SHM_OFI_RMA_WRITE_QD_SOURCE=single-domain-fan-in-default
elif [ "$SHM_OFI_RMA_WRITES" = 1 ]; then
	# This is a transport-operation window, not the block workload's per-worker
	# QD. Random writes commonly produce several disjoint final-memory runs in
	# one userspace batch even when the block edge has aggregate QD1.
	SHM_OFI_RMA_WRITE_QD=16
	SHM_OFI_RMA_WRITE_QD_SOURCE=independent-fast-path-default
else
	SHM_OFI_RMA_WRITE_QD=1
	SHM_OFI_RMA_WRITE_QD_SOURCE=inactive-default
fi
SHM_OFI_RMA_WRITE_MIN_QD="${URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_MIN_QD:-16}"
LEAF_OFI_RMA_WRITES="${URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_WRITES:-$SHM_OFI_RMA_WRITES}"
OFI_ENDPOINT_RMA_READ_QD="$([ "$SHM_OFI_RMA_READS" = 1 ] && printf '%s' "$SHM_OFI_RMA_READ_QD" || printf 1)"
OFI_ENDPOINT_RMA_WRITE_QD="$([ "$SHM_OFI_RMA_WRITES" = 1 ] && printf '%s' "$SHM_OFI_RMA_WRITE_QD" || printf 1)"
OFI_RMA_WRITE_DELIVERY_COMPLETE="${URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE:-1}"
OFI_RMA_WRITE_MORE="${URING_PLAY_OFI_RMA_WRITE_MORE:-0}"
OFI_RMA_WRITE_MORE_BURST="${URING_PLAY_OFI_RMA_WRITE_MORE_BURST:-64}"
OFI_RMA_SOURCE_HUGETLB_CONFIRMED="${URING_PLAY_ZCNBLK_SHM_RMA_SOURCE_HUGETLB_CONFIRMED:-0}"
OFI_CONTROL_PORT_OFFSET="${URING_PLAY_OFI_CONTROL_PORT_OFFSET:-1000}"
LEAF_TRANSPORT="${URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT:-$REMOTE_TRANSPORT}"
LEAF_OFI_PROVIDER="${URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER:-$REMOTE_OFI_PROVIDER}"
LEAF_OFI_ENDPOINT="${URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT:-$REMOTE_OFI_ENDPOINT}"
LEAF_ZCMEM_HUGETLB="${URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB:-0}"
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
if [ -n "${URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS+x}" ]; then
	WAL_OWNER_INGRESS="$URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS"
	WAL_OWNER_INGRESS_SOURCE=explicit
elif [ "$BACKEND" = wal-tcp ] && [ "$START_LOCAL_LEAF" != 1 ] && \
	[ "$REPRESENTATIVE" = 1 ] && [ "$MODE" = write ]; then
	# Stable-owner fixes the representative external-WAL write regression.  It
	# also supports negotiated RMA reads, including mixed batches, but keep the
	# automatic policy scoped to the measured write case until an owner-count
	# sweep proves a broader default.
	WAL_OWNER_INGRESS=1
	WAL_OWNER_INGRESS_SOURCE=auto-representative-external-write
elif [ "$BACKEND" = wal-tcp ] && [ "$START_LOCAL_LEAF" != 1 ] && \
	[ "$REPRESENTATIVE" = 1 ]; then
	WAL_OWNER_INGRESS=0
	WAL_OWNER_INGRESS_SOURCE=auto-representative-external-read-mixed
else
	WAL_OWNER_INGRESS=0
	WAL_OWNER_INGRESS_SOURCE=default-off
fi
if [ -n "${URING_PLAY_ZCNBLK_SHM_SECTOR_ORDER_SLOTS+x}" ]; then
	SECTOR_ORDER_SLOTS="$URING_PLAY_ZCNBLK_SHM_SECTOR_ORDER_SLOTS"
elif [ "$WAL_OWNER_INGRESS" = 1 ]; then
	# Stable-owner scheduling can have two active ordering generations per
	# worker. Size the hash table to avoid false dependencies by default.
	active_order_target=$((LANES * REGION_BYTES_PER_WORKER / 4096 * 2))
	SECTOR_ORDER_SLOTS=1
	while [ "$SECTOR_ORDER_SLOTS" -lt "$active_order_target" ]; do
		SECTOR_ORDER_SLOTS=$((SECTOR_ORDER_SLOTS * 2))
	done
else
	SECTOR_ORDER_SLOTS=65536
fi
if [ -n "${URING_PLAY_ZCNBLK_SHM_OWNER_COUNT+x}" ]; then
	WAL_OWNER_COUNT="$URING_PLAY_ZCNBLK_SHM_OWNER_COUNT"
	WAL_OWNER_COUNT_SOURCE=explicit
elif [ "$SHM_OFI_RMA_WRITES" = 1 ] && \
	[ "$SHM_OFI_RMA_WRITE_OWNER_MODE" = single-domain-fan-in ]; then
	WAL_OWNER_COUNT=1
	WAL_OWNER_COUNT_SOURCE=single-domain-fan-in
else
	WAL_OWNER_COUNT="$LANES"
	WAL_OWNER_COUNT_SOURCE=placement-lanes-default
fi
WAL_OWNER_CPU_LIST="${URING_PLAY_ZCNBLK_SHM_OWNER_CPU_LIST:-}"
if [ "$WAL_OWNER_INGRESS" = 1 ]; then
	RMA_WRITE_ENDPOINT_COUNT="$WAL_OWNER_COUNT"
	REMOTE_STREAM_COUNT="$WAL_OWNER_COUNT"
else
	RMA_WRITE_ENDPOINT_COUNT="$LANES"
	REMOTE_STREAM_COUNT="$LANES"
fi
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
if [ -n "${URING_PLAY_ZCNBLK_SHM_OWNER_QUEUE_DEPTH+x}" ]; then
	WAL_OWNER_QUEUE_DEPTH="$URING_PLAY_ZCNBLK_SHM_OWNER_QUEUE_DEPTH"
	WAL_OWNER_QUEUE_DEPTH_SOURCE=explicit
elif [ "$WAL_OWNER_INGRESS" = 1 ] && [ $((LANES * IODEPTH)) -gt 128 ]; then
	# Every ingress lane can feed every stable owner. A 128-entry owner queue
	# caps a multi-lane fan-in even when each worker's QD is below 128, so cover
	# the benchmark's full aggregate outstanding window.
	WAL_OWNER_QUEUE_DEPTH=$((LANES * IODEPTH))
	WAL_OWNER_QUEUE_DEPTH_SOURCE=aggregate-outstanding-depth
else
	WAL_OWNER_QUEUE_DEPTH=128
	WAL_OWNER_QUEUE_DEPTH_SOURCE=default-128
fi
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
WAL_FOREGROUND_READ_IMMEDIATE="${URING_PLAY_ZCNBLK_SHM_WAL_FOREGROUND_READ_IMMEDIATE:-1}"
WAL_CQ_DELAY_SPINS="${URING_PLAY_ZCNBLK_SHM_WAL_CQ_DELAY_SPINS:-0}"
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
LEAF_ALLOW_VOLATILE_SYNC="${URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC:-1}"
PERF_STAT="${PERF_STAT:-1}"
BUILD="${BUILD:-0}"
SET_GOVERNOR="${SET_GOVERNOR:-}"
OUTDIR="${OUTDIR:-$ROOT/bench-results/local-zcnblk-shm-$(date -u +%Y%m%dT%H%M%SZ)}"
APP_ARENA_SOCKET="${URING_PLAY_ZCNBLK_SHM_APP_ARENA_SOCKET:-/tmp/zcnblk-app-arena-$$.sock}"
LIVE_MIGRATION_CONTROL_ADDR="${ZCNBLK_WAL_LIVE_MIGRATION_CONTROL_ADDR:-}"
LIVE_MIGRATION_START_BEFORE_REPEAT="${ZCNBLK_WAL_LIVE_MIGRATION_START_BEFORE_REPEAT:-2}"
LIVE_MIGRATION_CUTOVER_AFTER_REPEAT="${ZCNBLK_WAL_LIVE_MIGRATION_CUTOVER_AFTER_REPEAT:-2}"
LIVE_MIGRATION_READY_TIMEOUT_SECONDS="${ZCNBLK_WAL_LIVE_MIGRATION_READY_TIMEOUT_SECONDS:-120}"
DIRECT_MIGRATION_CONTROL_SOCKET="${URING_PLAY_ZCNBLK_SHM_MIGRATION_CONTROL_SOCKET:-}"
DIRECT_MIGRATION_SOURCE_ADDR="${URING_PLAY_ZCNBLK_SHM_MIGRATION_SOURCE_ADDR:-}"
DIRECT_MIGRATION_DEST_ADDR="${URING_PLAY_ZCNBLK_SHM_MIGRATION_DEST_ADDR:-}"
DIRECT_MIGRATION_COPY_METHOD="${URING_PLAY_ZCNBLK_SHM_MIGRATION_TCP_COPY_METHOD:-splice}"
DIRECT_MIGRATION_CATCHUP_PASSES="${URING_PLAY_ZCNBLK_SHM_MIGRATION_CATCHUP_PASSES:-2}"
DIRECT_MIGRATION_QUIESCE_TIMEOUT_MS="${URING_PLAY_ZCNBLK_SHM_MIGRATION_QUIESCE_TIMEOUT_MS:-5000}"
DIRECT_MIGRATION_COPY_CPU_LIST="${ZCNBLK_WAL_MIGRATION_COPY_CPU_LIST:-}"
DIRECT_MIGRATION_CONTROL_CPU="${ZCNBLK_WAL_MIGRATION_CONTROL_CPU:-}"
DIRECT_MIGRATION_AFTER_REPEAT="${ZCNBLK_WAL_DIRECT_MIGRATION_AFTER_REPEAT:-0}"
DIRECT_MIGRATION_EPOCH="${ZCNBLK_WAL_DIRECT_MIGRATION_EPOCH:-2}"
DIRECT_MIGRATION_VOLUME_BYTES="${ZCNBLK_WAL_DIRECT_MIGRATION_VOLUME_BYTES:-$((SIZE_MIB * 1024 * 1024))}"
DIRECT_MIGRATION_CHUNK_BYTES="${ZCNBLK_WAL_DIRECT_MIGRATION_CHUNK_BYTES:-1048576}"
DIRECT_MIGRATION_GRANULE_BYTES="${ZCNBLK_WAL_DIRECT_MIGRATION_GRANULE_BYTES:-4096}"
CONTINUITY_PROOF="${ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_PROOF:-0}"
CONTINUITY_PROOF_OFFSET="${ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_OFFSET:-0}"
CONTINUITY_PROOF_SLOTS="${ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_SLOTS:-64}"
CONTINUITY_PROOF_INTERVAL_US="${ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_INTERVAL_US:-500}"
CONTINUITY_PROOF_SYNC_EVERY="${ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_SYNC_EVERY:-4096}"
CONTINUITY_CPU="${ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_CPU:-}"

block_lease=""
perf_lease=""
target_pid=""
target_job_pid=""
continuity_pid=""
continuity_job_pid=""
leaf_pid=""
kernel_state_pid=""
kernel_log_start_line=0
declare -a tracked_pids=()
declare -a kthread_pids=()
pid_file="$OUTDIR/target.pid"
continuity_pid_file="$OUTDIR/continuity.pid"
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

leaf_listener_count() {
	local base="$LEAF_PORT"
	if [ "$LEAF_TRANSPORT" != tcp ]; then
		base=$((base + OFI_CONTROL_PORT_OFFSET))
	fi
	ss -H -ltn | awk -v base="$base" -v lanes="$LANES" '
		{
			port=$4
			sub(/^.*:/, "", port)
			if (port + 0 >= base && port + 0 < base + lanes) seen[port]=1
		}
		END { for (port in seen) count++; print count + 0 }
	'
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

cpu_llc_domain() {
	local cpu="$1" cache_path cache_level cache_type
	local best_level=-1 best_domain=unknown

	for cache_path in "/sys/devices/system/cpu/cpu$cpu"/cache/index*; do
		[ -d "$cache_path" ] || continue
		[ -r "$cache_path/level" ] && [ -r "$cache_path/type" ] && \
			[ -r "$cache_path/shared_cpu_list" ] || continue
		cache_level="$(cat "$cache_path/level")"
		cache_type="$(cat "$cache_path/type")"
		[[ "$cache_level" =~ ^[0-9]+$ ]] || continue
		case "$cache_type" in
		Unified|Data) ;;
		*) continue ;;
		esac
		if [ "$cache_level" -gt "$best_level" ]; then
			best_level="$cache_level"
			best_domain="$(cat "$cache_path/shared_cpu_list")"
		fi
	done
	printf '%s' "$best_domain"
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

check_kernel_timing_faults() {
	local current_line strict=0

	current_line="$(sudo -n dmesg | wc -l)"
	if [ "$current_line" -ge "$kernel_log_start_line" ]; then
		sudo -n dmesg | tail -n "+$((kernel_log_start_line + 1))" \
			>"$OUTDIR/kernel-timing.log"
	else
		# The ring wrapped; conservatively inspect the complete retained log.
		sudo -n dmesg >"$OUTDIR/kernel-timing.log"
	fi
	if grep -Eiq 'watchdog: BUG: soft lockup|rcu[^:]*: INFO:.*stall|blocked for more than|hung[_ -]task' \
		"$OUTDIR/kernel-timing.log"; then
		grep -Ei 'watchdog: BUG: soft lockup|rcu[^:]*: INFO:.*stall|blocked for more than|hung[_ -]task' \
			"$OUTDIR/kernel-timing.log" >&2 || true
		[ "$REPRESENTATIVE" = 1 ] && strict=1
		[ "${URING_PLAY_TOPOLOGY_STRICT:-0}" = 1 ] && strict=1
		[ "${URING_PLAY_TOPOLOGY_FATAL:-0}" = 1 ] && strict=1
		[ "$strict" != 1 ] || \
			die "kernel timing fault invalidates this representative run; no benchmark result is accepted"
		printf 'PERF WARNING: kernel timing fault observed; results are non-representative\n' >&2
	fi
}

safe_stop_target() {
	if [ -z "$target_pid" ] && [ -n "$target_job_pid" ] && [ -d "/proc/$target_job_pid" ]; then
		local child child_comm
		while read -r child; do
			[ -n "$child" ] || continue
			[ -r "/proc/$child/comm" ] || continue
			child_comm="$(cat "/proc/$child/comm")"
			if [ "$child_comm" = "zcnblk-shm-targ" ]; then
				target_pid="$child"
				break
			fi
		done < <(ps -o pid= --ppid "$target_job_pid")
	fi
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

safe_stop_continuity() {
	[ -n "$continuity_pid" ] || return 0
	[ -r "/proc/$continuity_pid/comm" ] || return 0
	local comm
	comm="$(cat "/proc/$continuity_pid/comm")"
	[ "$comm" = "zcnblk-edge-con" ] || {
		printf 'refusing to signal continuity pid=%s comm=%s\n' "$continuity_pid" "$comm" >&2
		return 1
	}
	sudo -n kill -TERM "$continuity_pid" 2>/dev/null || true
	for _ in $(seq 1 200); do
		[ ! -e "/proc/$continuity_pid" ] && return 0
		[[ "$(awk '{print $3}' "/proc/$continuity_pid/stat" 2>/dev/null || true)" == Z ]] && return 0
		sleep 0.01
	done
	printf 'continuity proof pid=%s did not stop after SIGTERM\n' "$continuity_pid" >&2
	return 1
}

safe_stop_leaf() {
	[ -n "$leaf_pid" ] || return 0
	[ -r "/proc/$leaf_pid/comm" ] || return 0
	local comm
	comm="$(cat "/proc/$leaf_pid/comm")"
	[ "$comm" = "zcnblk-wal-leaf" ] || {
		printf 'refusing to signal leaf pid=%s comm=%s\n' "$leaf_pid" "$comm" >&2
		return 1
	}
	kill -TERM "$leaf_pid" 2>/dev/null || true
	wait "$leaf_pid" 2>/dev/null || true
	leaf_pid=""
}

safe_unload_client_module() {
	local attempt

	for attempt in $(seq 1 100); do
		grep -q '^zcnblk_client_mod ' /proc/modules 2>/dev/null || return 0
		if sudo -n rmmod zcnblk_client_mod 2>/dev/null; then
			return 0
		fi
		sleep 0.05
	done
	printf 'zcnblk-shm-block-bench: ERROR: zcnblk_client_mod remained busy during cleanup\n' >&2
	return 1
}

restore_governors() {
	[ -s "$governors_file" ] || return 0
	while read -r path governor; do
		[ -w "$path" ] && printf '%s' "$governor" >"$path" || \
			sudo -n sh -c 'printf "%s" "$1" > "$2"' sh "$governor" "$path" || true
	done <"$governors_file"
}

live_migration_command() {
	local command="$1" emit="${2:-1}" response host port
	[ -n "$LIVE_MIGRATION_CONTROL_ADDR" ] || die "live migration control address is empty"
	host="${LIVE_MIGRATION_CONTROL_ADDR%:*}"
	port="${LIVE_MIGRATION_CONTROL_ADDR##*:}"
	exec 8<>"/dev/tcp/$host/$port"
	printf '%s\n' "$command" >&8
	IFS= read -r response <&8
	exec 8>&-
	if [ "$emit" = 1 ]; then
		printf 'command=%s response=%s\n' "$command" "$response" | tee -a "$OUTDIR/live-migration-control.log" >&2
	fi
	[[ "$response" == OK\ * ]] || die "live migration command $command failed: $response"
	printf '%s' "$response"
}

wait_for_live_migration_base() {
	local deadline status ready
	deadline=$((SECONDS + LIVE_MIGRATION_READY_TIMEOUT_SECONDS))
	while :; do
		status="$(live_migration_command status 0)"
		ready="$(awk '{ value=$0; count=0; while (sub(/base=true/, "", value)) count++; print count }' <<<"$status")"
		if [ "$ready" -eq "$LANES" ]; then
			printf 'command=status response=%s\n' "$status" | tee -a "$OUTDIR/live-migration-control.log" >&2
			return 0
		fi
		[ "$SECONDS" -lt "$deadline" ] || \
			die "live migration base copy did not become ready on all $LANES lanes: $status"
		sleep 0.01
	done
}

drive_live_migration_cutover() {
	local barrier_started_ns barrier_elapsed_ns cutover_started_ns cutover_elapsed_ns deadline status
	[ -x "$EDGE_SYNC_BIN" ] || die "live migration edge-sync binary is missing: $EDGE_SYNC_BIN"
	barrier_started_ns="$(date +%s%N)"
	# The userspace target interprets fsync(2) on the block edge as a global
	# admitted-lane-vector HWM drain. No placement decision occurs in the edge.
	sudo -n "$EDGE_SYNC_BIN" /dev/zcnblk0 | tee "$OUTDIR/live-migration-edge-sync.log"
	barrier_elapsed_ns=$(( $(date +%s%N) - barrier_started_ns ))
	printf 'edge_barrier=global-sync-hwm elapsed_ns=%s block_identity=unchanged\n' \
		"$barrier_elapsed_ns" | tee -a "$OUTDIR/live-migration-control.log"
	cutover_started_ns="$(date +%s%N)"
	live_migration_command cutover >/dev/null
	deadline=$((SECONDS + LIVE_MIGRATION_READY_TIMEOUT_SECONDS))
	while :; do
		status="$(live_migration_command status 0)"
		grep -q 'phase=active_secondary' <<<"$status" && break
		[ "$SECONDS" -lt "$deadline" ] || \
			die "idle-lane cutover wake did not publish the destination route: $status"
		sleep 0.001
	done
	printf 'command=status response=%s\n' "$status" | tee -a "$OUTDIR/live-migration-control.log" >&2
	cutover_elapsed_ns=$(( $(date +%s%N) - cutover_started_ns ))
	# Prove a global barrier traverses the already-published destination while
	# the target and block device remain the same sessions and identity.
	sudo -n "$EDGE_SYNC_BIN" /dev/zcnblk0 | tee "$OUTDIR/live-migration-post-cutover-sync.log"
	if [ "$REMOTE_TRANSPORT" = ofi ] || [ "$REMOTE_TRANSPORT" = rdm ] || \
		[ "$REMOTE_TRANSPORT" = efa ]; then
		wake_source=existing-ofi-cq-progress-loop-phase-check
	else
		wake_source=userspace-coordinator-targeted-signal
	fi
	printf 'cutover_probe=pass wake_source=%s lanes=%s client_target_session_reconnect=false control_observed_elapsed_ns=%s\n' \
		"$wake_source" "$LANES" "$cutover_elapsed_ns" | tee -a "$OUTDIR/live-migration-control.log"
}

cleanup() {
	local status=$? cleanup_failed=0
	set +e
	if [ -n "$kernel_state_pid" ] && kill -0 "$kernel_state_pid" 2>/dev/null; then
		kill "$kernel_state_pid" 2>/dev/null
		wait "$kernel_state_pid" 2>/dev/null
	fi
	safe_stop_continuity || cleanup_failed=1
	if [ -n "$continuity_job_pid" ]; then
		wait "$continuity_job_pid" 2>/dev/null
	fi
	safe_stop_target
	if [ -n "$target_job_pid" ]; then
		wait "$target_job_pid" 2>/dev/null
	fi
	safe_stop_leaf
	safe_unload_client_module || cleanup_failed=1
	restore_governors
	[ -n "$perf_lease" ] && "$COORD_BIN" release "$perf_lease" >>"$OUTDIR/coordination.log" 2>&1
	[ -n "$block_lease" ] && "$COORD_BIN" release "$block_lease" >>"$OUTDIR/coordination.log" 2>&1
	[ "$status" -ne 0 ] || [ "$cleanup_failed" -eq 0 ] || status=1
	exit "$status"
}

trap cleanup EXIT INT TERM

[ "$LANES" -gt 0 ] || die "LANES must be positive"
[ "$BLOCK_SIZE" = 512 ] || [ "$BLOCK_SIZE" = 1024 ] || \
	[ "$BLOCK_SIZE" = 2048 ] || [ "$BLOCK_SIZE" = 4096 ] || \
	die "BLOCK_SIZE must be 512, 1024, 2048, or 4096"
[ "$BLOCK_SIZE" = 4096 ] || [ "$MODE" = read ] || \
	die "sub-4K block edges are deliberately read-only until sub-page write ordering is implemented"
[ "$REPEATS" -gt 0 ] || die "REPEATS must be positive"
[ "$CONTINUITY_PROOF" = 0 ] || [ "$CONTINUITY_PROOF" = 1 ] || \
	die "ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_PROOF must be zero or one"
if [ "$CONTINUITY_PROOF" = 1 ]; then
	[ -n "$LIVE_MIGRATION_CONTROL_ADDR$DIRECT_MIGRATION_CONTROL_SOCKET" ] || \
		die "continuity proof requires a gateway control address or direct migration control socket"
	for value in "$CONTINUITY_PROOF_OFFSET" "$CONTINUITY_PROOF_SLOTS" \
		"$CONTINUITY_PROOF_INTERVAL_US" "$CONTINUITY_PROOF_SYNC_EVERY"; do
		[[ "$value" =~ ^[0-9]+$ ]] || die "continuity proof values must be unsigned integers"
	done
	[ "$CONTINUITY_PROOF_SLOTS" -gt 0 ] || die "continuity proof slots must be non-zero"
	[ $((CONTINUITY_PROOF_OFFSET % 4096)) -eq 0 ] || \
		die "continuity proof offset must be 4096-aligned"
	proof_end=$((CONTINUITY_PROOF_OFFSET + CONTINUITY_PROOF_SLOTS * 4096))
	benchmark_end=$((REGION_BYTES_PER_WORKER * LANES))
	[ "$CONTINUITY_PROOF_OFFSET" -ge "$benchmark_end" ] || \
		die "continuity proof range overlaps the random-I/O benchmark region"
	[ "$proof_end" -le "$((SIZE_MIB * 1024 * 1024))" ] || \
		die "continuity proof range exceeds the block device"
fi
if [ -n "$DIRECT_MIGRATION_CONTROL_SOCKET" ]; then
	[ "$BACKEND" = wal-tcp ] || die "direct migration currently requires BACKEND=wal-tcp"
	[ "$START_LOCAL_LEAF" = 0 ] || die "direct migration requires explicit external terminal leaves"
	[ "$WAL_OWNER_INGRESS" = 1 ] || die "direct migration requires stable userspace owner ingress"
	[ -n "$DIRECT_MIGRATION_SOURCE_ADDR" ] || die "direct migration source address is empty"
	[ -n "$DIRECT_MIGRATION_DEST_ADDR" ] || die "direct migration destination address is empty"
	[ -x "$DIRECT_MIGRATECTL_BIN" ] || die "direct migration control binary is missing: $DIRECT_MIGRATECTL_BIN"
	[[ "$DIRECT_MIGRATION_AFTER_REPEAT" =~ ^[1-9][0-9]*$ ]] && \
		[ "$DIRECT_MIGRATION_AFTER_REPEAT" -lt "$REPEATS" ] || \
		die "direct migration cutover repeat must be in 1..REPEATS-1"
	for value in "$DIRECT_MIGRATION_EPOCH" "$DIRECT_MIGRATION_VOLUME_BYTES" \
		"$DIRECT_MIGRATION_CHUNK_BYTES" "$DIRECT_MIGRATION_GRANULE_BYTES" \
		"$DIRECT_MIGRATION_CATCHUP_PASSES" "$DIRECT_MIGRATION_QUIESCE_TIMEOUT_MS"; do
		[[ "$value" =~ ^[0-9]+$ ]] || die "direct migration values must be unsigned integers"
	done
	[ $((DIRECT_MIGRATION_VOLUME_BYTES % 4096)) -eq 0 ] && \
		[ $((DIRECT_MIGRATION_CHUNK_BYTES % 4096)) -eq 0 ] && \
		[ $((DIRECT_MIGRATION_GRANULE_BYTES % 4096)) -eq 0 ] || \
		die "direct migration volume, chunk, and granule bytes must be 4K aligned"
	if [ "$REPRESENTATIVE" = 1 ] || [ "${URING_PLAY_TOPOLOGY_STRICT:-0}" = 1 ] || \
		[ "${URING_PLAY_TOPOLOGY_FATAL:-0}" = 1 ]; then
		[ -n "$DIRECT_MIGRATION_COPY_CPU_LIST" ] || \
			die "strict direct migration requires ZCNBLK_WAL_MIGRATION_COPY_CPU_LIST"
		[ -n "$DIRECT_MIGRATION_CONTROL_CPU" ] || \
			die "strict direct migration requires ZCNBLK_WAL_MIGRATION_CONTROL_CPU"
		[ -n "$CONTINUITY_CPU" ] || [ "$CONTINUITY_PROOF" != 1 ] || \
			die "strict continuity proof requires ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_CPU"
		mapfile -t direct_copy_cpus < <(expand_cpu_list "$DIRECT_MIGRATION_COPY_CPU_LIST")
		[ "${#direct_copy_cpus[@]}" -eq "$WAL_OWNER_COUNT" ] || \
			die "direct migration copy CPU list must provide one CPU per owner"
	fi
fi
if [ -n "$LIVE_MIGRATION_CONTROL_ADDR" ]; then
	[[ "$LIVE_MIGRATION_START_BEFORE_REPEAT" =~ ^[0-9]+$ ]] && \
		[ "$LIVE_MIGRATION_START_BEFORE_REPEAT" -ge 1 ] && \
		[ "$LIVE_MIGRATION_START_BEFORE_REPEAT" -le "$REPEATS" ] || \
		die "migration start repeat must be in 1..REPEATS"
	[[ "$LIVE_MIGRATION_CUTOVER_AFTER_REPEAT" =~ ^[0-9]+$ ]] && \
		[ "$LIVE_MIGRATION_CUTOVER_AFTER_REPEAT" -ge "$LIVE_MIGRATION_START_BEFORE_REPEAT" ] && \
		[ "$LIVE_MIGRATION_CUTOVER_AFTER_REPEAT" -lt "$REPEATS" ] || \
		die "migration cutover repeat must be >= start repeat and < REPEATS"
	[[ "$LIVE_MIGRATION_READY_TIMEOUT_SECONDS" =~ ^[0-9]+$ ]] && \
		[ "$LIVE_MIGRATION_READY_TIMEOUT_SECONDS" -gt 0 ] || \
		die "migration ready timeout must be positive"
fi
[[ "$MIN_IOPS_PER_REP" =~ ^[0-9]+$ ]] || die "MIN_IOPS_PER_REP must be a non-negative integer"
[[ "$MIN_MEAN_IOPS" =~ ^[0-9]+$ ]] || die "MIN_MEAN_IOPS must be a non-negative integer"
if [ "$REPRESENTATIVE" = 1 ] && [ "$REPEATS" -lt 3 ]; then
	die "representative block measurements require REPEATS>=3"
fi
[[ "$EXTERNAL_NIC_LOW_LATENCY_CONFIRMED" =~ ^[01]$ ]] || \
	die "URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED must be zero or one"
[[ "$BLOCK_COMPLETION_BATCH" =~ ^[0-9]+$ ]] && \
	[ "$BLOCK_COMPLETION_BATCH" -gt 0 ] && [ "$BLOCK_COMPLETION_BATCH" -le "$IODEPTH" ] || \
	die "URING_PLAY_BLOCKBENCH_COMPLETION_BATCH must be in 1..IODEPTH"
[[ "$BLOCK_WAIT_MIN_COMPLETIONS" =~ ^[0-9]+$ ]] && \
	[ "$BLOCK_WAIT_MIN_COMPLETIONS" -gt 0 ] && \
	[ "$BLOCK_WAIT_MIN_COMPLETIONS" -le "$BLOCK_COMPLETION_BATCH" ] || \
	die "URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS must be in 1..completion batch"
[[ "$BLOCK_FUA_WRITES" =~ ^[01]$ ]] || \
	die "URING_PLAY_BLOCKBENCH_FUA_WRITES must be zero or one"
[[ "$BLOCK_NOATIME" =~ ^[01]$ ]] || \
	die "URING_PLAY_BLOCKBENCH_NOATIME must be zero or one"
[[ "$BLOCK_REGISTERED_RING" =~ ^[01]$ ]] || \
	die "URING_PLAY_BLOCKBENCH_REGISTERED_RING must be zero or one"
case "$SHM_ARENA_BACKING" in
vmalloc|hugetlb|auto) ;;
*) die "URING_PLAY_ZCNBLK_SHM_ARENA_BACKING must be vmalloc, hugetlb, or auto" ;;
esac
if [ "$BLOCK_FUA_WRITES" = 1 ] && [ "$MODE" = read ]; then
	die "URING_PLAY_BLOCKBENCH_FUA_WRITES=1 requires a write or mixed workload"
fi
[[ "$SHM_RING_ENTRIES" =~ ^[0-9]+$ ]] && [ "$SHM_RING_ENTRIES" -gt 0 ] || \
	die "SHM_RING_ENTRIES must be a positive integer"
[[ "$SHM_PAYLOAD_ENTRIES" =~ ^[0-9]+$ ]] && [ "$SHM_PAYLOAD_ENTRIES" -gt 0 ] || \
	die "SHM_PAYLOAD_ENTRIES must be a positive integer"
for index_shape in "descriptor:$SHM_RING_ENTRIES" "payload:$SHM_PAYLOAD_ENTRIES"; do
	index_name="${index_shape%%:*}"
	index_entries="${index_shape#*:}"
	if (( (index_entries & (index_entries - 1)) != 0 )); then
		printf 'PERF WARNING: shm %s entries=%s is not a power of two; the kernel block edge must use integer division for every corresponding ring index\n' \
			"$index_name" "$index_entries" >&2
		if [ "$REPRESENTATIVE" = 1 ] || [ "${URING_PLAY_TOPOLOGY_STRICT:-0}" = 1 ] || \
			[ "${URING_PLAY_TOPOLOGY_FATAL:-0}" = 1 ]; then
			die "representative/strict block runs require power-of-two shm $index_name entries"
		fi
	fi
done
[[ "$KERNEL_QUEUE_DEPTH" =~ ^[0-9]+$ ]] && [ "$KERNEL_QUEUE_DEPTH" -gt 0 ] || \
	die "KERNEL_QUEUE_DEPTH must be a positive integer"
[[ "$KERNEL_PIPELINE_DEPTH" =~ ^[0-9]+$ ]] && [ "$KERNEL_PIPELINE_DEPTH" -gt 0 ] || \
	die "KERNEL_PIPELINE_DEPTH must be a positive integer"
[[ "$KERNEL_WORKER_BATCH_DEQUEUE" =~ ^[01]$ ]] || \
	die "KERNEL_WORKER_BATCH_DEQUEUE must be zero or one"
[[ "$KERNEL_SEQUENCE_TELEMETRY_INTERVAL" =~ ^[0-9]+$ ]] || \
	die "KERNEL_SEQUENCE_TELEMETRY_INTERVAL must be a non-negative integer"
[[ "$KERNEL_COMPLETION_BATCH" =~ ^[0-9]+$ ]] && [ "$KERNEL_COMPLETION_BATCH" -gt 0 ] || \
	die "KERNEL_COMPLETION_BATCH must be a positive integer"
if [ -n "$BLOCK_WBT_LAT_USEC" ]; then
	[[ "$BLOCK_WBT_LAT_USEC" =~ ^[0-9]+$ ]] || \
		die "URING_PLAY_ZCNBLK_WBT_LAT_USEC must be a non-negative integer"
fi
[[ "$LANE_LOCAL_SEQUENCES" =~ ^[01]$ ]] || \
	die "URING_PLAY_ZCNBLK_SHM_LANE_LOCAL_SEQUENCES must be zero or one"
[[ "$APP_ARENA_BUFFERS" =~ ^[01]$ ]] || \
	die "URING_PLAY_ZCNBLK_SHM_APP_ARENA_BUFFERS must be zero or one"
if [ "$BLOCK_SIZE" != 4096 ] && [ "$APP_ARENA_BUFFERS" = 1 ]; then
	die "sub-page block reads cannot use the 4K application arena: blk-mq may merge distinct application slots; set URING_PLAY_ZCNBLK_SHM_APP_ARENA_BUFFERS=0"
fi
if [ "$APP_ARENA_BUFFERS" = 1 ]; then
	[ "$SHM_ARENA_BACKING" = hugetlb ] || \
		die "application arena buffers require URING_PLAY_ZCNBLK_SHM_ARENA_BACKING=hugetlb"
	case "$BLOCK_ENGINE" in
	uring-plain|uring-fixed) ;;
	*) die "application arena buffers require uring-plain or uring-fixed" ;;
	esac
	[ "$IODEPTH" -le "$SHM_PAYLOAD_ENTRIES" ] || \
		die "application arena buffers require IODEPTH <= SHM_PAYLOAD_ENTRIES"
fi
(( KERNEL_SEQUENCE_TELEMETRY_INTERVAL == 0 ||
   (KERNEL_SEQUENCE_TELEMETRY_INTERVAL & (KERNEL_SEQUENCE_TELEMETRY_INTERVAL - 1)) == 0 )) || \
	die "KERNEL_SEQUENCE_TELEMETRY_INTERVAL must be zero or a power of two"
[[ "$HCTX_NUMA_NODE" =~ ^-?[0-9]+$ ]] || die "HCTX_NUMA_NODE must be an integer"
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
if [ "$BACKEND" = wal-tcp ] && [ "$START_LOCAL_LEAF" != 1 ] && \
	[ "$WAL_OWNER_INGRESS" != 1 ] && [ "$WAL_OWNER_DISPATCH" != 1 ]; then
	printf 'PERF NOTE: external WAL uses lane-inline transport (mode=%s); stable-owner is the measured high-IOPS write path\n' "$MODE" >&2
	if [ "$REPRESENTATIVE" = 1 ] && [ "$MODE" = write ]; then
		die "representative external WAL write runs require stable-owner ingress or owner dispatch"
	fi
fi
if [ "$BACKEND" = wal-tcp ] && [ "$START_LOCAL_LEAF" != 1 ] && \
	[ "$IODEPTH" -le 16 ] && [ "$EXTERNAL_NIC_LOW_LATENCY_CONFIRMED" != 1 ]; then
	printf 'PERF WARNING: external low-QD WAL run has no client+leaf NIC low-latency confirmation; ENA adaptive interrupt moderation produced flow-dependent QD1 latency and first-run spread. Configure and verify both endpoints (for ENA: adaptive-rx off, rx-usecs 0, tx-usecs 0), then set URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED=1\n' >&2
	if [ "$REPRESENTATIVE" = 1 ] || [ "${URING_PLAY_TOPOLOGY_STRICT:-0}" = 1 ] || \
		[ "${URING_PLAY_TOPOLOGY_FATAL:-0}" = 1 ]; then
		die "representative/strict external low-QD runs require explicit client+leaf NIC low-latency confirmation"
	fi
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
case "$SHM_OFI_RMA_WRITE_OWNER_MODE" in
placement|single-domain-fan-in) ;;
*) die "URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_OWNER_MODE must be placement or single-domain-fan-in" ;;
esac
[ "$SHM_OFI_RMA_WRITE_OWNER_MODE" != single-domain-fan-in ] || \
	[ "$SHM_OFI_RMA_WRITES" = 1 ] || \
	die "single-domain-fan-in owner mode requires OFI RMA writes"
[[ "$SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED" =~ ^[01]$ ]] || \
	die "URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED must be zero or one"
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
[[ "$WAL_FOREGROUND_READ_IMMEDIATE" =~ ^[01]$ ]] || \
	die "WAL foreground read immediate must be zero or one"
[[ "$SHM_OFI_RMA_READ_QD" =~ ^[0-9]+$ ]] && [ "$SHM_OFI_RMA_READ_QD" -gt 0 ] && \
	[ "$SHM_OFI_RMA_READ_QD" -le 1024 ] || \
	die "URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD must be in 1..=1024"
[[ "$OFI_SELECTIVE_COMPLETION" =~ ^[01]$ ]] || \
	die "URING_PLAY_OFI_SELECTIVE_COMPLETION must be zero or one"
[[ "$OFI_RMA_DEFER_TAIL_COMPLETION" =~ ^[01]$ ]] || \
	die "URING_PLAY_OFI_RMA_DEFER_TAIL_COMPLETION must be zero or one"
[[ "$OFI_RMA_READ_MORE" =~ ^[01]$ ]] || \
	die "URING_PLAY_OFI_RMA_READ_MORE must be zero or one"
[[ "$OFI_RMA_READ_COMPLETION_STRIDE" =~ ^[0-9]+$ ]] && \
	[ "$OFI_RMA_READ_COMPLETION_STRIDE" -ge 1 ] && \
	[ "$OFI_RMA_READ_COMPLETION_STRIDE" -le 65536 ] || \
	die "URING_PLAY_OFI_RMA_READ_COMPLETION_STRIDE must be in 1..=65536"
if [ "$OFI_RMA_READ_COMPLETION_STRIDE" -gt 1 ] && [ "$OFI_SELECTIVE_COMPLETION" != 1 ]; then
	die "RMA read completion stride above one requires URING_PLAY_OFI_SELECTIVE_COMPLETION=1"
fi
if [ "$SHM_OFI_RMA_READS" = 1 ]; then
	[ "$REMOTE_TRANSPORT" = ofi ] || [ "$REMOTE_TRANSPORT" = rdm ] || [ "$REMOTE_TRANSPORT" = efa ] || \
		die "OFI RMA reads require URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=ofi"
	if [ "$WAL_OWNER_INGRESS" = 1 ]; then
		printf 'PERF NOTE: stable-owner OFI reads use the owner endpoint RMA queue and place completions directly in request-owned shared slots\n' >&2
	fi
	if [ "$WAL_LANE_WINDOW" -lt "$SHM_OFI_RMA_READ_QD" ]; then
		printf 'PERF WARNING: wal_lane_window=%s is below rma_read_qd=%s and caps lane overlap\n' \
			"$WAL_LANE_WINDOW" "$SHM_OFI_RMA_READ_QD" >&2
		[ "$REPRESENTATIVE" != 1 ] || die "representative OFI RMA runs require wal_lane_window >= rma_read_qd"
	fi
	if [ "$MIN_MEAN_IOPS" -ge 12000000 ]; then
		[ "$REPRESENTATIVE" = 1 ] || \
			die "12M OFI RMA read record gate requires REPRESENTATIVE=1"
		[ "$MIN_IOPS_PER_REP" -ge 12000000 ] || \
			die "12M OFI RMA read record gate requires MIN_IOPS_PER_REP>=12000000"
		[ "$REMOTE_OFI_PROVIDER" = efa ] || \
			die "12M OFI RMA read record gate requires the EFA provider"
		[ "$OFI_EFA_FABRIC" = efa-direct ] || \
			die "12M OFI RMA read record gate requires URING_PLAY_OFI_EFA_FABRIC=efa-direct"
		[ "$EFA_USE_DEVICE_RDMA" = 1 ] || \
			die "12M OFI RMA read record gate requires FI_EFA_USE_DEVICE_RDMA=1"
		[ "$OFI_SELECTIVE_COMPLETION" = 1 ] || \
			die "12M OFI RMA read record gate requires selective completion"
		[ "$OFI_RMA_READ_COMPLETION_STRIDE" -ge "$SHM_OFI_RMA_READ_QD" ] || \
			die "12M OFI RMA read record gate requires completion stride >= per-lane RMA read QD"
		[ "$OFI_RMA_DEFER_TAIL_COMPLETION" = 1 ] || \
			die "12M OFI RMA read record gate requires deferred real tail markers"
		[ "$OFI_RMA_READ_MORE" = 1 ] || \
			die "12M OFI RMA read record gate requires FI_MORE read doorbell batching"
	fi
fi
if [ "$REMOTE_TRANSPORT" = ofi ] || [ "$REMOTE_TRANSPORT" = rdm ] || [ "$REMOTE_TRANSPORT" = efa ]; then
	[ -z "$LEAF_SOURCE_ADDR" ] && [ -z "$LEAF_SOURCE_ADDRS" ] || \
		die "OFI WAL transport uses URING_PLAY_OFI_DOMAIN; leaf source-address binding is TCP-only"
fi
[[ "$SHM_OFI_RMA_WRITE_QD" =~ ^[0-9]+$ ]] && [ "$SHM_OFI_RMA_WRITE_QD" -gt 0 ] && \
	[ "$SHM_OFI_RMA_WRITE_QD" -le 1024 ] || \
	die "URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_QD must be in 1..=1024"
[[ "$SHM_OFI_RMA_WRITE_MIN_QD" =~ ^[0-9]+$ ]] && [ "$SHM_OFI_RMA_WRITE_MIN_QD" -gt 0 ] && \
	[ "$SHM_OFI_RMA_WRITE_MIN_QD" -le 1024 ] || \
	die "URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_MIN_QD must be in 1..=1024"
[[ "$OFI_RMA_WRITE_MORE" =~ ^[01]$ ]] || \
	die "URING_PLAY_OFI_RMA_WRITE_MORE must be zero or one"
[[ "$OFI_RMA_WRITE_MORE_BURST" =~ ^[0-9]+$ ]] && \
	[ "$OFI_RMA_WRITE_MORE_BURST" -ge 1 ] && [ "$OFI_RMA_WRITE_MORE_BURST" -le 65536 ] || \
	die "URING_PLAY_OFI_RMA_WRITE_MORE_BURST must be in 1..=65536"
if [ "$SHM_OFI_RMA_WRITES" = 1 ]; then
	[ "$REMOTE_TRANSPORT" = ofi ] || [ "$REMOTE_TRANSPORT" = rdm ] || [ "$REMOTE_TRANSPORT" = efa ] || \
		die "OFI RMA writes require URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=ofi"
	[ "$WAL_OWNER_INGRESS" = 1 ] || die "OFI RMA writes require stable WAL owner ingress"
	[ "$WAL_OWNER_PIPELINE_BATCHES" -eq 1 ] || \
		die "OFI RMA writes require URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_BATCHES=1"
	[ "$OFI_RMA_WRITE_DELIVERY_COMPLETE" = 1 ] || \
		die "OFI RMA writes require URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1"
	if [ "$SHM_OFI_RMA_WRITE_OWNER_MODE" = single-domain-fan-in ]; then
		[ "$WAL_OWNER_COUNT" -eq 1 ] || \
			die "single-domain-fan-in requires exactly one stable WAL owner"
	else
		if [ "$REMOTE_OFI_PROVIDER" = efa ] && [ "$WAL_OWNER_COUNT" -gt 1 ] && \
			[ "$SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED" != 1 ]; then
			printf 'PERF WARNING: EFA RMA writes use %s stable-owner endpoints on one configured OFI domain; measured same-domain endpoint contention can erase scaling. Use URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_OWNER_MODE=single-domain-fan-in for a single-leaf/single-rail capability run, or explicitly confirm this multi-endpoint placement topology.\n' \
				"$WAL_OWNER_COUNT" >&2
			if [ "$REPRESENTATIVE" = 1 ] || [ "${URING_PLAY_TOPOLOGY_STRICT:-0}" = 1 ] || \
				[ "${URING_PLAY_TOPOLOGY_FATAL:-0}" = 1 ]; then
				die "representative/strict same-domain EFA RMA writes require fan-in or explicit multi-endpoint confirmation"
			fi
		fi
	fi
	if [ "$SHM_OFI_RMA_WRITE_QD" -lt "$SHM_OFI_RMA_WRITE_MIN_QD" ]; then
		printf 'PERF WARNING: rma_write_qd=%s is below the delivery-complete payload-operation floor=%s; random records in one userspace batch will require serial RMA completion waves even when block per-worker QD is one\n' \
			"$SHM_OFI_RMA_WRITE_QD" "$SHM_OFI_RMA_WRITE_MIN_QD" >&2
		if [ "$REPRESENTATIVE" = 1 ] || [ "${URING_PLAY_TOPOLOGY_STRICT:-0}" = 1 ] || \
			[ "${URING_PLAY_TOPOLOGY_FATAL:-0}" = 1 ]; then
			die "representative/strict RMA write runs require rma_write_qd >= $SHM_OFI_RMA_WRITE_MIN_QD"
		fi
	fi
	if [ "$SHM_ARENA_BACKING" != hugetlb ]; then
		printf 'PERF WARNING: requested shared-arena backing is %s, not explicit external HugeTLB; this run cannot be classified as a HugeTLB-backed RMA source path\n' "$SHM_ARENA_BACKING" >&2
		if [ "$REPRESENTATIVE" = 1 ] || [ "${URING_PLAY_TOPOLOGY_STRICT:-0}" = 1 ] || \
			[ "${URING_PLAY_TOPOLOGY_FATAL:-0}" = 1 ]; then
			die "representative/strict RMA write runs require URING_PLAY_ZCNBLK_SHM_ARENA_BACKING=hugetlb"
		fi
	fi
	if [ "$REPRESENTATIVE" = 1 ]; then
		[ "$SHM_OFI_RMA_WRITES_REQUIRED" = 1 ] || \
			die "representative RMA write runs must forbid message-payload fallback"
		[ "$MODE" = write ] || die "representative RMA write attribution currently requires MODE=write"
	fi
fi
if [ "$START_LOCAL_LEAF" = 1 ] && [ "$BACKEND" = wal-tcp ] && \
	[ "$LEAF_TRANSPORT" != "$REMOTE_TRANSPORT" ]; then
	die "local leaf transport $LEAF_TRANSPORT does not match remote transport $REMOTE_TRANSPORT"
fi
if [ "$BACKEND" = wal-tcp ] && [ "$START_LOCAL_LEAF" != 1 ] && \
	{ [ "$REPRESENTATIVE" = 1 ] || [ "${URING_PLAY_TOPOLOGY_STRICT:-0}" = 1 ] || \
		[ "${URING_PLAY_TOPOLOGY_FATAL:-0}" = 1 ]; }; then
	[ -n "$EXTERNAL_LEAF_TOPOLOGY_ARTIFACT" ] && [ -r "$EXTERNAL_LEAF_TOPOLOGY_ARTIFACT" ] || \
		die "representative/strict external WAL runs require EXTERNAL_LEAF_TOPOLOGY_ARTIFACT"
	grep -q '^lane_to_worker_cpu=' "$EXTERNAL_LEAF_TOPOLOGY_ARTIFACT" || \
		die "external leaf topology artifact lacks lane_to_worker_cpu mapping"
	if [ "$REMOTE_TRANSPORT" = ofi ] || [ "$REMOTE_TRANSPORT" = rdm ] || [ "$REMOTE_TRANSPORT" = efa ]; then
		grep -q '^lane_to_nic=' "$EXTERNAL_LEAF_TOPOLOGY_ARTIFACT" || \
			die "external EFA leaf topology artifact lacks lane_to_nic mapping"
	fi
fi
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
external_leaf_cpu_map=none
external_leaf_nic_map=none
if [ -n "$EXTERNAL_LEAF_TOPOLOGY_ARTIFACT" ] && [ -r "$EXTERNAL_LEAF_TOPOLOGY_ARTIFACT" ]; then
	cp "$EXTERNAL_LEAF_TOPOLOGY_ARTIFACT" "$OUTDIR/external-leaf-topology.log"
	external_leaf_cpu_map="$(sed -n 's/^lane_to_worker_cpu=//p' "$OUTDIR/external-leaf-topology.log" | head -n 1)"
	external_leaf_nic_map="$(sed -n 's/^lane_to_nic=//p' "$OUTDIR/external-leaf-topology.log" | head -n 1)"
fi

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
		if grep -q '^cloud_provider=' "$BOOTSTRAP_MANIFEST"; then
			grep -Eq '^cloud_provider=(ec2|gce)$' "$BOOTSTRAP_MANIFEST" || \
				die "bootstrap manifest does not identify a supported cloud provider"
			grep -Eq '^instance_id=(i-[0-9a-f]+|[0-9]+)$' "$BOOTSTRAP_MANIFEST" || \
				die "bootstrap manifest does not identify an EC2 or GCE instance"
		else
			grep -Eq '^instance_id=i-[0-9a-f]+$' "$BOOTSTRAP_MANIFEST" || \
				die "legacy bootstrap manifest does not identify an EC2 instance"
		fi
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
		--bin zcnblk-order-smoke --bin zcnblk-wal-leaf --bin zcnblk-edge-sync \
		--bin zcnblk-edge-continuity)
	make -C "$ROOT/kmods" all
	sign_file="/usr/src/linux-headers-$(uname -r)/scripts/sign-file"
	[ -x "$sign_file" ] || die "module signing helper is missing: $sign_file"
	sudo -n "$sign_file" sha256 /root/mok/MOK.priv /root/mok/MOK.pem "$MODULE"
fi

[ -x "$TARGET_BIN" ] || die "target binary is missing: $TARGET_BIN (set BUILD=1)"
[ -x "$BENCH_BIN" ] || die "benchmark binary is missing: $BENCH_BIN (set BUILD=1)"
[ "$CONTINUITY_PROOF" != 1 ] || [ -x "$EDGE_CONTINUITY_BIN" ] || \
	die "continuity proof binary is missing: $EDGE_CONTINUITY_BIN (set BUILD=1)"
[ "$ORDER_SMOKE_PAIRS" = 0 ] || [ -x "$ORDER_BIN" ] || \
	die "order smoke binary is missing: $ORDER_BIN (set BUILD=1)"
[ "$START_LOCAL_LEAF" != 1 ] || [ -x "$LEAF_BIN" ] || die "leaf binary is missing: $LEAF_BIN (set BUILD=1)"
[ -r "$MODULE" ] || die "kernel module is missing: $MODULE (set BUILD=1)"

# Capture before module insertion so representative numbers cannot be printed
# after a soft-lockup, RCU stall, or hung-task event caused during setup or I/O.
kernel_log_start_line="$(sudo -n dmesg | wc -l)"

log "loading placement-free shared-memory client edge"
sudo -n insmod "$MODULE" transport=shm lanes="$LANES" connections_per_lane=1 \
	size_mib="$SIZE_MIB" queues="$KERNEL_QUEUES" queue_depth="$KERNEL_QUEUE_DEPTH" \
	logical_block_size="$BLOCK_SIZE" read_only="$([ "$BLOCK_SIZE" = 4096 ] && printf 0 || printf 1)" \
	worker_batch_dequeue="$KERNEL_WORKER_BATCH_DEQUEUE" \
	disable_merges="$KERNEL_DISABLE_MERGES" \
	shm_sequence_telemetry_interval="$KERNEL_SEQUENCE_TELEMETRY_INTERVAL" \
	shm_completion_batch="$KERNEL_COMPLETION_BATCH" \
	shm_bio_arena_zero_copy="$APP_ARENA_BUFFERS" \
	shm_bio_arena_zero_copy_required="$APP_ARENA_BUFFERS" \
	shm_sector_order_slots="$SECTOR_ORDER_SLOTS" \
	max_frame_bytes="$MAX_FRAME_BYTES" \
	pipeline_depth="$KERNEL_PIPELINE_DEPTH" shm_ring_entries="$SHM_RING_ENTRIES" \
	shm_payload_entries="$SHM_PAYLOAD_ENTRIES" shm_poll_us="$KERNEL_POLL_US" \
	shm_poll_clock_check_spins="$POLL_CLOCK_CHECK_SPINS" \
	hctx_numa_node="$HCTX_NUMA_NODE" pin_threads=0
for _ in $(seq 1 100); do
	[ -e /dev/zcnblk0 ] && [ -e /dev/zcnblk-shmctl ] && break
	sleep 0.05
done
[ -e /dev/zcnblk0 ] && [ -e /dev/zcnblk-shmctl ] || die "shared block edge did not appear"
expected_block_ro="$([ "$BLOCK_SIZE" = 4096 ] && printf 0 || printf 1)"
actual_block_ro="$(cat /sys/block/zcnblk0/ro)"
[ "$actual_block_ro" = "$expected_block_ro" ] || \
	die "zcnblk0 read-only contract mismatch: block_size=$BLOCK_SIZE expected_ro=$expected_block_ro actual_ro=$actual_block_ro"
log "verified block edge read-only contract: block_size=$BLOCK_SIZE ro=$actual_block_ro"
expected_nomerges="$([ "$KERNEL_DISABLE_MERGES" = 1 ] && printf 2 || printf 0)"
actual_nomerges="$(cat /sys/block/zcnblk0/queue/nomerges)"
[ "$actual_nomerges" = "$expected_nomerges" ] || \
	die "zcnblk0 merge contract mismatch: requested=$KERNEL_DISABLE_MERGES expected_nomerges=$expected_nomerges actual_nomerges=$actual_nomerges"
printf 'block_nomerges=%s source=verified-sysfs\n' "$actual_nomerges" | \
	tee -a "$OUTDIR/preflight.log"
wbt_path=/sys/block/zcnblk0/queue/wbt_lat_usec
if [ -r "$wbt_path" ]; then
	block_wbt_before="$(cat "$wbt_path")"
	if [ -n "$BLOCK_WBT_LAT_USEC" ]; then
		printf '%s\n' "$BLOCK_WBT_LAT_USEC" | sudo -n tee "$wbt_path" >/dev/null
	fi
	block_wbt_actual="$(cat "$wbt_path")"
	[ -z "$BLOCK_WBT_LAT_USEC" ] || [ "$block_wbt_actual" = "$BLOCK_WBT_LAT_USEC" ] || \
		die "zcnblk0 WBT latency mismatch: requested=$BLOCK_WBT_LAT_USEC actual=$block_wbt_actual"
	printf 'block_wbt_lat_usec_before=%s block_wbt_lat_usec_actual=%s source=%s\n' \
		"$block_wbt_before" "$block_wbt_actual" \
		"$([ -n "$BLOCK_WBT_LAT_USEC" ] && printf explicit || printf kernel-default)" | \
		tee -a "$OUTDIR/preflight.log"
else
	printf 'block_wbt_lat_usec_before=unavailable block_wbt_lat_usec_actual=unavailable source=kernel-interface-absent\n' | \
		tee -a "$OUTDIR/preflight.log"
fi

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
if [ -z "$CLIENT_CPU_LIST$TARGET_CPU_LIST$KERNEL_CPU_LIST$LEAF_CPU_LIST" ]; then
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
fi
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
		cpu="${client_cpus[$lane]}"
		cpu_allowed=false
		while IFS= read -r allowed_cpu; do
			if [ "$allowed_cpu" = "$cpu" ]; then
				cpu_allowed=true
				break
			fi
		done < <(expand_cpu_list "$hctx")
		[ "$cpu_allowed" = true ] || \
			die "explicit lane $lane client CPU $cpu is outside hctx map $hctx"

	done
fi
declare -a client_numa_nodes=() target_numa_nodes=() kernel_numa_nodes=()
lane_numa_local=true
for ((lane = 0; lane < LANES; lane++)); do
	client_node="$(cpu_numa_node "${client_cpus[$lane]}")"
	target_node="$(cpu_numa_node "${target_cpus[$lane]}")"
	kernel_node="$(cpu_numa_node "${kernel_cpus[$lane]}")"
	client_numa_nodes+=("$client_node")
	target_numa_nodes+=("$target_node")
	kernel_numa_nodes+=("$kernel_node")
	if [ "$client_node" = unknown ] || [ "$target_node" = unknown ] || \
		[ "$kernel_node" = unknown ] || [ "$client_node" != "$target_node" ] || \
		[ "$client_node" != "$kernel_node" ]; then
		lane_numa_local=false
		printf 'PERF WARNING: lane=%s is not NUMA-local: client_cpu=%s node=%s target_cpu=%s node=%s kernel_cpu=%s node=%s\n' \
			"$lane" "${client_cpus[$lane]}" "$client_node" \
			"${target_cpus[$lane]}" "$target_node" \
			"${kernel_cpus[$lane]}" "$kernel_node" >&2
	fi
done
if [ "$lane_numa_local" != true ] && { [ "$REPRESENTATIVE" = 1 ] || \
	[ "${URING_PLAY_TOPOLOGY_STRICT:-0}" = 1 ] || \
	[ "${URING_PLAY_TOPOLOGY_FATAL:-0}" = 1 ]; }; then
	die "strict topology requires every lane's client, target, and completion CPUs to share one NUMA node"
fi
declare -a client_llc_domains=() target_llc_domains=() kernel_llc_domains=()
lane_llc_local=true
for ((lane = 0; lane < LANES; lane++)); do
	client_llc="$(cpu_llc_domain "${client_cpus[$lane]}")"
	target_llc="$(cpu_llc_domain "${target_cpus[$lane]}")"
	kernel_llc="$(cpu_llc_domain "${kernel_cpus[$lane]}")"
	client_llc_domains+=("$client_llc")
	target_llc_domains+=("$target_llc")
	kernel_llc_domains+=("$kernel_llc")
	if [ "$client_llc" = unknown ] || [ "$target_llc" = unknown ] || \
		[ "$kernel_llc" = unknown ]; then
		lane_llc_local=false
		printf 'PERF WARNING: lane=%s cannot prove client/target/kernel last-level-cache locality: client_cpu=%s llc=%s target_cpu=%s llc=%s kernel_cpu=%s llc=%s\n' \
			"$lane" "${client_cpus[$lane]}" "$client_llc" \
			"${target_cpus[$lane]}" "$target_llc" \
			"${kernel_cpus[$lane]}" "$kernel_llc" >&2
	elif [ "$client_llc" != "$target_llc" ] || [ "$client_llc" != "$kernel_llc" ]; then
		lane_llc_local=false
		printf 'PERF WARNING: lane=%s crosses last-level-cache domains: client_cpu=%s llc=%s target_cpu=%s llc=%s kernel_cpu=%s llc=%s; shared-ring and completion cachelines will cross the interconnect\n' \
			"$lane" "${client_cpus[$lane]}" "$client_llc" \
			"${target_cpus[$lane]}" "$target_llc" \
			"${kernel_cpus[$lane]}" "$kernel_llc" >&2
	fi
done
if [ "$lane_llc_local" != true ] && \
	{ [ "$REPRESENTATIVE" = 1 ] || [ "${URING_PLAY_TOPOLOGY_STRICT:-0}" = 1 ] || \
		[ "${URING_PLAY_TOPOLOGY_FATAL:-0}" = 1 ]; }; then
	die "representative/strict block runs require client, target, and completion worker locality within each lane's last-level-cache domain"
fi
role_cpu_sharing=none
for ((lane = 0; lane < LANES; lane++)); do
	if [ "${client_cpus[$lane]}" = "${kernel_cpus[$lane]}" ]; then
		role_cpu_sharing=client+kernel
	elif [ "${client_cpus[$lane]}" = "${target_cpus[$lane]}" ]; then
		role_cpu_sharing=client+target
	elif [ "${target_cpus[$lane]}" = "${kernel_cpus[$lane]}" ]; then
		role_cpu_sharing=target+kernel
	fi
done
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
case "$SHM_ARENA_BACKING" in
hugetlb|auto)
	if [ -z "$SHM_ARENA_CPU_LIST" ]; then
		# The shared request/completion/payload arena is touched before lane
		# workers start. Leaving that first touch on target_cpus[0] silently
		# makes every lane on another NUMA node perform remote-memory ring
		# traffic. The lane target CPUs are the authoritative locality map.
		SHM_ARENA_CPU_LIST="$target_cpu_list"
		SHM_ARENA_CPU_LIST_SOURCE=auto-target-lanes
	fi
	SHM_ARENA_LOCALITY=lane-target-numa
	mapfile -t arena_cpus < <(expand_cpu_list "$SHM_ARENA_CPU_LIST")
	[ "${#arena_cpus[@]}" -eq "$LANES" ] || \
		die "URING_PLAY_ZCNBLK_SHM_ARENA_CPU_LIST must provide exactly one CPU per lane"
	for ((lane = 0; lane < LANES; lane++)); do
		arena_node="$(cpu_numa_node "${arena_cpus[$lane]}")"
		target_node="$(cpu_numa_node "${target_cpus[$lane]}")"
		if [ "$arena_node" != unknown ] && [ "$target_node" != unknown ] && \
			[ "$arena_node" != "$target_node" ]; then
			printf 'PERF WARNING: lane=%s arena_cpu=%s arena_numa=%s target_cpu=%s target_numa=%s; shared-arena memory is not target-lane-local\n' \
				"$lane" "${arena_cpus[$lane]}" "$arena_node" "${target_cpus[$lane]}" "$target_node" >&2
			if [ "$REPRESENTATIVE" = 1 ] || [ "${URING_PLAY_TOPOLOGY_STRICT:-0}" = 1 ] || \
				[ "${URING_PLAY_TOPOLOGY_FATAL:-0}" = 1 ]; then
				die "representative shared-arena topology requires every lane to be NUMA-local to its target worker"
			fi
		fi
	done
	;;
*)
	[ "$SHM_ARENA_CPU_LIST_SOURCE" != unassigned ] || SHM_ARENA_CPU_LIST_SOURCE=inactive
	;;
esac
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
if [ -n "$DIRECT_MIGRATION_CONTROL_SOCKET" ] && \
	{ [ "$REPRESENTATIVE" = 1 ] || [ "${URING_PLAY_TOPOLOGY_STRICT:-0}" = 1 ] || \
		[ "${URING_PLAY_TOPOLOGY_FATAL:-0}" = 1 ]; }; then
	declare -A direct_system_cpu_seen=()
	direct_system_cpus=("${direct_copy_cpus[@]}" "$DIRECT_MIGRATION_CONTROL_CPU")
	[ "$CONTINUITY_PROOF" != 1 ] || direct_system_cpus+=("$CONTINUITY_CPU")
	for cpu in "${direct_system_cpus[@]}"; do
		[ -d "/sys/devices/system/cpu/cpu$cpu" ] || \
			die "direct migration system CPU $cpu is not online"
		[[ ",$all_cpu_list," != *",$cpu,"* ]] || \
			die "direct migration system CPU $cpu overlaps a foreground block/owner role"
		[ -z "${direct_system_cpu_seen[$cpu]:-}" ] || \
			die "direct migration system CPU $cpu is assigned to more than one role"
		direct_system_cpu_seen[$cpu]=1
	done
fi

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

block_write_completion=ordinary-device-ack
if [ "$BLOCK_FUA_WRITES" = 1 ]; then
	block_write_completion=remote-fua-drain
elif [ "$BACKEND" = wal-tcp ] && [ "$WAL_OWNER_INGRESS" = 1 ]; then
	block_write_completion=early-local-retained-wal-admission
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
	printf 'role_cpu_sharing=%s\n' "$role_cpu_sharing"
	printf 'block_per_worker_qd=%s block_workers=%s block_aggregate_outstanding_depth=%s\n' \
		"$IODEPTH" "$LANES" "$((LANES * IODEPTH))"
	printf 'minimum_iops_per_repeat=%s minimum_mean_iops=%s\n' \
		"$MIN_IOPS_PER_REP" "$MIN_MEAN_IOPS"
	printf 'topology_cpu_list=%s\n' "${TOPOLOGY_CPU_LIST:-unrestricted}"
	printf 'coordinator_cpu=%s\n' "$coordinator_cpu"
	for ((lane = 0; lane < LANES; lane++)); do
		lane_leaf_cpu=none
		[ "$START_LOCAL_LEAF" != 1 ] || lane_leaf_cpu="${leaf_cpus[$lane]}"
		printf 'lane=%s client_cpu=%s target_cpu=%s transport_cpu=%s kernel_cpu=%s leaf_cpu=%s hctx_cpus=%s client_numa=%s target_numa=%s kernel_numa=%s numa_local=%s client_llc=%s target_llc=%s kernel_llc=%s llc_local=%s\n' \
			"$lane" "${client_cpus[$lane]}" "${target_cpus[$lane]}" \
			"$([ "$WAL_SPLIT_TRANSPORT" = 1 ] && printf '%s' "${transport_cpus[$lane]}" || printf inline)" \
			"${kernel_cpus[$lane]}" "$lane_leaf_cpu" "$(cat "/sys/block/zcnblk0/mq/$lane/cpu_list")" \
			"${client_numa_nodes[$lane]}" "${target_numa_nodes[$lane]}" \
			"${kernel_numa_nodes[$lane]}" \
			"$([ "${client_numa_nodes[$lane]}" = "${target_numa_nodes[$lane]}" ] && \
				[ "${client_numa_nodes[$lane]}" = "${kernel_numa_nodes[$lane]}" ] && printf true || printf false)" \
			"${client_llc_domains[$lane]}" "${target_llc_domains[$lane]}" \
			"${kernel_llc_domains[$lane]}" \
			"$([ "${client_llc_domains[$lane]}" = "${target_llc_domains[$lane]}" ] && \
				[ "${client_llc_domains[$lane]}" = "${kernel_llc_domains[$lane]}" ] && printf true || printf false)"
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
	printf 'external_nic_low_latency_confirmed=%s scope=client-and-leaf\n' \
		"$EXTERNAL_NIC_LOW_LATENCY_CONFIRMED"
	printf 'target_poll_us=%s\n' "$POLL_US"
	printf 'target_busy_poll_us=%s\n' "$BUSY_POLL_US"
	printf 'target_busy_hysteresis_us=%s\n' "$BUSY_HYSTERESIS_US"
	printf 'target_poll_clock_check_spins=%s\n' "$POLL_CLOCK_CHECK_SPINS"
	printf 'kernel_completion_poll_us=%s kernel_idle_recheck_us=%s\n' \
		"$KERNEL_POLL_US" "$KERNEL_POLL_US"
	printf 'kernel_state_interval_ms=%s\n' "$KERNEL_STATE_INTERVAL_MS"
	printf 'block_ring_mode=%s registered_ring=%s sqpoll_cpu_list=%s sqpoll_idle_ms=%s\n' \
		"$BLOCK_RING_MODE" "$BLOCK_REGISTERED_RING" "$sqpoll_cpu_list" "$SQPOLL_IDLE_MS"
	printf 'block_engine=%s fua_writes=%s noatime=%s write_completion=%s\n' \
		"$BLOCK_ENGINE" "$BLOCK_FUA_WRITES" "$BLOCK_NOATIME" "$block_write_completion"
	printf 'block_size=%s block_edge_read_only=%s\n' "$BLOCK_SIZE" \
		"$([ "$BLOCK_SIZE" = 4096 ] && printf false || printf true)"
	printf 'block_latency_sample_rate=%s\n' "$LATENCY_SAMPLE_RATE"
	printf 'live_migration_continuity_proof=%s proof_offset=%s proof_slots=%s proof_interval_us=%s proof_sync_every=%s\n' \
		"$CONTINUITY_PROOF" "$CONTINUITY_PROOF_OFFSET" "$CONTINUITY_PROOF_SLOTS" \
		"$CONTINUITY_PROOF_INTERVAL_US" "$CONTINUITY_PROOF_SYNC_EVERY"
	printf 'direct_migration_control=%s after_repeat=%s source=%s destination=%s copy_method=%s copy_cpu_list=%s control_cpu=%s continuity_cpu=%s foreground_hops=1 migration_gateway=false\n' \
		"$([ -n "$DIRECT_MIGRATION_CONTROL_SOCKET" ] && printf enabled || printf disabled)" \
		"$DIRECT_MIGRATION_AFTER_REPEAT" "${DIRECT_MIGRATION_SOURCE_ADDR:-none}" \
		"${DIRECT_MIGRATION_DEST_ADDR:-none}" "$DIRECT_MIGRATION_COPY_METHOD" \
		"${DIRECT_MIGRATION_COPY_CPU_LIST:-none}" "${DIRECT_MIGRATION_CONTROL_CPU:-none}" \
		"${CONTINUITY_CPU:-none}"
	printf 'community_frontend=linux-block topology_class=%s placement_scope=%s transport=%s paths=%s\n' \
		"$ZCCUSAN_TOPOLOGY_CLASS" "$ZCCUSAN_PLACEMENT_SCOPE" \
		"$ZCCUSAN_TOPOLOGY_TRANSPORT" "$ZCCUSAN_TOPOLOGY_PATH_COUNT"
	printf 'block_ring_stats=%s block_completion_batch=%s block_wait_min_completions=%s block_fused_submit_wait=%s block_cqe_spin=%s block_cqe_adaptive_spin=%s block_cqe_adaptive_spin_min=%s block_cqe_adaptive_spin_max=%s block_cqe_adaptive_wait_ns=%s block_cqe_hot_poll=%s block_cqe_hot_poll_progress_spins=%s\n' \
		"$BLOCK_RING_STATS" "$BLOCK_COMPLETION_BATCH" "$BLOCK_WAIT_MIN_COMPLETIONS" "$BLOCK_FUSED_SUBMIT_WAIT" "$BLOCK_CQE_SPIN" "$BLOCK_CQE_ADAPTIVE_SPIN" \
		"$BLOCK_CQE_ADAPTIVE_SPIN_MIN" "$BLOCK_CQE_ADAPTIVE_SPIN_MAX" \
		"$BLOCK_CQE_ADAPTIVE_WAIT_NS" "$BLOCK_CQE_HOT_POLL" "$BLOCK_CQE_HOT_POLL_PROGRESS_SPINS"
	printf 'shm_descriptor_entries_per_channel=%s index_operation=bit-mask\n' "$SHM_RING_ENTRIES"
	printf 'kernel_queues=%s kernel_queue_depth=%s kernel_pipeline_depth=%s hctx_numa_node=%s\n' \
		"$KERNEL_QUEUES" "$KERNEL_QUEUE_DEPTH" "$KERNEL_PIPELINE_DEPTH" "$HCTX_NUMA_NODE"
	printf 'kernel_worker_batch_dequeue=%s\n' "$KERNEL_WORKER_BATCH_DEQUEUE"
	printf 'kernel_disable_merges=%s\n' "$KERNEL_DISABLE_MERGES"
	printf 'kernel_sequence_telemetry_interval=%s\n' \
		"$KERNEL_SEQUENCE_TELEMETRY_INTERVAL"
	printf 'kernel_completion_batch=%s\n' "$KERNEL_COMPLETION_BATCH"
	printf 'lane_local_sequences_requested=%s expected_sync_boundary=%s\n' \
		"$LANE_LOCAL_SEQUENCES" \
		"$([ "$LANE_LOCAL_SEQUENCES" = 1 ] && printf admitted-lane-vector-hwm || printf global-completion-hwm)"
	printf 'application_arena_buffers=%s application_buffer_copy_on_block_edge=%s\n' \
		"$APP_ARENA_BUFFERS" "$([ "$APP_ARENA_BUFFERS" = 1 ] && printf no || printf yes)"
	printf 'shm_sector_order_slots=%s\n' "$SECTOR_ORDER_SLOTS"
	printf 'shm_payload_entries_per_channel=%s index_operation=bit-mask\n' "$SHM_PAYLOAD_ENTRIES"
	printf 'shm_arena_cpu_list=%s shm_arena_cpu_list_source=%s locality=%s\n' \
		"${SHM_ARENA_CPU_LIST:-none}" "$SHM_ARENA_CPU_LIST_SOURCE" "$SHM_ARENA_LOCALITY"
	printf 'ofi_selective_completion=%s ofi_rma_read_completion_stride=%s ofi_rma_defer_tail_completion=%s ofi_rma_read_more=%s ofi_rma_read_tail_marker=%s synthetic_partial_flush=fallback-only\n' \
		"$OFI_SELECTIVE_COMPLETION" "$OFI_RMA_READ_COMPLETION_STRIDE" \
		"$OFI_RMA_DEFER_TAIL_COMPLETION" "$OFI_RMA_READ_MORE" \
		"$([ "$OFI_RMA_DEFER_TAIL_COMPLETION" = 1 ] && printf deferred-real-request || printf disabled)"
	printf 'record_12m_rma_read_gate=%s record_mean_iops_floor=%s record_per_rep_iops_floor=%s\n' \
		"$([ "$SHM_OFI_RMA_READS" = 1 ] && [ "$MIN_MEAN_IOPS" -ge 12000000 ] && printf enabled || printf disabled)" \
		"$MIN_MEAN_IOPS" "$MIN_IOPS_PER_REP"
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
	printf 'wal_owner_ingress=%s wal_owner_ingress_source=%s wal_owner_count=%s wal_owner_count_source=%s wal_owner_cpu_list=%s\n' \
		"$WAL_OWNER_INGRESS" "$WAL_OWNER_INGRESS_SOURCE" "$WAL_OWNER_COUNT" \
		"$WAL_OWNER_COUNT_SOURCE" \
		"$([ "$WAL_OWNER_INGRESS" = 1 ] && join_comma "${owner_cpus[@]}" || printf none)"
	printf 'wal_owner_write_fill_us=%s wal_owner_write_fill_min=%s wal_owner_pipeline_batches=%s wal_owner_pipeline_refill_spins=%s wal_owner_mixed_hysteresis_us=%s\n' \
		"$WAL_OWNER_WRITE_FILL_US" "$WAL_OWNER_WRITE_FILL_MIN" "$WAL_OWNER_PIPELINE_BATCHES" \
		"$WAL_OWNER_PIPELINE_REFILL_SPINS" "$WAL_OWNER_MIXED_HYSTERESIS_US"
	printf 'wal_owner_debounce_us=%s wal_owner_backlog_low_records=%s wal_owner_backlog_high_records=%s\n' \
		"$WAL_OWNER_DEBOUNCE_US" "$WAL_OWNER_BACKLOG_LOW_RECORDS" "$WAL_OWNER_BACKLOG_HIGH_RECORDS"
	printf 'wal_owner_batch_records=%s wal_owner_fragment_records=%s wal_owner_fragment_fill_us=%s wal_owner_queue_depth=%s wal_owner_queue_depth_source=%s\n' \
		"$WAL_OWNER_BATCH_RECORDS" "$WAL_OWNER_FRAGMENT_RECORDS" "$WAL_OWNER_FRAGMENT_FILL_US" \
		"$WAL_OWNER_QUEUE_DEPTH" "$WAL_OWNER_QUEUE_DEPTH_SOURCE"
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
	printf 'wal_foreground_read_immediate=%s\n' "$WAL_FOREGROUND_READ_IMMEDIATE"
	printf 'wal_cq_delay_spins=%s\n' "$WAL_CQ_DELAY_SPINS"
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
	printf 'remote_transport=%s remote_ofi_provider=%s remote_ofi_endpoint=%s ofi_domain=%s lane_ofi_domains=%s ofi_cq_sleep_ns=%s wal_ofi_message_bytes=%s wal_ofi_hugetlb_confirmed=%s efa_use_device_rdma=%s shm_ofi_rma_reads=%s shm_ofi_rma_read_qd=%s rma_read_aggregate_outstanding_depth=%s leaf_ofi_rma_reads=%s shm_ofi_rma_writes=%s shm_ofi_rma_writes_required=%s shm_ofi_rma_write_qd=%s rma_write_aggregate_outstanding_depth=%s leaf_ofi_rma_writes=%s rma_write_delivery_complete=%s rma_write_more=%s rma_write_more_burst=%s\n' \
		"$REMOTE_TRANSPORT" "$REMOTE_OFI_PROVIDER" "$REMOTE_OFI_ENDPOINT" \
		"${OFI_DOMAIN:-auto}" "${SHM_OFI_DOMAINS:-single-domain}" "$OFI_CQ_SLEEP_NS" "$WAL_OFI_MESSAGE_BYTES" \
		"$WAL_OFI_HUGETLB_CONFIRMED" "$EFA_USE_DEVICE_RDMA" \
		"$SHM_OFI_RMA_READS" "$SHM_OFI_RMA_READ_QD" "$((LANES * SHM_OFI_RMA_READ_QD))" \
		"$LEAF_OFI_RMA_READS" "$SHM_OFI_RMA_WRITES" "$SHM_OFI_RMA_WRITES_REQUIRED" \
		"$SHM_OFI_RMA_WRITE_QD" "$((RMA_WRITE_ENDPOINT_COUNT * SHM_OFI_RMA_WRITE_QD))" \
		"$LEAF_OFI_RMA_WRITES" "$OFI_RMA_WRITE_DELIVERY_COMPLETE" "$OFI_RMA_WRITE_MORE" \
		"$OFI_RMA_WRITE_MORE_BURST"
	printf 'rma_write_completion=delivery-cq-before-doorbell-result-hwm rma_write_pipeline_batches=%s rma_write_overlap_order=delivery-barrier end_to_end_zero_copy=no\n' \
		"$WAL_OWNER_PIPELINE_BATCHES"
	printf 'rma_write_qd_source=%s rma_write_representative_min_qd=%s rma_write_qd_scope=per-owner-payload-operations block_qd_coupled=no\n' \
		"$SHM_OFI_RMA_WRITE_QD_SOURCE" "$SHM_OFI_RMA_WRITE_MIN_QD"
	printf 'rma_write_owner_mode=%s rma_write_endpoint_count=%s multi_endpoint_confirmed=%s ingress_lane_fan_in=%s\n' \
		"$SHM_OFI_RMA_WRITE_OWNER_MODE" "$RMA_WRITE_ENDPOINT_COUNT" \
		"$SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED" \
		"$([ "$WAL_OWNER_INGRESS" = 1 ] && [ "$WAL_OWNER_COUNT" -lt "$LANES" ] && printf '%s-to-%s' "$LANES" "$WAL_OWNER_COUNT" || printf none)"
	printf 'remote_stream_count=%s remote_stream_scope=%s\n' \
		"$REMOTE_STREAM_COUNT" \
		"$([ "$WAL_OWNER_INGRESS" = 1 ] && printf stable-userspace-owners || printf ingress-lanes)"
	printf 'rma_source_backing_requested=%s legacy_hugetlb_confirmation=%s actual_backing=validated-from-target-log-before-benchmark\n' \
		"$SHM_ARENA_BACKING" "$OFI_RMA_SOURCE_HUGETLB_CONFIRMED"
	printf 'order_smoke_pairs=%s\n' "$ORDER_SMOKE_PAIRS"
	printf 'shm_payload_slot_bytes=%s\n' "$MAX_FRAME_BYTES"
	printf 'shm_lease_release_batch=%s\n' "$LEASE_RELEASE_BATCH"
	printf 'backend=%s local_leaf=%s leaf_target=%s leaf_addr=%s leaf_port=%s leaf_submit_mode=%s leaf_allow_volatile_sync=%s\n' \
		"$BACKEND" "$START_LOCAL_LEAF" "$LEAF_TARGET" "$LEAF_ADDR" "$LEAF_PORT" "$LEAF_SUBMIT_MODE" "$LEAF_ALLOW_VOLATILE_SYNC"
	printf 'leaf_source_addr=%s\n' "${LEAF_SOURCE_ADDR:-kernel-route}"
	printf 'leaf_addrs=%s leaf_source_addrs=%s\n' \
		"${LEAF_ADDRS:-single-address}" "${LEAF_SOURCE_ADDRS:-single-source}"
	printf 'external_leaf_topology_artifact=%s leaf_lane_to_worker_cpu=%s leaf_lane_to_nic=%s\n' \
		"${EXTERNAL_LEAF_TOPOLOGY_ARTIFACT:-local-leaf}" \
		"$external_leaf_cpu_map" "$external_leaf_nic_map"
} | tee "$OUTDIR/topology.log"
ps -eLo pid,tid,psr,pcpu,comm --sort=-pcpu | head -n 80 >"$OUTDIR/process-noise.before" || true

block_hugepages=0
if [ "$APP_ARENA_BUFFERS" = 1 ]; then
	printf 'hugetlb_preflight: benchmark buffers alias the shared target arena; no separate client allocation\n' | tee -a "$OUTDIR/preflight.log"
elif [ "$BUFFER_MODE" != hugetlb ]; then
	printf 'PERF WARNING: BUFFER_MODE=%s; this is not a hugetlb representative run\n' "$BUFFER_MODE" | tee -a "$OUTDIR/preflight.log" >&2
	[ "$REPRESENTATIVE" != 1 ] || die "representative runs require BUFFER_MODE=hugetlb"
else
	block_hugepages=$((LANES * IODEPTH))
fi
arena_hugepages=0
hugepage_bytes=$(( $(awk '/Hugepagesize:/{print $2}' /proc/meminfo) * 1024 ))
if [ "$SHM_ARENA_BACKING" = hugetlb ]; then
	arena_payload_bytes=$((LANES * SHM_PAYLOAD_ENTRIES * MAX_FRAME_BYTES))
	arena_metadata_bytes=$((6 * 4096 + LANES * (320 + SHM_RING_ENTRIES * (64 + 64 + 16) + SHM_PAYLOAD_ENTRIES * 8)))
	arena_hugepages=$(( (arena_payload_bytes + arena_metadata_bytes + hugepage_bytes - 1) / hugepage_bytes ))
fi
leaf_hugepages=0
if [ "$START_LOCAL_LEAF" = 1 ] && [ "$LEAF_ZCMEM_HUGETLB" = 1 ]; then
	leaf_hugepages=$(( (SIZE_MIB * 1024 * 1024 + hugepage_bytes - 1) / hugepage_bytes ))
fi
required_hugepages=$((block_hugepages + arena_hugepages + leaf_hugepages))
hugepages_free="$(awk '/HugePages_Free:/{print $2}' /proc/meminfo)"
printf 'hugetlb_preflight: free_pages=%s required_pages=%s block_buffer_pages=%s shared_arena_pages=%s local_leaf_pages=%s\n' \
	"$hugepages_free" "$required_hugepages" "$block_hugepages" "$arena_hugepages" "$leaf_hugepages" | tee -a "$OUTDIR/preflight.log"
if [ "$hugepages_free" -lt "$required_hugepages" ]; then
	die "hugetlb needs at least $required_hugepages free pages (block=$block_hugepages arena=$arena_hugepages local_leaf=$leaf_hugepages); found $hugepages_free"
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
	[ "$(leaf_listener_count)" -eq 0 ] || die "one or more WAL leaf lane/control ports are already listening"
	log "starting terminal userspace WAL leaf on cpu $leaf_cpu_list"
	env URING_PLAY_PIN_CPU_LIST="$leaf_cpu_list" URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT="$LEAF_TRANSPORT" \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER="$LEAF_OFI_PROVIDER" \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT="$LEAF_OFI_ENDPOINT" \
		URING_PLAY_ZCNBLK_WAL_LEAF_SPIN_READS="$LEAF_SPIN_READS" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN="$LEAF_ADAPTIVE_SPIN" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MIN="$LEAF_ADAPTIVE_SPIN_MIN" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MAX="$LEAF_ADAPTIVE_SPIN_MAX" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_WAIT_NS="$LEAF_ADAPTIVE_WAIT_NS" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_HYSTERESIS_NS="$LEAF_ADAPTIVE_HYSTERESIS_NS" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC="$LEAF_ALLOW_VOLATILE_SYNC" \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS="$LEAF_OFI_RMA_READS" \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_WRITES="$LEAF_OFI_RMA_WRITES" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB="$LEAF_ZCMEM_HUGETLB" \
		URING_PLAY_OFI_DOMAIN="$OFI_DOMAIN" \
		URING_PLAY_OFI_CONTROL_PORT_OFFSET="$OFI_CONTROL_PORT_OFFSET" \
		URING_PLAY_OFI_CQ_SLEEP_NS="$OFI_CQ_SLEEP_NS" \
		URING_PLAY_OFI_RMA_READ_QD="$OFI_ENDPOINT_RMA_READ_QD" \
		URING_PLAY_OFI_RMA_WRITE_QD="$OFI_ENDPOINT_RMA_WRITE_QD" \
		URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE="$OFI_RMA_WRITE_DELIVERY_COMPLETE" \
		URING_PLAY_OFI_RMA_WRITE_MORE="$OFI_RMA_WRITE_MORE" \
		URING_PLAY_OFI_RMA_WRITE_MORE_BURST="$OFI_RMA_WRITE_MORE_BURST" \
		URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES="$WAL_OFI_MESSAGE_BYTES" \
		URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED="$WAL_OFI_HUGETLB_CONFIRMED" \
		FI_EFA_IFACE="$EFA_IFACE" \
		FI_EFA_USE_DEVICE_RDMA="$EFA_USE_DEVICE_RDMA" \
		URING_PLAY_OFI_EFA_FABRIC="$OFI_EFA_FABRIC" \
		"$LEAF_BIN" "$LEAF_TARGET" "$LEAF_ADDR" "$LEAF_PORT" "$LANES" 1 4096 "$LANES" true "$LEAF_SUBMIT_MODE" \
		>"$OUTDIR/leaf.log" 2>&1 &
	leaf_pid=$!
	for _ in $(seq 1 100); do
		[ "$(leaf_listener_count)" -eq "$LANES" ] && break
		[ -e "/proc/$leaf_pid" ] || die "WAL leaf exited before listening; see $OUTDIR/leaf.log"
		sleep 0.05
	done
	[ "$(leaf_listener_count)" -eq "$LANES" ] || \
		die "WAL leaf did not open all $LANES lane/control listeners"
fi

log "starting userspace shared target/fan; no placement decision exists in the kernel edge"
app_arena_socket=""
[ "$APP_ARENA_BUFFERS" != 1 ] || app_arena_socket="$APP_ARENA_SOCKET"
target_priv=(sudo -n env)
if [ "${TARGET_MEMLOCK_UNLIMITED:-0}" = 1 ]; then
	command -v prlimit >/dev/null || die "TARGET_MEMLOCK_UNLIMITED=1 requires prlimit"
	target_priv=(sudo -n prlimit --memlock=unlimited -- env)
fi
"${target_priv[@]}" URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE="$pid_file" \
	URING_PLAY_ZCNBLK_SHM_ARENA_BACKING="$SHM_ARENA_BACKING" \
	URING_PLAY_ZCNBLK_SHM_ARENA_CPU_LIST="$SHM_ARENA_CPU_LIST" \
	URING_PLAY_ZCNBLK_SHM_ARENA_CPU="${target_cpus[0]}" \
	URING_PLAY_ZCNBLK_SHM_APP_ARENA_SOCKET="$app_arena_socket" \
	URING_PLAY_ZCNBLK_SHM_LANE_LOCAL_SEQUENCES="$LANE_LOCAL_SEQUENCES" \
	URING_PLAY_TOPOLOGY_REPRESENTATIVE="$REPRESENTATIVE" \
	URING_PLAY_ZCNBLK_SHM_POLL_CLOCK_CHECK_SPINS="$POLL_CLOCK_CHECK_SPINS" \
	URING_PLAY_ZCNBLK_SHM_HTB_SUSTAINED_IOPS="$HTB_SUSTAINED_IOPS" \
	URING_PLAY_ZCNBLK_SHM_HTB_PEAK_IOPS="$HTB_PEAK_IOPS" \
	URING_PLAY_ZCNBLK_SHM_HTB_QUANTUM_OPS="$HTB_QUANTUM_OPS" \
	URING_PLAY_ZCNBLK_SHM_HTB_BURST_SECONDS="$HTB_BURST_SECONDS" \
	URING_PLAY_ZCNBLK_SHM_HTB_CONTROL_FILE="$HTB_CONTROL_FILE" \
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
	URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT="$REMOTE_TRANSPORT" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER="$REMOTE_OFI_PROVIDER" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT="$REMOTE_OFI_ENDPOINT" \
	URING_PLAY_OFI_THREADING="$OFI_THREADING" \
	URING_PLAY_OFI_SELECTIVE_COMPLETION="$OFI_SELECTIVE_COMPLETION" \
	URING_PLAY_OFI_RMA_READ_COMPLETION_STRIDE="$OFI_RMA_READ_COMPLETION_STRIDE" \
	URING_PLAY_OFI_RMA_DEFER_TAIL_COMPLETION="$OFI_RMA_DEFER_TAIL_COMPLETION" \
	URING_PLAY_OFI_RMA_READ_MORE="$OFI_RMA_READ_MORE" \
	URING_PLAY_ZCNBLK_SHM_OFI_DOMAINS="$SHM_OFI_DOMAINS" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS="$SHM_OFI_RMA_READS" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD="$SHM_OFI_RMA_READ_QD" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES="$SHM_OFI_RMA_WRITES" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES_REQUIRED="$SHM_OFI_RMA_WRITES_REQUIRED" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_QD="$SHM_OFI_RMA_WRITE_QD" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_OWNER_MODE="$SHM_OFI_RMA_WRITE_OWNER_MODE" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED="$SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED" \
	URING_PLAY_OFI_DOMAIN="$OFI_DOMAIN" \
	URING_PLAY_OFI_CQ_SLEEP_NS="$OFI_CQ_SLEEP_NS" \
	URING_PLAY_OFI_RMA_READ_QD="$OFI_ENDPOINT_RMA_READ_QD" \
	URING_PLAY_OFI_RMA_WRITE_QD="$OFI_ENDPOINT_RMA_WRITE_QD" \
	URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE="$OFI_RMA_WRITE_DELIVERY_COMPLETE" \
	URING_PLAY_OFI_RMA_WRITE_MORE="$OFI_RMA_WRITE_MORE" \
	URING_PLAY_OFI_RMA_WRITE_MORE_BURST="$OFI_RMA_WRITE_MORE_BURST" \
	URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES="$WAL_OFI_MESSAGE_BYTES" \
	URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED="$WAL_OFI_HUGETLB_CONFIRMED" \
	FI_EFA_IFACE="$EFA_IFACE" \
	FI_EFA_USE_DEVICE_RDMA="$EFA_USE_DEVICE_RDMA" \
	URING_PLAY_OFI_EFA_FABRIC="$OFI_EFA_FABRIC" \
	URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_RECORDS="$WAL_EXTENT_RECORDS" \
	URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_FILL_US="$WAL_EXTENT_FILL_US" \
	URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_MIN_BATCH_RECORDS="$WAL_SPLIT_MIN_BATCH_RECORDS" \
	URING_PLAY_ZCNBLK_SHM_WAL_FOREGROUND_READ_IMMEDIATE="$WAL_FOREGROUND_READ_IMMEDIATE" \
	URING_PLAY_ZCNBLK_SHM_WAL_CQ_DELAY_SPINS="$WAL_CQ_DELAY_SPINS" \
	URING_PLAY_ZCNBLK_SHM_WAL_COMPACT_WRITES="$WAL_COMPACT_WRITES" \
	URING_PLAY_ZCNBLK_SHM_SUBPAGE_READS="$([ "$BLOCK_SIZE" = 4096 ] && printf 0 || printf 1)" \
	URING_PLAY_ZCNBLK_SHM_DIRTY_PRESSURE_RESERVE="$DIRTY_PRESSURE_RESERVE" \
	URING_PLAY_ZCNBLK_SHM_WAL_DEBUG_STATE="$WAL_DEBUG_STATE" \
	URING_PLAY_ROUTE_PROBE="${URING_PLAY_ROUTE_PROBE:-0}" \
	URING_PLAY_EXPECT_ROUTE_DEV="${URING_PLAY_EXPECT_ROUTE_DEV:-}" \
	URING_PLAY_EXPECT_ROUTE_SRC="${URING_PLAY_EXPECT_ROUTE_SRC:-}" \
	URING_PLAY_ZCNBLK_SHM_MIGRATION_CONTROL_SOCKET="$DIRECT_MIGRATION_CONTROL_SOCKET" \
	URING_PLAY_ZCNBLK_SHM_MIGRATION_SOURCE_ADDR="$DIRECT_MIGRATION_SOURCE_ADDR" \
	URING_PLAY_ZCNBLK_SHM_MIGRATION_DEST_ADDR="$DIRECT_MIGRATION_DEST_ADDR" \
	URING_PLAY_ZCNBLK_SHM_MIGRATION_TCP_COPY_METHOD="$DIRECT_MIGRATION_COPY_METHOD" \
	URING_PLAY_ZCNBLK_SHM_MIGRATION_CATCHUP_PASSES="$DIRECT_MIGRATION_CATCHUP_PASSES" \
	URING_PLAY_ZCNBLK_SHM_MIGRATION_QUIESCE_TIMEOUT_MS="$DIRECT_MIGRATION_QUIESCE_TIMEOUT_MS" \
	ZCNBLK_WAL_MIGRATION_COPY_CPU_LIST="$DIRECT_MIGRATION_COPY_CPU_LIST" \
	ZCNBLK_WAL_MIGRATION_CONTROL_CPU="$DIRECT_MIGRATION_CONTROL_CPU" \
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
for _ in $(seq 1 "${TARGET_READY_ATTEMPTS:-400}"); do
	[ -s "$pid_file" ] && break
	if ! kill -0 "$target_job_pid" 2>/dev/null; then
		wait "$target_job_pid" || true
		die "target exited before publishing its PID file; inspect $OUTDIR/target.log"
	fi
	sleep 0.05
done
[ -s "$pid_file" ] || die "target did not publish its PID file"
target_pid="$(cat "$pid_file")"
[[ "$target_pid" =~ ^[0-9]+$ ]] || die "invalid target PID: $target_pid"
if [ -n "$DIRECT_MIGRATION_CONTROL_SOCKET" ]; then
	for _ in $(seq 1 400); do
		[ -S "$DIRECT_MIGRATION_CONTROL_SOCKET" ] && break
		[ -r "/proc/$target_pid/comm" ] || die "target exited before publishing direct migration control"
		sleep 0.01
	done
	[ -S "$DIRECT_MIGRATION_CONTROL_SOCKET" ] || \
		die "target did not publish direct migration control socket"
	grep -q "^zcnblk-shm-target-direct-route-control: .* control_cpu=$DIRECT_MIGRATION_CONTROL_CPU " \
		"$OUTDIR/target.log" || \
		die "target did not confirm the requested direct migration control CPU"
fi
arena_line="$(grep '^zcnblk-shm-target-shared-arena:' "$OUTDIR/target.log" | tail -n 1 || true)"
[ -n "$arena_line" ] || die "target did not report the shared-arena backing before benchmarking"
printf 'actual_%s\n' "$arena_line" | tee -a "$OUTDIR/topology.log"
sequence_line="$(grep '^zcnblk-shm-target-sequencing:' "$OUTDIR/target.log" | tail -n 1 || true)"
[ -n "$sequence_line" ] || die "target did not report its sequencing contract before benchmarking"
printf 'actual_%s\n' "$sequence_line" | tee -a "$OUTDIR/topology.log"
if [ "$LANE_LOCAL_SEQUENCES" = 1 ]; then
	grep -q ' mode=lane-local .* sync_boundary=admitted-lane-vector-hwm' <<<"$sequence_line" || \
		die "target did not negotiate the requested lane-local sequencing contract"
else
	grep -q ' mode=global .* sync_boundary=global-completion-hwm' <<<"$sequence_line" || \
		die "target unexpectedly negotiated lane-local sequencing"
fi
if [ "$SHM_ARENA_BACKING" = hugetlb ]; then
	grep -q ' backing=external-hugetlb-memfd .* import_active=true ' <<<"$arena_line" || \
		die "target did not activate the required external HugeTLB shared arena"
	arena_topology_line="$(grep '^zcnblk-shm-target-arena-topology:' "$OUTDIR/target.log" | tail -n 1 || true)"
	[ -n "$arena_topology_line" ] || \
		die "HugeTLB target did not report per-lane shared-arena first-touch topology"
	printf 'actual_%s\n' "$arena_topology_line" | tee -a "$OUTDIR/topology.log"
fi
if [ "$APP_ARENA_BUFFERS" = 1 ]; then
	for _ in $(seq 1 100); do
		[ -S "$app_arena_socket" ] && break
		sleep 0.01
	done
	[ -S "$app_arena_socket" ] || die "target did not publish the application arena socket"
	grep -q '^zcnblk-shm-target-app-arena:' "$OUTDIR/target.log" || \
		die "target did not report the application arena ownership contract"
fi
if [ "$SHM_OFI_RMA_WRITES" = 1 ]; then
	for _ in $(seq 1 200); do
		rma_windows="$(grep -c '^zcnblk-shm-target-ofi-rma-write-window: lane=.* completion=initiator-delivery-cq-before-doorbell' "$OUTDIR/target.log" || true)"
		[ "$rma_windows" -eq "$WAL_OWNER_COUNT" ] && break
		[ -r "/proc/$target_pid/comm" ] || die "target exited during RMA write-window negotiation"
		sleep 0.05
	done
	rma_windows="$(grep -c '^zcnblk-shm-target-ofi-rma-write-window: lane=.* completion=initiator-delivery-cq-before-doorbell' "$OUTDIR/target.log" || true)"
	[ "$rma_windows" -eq "$WAL_OWNER_COUNT" ] || \
		die "RMA write-window negotiation covered $rma_windows owners, expected $WAL_OWNER_COUNT"
fi

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
if [ "$CONTINUITY_PROOF" = 1 ]; then
	log "starting one-open-descriptor migration continuity and data proof"
	continuity_prefix=(sudo -n env ZCNBLK_EDGE_CONTINUITY_PID_FILE="$continuity_pid_file")
	if [ -n "$CONTINUITY_CPU" ]; then
		continuity_prefix+=(taskset -c "$CONTINUITY_CPU")
	fi
	"${continuity_prefix[@]}" \
		"$EDGE_CONTINUITY_BIN" /dev/zcnblk0 "$CONTINUITY_PROOF_OFFSET" \
		"$CONTINUITY_PROOF_SLOTS" "$CONTINUITY_PROOF_INTERVAL_US" \
		"$CONTINUITY_PROOF_SYNC_EVERY" >"$OUTDIR/continuity.log" 2>&1 &
	continuity_job_pid=$!
	for _ in $(seq 1 400); do
		[ -s "$continuity_pid_file" ] && \
			grep -q '^zcnblk-edge-continuity-start:' "$OUTDIR/continuity.log" && break
		if ! kill -0 "$continuity_job_pid" 2>/dev/null; then
			wait "$continuity_job_pid" || true
			die "continuity proof exited before becoming ready; inspect $OUTDIR/continuity.log"
		fi
		sleep 0.01
	done
	[ -s "$continuity_pid_file" ] || die "continuity proof did not publish its PID"
	continuity_pid="$(cat "$continuity_pid_file")"
	[[ "$continuity_pid" =~ ^[0-9]+$ ]] || die "invalid continuity proof PID"
	grep -q '^zcnblk-edge-continuity-start:' "$OUTDIR/continuity.log" || \
		die "continuity proof did not finish seeding its reserved range"
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

# A frontend integration test may reuse the exact representative topology
# setup while replacing zcblockbench with QEMU, iSCSI, or another userspace
# edge.  The command receives only the already-established edge paths; it does
# not gain any placement responsibility.
if [ -n "${EXTERNAL_FRONTEND_COMMAND:-}" ]; then
	log "running external frontend against the established userspace stage"
	export URING_PLAY_ZCNBLK_SHM_APP_ARENA_SOCKET="$app_arena_socket"
	export ZCNBLK_FRONTEND_DEVICE=/dev/zcnblk0
	set +e
	bash -c "$EXTERNAL_FRONTEND_COMMAND" | tee "$OUTDIR/external-frontend.log"
	external_frontend_status=${PIPESTATUS[0]}
	set -e
	preserve_external_frontend_log() {
		local key=$1 destination=$2 required=$3 source
		source="$(awk -F= -v key="$key" '$1 == key { print substr($0, length(key) + 2) }' \
			"$OUTDIR/external-frontend.log" | tail -n 1)"
		if [ -z "$source" ] || [ ! -r "$source" ]; then
			[ "$required" != 1 ] || \
				die "external frontend did not leave a readable $key artifact: ${source:-missing}"
			return 0
		fi
		cp -- "$source" "$OUTDIR/$destination"
	}
	if [ "$external_frontend_status" -eq 0 ]; then
		external_artifact_required=1
	else
		external_artifact_required=0
	fi
	preserve_external_frontend_log guest_log qemu-exact.log "$external_artifact_required"
	preserve_external_frontend_log backend_log backend-exact.log "$external_artifact_required"
	preserve_external_frontend_log qemu_vcpu_pin_log qemu-vcpu-pinning.log 0
	# Preserve the exact kernel-edge alias accounting before module teardown.
	# An external frontend replaces zcblockbench, so its application-arena proof
	# must not disappear with the ordinary EXIT cleanup.
	if [ "$APP_ARENA_BUFFERS" = 1 ]; then
		debug_state=/sys/kernel/debug/zcnblk/state
		sudo -n test -r "$debug_state" || \
			die "application-arena external frontend has no readable $debug_state"
		sudo -n cat "$debug_state" | tee "$OUTDIR/block-edge-state.after-external.log"
		if [ "$external_frontend_status" -ne 0 ]; then
			die "external frontend exited with status $external_frontend_status; exact failure artifacts were preserved when available"
		fi
		grep -q 'bio_arena_zero_copy_required=1' "$OUTDIR/block-edge-state.after-external.log" || \
			die "external frontend did not retain required block-edge arena aliasing"
		grep -Eq 'bio_alias_(writes|reads)=[1-9][0-9]*' "$OUTDIR/block-edge-state.after-external.log" || \
			die "external frontend completed no block-edge arena aliases"
		grep -q 'bio_alias_busy_fallbacks=0' "$OUTDIR/block-edge-state.after-external.log" || \
			die "external frontend used a block-edge arena copy fallback"
		grep -q 'bio_alias_required_rejects=0' "$OUTDIR/block-edge-state.after-external.log" || \
			die "external frontend submitted a non-aliasing buffer to the required arena path"
	fi
	[ "$external_frontend_status" -eq 0 ] || \
		die "external frontend exited with status $external_frontend_status; exact failure artifacts were preserved when available"
	safe_stop_target
	target_pid=""
	wait "$target_job_pid" || true
	target_job_pid=""
	grep 'zcnblk-shm-target-summary:' "$OUTDIR/target.log" | tee "$OUTDIR/summary.log"
	# Keep the ordinary EXIT cleanup armed: it unloads the client module,
	# restores governors, releases coordination leases, and stops a local leaf
	# when the external frontend used one.
	exit 0
fi

if [ "$ORDER_SMOKE_PAIRS" -gt 0 ]; then
	log "proving same-sector ordering and sync across the live $LANES-lane path"
	order_started_ns="$(date +%s%N)"
	sudo -n env "URING_PLAY_PIN_CPU_LIST=$client_cpu_list" \
		"$ORDER_BIN" /dev/zcnblk0 "$ORDER_SMOKE_PAIRS" | tee "$OUTDIR/order-smoke.log"
	order_elapsed_ns=$(( $(date +%s%N) - order_started_ns ))
	printf 'completion_semantics=remote-global-sync-drain elapsed_ns=%s\n' \
		"$order_elapsed_ns" | tee -a "$OUTDIR/order-smoke.log"
	grep -q 'sync_terminal_state=true' "$OUTDIR/order-smoke.log" || \
		die "multi-lane order smoke did not prove terminal sync state"
fi

if [ -n "$CONTRACT_SMOKE_BLOCK" ]; then
	[[ "$CONTRACT_SMOKE_BLOCK" =~ ^[0-9]+$ ]] || \
		die "CONTRACT_SMOKE_BLOCK must be a non-negative integer"
	log "proving native FUA, I/O priority, write lifetime, and readback"
	contract_started_ns="$(date +%s%N)"
	sudo -n env "URING_PLAY_PIN_CPU_LIST=$client_cpu_list" \
		"$CONTRACT_BIN" /dev/zcnblk0 "$CONTRACT_SMOKE_BLOCK" | \
		tee "$OUTDIR/contract-smoke.log"
	contract_elapsed_ns=$(( $(date +%s%N) - contract_started_ns ))
	printf 'completion_semantics=remote-fua-drain elapsed_ns=%s\n' \
		"$contract_elapsed_ns" | tee -a "$OUTDIR/contract-smoke.log"
	grep -q 'fua=RWF_DSYNC' "$OUTDIR/contract-smoke.log" || \
		die "contract smoke did not issue native FUA"
fi

log "running $REPEATS repeated $MODE controls on the shared host"
for ((rep = 1; rep <= REPEATS; rep++)); do
	if [ -n "$LIVE_MIGRATION_CONTROL_ADDR" ] && \
		[ "$rep" -eq "$LIVE_MIGRATION_START_BEFORE_REPEAT" ]; then
		log "starting userspace base copy before repeat $rep"
		live_migration_command start >/dev/null
	fi
	result_log="$OUTDIR/rep$rep.log"
	perf_log="$OUTDIR/rep$rep.perf"
	context_before="$OUTDIR/rep$rep.context.before"
	context_after="$OUTDIR/rep$rep.context.after"
	snapshot_contexts "$context_before"
	bench=(env "URING_PLAY_PIN_CPU_LIST=$client_cpu_list"
		"URING_PLAY_BLOCKBENCH_WRITE_COMPLETION_SEMANTICS=$block_write_completion"
		"URING_PLAY_ZCNBLK_SHM_APP_ARENA_SOCKET=$app_arena_socket"
		"ZCCUSAN_PLACEMENT_SCOPE=$ZCCUSAN_PLACEMENT_SCOPE"
		"ZCCUSAN_TOPOLOGY_CLASS=$ZCCUSAN_TOPOLOGY_CLASS"
		"ZCCUSAN_TOPOLOGY_PATH_COUNT=$ZCCUSAN_TOPOLOGY_PATH_COUNT"
		"ZCCUSAN_TOPOLOGY_TRANSPORT=$ZCCUSAN_TOPOLOGY_TRANSPORT"
		"ZCCUSAN_TOPOLOGY_LANE_COUNT=$LANES"
		"ZCCUSAN_TOPOLOGY_WORKER_COUNT=$LANES"
		"ZCCUSAN_TOPOLOGY_NUMA_NODE_COUNT=$ZCCUSAN_TOPOLOGY_NUMA_NODE_COUNT"
		"ZCCUSAN_TOPOLOGY_NUMA_LOCAL=$ZCCUSAN_TOPOLOGY_NUMA_LOCAL"
		"URING_PLAY_TOPOLOGY_STRICT=$REPRESENTATIVE"
		"URING_PLAY_BLOCKBENCH_RING_STATS=$BLOCK_RING_STATS"
		"URING_PLAY_BLOCKBENCH_COMPLETION_BATCH=$BLOCK_COMPLETION_BATCH"
		"URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS=$BLOCK_WAIT_MIN_COMPLETIONS"
		"URING_PLAY_BLOCKBENCH_FUSED_SUBMIT_WAIT=$BLOCK_FUSED_SUBMIT_WAIT"
		"URING_PLAY_CQE_SPIN=$BLOCK_CQE_SPIN"
		"URING_PLAY_CQE_ADAPTIVE_SPIN=$BLOCK_CQE_ADAPTIVE_SPIN"
		"URING_PLAY_CQE_ADAPTIVE_SPIN_MIN=$BLOCK_CQE_ADAPTIVE_SPIN_MIN"
		"URING_PLAY_CQE_ADAPTIVE_SPIN_MAX=$BLOCK_CQE_ADAPTIVE_SPIN_MAX"
		"URING_PLAY_CQE_ADAPTIVE_WAIT_NS=$BLOCK_CQE_ADAPTIVE_WAIT_NS"
		"URING_PLAY_CQE_HOT_POLL=$BLOCK_CQE_HOT_POLL"
		"URING_PLAY_CQE_HOT_POLL_PROGRESS_SPINS=$BLOCK_CQE_HOT_POLL_PROGRESS_SPINS"
		"$BENCH_BIN" /dev/zcnblk0
		--engine "$BLOCK_ENGINE" --mode "$MODE" --workers "$LANES"
		--ops-per-worker "$OPS_PER_WORKER" --bs "$BLOCK_SIZE" --iodepth "$IODEPTH"
		--region-bytes-per-worker "$REGION_BYTES_PER_WORKER"
		--read-percent "$READ_PERCENT" --ring-entries "$RING_ENTRIES"
		--ring-mode "$BLOCK_RING_MODE" --sqpoll-idle-ms "$SQPOLL_IDLE_MS"
		--buffer-mode "$BUFFER_MODE" --pin true)
	bench+=(--noatime "$BLOCK_NOATIME")
	bench+=(--registered-ring "$BLOCK_REGISTERED_RING")
	if [ "$LATENCY_SAMPLE_RATE" -gt 0 ]; then
		bench+=(--latency-sample-rate "$LATENCY_SAMPLE_RATE")
	fi
	if [ "$BLOCK_FUA_WRITES" = 1 ]; then
		bench+=(--fua)
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
	check_kernel_timing_faults
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
	rep_iops="$(awk '{ for (i=1; i<=NF; i++) if ($i ~ /^ops_per_sec=/) { split($i,a,"="); print a[2]; exit } }' <<<"$line")"
	[ -n "$rep_iops" ] || die "repeat $rep result has no ops_per_sec field"
	awk -v actual="$rep_iops" -v minimum="$MIN_IOPS_PER_REP" \
		'BEGIN { exit !(actual + 0 >= minimum + 0) }' || \
		die "repeat $rep IOPS $rep_iops is below required $MIN_IOPS_PER_REP"
	latency_line="$(grep 'zcblockbench-latency:' "$result_log" | tail -n 1 || true)"
	[ -z "$latency_line" ] || printf 'repeat=%s %s\n' "$rep" "$latency_line" | tee -a "$OUTDIR/results.log"
	ring_line="$(grep 'zcblockbench-ring:' "$result_log" | tail -n 1 || true)"
	[ -z "$ring_line" ] || printf 'repeat=%s %s\n' "$rep" "$ring_line" | tee -a "$OUTDIR/results.log"
	if [ -n "$LIVE_MIGRATION_CONTROL_ADDR" ] && \
		[ "$rep" -eq "$LIVE_MIGRATION_CUTOVER_AFTER_REPEAT" ]; then
		log "waiting for base copy, draining the edge HWM, and cutting over after repeat $rep"
		wait_for_live_migration_base
		drive_live_migration_cutover
	fi
	if [ -n "$DIRECT_MIGRATION_CONTROL_SOCKET" ] && \
		[ "$rep" -eq "$DIRECT_MIGRATION_AFTER_REPEAT" ]; then
		log "copying source directly into the destination and switching the userspace owner route after repeat $rep"
		sudo -n timeout "$LIVE_MIGRATION_READY_TIMEOUT_SECONDS" \
			"$DIRECT_MIGRATECTL_BIN" "$DIRECT_MIGRATION_CONTROL_SOCKET" migrate \
			"$DIRECT_MIGRATION_EPOCH" "$DIRECT_MIGRATION_VOLUME_BYTES" \
			"$DIRECT_MIGRATION_CHUNK_BYTES" "$DIRECT_MIGRATION_GRANULE_BYTES" | \
			tee "$OUTDIR/direct-migration-control.log"
		grep -q '^OK active_destination=true ' "$OUTDIR/direct-migration-control.log" || \
			die "direct migration did not activate its destination"
		grep -q 'foreground_hops=1 foreground_payload_rebuffer_copies=0' \
			"$OUTDIR/direct-migration-control.log" || \
			die "direct migration reintroduced a foreground proxy or payload copy"
		grep -q 'copy_payload_userspace_buffers=0 copy_method=Splice' \
			"$OUTDIR/direct-migration-control.log" || \
			die "direct migration did not retain the socket-pipe-socket splice path"
	fi
done

if [ "$CONTINUITY_PROOF" = 1 ]; then
	log "stopping continuity proof after destination activation and verifying its final HWM"
	safe_stop_continuity || die "continuity proof did not stop cleanly"
	wait "$continuity_job_pid" || die "continuity proof reported a data or identity failure"
	continuity_job_pid=""
	continuity_pid=""
	grep 'ZCNBLK_EDGE_CONTINUITY_PASS' "$OUTDIR/continuity.log" | tee -a "$OUTDIR/results.log"
	grep -q 'identity_stable=true open_descriptor_replaced=false .* mismatches=0 ' \
		"$OUTDIR/continuity.log" || die "continuity proof did not prove stable identity and exact data"
fi
if [ -n "$DIRECT_MIGRATION_CONTROL_SOCKET" ]; then
	grep -q '^zcnblk-shm-target-direct-route-cutover: .*foreground_hops=1 payload_rebuffer_copies=0 client_block_reconnect=false$' \
		"$OUTDIR/target.log" || \
		die "target did not prove an exact direct-route cutover"
fi

if [ "$APP_ARENA_BUFFERS" = 1 ]; then
	sudo -n cat /sys/kernel/debug/zcnblk/state | tee "$OUTDIR/kernel-arena-final.log"
	expected_alias_writes="$(awk '{ for (i=1; i<=NF; i++) if ($i ~ /^writes=/) { split($i,a,"="); total+=a[2] } } END { print total+0 }' "$OUTDIR/results.log")"
	expected_alias_reads="$(awk '{ for (i=1; i<=NF; i++) if ($i ~ /^reads=/) { split($i,a,"="); total+=a[2] } } END { print total+0 }' "$OUTDIR/results.log")"
	alias_counter() {
		local name=$1
		awk -v name="$name" '{ for (i=1; i<=NF; i++) if ($i ~ ("^" name "=")) { split($i,a,"="); print a[2]; exit } }' "$OUTDIR/kernel-arena-final.log"
	}
	actual_alias_writes="$(alias_counter bio_alias_writes)"
	actual_alias_reads="$(alias_counter bio_alias_reads)"
	alias_fallbacks="$(alias_counter bio_alias_busy_fallbacks)"
	alias_retries="$(alias_counter bio_alias_required_retries)"
	alias_rejects="$(alias_counter bio_alias_required_rejects)"
	[ "$actual_alias_writes" = "$expected_alias_writes" ] || \
		die "arena write alias count $actual_alias_writes does not match completed writes $expected_alias_writes"
	[ "$actual_alias_reads" = "$expected_alias_reads" ] || \
		die "arena read alias count $actual_alias_reads does not match completed reads $expected_alias_reads"
	[ "$alias_fallbacks" = 0 ] || die "arena alias path recorded $alias_fallbacks copy fallbacks"
	[ "$alias_retries" = 0 ] || die "arena alias path recorded $alias_retries required-slot retries"
	[ "$alias_rejects" = 0 ] || die "arena alias path recorded $alias_rejects rejected mismatches"
	printf 'arena_alias_validation=pass writes=%s reads=%s copy_fallbacks=0 required_slot_retries=0 rejected_mismatches=0\n' \
		"$actual_alias_writes" "$actual_alias_reads" | tee -a "$OUTDIR/results.log"
fi

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
mean_iops="$(awk '{ for (i=1; i<=NF; i++) if ($i ~ /^mean_iops=/) { split($i,a,"="); print a[2]; exit } }' "$OUTDIR/summary.log")"
[ -n "$mean_iops" ] || die "summary has no mean_iops field"
awk -v actual="$mean_iops" -v minimum="$MIN_MEAN_IOPS" \
	'BEGIN { exit !(actual + 0 >= minimum + 0) }' || \
	die "mean IOPS $mean_iops is below required $MIN_MEAN_IOPS"
target_summary="$(grep 'zcnblk-shm-target-summary:' "$OUTDIR/target.log" | tail -n 1)"
[ -n "$target_summary" ] || die "target produced no final summary"
printf '%s\n' "$target_summary" | tee -a "$OUTDIR/summary.log"
if [ "$CONTINUITY_PROOF" = 1 ]; then
	awk '
		{
			for (i=1; i<=NF; i++) {
				if ($i ~ /^reads=/) { split($i,a,"="); reads=a[2]+0 }
				if ($i ~ /^dirty_read_hits=/) { split($i,a,"="); dirty=a[2]+0 }
				if ($i ~ /^syncs=/) { split($i,a,"="); syncs=a[2]+0 }
			}
		}
		END { exit !(reads > 0 && dirty > 0 && syncs > 1) }
	' <<<"$target_summary" || \
		die "continuity proof did not exercise dirty look-aside reads and repeated global HWM drains"
	printf 'continuity_cache_evidence=pass dirty_overlay_reads=true repeated_global_hwm_drains=true route_epoch_fence=required-by-parent-harness\n' | \
		tee -a "$OUTDIR/summary.log"
fi
grep 'zcnblk-shm-target-ofi-rma-queue:' "$OUTDIR/target.log" | tee -a "$OUTDIR/summary.log" || true
if [ "$SHM_OFI_RMA_READS" = 1 ] && [ "$OFI_SELECTIVE_COMPLETION" = 1 ] &&
	[ "$OFI_RMA_READ_COMPLETION_STRIDE" -gt 1 ] && [ "$OFI_RMA_DEFER_TAIL_COMPLETION" = 1 ]; then
	grep -Eq 'rma_read_forced_markers=[1-9][0-9]*' "$OUTDIR/target.log" || \
		die "deferred-tail run emitted no forced real-read completion marker"
	awk '
		/zcofi-endpoint-stats:/ {
			posts = markers = fast = -1
			for (i = 1; i <= NF; i++) {
				split($i, field, "=")
				if (field[1] == "read_posts") posts = field[2] + 0
				if (field[1] == "rma_read_marker_posts") markers = field[2] + 0
				if (field[1] == "rma_read_unsignaled_fast_posts") fast = field[2] + 0
			}
			if (posts > 0) {
				seen++
				if (markers <= 0 || fast <= 0 || markers + fast != posts) bad = 1
			}
		}
		END { exit seen == 0 || bad }
	' "$OUTDIR/target.log" || \
		die "selective RMA read accounting did not prove marker plus unsignaled fast posts"
	if [ "$OFI_RMA_READ_MORE" = 1 ]; then
		grep -Eq 'rma_read_more_posts=[1-9][0-9]*' "$OUTDIR/target.log" || \
			die "RMA read FI_MORE batching was requested but no staged posts were reported"
	fi
	if grep -Eq 'rma_read_flush_posts=[1-9][0-9]*' "$OUTDIR/target.log"; then
		die "deferred-tail run posted a synthetic RMA read flush"
	fi
	if grep -Eq 'rma_read_markers_inflight=[1-9][0-9]*' "$OUTDIR/target.log"; then
		die "deferred-tail run stopped with completion markers still in flight"
	fi
	printf 'rma_read_completion_gate=pass marker_source=real-read unsignaled_entry=%s marker_entry=fi_readmsg accounting=exact synthetic_flush_posts=0 marker_inflight=0\n' \
		"$([ "$OFI_RMA_READ_MORE" = 1 ] && printf 'fi_readmsg+FI_MORE' || printf fi_read)" | \
		tee -a "$OUTDIR/summary.log"
fi
if [ "$SHM_OFI_RMA_READS" = 1 ] && [ "$MIN_MEAN_IOPS" -ge 12000000 ]; then
	awk '
		/zcofi-endpoint-profile:/ {
			selective = provider = fabric = direct = emulated = more = -1
			for (i = 1; i <= NF; i++) {
				split($i, field, "=")
				if (field[1] == "selective_completion") selective = field[2] + 0
				if (field[1] == "provider") provider = field[2]
				if (field[1] == "fabric") fabric = field[2]
				if (field[1] == "efa_direct") direct = field[2] + 0
				if (field[1] == "efa_emulated_read") emulated = field[2] + 0
				if (field[1] == "rma_read_more") more = field[2] + 0
			}
			if (selective == 1) {
				seen++
				if (provider != "efa" || fabric != "efa-direct" || direct != 1 ||
				    emulated != 0 || more != 1) bad = 1
			}
		}
		END { exit seen == 0 || bad }
	' "$OUTDIR/target.log" || \
		die "12M record did not prove EFA-direct device RMA reads with FI_MORE on every data endpoint"
	printf 'record_12m_rma_read_gate=pass provider=efa fabric=efa-direct device_rdma=1 emulated_read=0 selective_completion=1 fi_more=1 per_rep_floor=12000000 mean_floor=12000000\n' | \
		tee -a "$OUTDIR/summary.log"
fi
target_summary="$(grep 'zcnblk-shm-target-summary:' "$OUTDIR/target.log" | tail -n 1)"
if [ "$ORDER_SMOKE_PAIRS" -gt 0 ]; then
	grep -Eq 'syncs=[1-9][0-9]*' <<<"$target_summary" || \
		die "order smoke completed without a target sync"
fi
if [ -n "$CONTRACT_SMOKE_BLOCK" ]; then
	grep -Eq 'fua_requests=[1-9][0-9]*' <<<"$target_summary" || \
		die "contract smoke completed without a native target FUA"
fi
if [ "$BLOCK_FUA_WRITES" = 1 ]; then
	grep -Eq 'fua_requests=[1-9][0-9]*' <<<"$target_summary" || \
		die "FUA benchmark completed without a native target FUA"
fi
if [ "$START_LOCAL_LEAF" = 1 ]; then
	grep 'zcnblk-shm-target-remote-leaf-summary:' "$OUTDIR/target.log" | tee -a "$OUTDIR/summary.log"
	grep 'zcnblk-wal-leaf-summary:' "$OUTDIR/leaf.log" | tee -a "$OUTDIR/summary.log"
fi
printf 'artifact=%s\n' "$OUTDIR" | tee -a "$OUTDIR/summary.log"
