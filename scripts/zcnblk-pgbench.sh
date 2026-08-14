#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${OUTDIR:-$ROOT/bench-results/zcnblk-pgbench-$(date -u +%Y%m%dT%H%M%SZ)}"
SCALE="${SCALE:-1000}"
CLIENTS="${CLIENTS:-256}"
JOBS="${JOBS:-16}"
DURATION="${DURATION:-20}"
REPEATS="${REPEATS:-3}"
WARMUP_SECONDS="${WARMUP_SECONDS:-0}"
PGBENCH_BUILTIN="${PGBENCH_BUILTIN:-tpcb-like}"
TRACK_WAL_IO_TIMING="${TRACK_WAL_IO_TIMING:-off}"
VECTOR_HWM="${VECTOR_HWM:-1}"
ORDERING_EPOCHS="${ORDERING_EPOCHS:-$VECTOR_HWM}"
WAL_DEBUG_STATE="${WAL_DEBUG_STATE:-0}"
SIZE_MIB="${SIZE_MIB:-65536}"
LEAF_SIZE="${LEAF_SIZE:-64G}"
PORT="${PORT:-55432}"
LEAF_PORT="${LEAF_PORT:-29000}"
LEAF_HOST="${LEAF_HOST:-127.0.0.1}"
LEAF_SOURCE_ADDR="${LEAF_SOURCE_ADDR:-}"
START_LOCAL_LEAF="${START_LOCAL_LEAF:-1}"
EXTERNAL_LEAF_TOPOLOGY_ARTIFACT="${EXTERNAL_LEAF_TOPOLOGY_ARTIFACT:-}"
WAL_TRANSPORT="${WAL_TRANSPORT:-tcp}"
OFI_PROVIDER="${OFI_PROVIDER:-efa}"
OFI_ENDPOINT="${OFI_ENDPOINT:-rdm}"
OFI_DOMAIN="${OFI_DOMAIN:-${URING_PLAY_OFI_DOMAIN:-}}"
OFI_CONTROL_PORT_OFFSET="${OFI_CONTROL_PORT_OFFSET:-1000}"
OFI_CQ_SLEEP_NS="${OFI_CQ_SLEEP_NS:-0}"
OFI_MESSAGE_BYTES="${OFI_MESSAGE_BYTES:-1048576}"
OFI_RMA_READS="${OFI_RMA_READS:-0}"
OFI_RMA_WRITE_QD="${OFI_RMA_WRITE_QD:-16}"
OFI_RMA_WRITE_MIN_QD="${OFI_RMA_WRITE_MIN_QD:-16}"
OFI_RMA_DELIVERY_COMPLETE="${OFI_RMA_DELIVERY_COMPLETE:-1}"
OFI_RMA_WRITE_OWNER_MODE="${OFI_RMA_WRITE_OWNER_MODE:-placement}"
OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED="${OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED:-0}"
OFI_HUGETLB_CONFIRMED="${OFI_HUGETLB_CONFIRMED:-0}"
OFI_RMA_SOURCE_HUGETLB_CONFIRMED="${OFI_RMA_SOURCE_HUGETLB_CONFIRMED:-0}"
LEAF_ZCMEM_HUGETLB="${LEAF_ZCMEM_HUGETLB:-0}"
LANES="${LANES:-2}"
KERNEL_QUEUES="${KERNEL_QUEUES:-$LANES}"
TARGET_CPU_LIST="${TARGET_CPU_LIST:-1,9}"
KTHREAD_CPU_LIST="${KTHREAD_CPU_LIST:-2,10}"
LEAF_CPU_LIST="${LEAF_CPU_LIST:-3,11}"
OWNER_INGRESS="${OWNER_INGRESS:-1}"
OWNER_COUNT="${OWNER_COUNT:-$LANES}"
OWNER_CPU_LIST="${OWNER_CPU_LIST:-18,26}"
OWNER_PIPELINE_BATCHES="${OWNER_PIPELINE_BATCHES:-1}"
POSTGRES_CPU_LIST="${POSTGRES_CPU_LIST:-4-7,12-15,20-23,28-31}"
PGBENCH_CPU_LIST="${PGBENCH_CPU_LIST:-0,8,16,24}"
SYNC_COORDINATOR_CPU="${SYNC_COORDINATOR_CPU:-17}"
MAX_CONNECTIONS="${MAX_CONNECTIONS:-420}"
SHARED_BUFFERS="${SHARED_BUFFERS:-4GB}"
SECTOR_ORDER_SLOTS="${SECTOR_ORDER_SLOTS:-4194304}"

COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
COORDINATION_SCOPE="${COORDINATION_SCOPE:-shared-host}"
BOOTSTRAP_MANIFEST="${ZCUTILS_BOOTSTRAP_MANIFEST:-$HOME/.local/state/zcutils/adhoc-bootstrap.env}"
MODULE="$ROOT/kmods/zcnblk_client_mod.ko"
TARGET="$ROOT/target/release/zcnblk-shm-target"
LEAF="$ROOT/target/release/zcnblk-wal-leaf"
if [ -z "${PGBIN:-}" ] && command -v pg_config >/dev/null 2>&1; then
	PGBIN="$(pg_config --bindir)"
fi
PGBIN="${PGBIN:-/usr/lib/postgresql/17/bin}"
MOUNTPOINT=/mnt/zc-pgbench-hwm
SOCKET_DIR=/tmp/zc-pgbench-hwm-socket
DATA_DIR="$MOUNTPOINT/data"

block_token=
perf_token=
leaf_pid=
target_job_pid=
target_pid=
postgres_started=0
mounted=0
kernel_pids=()

die() { printf 'zcnblk-pgbench: ERROR: %s\n' "$*" >&2; exit 1; }
token_from_result() { sed -n 's/.* token=\([^ ]*\).*/\1/p' <<<"$1"; }
env_true() {
	case "${1:-}" in
		1 | true | TRUE | yes | YES | on | ON) return 0 ;;
		*) return 1 ;;
	esac
}

