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
KERNEL_QUEUES="${KERNEL_QUEUES:-2}"
TARGET_CPU_LIST="${TARGET_CPU_LIST:-1,9}"
KTHREAD_CPU_LIST="${KTHREAD_CPU_LIST:-2,10}"
LEAF_CPU_LIST="${LEAF_CPU_LIST:-3,11}"
POSTGRES_CPU_LIST="${POSTGRES_CPU_LIST:-4-7,12-15,20-23,28-31}"
PGBENCH_CPU_LIST="${PGBENCH_CPU_LIST:-0,8,16,24}"
SYNC_COORDINATOR_CPU="${SYNC_COORDINATOR_CPU:-17}"

COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
COORDINATION_SCOPE="${COORDINATION_SCOPE:-shared-host}"
BOOTSTRAP_MANIFEST="${ZCUTILS_BOOTSTRAP_MANIFEST:-$HOME/.local/state/zcutils/adhoc-bootstrap.env}"
MODULE="$ROOT/kmods/zcnblk_client_mod.ko"
TARGET="$ROOT/target/release/zcnblk-shm-target"
LEAF="$ROOT/target/release/zcnblk-wal-leaf"
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
			--resource "cpu=0-31;memory-bandwidth=*;port=$LEAF_PORT-$((LEAF_PORT + 1)),$PORT" \
			--note 'two-lane topology-explicit durable PostgreSQL benchmark')"
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
		grep -Eq '^instance_id=i-[0-9a-f]+$' "$BOOTSTRAP_MANIFEST" || \
			die 'bootstrap manifest does not identify an EC2 instance'
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
if [ "$preflight_warnings" -ne 0 ] &&
	(env_true "${URING_PLAY_TOPOLOGY_STRICT:-0}" || env_true "${URING_PLAY_TOPOLOGY_FATAL:-0}"); then
	die 'strict topology preflight rejected this benchmark before representative numbers were printed'
fi

sudo -n insmod "$MODULE" transport=shm lanes=2 connections_per_lane=1 \
	size_mib="$SIZE_MIB" queues="$KERNEL_QUEUES" queue_depth=256 shm_sector_order_slots=4194304 \
	max_frame_bytes=4096 pipeline_depth=128 shm_ring_entries=512 \
	shm_payload_entries=8192 shm_poll_us=1000 shm_ordering_epochs="$ORDERING_EPOCHS" pin_threads=0

declare -a postgres_connection_hctxs=("" "")
for hctx_cpu_file in /sys/block/zcnblk0/mq/*/cpu_list; do
	[ -r "$hctx_cpu_file" ] || die 'zcnblk0 did not expose an hctx CPU map'
	hctx="${hctx_cpu_file%/cpu_list}"
	hctx="${hctx##*/}"
	if cpu_lists_intersect "$POSTGRES_CPU_LIST" "$(cat "$hctx_cpu_file")"; then
		connection=$((hctx % 2))
		postgres_connection_hctxs[$connection]="${postgres_connection_hctxs[$connection]}${postgres_connection_hctxs[$connection]:+,}$hctx"
	fi
done
for connection in 0 1; do
	if [ -z "${postgres_connection_hctxs[$connection]}" ]; then
		topology_representative=0
		warn_preflight "PostgreSQL CPU list $POSTGRES_CPU_LIST reaches no hctx mapped to connection $connection (hctx modulo 2)."
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
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
		"$LEAF" "zcmem:$LEAF_SIZE" "$LEAF_HOST" "$LEAF_PORT" 2 1 4096 2 true blocking \
		>"$OUTDIR/leaf.log" 2>&1 &
	leaf_pid=$!
	for _ in $(seq 1 200); do
		listeners="$(ss -H -ltn | awk -v first=":$LEAF_PORT" -v second=":$((LEAF_PORT + 1))" \
			'$4 ~ first"$" || $4 ~ second"$" {count++} END {print count + 0}')"
		[ "$listeners" -eq 2 ] && break
		[ -r "/proc/$leaf_pid/comm" ] || die 'leaf exited during startup'
		sleep 0.05
	done
	[ "${listeners:-0}" -eq 2 ] || die 'leaf did not open both lane listeners'
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
	>"$OUTDIR/target.log" 2>&1 &
target_job_pid=$!
for _ in $(seq 1 200); do [ -s "$OUTDIR/target.pid" ] && break; sleep 0.05; done
[ -s "$OUTDIR/target.pid" ] || die 'target did not publish its PID'
target_pid="$(cat "$OUTDIR/target.pid")"

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
	sudo -n taskset -pc "$cpu" "$pid" >>"$OUTDIR/kthreads.log"
done

{
	printf 'classification=%s\ncoordination_honored=%s\n' \
		"$([ "$START_LOCAL_LEAF" = 1 ] && printf local-shared-system || printf remote-userspace-leaf)" \
		"$coord_honored"
	printf 'leaf_host=%s leaf_port=%s leaf_source_addr=%s local_leaf=%s\n' \
		"$LEAF_HOST" "$LEAF_PORT" "${LEAF_SOURCE_ADDR:-kernel-route}" "$START_LOCAL_LEAF"
	printf 'topology_representative=%s preflight_warnings=%s\n' "$topology_representative" "$preflight_warnings"
	printf 'kernel_queues=%s target_cpus=%s kthread_cpus=%s leaf_cpus=%s\n' \
		"$KERNEL_QUEUES" "$TARGET_CPU_LIST" "$KTHREAD_CPU_LIST" "$LEAF_CPU_LIST"
	printf 'lane0_hctx=%s\n' "$(cat /sys/block/zcnblk0/mq/0/cpu_list)"
	printf 'lane1_hctx=%s\n' "$(cat /sys/block/zcnblk0/mq/1/cpu_list)"
	printf 'postgres_connection0_hctxs=%s postgres_connection1_hctxs=%s\n' \
		"${postgres_connection_hctxs[0]}" "${postgres_connection_hctxs[1]}"
	printf 'sync_coordinator_cpu=%s\n' "$SYNC_COORDINATOR_CPU"
	printf 'postgres_cpus=%s\npgbench_cpus=%s\n' "$POSTGRES_CPU_LIST" "$PGBENCH_CPU_LIST"
	printf 'scale=%s clients=%s jobs=%s duration=%s repeats=%s warmup_seconds=%s builtin=%s track_wal_io_timing=%s vector_hwm=%s ordering_epochs=%s\n' "$SCALE" "$CLIENTS" "$JOBS" "$DURATION" "$REPEATS" "$WARMUP_SECONDS" "$PGBENCH_BUILTIN" "$TRACK_WAL_IO_TIMING" "$VECTOR_HWM" "$ORDERING_EPOCHS"
	if [ "$VECTOR_HWM" = 1 ]; then
		printf 'write_completion=local-dirty-lease-admission; sync_completion=remote-volatile-leaf-hwm\n'
	else
		printf 'write_completion=local-dirty-lease-admission; sync_completion=remote-volatile-global-hwm\n'
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
	"-k $SOCKET_DIR -p $PORT -c max_connections=420 -c shared_buffers=4GB -c fsync=on -c synchronous_commit=on -c full_page_writes=on -c track_wal_io_timing=$TRACK_WAL_IO_TIMING -c checkpoint_timeout=30min -c max_wal_size=32GB -c min_wal_size=4GB" \
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

awk '/^latency average|^tps =/{print FILENAME ": " $0}' "$OUTDIR"/rep*.log
