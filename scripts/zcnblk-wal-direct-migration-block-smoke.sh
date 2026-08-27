#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${OUTDIR:-$ROOT/bench-results/local-zcnblk-wal-direct-migration-$(date -u +%Y%m%dT%H%M%SZ)}"
MODULE="${MODULE:-$ROOT/kmods/zcnblk_client_mod.ko}"
LEAF_BIN="${LEAF_BIN:-$ROOT/target/release/zcnblk-wal-leaf}"
TARGET_BIN="${TARGET_BIN:-$ROOT/target/release/zcnblk-shm-target}"
CONTINUITY_BIN="${CONTINUITY_BIN:-$ROOT/target/release/zcnblk-edge-continuity}"
VOLUME_MIB="${VOLUME_MIB:-64}"
SOURCE_BASE="${SOURCE_BASE:-29600}"
DESTINATION_BASE="${DESTINATION_BASE:-29700}"
TARGET_CPU="${TARGET_CPU:-1}"
OWNER_CPU="${OWNER_CPU:-2}"
CONTROL_CPU="${CONTROL_CPU:-3}"
CONTINUITY_CPU="${CONTINUITY_CPU:-4}"
SOURCE_LEAF_CPU_LIST="${SOURCE_LEAF_CPU_LIST:-5,6}"
DESTINATION_LEAF_CPU_LIST="${DESTINATION_LEAF_CPU_LIST:-7,8}"
CHUNK_BYTES="${CHUNK_BYTES:-1048576}"
CONTROL_SOCKET="$OUTDIR/direct-migration.sock"
jobs=()
target_pid=""
continuity_pid=""

stop_exact_pid() {
	local pid="$1"
	[ -n "$pid" ] || return 0
	[ -e "/proc/$pid" ] || return 0
	kill -TERM "$pid" 2>/dev/null || sudo -n kill -TERM "$pid" 2>/dev/null || true
	for _ in $(seq 1 100); do
		[ ! -e "/proc/$pid" ] && return 0
		sleep 0.02
	done
	kill -KILL "$pid" 2>/dev/null || sudo -n kill -KILL "$pid" 2>/dev/null || true
}

cleanup() {
	local status=$?
	set +e
	stop_exact_pid "$continuity_pid"
	stop_exact_pid "$target_pid"
	for pid in "${jobs[@]}"; do
		stop_exact_pid "$pid"
	done
	if grep -q '^zcnblk_client_mod ' /proc/modules 2>/dev/null; then
		sudo -n rmmod zcnblk_client_mod
	fi
	exit "$status"
}
trap cleanup EXIT INT TERM

sudo -n true
[ ! -e /dev/zcnblk0 ] || {
	printf 'refusing to disturb existing /dev/zcnblk0\n' >&2
	exit 1
}
for binary in "$LEAF_BIN" "$TARGET_BIN" "$CONTINUITY_BIN"; do
	[ -x "$binary" ] || {
		printf 'missing executable %s\n' "$binary" >&2
		exit 1
	}
done
[ -r "$MODULE" ]
command -v nc >/dev/null
mkdir -p "$OUTDIR"
volume_bytes=$((VOLUME_MIB * 1024 * 1024))

printf 'classification=correctness-only representative=false transport=tcp foreground_topology=/dev/zcnblk0->userspace-owner->active-terminal-leaf foreground_hops=1 migration_gateway=false lane_to_worker=0:0 lane_to_cpu=0:%s owner_to_cpu=0:%s migration_control_cpu=%s continuity_cpu=%s source_leaf_worker_cpus=%s destination_leaf_worker_cpus=%s per_worker_qd=128 aggregate_outstanding=128 raw_transport_rtt=not-measured theoretical_iops_ceiling=not-reported completion_semantics=remote-read+early-local-write+remote-sync volume_bytes=%s chunk_bytes=%s\n' \
	"$TARGET_CPU" "$OWNER_CPU" "$CONTROL_CPU" "$CONTINUITY_CPU" "$SOURCE_LEAF_CPU_LIST" "$DESTINATION_LEAF_CPU_LIST" "$volume_bytes" "$CHUNK_BYTES" | tee "$OUTDIR/topology.log"

env URING_PLAY_PIN_CPU_LIST="$SOURCE_LEAF_CPU_LIST" \
	URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
	URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
	URING_PLAY_ZCNBLK_WAL_LEAF_DYNAMIC_ACCEPT=1 \
	"$LEAF_BIN" "zcmem:$volume_bytes" 127.0.0.1 "$SOURCE_BASE" 1 2 4096 2 true blocking \
	>"$OUTDIR/source-leaf.log" 2>&1 &
jobs+=("$!")
env URING_PLAY_PIN_CPU_LIST="$DESTINATION_LEAF_CPU_LIST" \
	URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
	URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
	URING_PLAY_ZCNBLK_WAL_LEAF_DYNAMIC_ACCEPT=1 \
	"$LEAF_BIN" "zcmem:$volume_bytes" 127.0.0.1 "$DESTINATION_BASE" 1 2 4096 2 true blocking \
	>"$OUTDIR/destination-leaf.log" 2>&1 &
jobs+=("$!")
for _ in $(seq 1 100); do
	grep -q '^zcnblk-wal-leaf:' "$OUTDIR/source-leaf.log" 2>/dev/null && \
		grep -q '^zcnblk-wal-leaf:' "$OUTDIR/destination-leaf.log" 2>/dev/null && break
	sleep 0.02
