#!/usr/bin/env bash
set -euo pipefail

usage() {
	cat <<'EOF'
Usage: zcnblk-fs-app-bench.sh EDGE_RESULT_DIR APP_RESULT_DIR APP_SCRIPT

Creates one topology-explicit /dev/zcnblk0 client edge backed by a two-lane
userspace WAL target and userspace memory leaf. It mounts ext4 and invokes:

  APP_SCRIPT APP_RESULT_DIR ZCNBLK_MOUNTPOINT

The application script inherits the caller's environment. This runner performs
no mirror, stripe, placement, tier, or spill operation.

Optional environment:
  SIZE_MIB=8192 LEAF_SIZE=8G LEAF_HOST=127.0.0.1 LEAF_PORT=29200
  LEAF_SOURCE_ADDR= START_LOCAL_LEAF=1 KERNEL_QUEUES=2
  TARGET_CPU_LIST=1,9 KTHREAD_CPU_LIST=2,10 LEAF_CPU_LIST=3,11
  APP_CPU_LIST=0,4-8,12-31 SYNC_COORDINATOR_CPU=17
  VECTOR_HWM=1 ORDERING_EPOCHS=1
  MOUNTPOINT=/mnt/zc-fs-app-bench COORDINATION_SCOPE=shared-host
EOF
}

[ "$#" -eq 3 ] || { usage >&2; exit 2; }
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EDGE_RESULT_DIR="$1"
APP_RESULT_DIR="$2"
APP_SCRIPT="$3"

SIZE_MIB="${SIZE_MIB:-8192}"
LEAF_SIZE="${LEAF_SIZE:-8G}"
LEAF_HOST="${LEAF_HOST:-127.0.0.1}"
LEAF_PORT="${LEAF_PORT:-29200}"
LEAF_SOURCE_ADDR="${LEAF_SOURCE_ADDR:-}"
START_LOCAL_LEAF="${START_LOCAL_LEAF:-1}"
KERNEL_QUEUES="${KERNEL_QUEUES:-2}"
TARGET_CPU_LIST="${TARGET_CPU_LIST:-1,9}"
KTHREAD_CPU_LIST="${KTHREAD_CPU_LIST:-2,10}"
LEAF_CPU_LIST="${LEAF_CPU_LIST:-3,11}"
APP_CPU_LIST="${APP_CPU_LIST:-0,4-8,12-31}"
SYNC_COORDINATOR_CPU="${SYNC_COORDINATOR_CPU:-17}"
VECTOR_HWM="${VECTOR_HWM:-1}"
ORDERING_EPOCHS="${ORDERING_EPOCHS:-$VECTOR_HWM}"
MOUNTPOINT="${MOUNTPOINT:-/mnt/zc-fs-app-bench}"
COORDINATION_SCOPE="${COORDINATION_SCOPE:-shared-host}"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
BOOTSTRAP_MANIFEST="${ZCUTILS_BOOTSTRAP_MANIFEST:-$HOME/.local/state/zcutils/adhoc-bootstrap.env}"

MODULE="$ROOT/kmods/zcnblk_client_mod.ko"
TARGET="$ROOT/target/release/zcnblk-shm-target"
LEAF="$ROOT/target/release/zcnblk-wal-leaf"
TARGET_PID_FILE="$EDGE_RESULT_DIR/target.pid"

block_token=
perf_token=
leaf_pid=
target_job_pid=
target_pid=
mounted=0
module_loaded=0
kernel_pids=()

die() { printf 'zcnblk-fs-app-bench: ERROR: %s\n' "$*" >&2; exit 1; }
token_from_result() { sed -n 's/.* token=\([^ ]*\).*/\1/p' <<<"$1"; }
env_true() {
	case "${1:-}" in
		1 | true | TRUE | yes | YES | on | ON) return 0 ;;
		*) return 1 ;;
	esac
}

cpu_lists_intersect() {
	local first="$1" second="$2"
	awk -v first="$first" -v second="$second" '
		function add(value, set, pieces, range, count, i, cpu) {
			gsub(/[[:space:]]/, "", value)
			count = split(value, pieces, ",")
			for (i = 1; i <= count; i++) {
				split(pieces[i], range, "-")
				for (cpu = range[1]; cpu <= (range[2] == "" ? range[1] : range[2]); cpu++)
					set[cpu] = 1
			}
		}
		BEGIN {
			add(first, first_cpus)
			gsub(/[[:space:]]/, "", second)
			count = split(second, pieces, ",")
			for (i = 1; i <= count; i++) {
				split(pieces[i], range, "-")
				for (cpu = range[1]; cpu <= (range[2] == "" ? range[1] : range[2]); cpu++)
					if (cpu in first_cpus) exit 0
			}
			exit 1
		}
	'
}

