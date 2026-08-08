#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${OUTDIR:-$ROOT/bench-results/zcnblk-pgbench-$(date -u +%Y%m%dT%H%M%SZ)}"
SCALE="${SCALE:-1000}"
CLIENTS="${CLIENTS:-256}"
JOBS="${JOBS:-16}"
DURATION="${DURATION:-20}"
REPEATS="${REPEATS:-3}"
VECTOR_HWM="${VECTOR_HWM:-1}"
SIZE_MIB="${SIZE_MIB:-65536}"
LEAF_SIZE="${LEAF_SIZE:-64G}"
PORT="${PORT:-55432}"
LEAF_PORT="${LEAF_PORT:-29000}"

COORD=/home/rob/.local/bin/agent-coord
MODULE="$ROOT/kmods/zcnblk_client_mod.ko"
TARGET="$ROOT/target/release/zcnblk-shm-target"
LEAF="$ROOT/target/release/zcnblk-wal-leaf"
PGBIN=/usr/lib/postgresql/17/bin
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

die() { printf 'zcnblk-pgbench: ERROR: %s\n' "$*" >&2; exit 1; }
token_from_result() { sed -n 's/.* token=\([^ ]*\).*/\1/p' <<<"$1"; }

stop_exact() {
	local pid="$1" expected="$2" signal="$3" actual
	[ -n "$pid" ] && [ -r "/proc/$pid/comm" ] || return 0
	actual="$(cat "/proc/$pid/comm")"
	[ "$actual" = "$expected" ] || die "refusing signal: pid=$pid expected=$expected actual=$actual"
	sudo -n kill "-$signal" "$pid"
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
	stop_exact "$leaf_pid" zcnblk-wal-leaf TERM >>"$OUTDIR/cleanup.log" 2>&1
	[ -z "$leaf_pid" ] || wait "$leaf_pid" 2>/dev/null
	grep -q '^zcnblk_client_mod ' /proc/modules 2>/dev/null && sudo -n rmmod zcnblk_client_mod >>"$OUTDIR/cleanup.log" 2>&1
	sudo -n rm -rf "$MOUNTPOINT" "$SOCKET_DIR"
	[ -z "$perf_token" ] || "$COORD" release "$perf_token" >>"$OUTDIR/coordination.log" 2>&1
	[ -z "$block_token" ] || "$COORD" release "$block_token" >>"$OUTDIR/coordination.log" 2>&1
	exit "$status"
}
trap cleanup EXIT INT TERM

command -v sudo >/dev/null || die 'sudo is required'
sudo -n true || die 'passwordless sudo is required'
[ ! -e /dev/zcnblk0 ] || die '/dev/zcnblk0 already exists'
mkdir -p "$OUTDIR"

block_result="$($COORD request --owner codex:zcutils-pgbench-hwm --mode exclusive \
	--sensitivity high --priority 65 --ttl 3600 --resource 'block=zcnblk0' \
	--note 'durable PostgreSQL over placement-free zcnblk edge')"
printf '%s\n' "$block_result" | tee -a "$OUTDIR/coordination.log"
block_token="$(token_from_result "$block_result")"

perf_result="$($COORD request --owner codex:zcutils-pgbench-hwm --mode soft-exclusive \
	--sensitivity critical --priority 65 --ttl 3600 \
	--resource "cpu=0-31;memory-bandwidth=*;port=$LEAF_PORT-$((LEAF_PORT + 1)),$PORT" \
	--note 'two-lane topology-explicit durable PostgreSQL benchmark')"
printf '%s\n' "$perf_result" | tee -a "$OUTDIR/coordination.log"
perf_token="$(token_from_result "$perf_result")"
coord_honored=false
grep -q ' honored=true ' <<<"$perf_result" && coord_honored=true

sudo -n insmod "$MODULE" transport=shm lanes=2 connections_per_lane=1 \
	size_mib="$SIZE_MIB" queues=2 queue_depth=256 shm_sector_order_slots=4194304 \
	max_frame_bytes=4096 pipeline_depth=128 shm_ring_entries=512 \
	shm_payload_entries=8192 shm_poll_us=1000 pin_threads=0

sudo -n rm -rf "$MOUNTPOINT" "$SOCKET_DIR"
sudo -n mkdir -p "$MOUNTPOINT" "$SOCKET_DIR"
sudo -n chown postgres:postgres "$SOCKET_DIR"

env URING_PLAY_PIN_CPU_LIST=3,11 URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
	URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 \
	"$LEAF" "zcmem:$LEAF_SIZE" 127.0.0.1 "$LEAF_PORT" 2 1 4096 2 true blocking \
	>"$OUTDIR/leaf.log" 2>&1 &
leaf_pid=$!
for _ in $(seq 1 200); do
	ss -H -ltn | awk -v port=":$LEAF_PORT" '$4 ~ port"$" {found=1} END {exit !found}' && break
	[ -r "/proc/$leaf_pid/comm" ] || die 'leaf exited during startup'
	sleep 0.05
done