case "$WAL_TRANSPORT" in
	tcp)
		OFI_RMA_WRITES="${OFI_RMA_WRITES:-0}"
		OFI_RMA_WRITES_REQUIRED="${OFI_RMA_WRITES_REQUIRED:-0}"
		;;
	ofi)
		OFI_RMA_WRITES="${OFI_RMA_WRITES:-1}"
		OFI_RMA_WRITES_REQUIRED="${OFI_RMA_WRITES_REQUIRED:-1}"
		;;
	*) die 'WAL_TRANSPORT must be tcp or ofi' ;;
esac
if [ "$WAL_TRANSPORT" = tcp ] && env_true "$OFI_RMA_WRITES"; then
	die 'OFI_RMA_WRITES cannot be enabled for the TCP transport'
fi
if [ "$WAL_TRANSPORT" = ofi ] && [ -n "$LEAF_SOURCE_ADDR" ]; then
	die 'LEAF_SOURCE_ADDR is TCP-only; select EFA locality with OFI_DOMAIN'
fi
if env_true "$OFI_RMA_WRITES"; then
	env_true "$OWNER_INGRESS" || die 'RMA writes require OWNER_INGRESS=1'
	[ "$OWNER_PIPELINE_BATCHES" -eq 1 ] || die 'RMA writes require OWNER_PIPELINE_BATCHES=1'
	env_true "$OFI_RMA_DELIVERY_COMPLETE" || die 'RMA writes require OFI_RMA_DELIVERY_COMPLETE=1'
	env_true "$OFI_RMA_WRITES_REQUIRED" || die 'RMA benchmark runs require OFI_RMA_WRITES_REQUIRED=1'
fi
[[ "$OFI_RMA_WRITE_QD" =~ ^[0-9]+$ ]] && [ "$OFI_RMA_WRITE_QD" -gt 0 ] && \
	[ "$OFI_RMA_WRITE_QD" -le 1024 ] || die 'OFI_RMA_WRITE_QD must be in 1..=1024'
[[ "$OFI_RMA_WRITE_MIN_QD" =~ ^[0-9]+$ ]] && [ "$OFI_RMA_WRITE_MIN_QD" -gt 0 ] && \
	[ "$OFI_RMA_WRITE_MIN_QD" -le 1024 ] || die 'OFI_RMA_WRITE_MIN_QD must be in 1..=1024'
[ "$OFI_RMA_WRITE_OWNER_MODE" = placement ] || \
	die 'the topology-matched PostgreSQL harness supports OFI_RMA_WRITE_OWNER_MODE=placement only'
[[ "$OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED" =~ ^[01]$ ]] || \
	die 'OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED must be zero or one'
[[ "$LANES" =~ ^[0-9]+$ ]] && [ "$LANES" -gt 0 ] || die 'LANES must be a positive integer'
[ "$KERNEL_QUEUES" -eq "$LANES" ] || die 'KERNEL_QUEUES must equal LANES'
[ "$OWNER_COUNT" -eq "$LANES" ] || die 'OWNER_COUNT must equal LANES'
[ "$REPEATS" -ge 3 ] || die 'representative PostgreSQL transport comparisons require REPEATS>=3'
IFS=, read -r -a target_cpus <<<"$TARGET_CPU_LIST"
IFS=, read -r -a kthread_cpus <<<"$KTHREAD_CPU_LIST"
IFS=, read -r -a leaf_cpus <<<"$LEAF_CPU_LIST"
IFS=, read -r -a owner_cpus <<<"$OWNER_CPU_LIST"
[ "${#target_cpus[@]}" -eq "$LANES" ] || die 'TARGET_CPU_LIST must name exactly one individual CPU per lane'
[ "${#kthread_cpus[@]}" -eq "$LANES" ] || die 'KTHREAD_CPU_LIST must name exactly one individual CPU per lane'
[ "${#leaf_cpus[@]}" -eq "$LANES" ] || die 'LEAF_CPU_LIST must name exactly one individual CPU per lane'
[ "${#owner_cpus[@]}" -eq "$LANES" ] || die 'OWNER_CPU_LIST must name exactly one individual CPU per lane'

cpu_lists_intersect() {
	local first="$1" second="$2"
	awk -v first="$first" -v second="$second" '
		function add_first(value, pieces, range, count, i, cpu) {
			gsub(/[[:space:]]/, "", value)
			count = split(value, pieces, ",")
			for (i = 1; i <= count; i++) {
				split(pieces[i], range, "-")
				for (cpu = range[1]; cpu <= (range[2] == "" ? range[1] : range[2]); cpu++)
					first_cpus[cpu] = 1
			}
		}
		function has_intersection(value, pieces, range, count, i, cpu) {
			gsub(/[[:space:]]/, "", value)
			count = split(value, pieces, ",")
			for (i = 1; i <= count; i++) {
				split(pieces[i], range, "-")
				for (cpu = range[1]; cpu <= (range[2] == "" ? range[1] : range[2]); cpu++)
					if (cpu in first_cpus) return 1
			}
			return 0
		}
		BEGIN { add_first(first); exit !has_intersection(second) }
	'
}

stop_exact() {
	local pid="$1" expected="$2" signal="$3" actual
	[ -n "$pid" ] && [ -r "/proc/$pid/comm" ] || return 0
	actual="$(cat "/proc/$pid/comm")"
	[ "$actual" = "$expected" ] || die "refusing signal: pid=$pid expected=$expected actual=$actual"
	sudo -n kill "-$signal" "$pid"
}

