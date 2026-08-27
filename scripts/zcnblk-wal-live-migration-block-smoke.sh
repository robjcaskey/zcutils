#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${OUTDIR:-$ROOT/bench-results/local-zcnblk-wal-live-migration-block-$(date -u +%Y%m%dT%H%M%SZ)}"
MODULE="${MODULE:-$ROOT/kmods/zcnblk_client_mod.ko}"
LEAF_BIN="${LEAF_BIN:-$ROOT/target/release/zcnblk-wal-leaf}"
GATEWAY_BIN="${GATEWAY_BIN:-$ROOT/target/release/zcnblk-wal-live-migrate}"
TARGET_BIN="${TARGET_BIN:-$ROOT/target/release/zcnblk-shm-target}"
SMOKE_BIN="${SMOKE_BIN:-$ROOT/target/release/zcnblk-live-cutover-smoke}"
VOLUME_MIB="${VOLUME_MIB:-64}"
OPERATIONS="${OPERATIONS:-256}"
TARGET_CPU="${TARGET_CPU:-1}"
SOURCE_BASE="${SOURCE_BASE:-29300}"
DESTINATION_BASE="${DESTINATION_BASE:-29400}"
GATEWAY_BASE="${GATEWAY_BASE:-29200}"
CONTROL_ADDR="${CONTROL_ADDR:-127.0.0.1:29500}"
SYSTEM_BPS="${SYSTEM_BPS:-67108864}"
CHUNK_BYTES="${CHUNK_BYTES:-1048576}"
TRANSPORT="${TRANSPORT:-tcp}"
target_pid=""
jobs=()

mkdir -p "$OUTDIR"

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
	[ -n "$target_pid" ] && stop_exact_pid "$target_pid"
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
for binary in "$LEAF_BIN" "$GATEWAY_BIN" "$TARGET_BIN" "$SMOKE_BIN"; do
	[ -x "$binary" ] || {
		printf 'missing executable %s\n' "$binary" >&2
		exit 1
	}
done
[ -r "$MODULE" ]
[ "$TRANSPORT" = tcp ] || [ "$TRANSPORT" = ofi ] || {
	printf 'TRANSPORT must be tcp or ofi\n' >&2
	exit 1
}

volume_bytes=$((VOLUME_MIB * 1024 * 1024))
printf 'classification=correctness-only representative=false transport=%s lanes=1 lane_to_worker=0:0 lane_to_cpu=0:%s per_worker_qd=1 aggregate_outstanding=1 raw_transport_rtt=not-measured theoretical_iops_ceiling=not-reported completion_semantics=remote-volatile-sync-hwm volume_bytes=%s chunk_bytes=%s system_bytes_per_second=%s block_identity=stable-open-fd\n' \
	"$TRANSPORT" "$TARGET_CPU" "$volume_bytes" "$CHUNK_BYTES" "$SYSTEM_BPS" | tee "$OUTDIR/topology.log"

leaf_env=(env URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1
	URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1)
gateway_env=(env ZCNBLK_WAL_MIGRATION_TRANSPORT="$TRANSPORT")
target_env=(env URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT="$TRANSPORT")
if [ "$TRANSPORT" = ofi ]; then
	leaf_env+=(URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT=ofi
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER=sockets
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT=rdm
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_WRITES=1
		URING_PLAY_OFI_CQ_SLEEP_NS=0
		URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=1048576)
	gateway_env+=(ZCNBLK_WAL_MIGRATION_OFI_PROVIDER=sockets
		ZCNBLK_WAL_MIGRATION_OFI_ENDPOINT=rdm
		URING_PLAY_OFI_CQ_SLEEP_NS=0
		URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=1048576)
	target_env+=(URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER=sockets
		URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT=rdm
		URING_PLAY_OFI_CQ_SLEEP_NS=0
		URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=1048576)
fi

"${leaf_env[@]}" \
	"$LEAF_BIN" "zcmem:$volume_bytes" 127.0.0.1 "$SOURCE_BASE" 1 2 4096 1 false blocking \
	>"$OUTDIR/source-leaf.log" 2>&1 &