sudo -n env URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE="$OUTDIR/target.pid" \
	URING_PLAY_TOPOLOGY_REPRESENTATIVE=1 \
	URING_PLAY_ZCNBLK_SHM_COORDINATOR_CPU=17 \
	URING_PLAY_ZCNBLK_SHM_LEASE_RELEASE_BATCH=1 \
	URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH=4096 \
	URING_PLAY_ZCNBLK_SHM_READ_BATCH=512 \
	URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1 \
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
	URING_PLAY_ZCNBLK_SHM_LEAF_ADDR="127.0.0.1:$LEAF_PORT" \
	"$TARGET" /dev/zcnblk-shmctl wal-tcp 128 1,9 1000 1000 10000 \
	>"$OUTDIR/target.log" 2>&1 &
target_job_pid=$!
for _ in $(seq 1 200); do [ -s "$OUTDIR/target.pid" ] && break; sleep 0.05; done
[ -s "$OUTDIR/target.pid" ] || die 'target did not publish its PID'
target_pid="$(cat "$OUTDIR/target.pid")"

for lane in 0 1; do
	name="zcnblk-shm-$lane-0"
	pid="$(ps -e -o pid=,comm= | awk -v name="$name" '$2 == name {print $1}')"
	[ -n "$pid" ] || die "missing kernel lane thread $name"
	cpu=$((2 + lane * 8))
	sudo -n taskset -pc "$cpu" "$pid" >>"$OUTDIR/kthreads.log"
done

{
	printf 'classification=local-shared-system\ncoordination_honored=%s\n' "$coord_honored"
	printf 'lane0=target:1,kthread:2,leaf:3,hctx:%s\n' "$(cat /sys/block/zcnblk0/mq/0/cpu_list)"
	printf 'lane1=target:9,kthread:10,leaf:11,hctx:%s\n' "$(cat /sys/block/zcnblk0/mq/1/cpu_list)"
	printf 'sync_coordinator_cpu=17\n'
	printf 'postgres_cpus=4-7,12-15,20-23,28-31\npgbench_cpus=0,8,16,24\n'
	printf 'scale=%s clients=%s jobs=%s duration=%s repeats=%s vector_hwm=%s\n' "$SCALE" "$CLIENTS" "$JOBS" "$DURATION" "$REPEATS" "$VECTOR_HWM"
	printf 'completion=early-local-write-ack; durability=remote-all-lane-sync-hwm\n'
	printf 'hugepages_total=%s memlock_kib=%s loadavg=%s\n' \
		"$(awk '/HugePages_Total:/{print $2}' /proc/meminfo)" "$(ulimit -l)" "$(cat /proc/loadavg)"
} >"$OUTDIR/topology.log"

sudo -n mkfs.ext4 -F -E nodiscard /dev/zcnblk0 >"$OUTDIR/mkfs.log" 2>&1
sudo -n mount -o noatime /dev/zcnblk0 "$MOUNTPOINT"
mounted=1
sudo -n chown postgres:postgres "$MOUNTPOINT"
sudo -n -u postgres "$PGBIN/initdb" -D "$DATA_DIR" --no-locale --encoding=UTF8 >"$OUTDIR/initdb.log" 2>&1
sudo -n -u postgres taskset -c 4-7,12-15,20-23,28-31 "$PGBIN/pg_ctl" \
	-D "$DATA_DIR" -l "$MOUNTPOINT/postgres.log" -w start -o \
	"-k $SOCKET_DIR -p $PORT -c max_connections=420 -c shared_buffers=4GB -c fsync=on -c synchronous_commit=on -c full_page_writes=on -c checkpoint_timeout=30min -c max_wal_size=32GB -c min_wal_size=4GB" \
	>"$OUTDIR/pgctl-start.log" 2>&1
postgres_started=1
"$PGBIN/createdb" -h "$SOCKET_DIR" -p "$PORT" -U postgres pgbench

/usr/bin/time -f 'elapsed_seconds=%e' -o "$OUTDIR/init.time" \
	taskset -c 0,8,16,24 "$PGBIN/pgbench" -h "$SOCKET_DIR" -p "$PORT" \
	-U postgres -d pgbench -i -s "$SCALE" >"$OUTDIR/init.log" 2>&1

for rep in $(seq 1 "$REPEATS"); do
	taskset -c 0,8,16,24 "$PGBIN/pgbench" -h "$SOCKET_DIR" -p "$PORT" \
		-U postgres -d pgbench -c "$CLIENTS" -j "$JOBS" -T "$DURATION" -P 5 -r -M prepared \
		>"$OUTDIR/rep$rep.log" 2>&1
done

"$PGBIN/psql" -At -h "$SOCKET_DIR" -p "$PORT" -U postgres -d pgbench \
	-c "select * from pg_stat_wal; select * from pg_stat_bgwriter;" >"$OUTDIR/postgres-stats.log"
sudo -n -u postgres "$PGBIN/pg_ctl" -D "$DATA_DIR" -m fast -w stop >"$OUTDIR/pgctl-stop.log" 2>&1
postgres_started=0
sudo -n cp "$MOUNTPOINT/postgres.log" "$OUTDIR/postgres.log"
sudo -n chown "$(id -u):$(id -g)" "$OUTDIR/postgres.log"

awk '/^latency average|^tps =/{print FILENAME ": " $0}' "$OUTDIR"/rep*.log