snapshot_contexts() {
	local output="$1" pid status
	: >"$output"
	for pid in "$target_pid" "$leaf_pid" "${kernel_pids[@]}"; do
		[ -n "$pid" ] || continue
		if [ -d "/proc/$pid/task" ]; then
			for status in /proc/"$pid"/task/*/status; do
				[ -r "$status" ] || continue
				awk '
					/^Pid:/ { pid=$2 }
					/^Name:/ { name=$2 }
					/^voluntary_ctxt_switches:/ { voluntary=$2 }
					/^nonvoluntary_ctxt_switches:/ { involuntary=$2 }
					END { printf "%s %s %d %d\n", pid, name, voluntary+0, involuntary+0 }
				' "$status" >>"$output"
			done
		fi
	done
}

cleanup() {
	local status=$?
	trap - EXIT INT TERM
	set +e
	if [ "$postgres_started" = 1 ] && [ -d "$DATA_DIR" ]; then
		sudo -n -u postgres "$PGBIN/pg_ctl" -D "$DATA_DIR" -m fast -w stop >>"$OUTDIR/cleanup.log" 2>&1
	fi
	[ "$mounted" = 0 ] || sudo -n umount "$MOUNTPOINT" >>"$OUTDIR/cleanup.log" 2>&1
	stop_exact "$target_pid" zcnblk-shm-targ INT >>"$OUTDIR/cleanup.log" 2>&1
	[ -z "$target_job_pid" ] || wait "$target_job_pid" 2>/dev/null
	if [ "$START_LOCAL_LEAF" = 1 ]; then
		stop_exact "$leaf_pid" zcnblk-wal-leaf TERM >>"$OUTDIR/cleanup.log" 2>&1
		[ -z "$leaf_pid" ] || wait "$leaf_pid" 2>/dev/null
	fi
	grep -q '^zcnblk_client_mod ' /proc/modules 2>/dev/null && sudo -n rmmod zcnblk_client_mod >>"$OUTDIR/cleanup.log" 2>&1
	sudo -n rm -rf "$MOUNTPOINT" "$SOCKET_DIR"
	[ -z "$perf_token" ] || "$COORD_BIN" release "$perf_token" >>"$OUTDIR/coordination.log" 2>&1
	[ -z "$block_token" ] || "$COORD_BIN" release "$block_token" >>"$OUTDIR/coordination.log" 2>&1
	exit "$status"
}
trap cleanup EXIT INT TERM

command -v sudo >/dev/null || die 'sudo is required'
sudo -n true || die 'passwordless sudo is required'
[ ! -e /dev/zcnblk0 ] || die '/dev/zcnblk0 already exists'
mkdir -p "$OUTDIR"

coord_honored=false
case "$COORDINATION_SCOPE" in
	shared-host)
		[ -x "$COORD_BIN" ] || die "agent-coord not found at $COORD_BIN"
		block_result="$($COORD_BIN request --owner codex:zcutils-pgbench-hwm --mode exclusive \
			--sensitivity high --priority 65 --ttl 3600 --resource 'block=zcnblk0' \
			--note 'durable PostgreSQL over placement-free zcnblk edge')"
		printf '%s\n' "$block_result" | tee -a "$OUTDIR/coordination.log"
		block_token="$(token_from_result "$block_result")"

		perf_result="$($COORD_BIN request --owner codex:zcutils-pgbench-hwm --mode soft-exclusive \
			--sensitivity critical --priority 65 --ttl 3600 \
			--resource "cpu=0-31;memory-bandwidth=*;nic=*;port=$LEAF_PORT-$((LEAF_PORT + LANES - 1)),$((LEAF_PORT + OFI_CONTROL_PORT_OFFSET))-$((LEAF_PORT + OFI_CONTROL_PORT_OFFSET + LANES - 1)),$PORT" \
			--note "$LANES-lane topology-explicit durable PostgreSQL benchmark")"
		printf '%s\n' "$perf_result" | tee -a "$OUTDIR/coordination.log"
		perf_token="$(token_from_result "$perf_result")"
		grep -q ' honored=true ' <<<"$perf_result" && coord_honored=true
		;;
	dedicated-adhoc)
		[ -r "$BOOTSTRAP_MANIFEST" ] || die "dedicated adhoc coordination requires bootstrap manifest: $BOOTSTRAP_MANIFEST"
		grep -qx 'coordination_scope=dedicated-adhoc-instance' "$BOOTSTRAP_MANIFEST" || \
			die 'bootstrap manifest does not prove dedicated adhoc ownership'
		grep -qx 'coordination_honored=true' "$BOOTSTRAP_MANIFEST" || \
			die 'bootstrap manifest does not honor dedicated coordination'
		if grep -q '^cloud_provider=' "$BOOTSTRAP_MANIFEST"; then
			grep -Eq '^cloud_provider=(ec2|gce)$' "$BOOTSTRAP_MANIFEST" || \
				die 'bootstrap manifest does not identify a supported cloud provider'
			grep -Eq '^instance_id=(i-[0-9a-f]+|[0-9]+)$' "$BOOTSTRAP_MANIFEST" || \
				die 'bootstrap manifest does not identify an EC2 or GCE instance'
		else
			grep -Eq '^instance_id=i-[0-9a-f]+$' "$BOOTSTRAP_MANIFEST" || \
				die 'legacy bootstrap manifest does not identify an EC2 instance'
		fi
		printf 'scope=dedicated-adhoc honored=true manifest=%s\n' "$BOOTSTRAP_MANIFEST" | \
			tee -a "$OUTDIR/coordination.log"
		coord_honored=true
		;;
	*)
		die 'COORDINATION_SCOPE must be shared-host or dedicated-adhoc'
		;;
esac

hugepages_total="$(awk '/HugePages_Total:/{print $2}' /proc/meminfo)"
memlock_kib="$(ulimit -l)"
topology_representative=1
preflight_warnings=0
: >"$OUTDIR/preflight.log"
warn_preflight() {
	printf 'zcnblk-pgbench: WARNING: %s\n' "$*" | tee -a "$OUTDIR/preflight.log" >&2
	preflight_warnings=$((preflight_warnings + 1))
	topology_representative=0
}
if [ "$coord_honored" != true ]; then
	topology_representative=0
	warn_preflight 'CPU/memory-bandwidth soft exclusivity was not honored; results are shared-system measurements and must be repeated.'
fi
if [ "$hugepages_total" -eq 0 ]; then
	warn_preflight 'HugeTLB has no configured pages; this run cannot validate a HugeTLB-backed fast path.'
fi
if [ "$memlock_kib" != unlimited ] && [ "$memlock_kib" -lt 1048576 ]; then
	warn_preflight "memlock headroom is only ${memlock_kib} KiB; registered/fixed-buffer fast paths need a larger limit."
fi
if [ "$WAL_TRANSPORT" = ofi ]; then
	[ "$OFI_CQ_SLEEP_NS" -eq 0 ] || warn_preflight "OFI CQ polling sleeps for ${OFI_CQ_SLEEP_NS} ns; low-latency transport comparison requires zero."
	if [ "$OFI_PROVIDER" = efa ] && [ -z "$OFI_DOMAIN" ]; then
		warn_preflight 'EFA domain/NIC mapping is implicit; set OFI_DOMAIN before treating results as representative.'
	fi
	env_true "$OFI_HUGETLB_CONFIRMED" || warn_preflight 'OFI registered-buffer HugeTLB policy is not confirmed.'
	if env_true "$OFI_RMA_WRITES" && ! env_true "$OFI_RMA_SOURCE_HUGETLB_CONFIRMED"; then
		warn_preflight 'The registered RMA source is the vmalloc_user/remap_vmalloc_range shared arena, not an explicit HugeTLB mapping; reserve pages for the leaf, but do not classify this client source path as HugeTLB-backed.'
	fi
	if [ "$START_LOCAL_LEAF" = 1 ] && ! env_true "$LEAF_ZCMEM_HUGETLB"; then
		warn_preflight 'The local RMA leaf window is not MAP_HUGETLB-backed.'
	fi
fi
external_leaf_cpu_map=
external_leaf_nic_map=
if [ "$START_LOCAL_LEAF" != 1 ]; then
	if [ -z "$EXTERNAL_LEAF_TOPOLOGY_ARTIFACT" ] || [ ! -r "$EXTERNAL_LEAF_TOPOLOGY_ARTIFACT" ]; then
		warn_preflight 'External leaf topology evidence is missing; set EXTERNAL_LEAF_TOPOLOGY_ARTIFACT to the copied leaf-host artifact.'
	else
		cp "$EXTERNAL_LEAF_TOPOLOGY_ARTIFACT" "$OUTDIR/external-leaf-topology.log"
		external_leaf_cpu_map="$(sed -n 's/^lane_to_worker_cpu=//p' "$OUTDIR/external-leaf-topology.log" | head -n 1)"
		external_leaf_nic_map="$(sed -n 's/^lane_to_nic=//p' "$OUTDIR/external-leaf-topology.log" | head -n 1)"
		[ -n "$external_leaf_cpu_map" ] || warn_preflight 'External leaf topology artifact lacks lane_to_worker_cpu mapping.'
		if [ "$WAL_TRANSPORT" = ofi ] && [ -z "$external_leaf_nic_map" ]; then
			warn_preflight 'External EFA leaf topology artifact lacks lane_to_nic mapping.'
		fi
	fi
fi
if ! env_true "$OWNER_INGRESS"; then
	warn_preflight 'Stable userspace owner ingress is disabled; this run is not topology-matched to the RMA path.'
fi
if [ "$OWNER_PIPELINE_BATCHES" -ne 1 ]; then
	warn_preflight 'Owner pipeline depth is not one; completion semantics are not matched to the RMA overwrite-safety contract.'
fi
if env_true "$OFI_RMA_WRITES" && [ "$OFI_RMA_WRITE_QD" -lt "$OFI_RMA_WRITE_MIN_QD" ]; then
	warn_preflight "RMA payload-operation QD $OFI_RMA_WRITE_QD is below the delivery-complete floor $OFI_RMA_WRITE_MIN_QD; random records in one PostgreSQL writeback batch will serialize completion waves."
fi
if env_true "$OFI_RMA_WRITES" && [ "$OFI_PROVIDER" = efa ] && [ "$OWNER_COUNT" -gt 1 ] && \
	[ "$OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED" != 1 ]; then
	warn_preflight "EFA RMA writes use $OWNER_COUNT stable-owner endpoints on one configured OFI domain; explicitly set OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED=1 only when this matched multi-endpoint placement topology is intentional."
fi
if [ "$preflight_warnings" -ne 0 ] &&
	(env_true "${URING_PLAY_TOPOLOGY_STRICT:-0}" || env_true "${URING_PLAY_TOPOLOGY_FATAL:-0}"); then
	die 'strict topology preflight rejected this benchmark before representative numbers were printed'
fi

sudo -n insmod "$MODULE" transport=shm lanes="$LANES" connections_per_lane=1 \
	size_mib="$SIZE_MIB" queues="$KERNEL_QUEUES" queue_depth=256 shm_sector_order_slots="$SECTOR_ORDER_SLOTS" \
	max_frame_bytes=4096 pipeline_depth=128 shm_ring_entries=512 \
	shm_payload_entries=8192 shm_poll_us=1000 shm_ordering_epochs="$ORDERING_EPOCHS" pin_threads=0

declare -a postgres_connection_hctxs=()
for ((connection = 0; connection < LANES; connection++)); do
	postgres_connection_hctxs+=("")
done
for hctx_cpu_file in /sys/block/zcnblk0/mq/*/cpu_list; do
	[ -r "$hctx_cpu_file" ] || die 'zcnblk0 did not expose an hctx CPU map'
	hctx="${hctx_cpu_file%/cpu_list}"
	hctx="${hctx##*/}"
	if cpu_lists_intersect "$POSTGRES_CPU_LIST" "$(cat "$hctx_cpu_file")"; then
		connection=$((hctx % LANES))
		postgres_connection_hctxs[$connection]="${postgres_connection_hctxs[$connection]}${postgres_connection_hctxs[$connection]:+,}$hctx"
	fi
done
for ((connection = 0; connection < LANES; connection++)); do
	if [ -z "${postgres_connection_hctxs[$connection]}" ]; then
		topology_representative=0
		warn_preflight "PostgreSQL CPU list $POSTGRES_CPU_LIST reaches no hctx mapped to connection $connection (hctx modulo $LANES)."
	fi
done
if [ "$preflight_warnings" -ne 0 ] &&
	(env_true "${URING_PLAY_TOPOLOGY_STRICT:-0}" || env_true "${URING_PLAY_TOPOLOGY_FATAL:-0}"); then
	die 'strict topology preflight rejected this benchmark before representative numbers were printed'
fi

sudo -n rm -rf "$MOUNTPOINT" "$SOCKET_DIR"
sudo -n mkdir -p "$MOUNTPOINT" "$SOCKET_DIR"
sudo -n chown postgres:postgres "$SOCKET_DIR"

if [ "$START_LOCAL_LEAF" = 1 ]; then
	env URING_PLAY_PIN_CPU_LIST="$LEAF_CPU_LIST" URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT="$WAL_TRANSPORT" \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER="$OFI_PROVIDER" \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT="$OFI_ENDPOINT" \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS="$OFI_RMA_READS" \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_WRITES="$OFI_RMA_WRITES" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB="$LEAF_ZCMEM_HUGETLB" \
		URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED="$OFI_HUGETLB_CONFIRMED" \
		URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES="$OFI_MESSAGE_BYTES" \
		URING_PLAY_OFI_DOMAIN="$OFI_DOMAIN" \
		URING_PLAY_OFI_CONTROL_PORT_OFFSET="$OFI_CONTROL_PORT_OFFSET" \
		URING_PLAY_OFI_CQ_SLEEP_NS="$OFI_CQ_SLEEP_NS" \
		URING_PLAY_OFI_RMA_WRITE_QD="$OFI_RMA_WRITE_QD" \
		URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE="$OFI_RMA_DELIVERY_COMPLETE" \
		FI_EFA_USE_DEVICE_RDMA=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
		"$LEAF" "zcmem:$LEAF_SIZE" "$LEAF_HOST" "$LEAF_PORT" "$LANES" 1 4096 "$LANES" true blocking \
		>"$OUTDIR/leaf.log" 2>&1 &
	leaf_pid=$!
	for _ in $(seq 1 200); do
		if [ "$WAL_TRANSPORT" = ofi ]; then
			ready_port="$((LEAF_PORT + OFI_CONTROL_PORT_OFFSET))"
		else
			ready_port="$LEAF_PORT"
		fi
		listeners="$(ss -H -ltn | awk -v base="$ready_port" -v lanes="$LANES" '
			{
				address = $4
				sub(/^.*:/, "", address)
				port = address + 0
				if (port >= base && port < base + lanes) count++
			}
			END { print count + 0 }
		')"
		[ "$listeners" -eq "$LANES" ] && break
		[ -r "/proc/$leaf_pid/comm" ] || die 'leaf exited during startup'
		sleep 0.05
	done
	[ "${listeners:-0}" -eq "$LANES" ] || die 'leaf did not open every lane/control listener'
fi

sudo -n env URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE="$OUTDIR/target.pid" \
	URING_PLAY_TOPOLOGY_REPRESENTATIVE="$topology_representative" \
	URING_PLAY_ZCNBLK_SHM_COORDINATOR_CPU="$SYNC_COORDINATOR_CPU" \
	URING_PLAY_ZCNBLK_SHM_LEASE_RELEASE_BATCH=1 \
	URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH=4096 \
	URING_PLAY_ZCNBLK_SHM_READ_BATCH=512 \
	URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1 \
	URING_PLAY_ZCNBLK_SHM_VECTOR_HWM="$VECTOR_HWM" \
	URING_PLAY_ZCNBLK_SHM_WAL_DEBUG_STATE="$WAL_DEBUG_STATE" \
	URING_PLAY_ZCNBLK_SHM_WAL_OWNER_DISPATCH=0 \
	URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS="$OWNER_INGRESS" \
	URING_PLAY_ZCNBLK_SHM_OWNER_COUNT="$OWNER_COUNT" \
	URING_PLAY_ZCNBLK_SHM_OWNER_CPU_LIST="$OWNER_CPU_LIST" \
	URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_BATCHES="$OWNER_PIPELINE_BATCHES" \
	URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW=4 \
	URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS=1 \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_POLICY=adaptive \
	URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE=blocking \
	URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_RECORDS=512 \
	URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_FILL_US=20 \
	URING_PLAY_ZCNBLK_SHM_WAL_COMPACT_WRITES=1 \
	URING_PLAY_ZCNBLK_SHM_DIRTY_PRESSURE_RESERVE=0 \
	URING_PLAY_ZCNBLK_SHM_LEAF_ADDR="$LEAF_HOST:$LEAF_PORT" \
	URING_PLAY_ZCNBLK_SHM_LEAF_SOURCE_ADDR="$LEAF_SOURCE_ADDR" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT="$WAL_TRANSPORT" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER="$OFI_PROVIDER" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT="$OFI_ENDPOINT" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS="$OFI_RMA_READS" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES="$OFI_RMA_WRITES" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES_REQUIRED="$OFI_RMA_WRITES_REQUIRED" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_QD="$OFI_RMA_WRITE_QD" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_OWNER_MODE="$OFI_RMA_WRITE_OWNER_MODE" \
	URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED="$OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED" \
	URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED="$OFI_HUGETLB_CONFIRMED" \
	URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES="$OFI_MESSAGE_BYTES" \
	URING_PLAY_OFI_DOMAIN="$OFI_DOMAIN" \
	URING_PLAY_OFI_CONTROL_PORT_OFFSET="$OFI_CONTROL_PORT_OFFSET" \
	URING_PLAY_OFI_CQ_SLEEP_NS="$OFI_CQ_SLEEP_NS" \
	URING_PLAY_OFI_RMA_WRITE_QD="$OFI_RMA_WRITE_QD" \
	URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE="$OFI_RMA_DELIVERY_COMPLETE" \
	FI_EFA_USE_DEVICE_RDMA=1 \
	URING_PLAY_ROUTE_PROBE="${URING_PLAY_ROUTE_PROBE:-0}" \
	URING_PLAY_EXPECT_ROUTE_DEV="${URING_PLAY_EXPECT_ROUTE_DEV:-}" \
	URING_PLAY_EXPECT_ROUTE_SRC="${URING_PLAY_EXPECT_ROUTE_SRC:-}" \
	URING_PLAY_TOPOLOGY_STRICT="${URING_PLAY_TOPOLOGY_STRICT:-0}" \
	URING_PLAY_TOPOLOGY_FATAL="${URING_PLAY_TOPOLOGY_FATAL:-0}" \
	"$TARGET" /dev/zcnblk-shmctl wal-tcp 128 "$TARGET_CPU_LIST" 1000 1000 10000 \
	>"$OUTDIR/target.log" 2>&1 &
target_job_pid=$!
for _ in $(seq 1 200); do [ -s "$OUTDIR/target.pid" ] && break; sleep 0.05; done
[ -s "$OUTDIR/target.pid" ] || die 'target did not publish its PID'
target_pid="$(cat "$OUTDIR/target.pid")"
[ -r "/proc/$target_pid/comm" ] || die 'target exited immediately after publishing its PID'
if [ "$WAL_TRANSPORT" = ofi ]; then
	for _ in $(seq 1 200); do
		provider_ready=0
		delivery_ready=0
		windows_ready=0
		grep -q "zcofi-endpoint-profile: provider=$OFI_PROVIDER " "$OUTDIR/target.log" && provider_ready=1
		grep -q 'rma_write_delivery_complete=1' "$OUTDIR/target.log" && delivery_ready=1
		if env_true "$OFI_RMA_WRITES"; then
			rma_windows="$(grep -c '^zcnblk-shm-target-ofi-rma-write-window: lane=.* completion=initiator-delivery-cq-before-doorbell' "$OUTDIR/target.log" || true)"
			[ "$rma_windows" -eq "$OWNER_COUNT" ] && windows_ready=1
		else
			windows_ready=1
		fi
		[ "$provider_ready" -eq 1 ] && [ "$delivery_ready" -eq 1 ] && [ "$windows_ready" -eq 1 ] && break
		[ -r "/proc/$target_pid/comm" ] || die 'target exited during OFI negotiation'
		sleep 0.05
	done
	grep -q "zcofi-endpoint-profile: provider=$OFI_PROVIDER " "$OUTDIR/target.log" || \
		die "target did not report the requested OFI provider $OFI_PROVIDER"
	grep -q 'rma_write_delivery_complete=1' "$OUTDIR/target.log" || \
		die 'target OFI profile did not confirm remote-delivery RMA write completions'
	if env_true "$OFI_RMA_WRITES"; then
		rma_windows="$(grep -c '^zcnblk-shm-target-ofi-rma-write-window: lane=.* completion=initiator-delivery-cq-before-doorbell' "$OUTDIR/target.log" || true)"
		[ "$rma_windows" -eq "$OWNER_COUNT" ] || \
			die "RMA write-window negotiation covered $rma_windows lanes, expected $OWNER_COUNT"
	fi
fi

for ((lane = 0; lane < LANES; lane++)); do
	name="zcnblk-shm-$lane-0"
	pid="$(ps -e -o pid=,comm= | awk -v name="$name" '$2 == name {print $1}')"
	[ -n "$pid" ] || die "missing kernel lane thread $name"
	kernel_pids+=("$pid")
	cpu="${kthread_cpus[$lane]}"
	if ! cpu_lists_intersect "$cpu" "$(cat "/sys/block/zcnblk0/mq/$lane/cpu_list")"; then
		die "kernel lane $lane CPU $cpu is outside its hctx map ($(cat "/sys/block/zcnblk0/mq/$lane/cpu_list"))"
	fi
	sudo -n taskset -pc "$cpu" "$pid" >>"$OUTDIR/kthreads.log"
done

if [ "$START_LOCAL_LEAF" = 1 ]; then
	leaf_cpu_map=
	leaf_nic_map=
	for ((lane = 0; lane < LANES; lane++)); do
		leaf_cpu_map="${leaf_cpu_map}${leaf_cpu_map:+,}$lane:${leaf_cpus[$lane]}"
		leaf_nic_map="${leaf_nic_map}${leaf_nic_map:+,}$lane:${OFI_DOMAIN:-tcp-route}"
	done
else
	leaf_cpu_map="${external_leaf_cpu_map:-missing-see-external-leaf-topology.log}"
	leaf_nic_map="${external_leaf_nic_map:-missing-see-external-leaf-topology.log}"
fi
{
	printf 'classification=%s\ncoordination_honored=%s\n' \
		"$([ "$START_LOCAL_LEAF" = 1 ] && printf local-shared-system || printf remote-userspace-leaf)" \
		"$coord_honored"
	printf 'leaf_host=%s leaf_port=%s leaf_source_addr=%s local_leaf=%s\n' \
		"$LEAF_HOST" "$LEAF_PORT" "${LEAF_SOURCE_ADDR:-kernel-route}" "$START_LOCAL_LEAF"
	printf 'wal_transport=%s ofi_provider=%s ofi_endpoint=%s ofi_domain=%s ofi_cq_sleep_ns=%s ofi_message_bytes=%s\n' \
		"$WAL_TRANSPORT" "$OFI_PROVIDER" "$OFI_ENDPOINT" "${OFI_DOMAIN:-implicit}" "$OFI_CQ_SLEEP_NS" "$OFI_MESSAGE_BYTES"
	printf 'rma_writes=%s rma_writes_required=%s rma_write_qd_per_owner=%s rma_write_min_qd=%s rma_write_qd_scope=per-owner-payload-operations block_qd_coupled=no rma_delivery_complete=%s rma_reads=%s rma_write_owner_mode=%s multi_endpoint_confirmed=%s\n' \
		"$OFI_RMA_WRITES" "$OFI_RMA_WRITES_REQUIRED" "$OFI_RMA_WRITE_QD" "$OFI_RMA_WRITE_MIN_QD" "$OFI_RMA_DELIVERY_COMPLETE" "$OFI_RMA_READS" \
		"$OFI_RMA_WRITE_OWNER_MODE" "$OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED"
	printf 'rma_source_backing=vmalloc_user-remap_vmalloc_range rma_source_hugetlb_confirmed=%s\n' \
		"$OFI_RMA_SOURCE_HUGETLB_CONFIRMED"
	printf 'topology_representative=%s preflight_warnings=%s\n' "$topology_representative" "$preflight_warnings"
	printf 'lane_count=%s kernel_queues=%s target_cpus=%s kthread_cpus=%s leaf_cpus=%s owner_cpus=%s owner_count=%s owner_pipeline_batches=%s sector_order_slots=%s\n' \
		"$LANES" "$KERNEL_QUEUES" "$TARGET_CPU_LIST" "$KTHREAD_CPU_LIST" "$LEAF_CPU_LIST" "$OWNER_CPU_LIST" "$OWNER_COUNT" "$OWNER_PIPELINE_BATCHES" "$SECTOR_ORDER_SLOTS"
	lane_to_kthread_cpu=
	lane_to_ingress_worker_cpu=
	owner_lane_to_worker_cpu=
	for ((lane = 0; lane < LANES; lane++)); do
		lane_to_kthread_cpu="${lane_to_kthread_cpu}${lane_to_kthread_cpu:+,}$lane:${kthread_cpus[$lane]}"
		lane_to_ingress_worker_cpu="${lane_to_ingress_worker_cpu}${lane_to_ingress_worker_cpu:+,}$lane:${target_cpus[$lane]}"
		owner_lane_to_worker_cpu="${owner_lane_to_worker_cpu}${owner_lane_to_worker_cpu:+,}$lane:${owner_cpus[$lane]}"
	done
	printf 'lane_to_kthread_cpu=%s lane_to_ingress_worker_cpu=%s owner_lane_to_worker_cpu=%s leaf_lane_to_worker_cpu=%s\n' \
		"$lane_to_kthread_cpu" "$lane_to_ingress_worker_cpu" "$owner_lane_to_worker_cpu" "$leaf_cpu_map"
	printf 'leaf_lane_to_nic=%s external_leaf_topology_artifact=%s\n' \
		"$leaf_nic_map" "${EXTERNAL_LEAF_TOPOLOGY_ARTIFACT:-local-leaf}"
	printf 'placement_owner=separate-userspace-stable-extent-owner block_client_placement=no owner_ingress=%s\n' "$OWNER_INGRESS"
	for ((lane = 0; lane < LANES; lane++)); do
		printf 'lane%s_hctx=%s\n' "$lane" "$(cat "/sys/block/zcnblk0/mq/$lane/cpu_list")"
		printf 'postgres_connection%s_hctxs=%s\n' "$lane" "${postgres_connection_hctxs[$lane]}"
	done
	printf 'sync_coordinator_cpu=%s\n' "$SYNC_COORDINATOR_CPU"
	printf 'postgres_cpus=%s\npgbench_cpus=%s\n' "$POSTGRES_CPU_LIST" "$PGBENCH_CPU_LIST"
	printf 'scale=%s clients=%s jobs=%s duration=%s repeats=%s warmup_seconds=%s builtin=%s track_wal_io_timing=%s vector_hwm=%s ordering_epochs=%s max_connections=%s shared_buffers=%s\n' "$SCALE" "$CLIENTS" "$JOBS" "$DURATION" "$REPEATS" "$WARMUP_SECONDS" "$PGBENCH_BUILTIN" "$TRACK_WAL_IO_TIMING" "$VECTOR_HWM" "$ORDERING_EPOCHS" "$MAX_CONNECTIONS" "$SHARED_BUFFERS"
	if [ "$VECTOR_HWM" = 1 ]; then
		printf 'write_completion=local-dirty-lease-admission; sync_completion=remote-volatile-leaf-hwm\n'
	else
		printf 'write_completion=local-dirty-lease-admission; sync_completion=remote-volatile-global-hwm\n'
	fi
	if env_true "$OFI_RMA_WRITES"; then
		printf 'transport_write_payload_completion=initiator-delivery-cq-before-metadata-doorbell; remote_write_ack=doorbell-result-hwm; sync_fua=leaf-after-doorbell\n'
		printf 'copy_ledger=postgres-kernel-filesystem+one-block-edge-copy-to-shared-slot+registered-shared-slot-rma-direct-to-leaf-memory+metadata-doorbell-only; end_to_end_zero_copy=no\n'
	else
		printf 'transport_write_payload_completion=message-send-before-remote-result; remote_write_ack=message-result-hwm; sync_fua=leaf-after-message-payload\n'
		printf 'copy_ledger=postgres-kernel-filesystem+one-block-edge-copy-to-shared-slot+userspace-transport-message-gather+transport-provider-copy-to-leaf-memory; end_to_end_zero_copy=no\n'
	fi
	printf 'hugepages_total=%s memlock_kib=%s loadavg=%s\n' \
		"$hugepages_total" "$memlock_kib" "$(cat /proc/loadavg)"
	cat "$OUTDIR/preflight.log"
} >"$OUTDIR/topology.log"

sudo -n mkfs.ext4 -F -E nodiscard /dev/zcnblk0 >"$OUTDIR/mkfs.log" 2>&1
sudo -n mount -o noatime /dev/zcnblk0 "$MOUNTPOINT"
mounted=1
sudo -n chown postgres:postgres "$MOUNTPOINT"
sudo -n -u postgres "$PGBIN/initdb" -D "$DATA_DIR" --no-locale --encoding=UTF8 >"$OUTDIR/initdb.log" 2>&1
sudo -n -u postgres taskset -c "$POSTGRES_CPU_LIST" "$PGBIN/pg_ctl" \
	-D "$DATA_DIR" -l "$MOUNTPOINT/postgres.log" -w start -o \
	"-k $SOCKET_DIR -p $PORT -c max_connections=$MAX_CONNECTIONS -c shared_buffers=$SHARED_BUFFERS -c fsync=on -c synchronous_commit=on -c full_page_writes=on -c track_wal_io_timing=$TRACK_WAL_IO_TIMING -c checkpoint_timeout=30min -c max_wal_size=32GB -c min_wal_size=4GB" \
	>"$OUTDIR/pgctl-start.log" 2>&1
postgres_started=1
"$PGBIN/createdb" -h "$SOCKET_DIR" -p "$PORT" -U postgres pgbench

/usr/bin/time -f 'elapsed_seconds=%e' -o "$OUTDIR/init.time" \
	taskset -c "$PGBENCH_CPU_LIST" "$PGBIN/pgbench" -h "$SOCKET_DIR" -p "$PORT" \
	-U postgres -i -s "$SCALE" pgbench >"$OUTDIR/init.log" 2>&1

if [ "$WARMUP_SECONDS" -gt 0 ]; then
	taskset -c "$PGBENCH_CPU_LIST" "$PGBIN/pgbench" -h "$SOCKET_DIR" -p "$PORT" \
		-U postgres -c "$CLIENTS" -j "$JOBS" -T "$WARMUP_SECONDS" -M prepared \
		-b "$PGBENCH_BUILTIN" pgbench >"$OUTDIR/warmup.log" 2>&1
	if grep -Eq 'pgbench:.*client [0-9]+ (executing|sending|receiving|preparing)' \
		"$OUTDIR/warmup.log"; then
		die 'pgbench debug tracing contaminated the warmup; pass DBNAME positionally and do not use --debug'
	fi
fi

for rep in $(seq 1 "$REPEATS"); do
	snapshot_contexts "$OUTDIR/rep$rep.context.before"
	/usr/bin/time -f 'elapsed_seconds=%e\nuser_seconds=%U\nsystem_seconds=%S\nvoluntary_context_switches=%w\ninvoluntary_context_switches=%c' \
		-o "$OUTDIR/rep$rep.client.time" \
		taskset -c "$PGBENCH_CPU_LIST" "$PGBIN/pgbench" -h "$SOCKET_DIR" -p "$PORT" \
		-U postgres -c "$CLIENTS" -j "$JOBS" -T "$DURATION" -P 5 -r -M prepared \
		-b "$PGBENCH_BUILTIN" pgbench \
		>"$OUTDIR/rep$rep.log" 2>&1
	if grep -Eq 'pgbench:.*client [0-9]+ (executing|sending|receiving|preparing)' \
		"$OUTDIR/rep$rep.log"; then
		die 'pgbench debug tracing contaminated the benchmark; pass DBNAME positionally and do not use --debug'
	fi
	snapshot_contexts "$OUTDIR/rep$rep.context.after"
	awk '
		NR == FNR { voluntary[$1]=$3; involuntary[$1]=$4; next }
		($1 in voluntary) {
			v=$3-voluntary[$1]; iv=$4-involuntary[$1]
			printf "pid=%s name=%s voluntary=%d involuntary=%d total=%d\n", $1, $2, v, iv, v+iv
		}
	' "$OUTDIR/rep$rep.context.before" "$OUTDIR/rep$rep.context.after" \
		>"$OUTDIR/rep$rep.context.delta"
	transactions="$(awk '/number of transactions actually processed:/{print $6}' "$OUTDIR/rep$rep.log")"
	awk -v repeat="$rep" -v transactions="$transactions" '
		{
			split($5, total_field, "="); total += total_field[2]
			if ($2 ~ /zcnblk-shm-targ/) target += total_field[2]
			else if ($2 ~ /zcnblk-wal-leaf/) leaf += total_field[2]
			else kernel += total_field[2]
		}
		END {
			printf "repeat=%d transactions=%d storage_context_switches=%d per_1k_transactions=%.3f target=%d leaf=%d kernel=%d\n",
				repeat, transactions, total, total * 1000 / transactions, target, leaf, kernel
		}
	' "$OUTDIR/rep$rep.context.delta" | tee "$OUTDIR/rep$rep.context.summary"
done

"$PGBIN/psql" -At -h "$SOCKET_DIR" -p "$PORT" -U postgres -d pgbench \
	-c "select row_to_json(w) from pg_stat_wal w; select row_to_json(b) from pg_stat_bgwriter b;" \
	>"$OUTDIR/postgres-stats.log"
sudo -n -u postgres "$PGBIN/pg_ctl" -D "$DATA_DIR" -m fast -w stop >"$OUTDIR/pgctl-stop.log" 2>&1
postgres_started=0
sudo -n cp "$MOUNTPOINT/postgres.log" "$OUTDIR/postgres.log"
sudo -n chown "$(id -u):$(id -g)" "$OUTDIR/postgres.log"

printf 'repeat\ttransport\ttps\tlatency_ms\n' >"$OUTDIR/pgbench-repeats.tsv"
for rep in $(seq 1 "$REPEATS"); do
	tps="$(awk '/^tps =/{print $3; exit}' "$OUTDIR/rep$rep.log")"
	latency_ms="$(awk '/^latency average =/{print $4; exit}' "$OUTDIR/rep$rep.log")"
	[ -n "$tps" ] && [ -n "$latency_ms" ] || die "repeat $rep lacks pgbench TPS or latency output"
	printf '%s\t%s\t%s\t%s\n' "$rep" "$WAL_TRANSPORT" "$tps" "$latency_ms" >>"$OUTDIR/pgbench-repeats.tsv"
done
awk -F '\t' -v transport="$WAL_TRANSPORT" '
	NR == 1 { next }
	NR == 2 { min_tps=max_tps=$3; min_latency=max_latency=$4 }
	{
		count++
		total_tps += $3
		total_latency += $4
		if ($3 < min_tps) min_tps=$3
		if ($3 > max_tps) max_tps=$3
		if ($4 < min_latency) min_latency=$4
		if ($4 > max_latency) max_latency=$4
	}
	END {
		mean_tps=total_tps/count
		mean_latency=total_latency/count
		printf "zcnblk-pgbench-summary: transport=%s repeats=%d mean_tps=%.3f min_tps=%.3f max_tps=%.3f spread_pct=%.3f mean_latency_ms=%.3f min_latency_ms=%.3f max_latency_ms=%.3f\n",
			transport, count, mean_tps, min_tps, max_tps,
			(max_tps-min_tps)*100/mean_tps,
			mean_latency, min_latency, max_latency
	}
' "$OUTDIR/pgbench-repeats.tsv" | tee "$OUTDIR/summary.log"