done
sleep 0.1

sudo -n insmod "$MODULE" transport=shm lanes=1 connections_per_lane=1 \
	size_mib="$VOLUME_MIB" queues=1 queue_depth=128 max_frame_bytes=4096 \
	pipeline_depth=128 shm_ring_entries=128 shm_payload_entries=4096 \
	shm_poll_us=1000 pin_threads=0
for _ in $(seq 1 100); do
	[ -e /dev/zcnblk0 ] && [ -e /dev/zcnblk-shmctl ] && break
	sleep 0.05
done
[ -e /dev/zcnblk0 ] && [ -e /dev/zcnblk-shmctl ]

# The invoking user owns OUTDIR; only the target process needs device access.
# shellcheck disable=SC2024
sudo -n env \
	URING_PLAY_ZCNBLK_SHM_LEAF_ADDR="127.0.0.1:$SOURCE_BASE" \
	URING_PLAY_ZCNBLK_SHM_MIGRATION_SOURCE_ADDR="127.0.0.1:$SOURCE_BASE" \
	URING_PLAY_ZCNBLK_SHM_MIGRATION_DEST_ADDR="127.0.0.1:$DESTINATION_BASE" \
	URING_PLAY_ZCNBLK_SHM_MIGRATION_CONTROL_SOCKET="$CONTROL_SOCKET" \
	URING_PLAY_ZCNBLK_SHM_MIGRATION_TCP_COPY_METHOD=splice \
	URING_PLAY_ZCNBLK_SHM_MIGRATION_CATCHUP_PASSES=2 \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RESULT_RANGES=1 \
	URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_ZC_REQUIRED=0 \
	URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1 \
	URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS=1 \
	URING_PLAY_ZCNBLK_SHM_OWNER_COUNT=1 \
	URING_PLAY_ZCNBLK_SHM_OWNER_CPU_LIST="$OWNER_CPU" \
	URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE="$OUTDIR/target.pid" \
	taskset -c "$CONTROL_CPU" "$TARGET_BIN" /dev/zcnblk-shmctl wal-tcp 128 "$TARGET_CPU" 1000 1000 10000 \
	>"$OUTDIR/target.log" 2>&1 &
jobs+=("$!")
for _ in $(seq 1 200); do
	[ -s "$OUTDIR/target.pid" ] && [ -S "$CONTROL_SOCKET" ] && break
	sleep 0.05
done
if [ ! -s "$OUTDIR/target.pid" ] || [ ! -S "$CONTROL_SOCKET" ]; then
	cat "$OUTDIR/target.log" >&2
	exit 1
fi
target_pid="$(cat "$OUTDIR/target.pid")"
[[ "$target_pid" =~ ^[0-9]+$ ]]

# The invoking user owns OUTDIR; only the continuity process needs device access.
# shellcheck disable=SC2024
sudo -n env ZCNBLK_EDGE_CONTINUITY_PID_FILE="$OUTDIR/continuity.pid" \
	taskset -c "$CONTINUITY_CPU" "$CONTINUITY_BIN" /dev/zcnblk0 0 128 0 64 \
	>"$OUTDIR/continuity.log" 2>&1 &
jobs+=("$!")
for _ in $(seq 1 200); do
	[ -s "$OUTDIR/continuity.pid" ] && grep -q '^zcnblk-edge-continuity-start:' "$OUTDIR/continuity.log" 2>/dev/null && break
	sleep 0.02
done
if [ ! -s "$OUTDIR/continuity.pid" ]; then
	cat "$OUTDIR/continuity.log" >&2
	exit 1
fi
continuity_pid="$(cat "$OUTDIR/continuity.pid")"
[[ "$continuity_pid" =~ ^[0-9]+$ ]]

printf 'migrate 2 %s %s 4096\n' "$volume_bytes" "$CHUNK_BYTES" | \
	sudo -n timeout 60 nc -U "$CONTROL_SOCKET" | tee "$OUTDIR/migration-control.log"
grep -q '^OK active_destination=true ' "$OUTDIR/migration-control.log"
grep -q 'foreground_hops=1 foreground_payload_rebuffer_copies=0' "$OUTDIR/migration-control.log"
grep -q 'copy_payload_userspace_buffers=0 copy_method=Splice' "$OUTDIR/migration-control.log"

sleep 0.2
stop_exact_pid "$continuity_pid"
continuity_pid=""
wait "${jobs[$((${#jobs[@]} - 1))]}"
grep -q '^ZCNBLK_EDGE_CONTINUITY_PASS .*identity_stable=true .*open_descriptor_replaced=false .*mismatches=0 ' \
	"$OUTDIR/continuity.log"
grep -q '^zcnblk-shm-target-direct-route-cutover: .*foreground_hops=1 payload_rebuffer_copies=0 client_block_reconnect=false$' \
	"$OUTDIR/target.log"

printf 'ZCNBLK_WAL_DIRECT_MIGRATION_BLOCK_PASS device=/dev/zcnblk0 reconnects=0 remounts=0 foreground_hops=1 foreground_payload_rebuffer_copies=0 base_copy=socket-pipe-socket-splice client_data_continuous=true artifact=%s\n' "$OUTDIR"