stop_exact() {
	local pid="$1" expected="$2" signal="$3" actual
	[ -n "$pid" ] && [ -r "/proc/$pid/comm" ] || return 0
	actual="$(cat "/proc/$pid/comm")"
	[ "$actual" = "$expected" ] ||
		die "refusing signal: pid=$pid expected=$expected actual=$actual"
	sudo -n kill "-$signal" "$pid"
}

snapshot_contexts() {
	local output="$1" pid status
	: >"$output"
	for pid in "$target_pid" "$leaf_pid" "${kernel_pids[@]}"; do
		[ -n "$pid" ] || continue
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
	done
}

unmount_owned() {
	[ "$mounted" -ne 0 ] || return 0
	for _ in $(seq 1 100); do
		if sudo -n umount "$MOUNTPOINT" >>"$EDGE_RESULT_DIR/cleanup.log" 2>&1; then
			mounted=0
			return 0
		fi
		sleep 0.1
	done
	return 1
}

cleanup() {
	local status=$?
	trap - EXIT INT TERM
	set +e
	unmount_owned
	stop_exact "$target_pid" zcnblk-shm-targ INT >>"$EDGE_RESULT_DIR/cleanup.log" 2>&1
	[ -z "$target_job_pid" ] || wait "$target_job_pid" 2>/dev/null
	if [ "$START_LOCAL_LEAF" = 1 ]; then
		stop_exact "$leaf_pid" zcnblk-wal-leaf TERM >>"$EDGE_RESULT_DIR/cleanup.log" 2>&1
		[ -z "$leaf_pid" ] || wait "$leaf_pid" 2>/dev/null
	fi
	[ "$module_loaded" -eq 0 ] || sudo -n rmmod zcnblk_client_mod >>"$EDGE_RESULT_DIR/cleanup.log" 2>&1
	sudo -n rmdir "$MOUNTPOINT" >>"$EDGE_RESULT_DIR/cleanup.log" 2>&1
	[ -z "$perf_token" ] || "$COORD_BIN" release "$perf_token" >>"$EDGE_RESULT_DIR/coordination.log" 2>&1
	[ -z "$block_token" ] || "$COORD_BIN" release "$block_token" >>"$EDGE_RESULT_DIR/coordination.log" 2>&1
	exit "$status"
}
trap cleanup EXIT INT TERM

command -v sudo >/dev/null || die 'sudo is required'
sudo -n true || die 'passwordless sudo is required'
[ -x "$APP_SCRIPT" ] || die "application script is not executable: $APP_SCRIPT"
[ -f "$MODULE" ] || die "kernel module is missing: $MODULE"
[ -x "$TARGET" ] || die "userspace target is missing: $TARGET"
[ "$START_LOCAL_LEAF" != 1 ] || [ -x "$LEAF" ] || die "userspace leaf is missing: $LEAF"
[ ! -e /dev/zcnblk0 ] || die '/dev/zcnblk0 already exists'
mkdir -p "$EDGE_RESULT_DIR" "$APP_RESULT_DIR"
EDGE_RESULT_DIR="$(realpath "$EDGE_RESULT_DIR")"
APP_RESULT_DIR="$(realpath "$APP_RESULT_DIR")"
TARGET_PID_FILE="$EDGE_RESULT_DIR/target.pid"