jobs+=("$!")
"${leaf_env[@]}" \
	"$LEAF_BIN" "zcmem:$volume_bytes" 127.0.0.1 "$DESTINATION_BASE" 1 3 4096 1 false blocking \
	>"$OUTDIR/destination-leaf.log" 2>&1 &
jobs+=("$!")

"${gateway_env[@]}" "$GATEWAY_BIN" "127.0.0.1:$GATEWAY_BASE" "127.0.0.1:$SOURCE_BASE" \
	"127.0.0.1:$DESTINATION_BASE" "$CONTROL_ADDR" "$volume_bytes" 1 \
	"$CHUNK_BYTES" "$SYSTEM_BPS" >"$OUTDIR/gateway.log" 2>&1 &
jobs+=("$!")

sudo -n insmod "$MODULE" transport=shm lanes=1 connections_per_lane=1 \
	size_mib="$VOLUME_MIB" queues=1 queue_depth=128 max_frame_bytes=4096 \
	pipeline_depth=128 shm_ring_entries=128 shm_payload_entries=4096 \
	shm_poll_us=1000 pin_threads=0
for _ in $(seq 1 100); do
	[ -e /dev/zcnblk0 ] && [ -e /dev/zcnblk-shmctl ] && break
	sleep 0.05
done
[ -e /dev/zcnblk0 ] && [ -e /dev/zcnblk-shmctl ]

sudo -n "${target_env[@]}" URING_PLAY_ZCNBLK_SHM_LEAF_ADDR="127.0.0.1:$GATEWAY_BASE" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RESULT_RANGES=1 \
	URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE="$OUTDIR/target.pid" \
	"$TARGET_BIN" /dev/zcnblk-shmctl wal-tcp 128 "$TARGET_CPU" 1000 1000 10000 \
	>"$OUTDIR/target.log" 2>&1 &
jobs+=("$!")
for _ in $(seq 1 200); do
	[ -s "$OUTDIR/target.pid" ] && grep -q '^zcnblk-shm-target:' "$OUTDIR/target.log" 2>/dev/null && break
	sleep 0.05
done
[ -s "$OUTDIR/target.pid" ]
target_pid="$(cat "$OUTDIR/target.pid")"
[[ "$target_pid" =~ ^[0-9]+$ ]]
grep -q '^zcnblk-shm-target:' "$OUTDIR/target.log"

sudo -n env ZCNBLK_LIVE_CUTOVER_MODE=migration \
	"$SMOKE_BIN" /dev/zcnblk0 "$CONTROL_ADDR" "$OPERATIONS" \
	| tee "$OUTDIR/smoke.log"
grep -q '^ZCNBLK_LIVE_CUTOVER_PASS ' "$OUTDIR/smoke.log"

{
	exec 3<>"/dev/tcp/${CONTROL_ADDR%:*}/${CONTROL_ADDR##*:}"
	printf 'status\n' >&3
	IFS= read -r status <&3
	printf '%s\n' "$status"
	exec 3>&-
} | tee "$OUTDIR/final-status.log"
grep -q 'phase=active_secondary' "$OUTDIR/final-status.log"
grep -q 'client_reconnect=false cache_fence=epoch+hwm' "$OUTDIR/gateway.log"
grep -Eq '^zcnblk-shm-target-route-fence: .*placement_epoch=2 .*completion_hwm=[1-9][0-9]* .*cache_policy=dirty-sequence-overlay-retained-until-remote-completion .*result_boundary=true$' \
	"$OUTDIR/target.log"

printf 'ZCNBLK_WAL_LIVE_MIGRATION_BLOCK_PASS device=/dev/zcnblk0 reconnects=0 remounts=0 client_data_continuous=true route_epoch_fence=true dirty_lookaside_hwm_safe=true transport=%s base_copy=%s artifact=%s\n' \
	"$TRANSPORT" "$([ "$TRANSPORT" = tcp ] && printf splice || printf rma-one-registered-arena)" "$OUTDIR"
