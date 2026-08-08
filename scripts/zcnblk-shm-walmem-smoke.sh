#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
MODULE="${MODULE:-$ROOT/kmods/zcnblk_client_mod.ko}"
TARGET_BIN="${TARGET_BIN:-$ROOT/target/release/zcnblk-shm-target}"
TARGET_CPU="${TARGET_CPU:-1}"
SIZE_MIB="${SIZE_MIB:-128}"
SHM_RING_ENTRIES="${SHM_RING_ENTRIES:-128}"
SHM_PAYLOAD_ENTRIES="${SHM_PAYLOAD_ENTRIES:-4096}"
WRITEBACK_BATCH="${WRITEBACK_BATCH:-2048}"
OUTDIR="${OUTDIR:-$ROOT/bench-results/local-zcnblk-shm-walmem-correctness-$(date -u +%Y%m%dT%H%M%SZ)}"
pid_file="$OUTDIR/target.pid"

block_lease=""
cpu_lease=""
target_pid=""
target_job_pid=""
scratch=""

token_from_result() {
	sed -n 's/.* token=\([^ ]*\).*/\1/p' <<<"$1"
}

stop_target() {
	if [ -z "$target_pid" ] && [ -s "$pid_file" ]; then
		target_pid="$(cat "$pid_file")"
	fi
	[ -n "$target_pid" ] || return 0
	[ -r "/proc/$target_pid/comm" ] || return 0
	local comm
	comm="$(cat "/proc/$target_pid/comm")"
	[ "$comm" = "zcnblk-shm-targ" ] || {
		printf 'refusing to signal pid=%s comm=%s\n' "$target_pid" "$comm" >&2
		return 1
	}
	sudo -n kill -INT "$target_pid"
	for _ in $(seq 1 100); do
		[ ! -e "/proc/$target_pid" ] && break
		sleep 0.05
	done
	[ ! -e "/proc/$target_pid" ] || return 1
	if [ -n "$target_job_pid" ]; then
		wait "$target_job_pid" 2>/dev/null || true
		target_job_pid=""
	fi
	target_pid=""
}

cleanup() {
	local status=$?
	set +e
	stop_target
	if grep -q '^zcnblk_client_mod ' /proc/modules 2>/dev/null; then
		sudo -n rmmod zcnblk_client_mod
	fi
	[ -n "$scratch" ] && rm -rf -- "$scratch"
	[ -n "$cpu_lease" ] && "$COORD_BIN" release "$cpu_lease" >/dev/null 2>&1
	[ -n "$block_lease" ] && "$COORD_BIN" release "$block_lease" >/dev/null 2>&1
	exit "$status"
}

trap cleanup EXIT INT TERM

command -v sudo >/dev/null
sudo -n true
[ -x "$COORD_BIN" ]
[ -x "$TARGET_BIN" ]
[ -r "$MODULE" ]
[ ! -e /dev/zcnblk0 ]
mkdir -p "$OUTDIR"
scratch="$(mktemp -d)"

result="$($COORD_BIN request --owner codex:zcutils-walmem-smoke \
	--mode exclusive --sensitivity high --priority 50 --ttl 600 \
	--resource 'block=zcnblk0' --note 'zcnblk wal-memory correctness smoke')"
printf '%s\n' "$result" | tee "$OUTDIR/coordination.log"
block_lease="$(token_from_result "$result")"
[ -n "$block_lease" ]

result="$($COORD_BIN request --owner codex:zcutils-walmem-smoke \
	--mode shared --sensitivity normal --priority 50 --ttl 600 \
	--resource "cpu=$TARGET_CPU;memory-bandwidth=*" \
	--note 'short correctness test, not a performance measurement')"
printf '%s\n' "$result" | tee -a "$OUTDIR/coordination.log"
cpu_lease="$(token_from_result "$result")"
[ -n "$cpu_lease" ]

printf 'classification=correctness-only target_cpu=%s lanes=1 ring_entries=%s payload_entries=%s writeback_batch=%s\n' \
	"$TARGET_CPU" "$SHM_RING_ENTRIES" "$SHM_PAYLOAD_ENTRIES" "$WRITEBACK_BATCH" | tee "$OUTDIR/topology.log"

sudo -n insmod "$MODULE" transport=shm lanes=1 connections_per_lane=1 \
	size_mib="$SIZE_MIB" queues=1 queue_depth=128 max_frame_bytes=4096 \
	pipeline_depth="$SHM_RING_ENTRIES" shm_ring_entries="$SHM_RING_ENTRIES" \
	shm_payload_entries="$SHM_PAYLOAD_ENTRIES" shm_poll_us=1000 pin_threads=0
for _ in $(seq 1 100); do
	[ -e /dev/zcnblk0 ] && [ -e /dev/zcnblk-shmctl ] && break
	sleep 0.05
done
[ -e /dev/zcnblk0 ] && [ -e /dev/zcnblk-shmctl ]

sudo -n env URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH="$WRITEBACK_BATCH" \
	URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE="$pid_file" \
	"$TARGET_BIN" /dev/zcnblk-shmctl wal-memory 128 "$TARGET_CPU" 1000 1000 10000 \
	>"$OUTDIR/target.log" 2>&1 &
target_job_pid=$!
for _ in $(seq 1 100); do
	[ -s "$pid_file" ] && grep -q '^zcnblk-shm-target:' "$OUTDIR/target.log" 2>/dev/null && break
	[ -e "/proc/$target_job_pid" ] || [ -s "$pid_file" ] || {
		cat "$OUTDIR/target.log" >&2
		exit 1
	}
	sleep 0.05
done
grep -q '^zcnblk-shm-target:' "$OUTDIR/target.log"
target_pid="$(cat "$pid_file")"
[[ "$target_pid" =~ ^[0-9]+$ ]]
[ -r "/proc/$target_pid/comm" ]

dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' '\125' >"$scratch/pattern-a"
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' '\252' >"$scratch/pattern-b"

sudo -n dd if="$scratch/pattern-a" of=/dev/zcnblk0 bs=4096 seek=7 count=1 \
	oflag=direct conv=notrunc status=none
sudo -n dd if=/dev/zcnblk0 of="$scratch/read-a" bs=4096 skip=7 count=1 \
	iflag=direct status=none
cmp "$scratch/pattern-a" "$scratch/read-a"

sudo -n dd if="$scratch/pattern-b" of=/dev/zcnblk0 bs=4096 seek=7 count=1 \
	oflag=direct conv=notrunc status=none
sudo -n dd if=/dev/zcnblk0 of="$scratch/read-b-dirty" bs=4096 skip=7 count=1 \
	iflag=direct status=none
cmp "$scratch/pattern-b" "$scratch/read-b-dirty"

if sudo -n dd if=/dev/zero of=/dev/zcnblk0 bs=4096 count=0 conv=fsync status=none \
	2>"$OUTDIR/expected-sync-rejection.log"; then
	echo "wal-memory incorrectly acknowledged a block sync" >&2
	exit 1
fi
sudo -n dd if=/dev/zcnblk0 of="$scratch/read-b-after-rejected-sync" bs=4096 skip=7 count=1 \
	iflag=direct status=none
cmp "$scratch/pattern-b" "$scratch/read-b-after-rejected-sync"

stop_target
grep 'zcnblk-shm-target-summary:' "$OUTDIR/target.log" | tee "$OUTDIR/summary.log"
grep -q 'syncs=[1-9]' "$OUTDIR/summary.log"
printf 'zcnblk-shm-walmem-smoke: PASS dirty-read=true overwrite-order=true sync-fail-closed=true artifact=%s\n' "$OUTDIR"