coord_honored=false
case "$COORDINATION_SCOPE" in
	shared-host)
		[ -x "$COORD_BIN" ] || die "agent-coord not found: $COORD_BIN"
		block_result="$($COORD_BIN request --owner codex:zcutils-fs-app --mode exclusive \
			--sensitivity high --priority 65 --ttl 3600 --resource 'block=zcnblk0' \
			--note 'single placement-free zcnblk application edge')"
		printf '%s\n' "$block_result" | tee -a "$EDGE_RESULT_DIR/coordination.log"
		block_token="$(token_from_result "$block_result")"
		grep -q ' honored=true ' <<<"$block_result" || die '/dev/zcnblk0 advisory lock was not honored'

		perf_result="$($COORD_BIN request --owner codex:zcutils-fs-app --mode soft-exclusive \
			--sensitivity critical --priority 65 --ttl 3600 \
			--resource "cpu=0-31;memory-bandwidth=*;port=$LEAF_PORT-$((LEAF_PORT + 1))" \
			--note 'topology-explicit database benchmark over two zcnblk lanes')"
		printf '%s\n' "$perf_result" | tee -a "$EDGE_RESULT_DIR/coordination.log"
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
			tee -a "$EDGE_RESULT_DIR/coordination.log"
		coord_honored=true
		;;
	*)
		die 'COORDINATION_SCOPE must be shared-host or dedicated-adhoc'
		;;
esac

preflight_warnings=0
topology_representative=1
: >"$EDGE_RESULT_DIR/preflight.log"
warn_preflight() {
	printf 'zcnblk-fs-app-bench: WARNING: %s\n' "$*" | tee -a "$EDGE_RESULT_DIR/preflight.log" >&2
	preflight_warnings=$((preflight_warnings + 1))
}
[ "$coord_honored" = true ] || {
	topology_representative=0
	warn_preflight 'CPU/memory-bandwidth soft exclusivity was not honored; repeat this shared-system run.'
}
hugepages_total="$(awk '/HugePages_Total:/{print $2}' /proc/meminfo)"
anon_hugepages_kib="$(awk '/AnonHugePages:/{print $2}' /proc/meminfo)"
thp_enabled="$(cat /sys/kernel/mm/transparent_hugepage/enabled 2>/dev/null || printf unknown)"
memlock_kib="$(ulimit -l)"
[ "$hugepages_total" -gt 0 ] || {
	topology_representative=0
	warn_preflight "explicit HugeTLB pool is empty; THP policy is '$thp_enabled' with ${anon_hugepages_kib} KiB anonymous huge pages, while the zcnblk shared arena is vmalloc-backed and cannot consume HugeTLB pages."
}
if [ "$memlock_kib" != unlimited ] && [ "$memlock_kib" -lt 1048576 ]; then
	topology_representative=0
	warn_preflight "memlock headroom is only ${memlock_kib} KiB; fixed/registered buffers need more."
fi
if [ "$preflight_warnings" -ne 0 ] &&
	(env_true "${URING_PLAY_TOPOLOGY_STRICT:-0}" || env_true "${URING_PLAY_TOPOLOGY_FATAL:-0}"); then
	die 'strict topology preflight rejected the run before benchmark numbers were printed'
fi

sudo -n insmod "$MODULE" transport=shm lanes=2 connections_per_lane=1 \
	size_mib="$SIZE_MIB" queues="$KERNEL_QUEUES" queue_depth=256 shm_sector_order_slots=4194304 \
	max_frame_bytes=4096 pipeline_depth=128 shm_ring_entries=512 \
	shm_payload_entries=8192 shm_poll_us=1000 shm_ordering_epochs="$ORDERING_EPOCHS" pin_threads=0
module_loaded=1

declare -a app_connection_hctxs=("" "")
for hctx_cpu_file in /sys/block/zcnblk0/mq/*/cpu_list; do
	[ -r "$hctx_cpu_file" ] || die 'zcnblk0 did not expose an hctx CPU map'
	hctx="${hctx_cpu_file%/cpu_list}"
	hctx="${hctx##*/}"
	if cpu_lists_intersect "$APP_CPU_LIST" "$(cat "$hctx_cpu_file")"; then
		connection=$((hctx % 2))
		app_connection_hctxs[$connection]="${app_connection_hctxs[$connection]}${app_connection_hctxs[$connection]:+,}$hctx"
	fi
done
for connection in 0 1; do
	if [ -z "${app_connection_hctxs[$connection]}" ]; then
		topology_representative=0
		warn_preflight "APP_CPU_LIST=$APP_CPU_LIST reaches no hctx mapped to connection $connection (hctx modulo 2)."
	fi
done
if [ "$preflight_warnings" -ne 0 ] &&
	(env_true "${URING_PLAY_TOPOLOGY_STRICT:-0}" || env_true "${URING_PLAY_TOPOLOGY_FATAL:-0}"); then
	die 'strict topology preflight rejected the run before benchmark numbers were printed'
fi

[ ! -e "$MOUNTPOINT" ] || sudo -n rmdir "$MOUNTPOINT" ||
	die "mountpoint already exists and is not an empty directory: $MOUNTPOINT"
sudo -n mkdir -p "$MOUNTPOINT"

if [ "$START_LOCAL_LEAF" = 1 ]; then
	env URING_PLAY_PIN_CPU_LIST="$LEAF_CPU_LIST" URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
		"$LEAF" "zcmem:$LEAF_SIZE" "$LEAF_HOST" "$LEAF_PORT" 2 1 4096 2 true blocking \
		>"$EDGE_RESULT_DIR/leaf.log" 2>&1 &
	leaf_pid=$!
	listeners=0
	for _ in $(seq 1 200); do
		listeners="$(ss -H -ltn | awk -v first=":$LEAF_PORT" -v second=":$((LEAF_PORT + 1))" \
			'$4 ~ first"$" || $4 ~ second"$" {count++} END {print count + 0}')"
		[ "$listeners" -eq 2 ] && break
		[ -r "/proc/$leaf_pid/comm" ] || die 'userspace memory leaf exited during startup'
		sleep 0.05
	done
	[ "$listeners" -eq 2 ] || die 'userspace memory leaf did not open both lane listeners'
fi

sudo -n env URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE="$TARGET_PID_FILE" \
	URING_PLAY_TOPOLOGY_REPRESENTATIVE="$topology_representative" \
	URING_PLAY_ZCNBLK_SHM_COORDINATOR_CPU="$SYNC_COORDINATOR_CPU" \
	URING_PLAY_ZCNBLK_SHM_LEASE_RELEASE_BATCH=1 \
	URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH=4096 \
	URING_PLAY_ZCNBLK_SHM_READ_BATCH=512 \
	URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1 \
	URING_PLAY_ZCNBLK_SHM_WAL_DEBUG_STATE="${URING_PLAY_ZCNBLK_SHM_WAL_DEBUG_STATE:-0}" \
	URING_PLAY_ZCNBLK_SHM_VECTOR_HWM="$VECTOR_HWM" \
	URING_PLAY_ZCNBLK_SHM_WAL_OWNER_DISPATCH=0 \
	URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS=0 \
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
	URING_PLAY_ROUTE_PROBE="${URING_PLAY_ROUTE_PROBE:-0}" \
	URING_PLAY_EXPECT_ROUTE_DEV="${URING_PLAY_EXPECT_ROUTE_DEV:-}" \
	URING_PLAY_EXPECT_ROUTE_SRC="${URING_PLAY_EXPECT_ROUTE_SRC:-}" \
	URING_PLAY_TOPOLOGY_STRICT="${URING_PLAY_TOPOLOGY_STRICT:-0}" \
	URING_PLAY_TOPOLOGY_FATAL="${URING_PLAY_TOPOLOGY_FATAL:-0}" \
	"$TARGET" /dev/zcnblk-shmctl wal-tcp 128 "$TARGET_CPU_LIST" 1000 1000 10000 \
	>"$EDGE_RESULT_DIR/target.log" 2>&1 &
target_job_pid=$!
for _ in $(seq 1 200); do [ -s "$TARGET_PID_FILE" ] && break; sleep 0.05; done
[ -s "$TARGET_PID_FILE" ] || die 'userspace target did not publish its PID'
target_pid="$(cat "$TARGET_PID_FILE")"

IFS=, read -r -a kthread_cpus <<<"$KTHREAD_CPU_LIST"
[ "${#kthread_cpus[@]}" -eq 2 ] || die 'KTHREAD_CPU_LIST must name exactly two CPUs'
for lane in 0 1; do
	name="zcnblk-shm-$lane-0"
	pid="$(ps -e -o pid=,comm= | awk -v name="$name" '$2 == name {print $1}')"
	[ -n "$pid" ] || die "missing kernel lane thread $name"
	kernel_pids+=("$pid")
	cpu="${kthread_cpus[$lane]}"
	if ! cpu_lists_intersect "$cpu" "$(cat "/sys/block/zcnblk0/mq/$lane/cpu_list")"; then
		die "kernel lane $lane CPU $cpu is outside its hctx map ($(cat "/sys/block/zcnblk0/mq/$lane/cpu_list"))"
	fi
	sudo -n taskset -pc "$cpu" "$pid" >>"$EDGE_RESULT_DIR/kthreads.log"
done

{
	printf 'classification=%s\ncoordination_honored=%s\n' \
		"$([ "$START_LOCAL_LEAF" = 1 ] && printf local-shared-system || printf remote-userspace-leaf)" \
		"$coord_honored"
	printf 'leaf_host=%s leaf_port=%s leaf_source_addr=%s local_leaf=%s\n' \
		"$LEAF_HOST" "$LEAF_PORT" "${LEAF_SOURCE_ADDR:-kernel-route}" "$START_LOCAL_LEAF"
	printf 'topology_representative=%s\npreflight_warnings=%s\n' "$topology_representative" "$preflight_warnings"
	printf 'pipeline=/dev/zcnblk0 -> userspace-wal-target -> two-lane-tcp -> userspace-zcmem-leaf\n'
	printf 'placement=none\nmirror=none\nstripe=none\n'
	printf 'kernel_queues=%s\ntarget_cpus=%s\nkthread_cpus=%s\nleaf_cpus=%s\napp_cpus=%s\n' \
		"$KERNEL_QUEUES" "$TARGET_CPU_LIST" "$KTHREAD_CPU_LIST" "$LEAF_CPU_LIST" "$APP_CPU_LIST"
	printf 'lane0_hctx=%s\nlane1_hctx=%s\n' \
		"$(cat /sys/block/zcnblk0/mq/0/cpu_list)" "$(cat /sys/block/zcnblk0/mq/1/cpu_list)"
	printf 'app_connection0_hctxs=%s\napp_connection1_hctxs=%s\n' \
		"${app_connection_hctxs[0]}" "${app_connection_hctxs[1]}"
	printf 'sync_coordinator_cpu=%s\nvector_hwm=%s\nordering_epochs=%s\n' \
		"$SYNC_COORDINATOR_CPU" "$VECTOR_HWM" "$ORDERING_EPOCHS"
	printf 'write_completion=local-dirty-lease-admission\nsync_completion=remote-volatile-leaf-hwm\n'
	printf 'hugetlb_pages_total=%s\nthp_enabled=%s\nanon_hugepages_kib=%s\n' \
		"$hugepages_total" "$thp_enabled" "$anon_hugepages_kib"
	printf 'shared_arena_backing=vmalloc_user+remap_vmalloc_range\n'
	printf 'memlock_kib=%s\nloadavg=%s\n' "$memlock_kib" "$(cat /proc/loadavg)"
	cat "$EDGE_RESULT_DIR/preflight.log"
} >"$EDGE_RESULT_DIR/topology.log"

sudo -n mkfs.ext4 -F -E nodiscard /dev/zcnblk0 >"$EDGE_RESULT_DIR/mkfs.log" 2>&1
sudo -n mount -o noatime /dev/zcnblk0 "$MOUNTPOINT"
mounted=1
sudo -n chown "$(id -u):$(id -g)" "$MOUNTPOINT"

snapshot_contexts "$EDGE_RESULT_DIR/storage-context.before"
env EXPECT_ZCNBLK=1 ZCNBLK_MOUNTPOINT="$MOUNTPOINT" \
	"$APP_SCRIPT" "$APP_RESULT_DIR" "$MOUNTPOINT"
snapshot_contexts "$EDGE_RESULT_DIR/storage-context.after"
sync -f "$MOUNTPOINT"
cat /sys/block/zcnblk0/stat >"$EDGE_RESULT_DIR/block-stat.txt"

awk '
	NR == FNR { voluntary[$1]=$3; involuntary[$1]=$4; name[$1]=$2; next }
	($1 in voluntary) {
		v=$3-voluntary[$1]; iv=$4-involuntary[$1]
		printf "pid=%s name=%s voluntary=%d involuntary=%d total=%d\n", $1, name[$1], v, iv, v+iv
	}
' "$EDGE_RESULT_DIR/storage-context.before" "$EDGE_RESULT_DIR/storage-context.after" \
	>"$EDGE_RESULT_DIR/storage-context.delta"

printf 'edge_results=%s\napplication_results=%s\n' "$EDGE_RESULT_DIR" "$APP_RESULT_DIR"
cat "$EDGE_RESULT_DIR/storage-context.delta"
