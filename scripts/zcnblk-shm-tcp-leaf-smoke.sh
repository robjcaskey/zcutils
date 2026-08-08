#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
MODULE="${MODULE:-$ROOT/kmods/zcnblk_client_mod.ko}"
TARGET_BIN="${TARGET_BIN:-$ROOT/target/release/zcnblk-shm-target}"
LEAF_BIN="${LEAF_BIN:-$ROOT/target/release/zcnblk-wal-leaf}"
ORDER_BIN="${ORDER_BIN:-$ROOT/target/release/zcnblk-order-smoke}"
ORDER_PAIRS="${ORDER_PAIRS:-32}"
TARGET_CPU="${TARGET_CPU:-1}"
LEAF_CPU="${LEAF_CPU:-2}"
LEAF_ADDR="${LEAF_ADDR:-127.0.0.1}"
LEAF_PORT="${LEAF_PORT:-29000}"
SIZE_MIB="${SIZE_MIB:-128}"
SHM_RING_ENTRIES="${SHM_RING_ENTRIES:-128}"
SHM_PAYLOAD_ENTRIES="${SHM_PAYLOAD_ENTRIES:-4096}"
WRITEBACK_BATCH="${WRITEBACK_BATCH:-2048}"
REMOTE_SEND_MODE="${URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE:-blocking}"
REMOTE_RECV_SPINS="${URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_SPINS:-0}"
REMOTE_RECV_POLICY="${URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_POLICY:-adaptive}"
REMOTE_RECV_ADAPTIVE_SPIN_MIN="${URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MIN:-0}"
REMOTE_RECV_ADAPTIVE_SPIN_MAX="${URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MAX:-4096}"
REMOTE_RECV_ADAPTIVE_WAIT_NS="${URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_WAIT_NS:-50000}"
REMOTE_SEND_RING_ENTRIES="${URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_RING_ENTRIES:-256}"
REMOTE_SEND_ZC_REQUIRED="${URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_ZC_REQUIRED:-1}"
ALLOW_UNSAFE_SEND_ZC="${URING_PLAY_ALLOW_UNSAFE_SEND_ZC:-0}"
OUTDIR="${OUTDIR:-$ROOT/bench-results/local-zcnblk-shm-tcp-leaf-correctness-$(date -u +%Y%m%dT%H%M%SZ)}"
pid_file="$OUTDIR/target.pid"

block_lease=""
host_lease=""
target_pid=""
target_job_pid=""
leaf_pid=""
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
		printf 'refusing to signal target pid=%s comm=%s\n' "$target_pid" "$comm" >&2
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

stop_leaf() {
	[ -n "$leaf_pid" ] || return 0
	if [ -e "/proc/$leaf_pid" ]; then
		local comm
		comm="$(cat "/proc/$leaf_pid/comm")"
		[ "$comm" = "zcnblk-wal-lea" ] || {
			printf 'refusing to signal leaf pid=%s comm=%s\n' "$leaf_pid" "$comm" >&2
			return 1
		}
		kill -TERM "$leaf_pid"
	fi
	wait "$leaf_pid" 2>/dev/null || true
	leaf_pid=""
}

cleanup() {
	local status=$?
	set +e
	stop_target
	stop_leaf
	if grep -q '^zcnblk_client_mod ' /proc/modules 2>/dev/null; then
		sudo -n rmmod zcnblk_client_mod
	fi
	[ -n "$scratch" ] && rm -rf -- "$scratch"
	[ -n "$host_lease" ] && "$COORD_BIN" release "$host_lease" >/dev/null 2>&1
	[ -n "$block_lease" ] && "$COORD_BIN" release "$block_lease" >/dev/null 2>&1
	exit "$status"
}

trap cleanup EXIT INT TERM

command -v sudo >/dev/null
command -v ss >/dev/null
sudo -n true
[ -x "$COORD_BIN" ]
[ -x "$TARGET_BIN" ]
[ -x "$LEAF_BIN" ]
[ -x "$ORDER_BIN" ]
[ -r "$MODULE" ]
[ ! -e /dev/zcnblk0 ]
mkdir -p "$OUTDIR"
scratch="$(mktemp -d)"

result="$($COORD_BIN request --owner codex:zcutils-shm-tcp-smoke \
	--mode exclusive --sensitivity high --priority 50 --ttl 600 \
	--resource 'block=zcnblk0' --note 'zcnblk shared onramp to TCP WAL leaf smoke')"
printf '%s\n' "$result" | tee "$OUTDIR/coordination.log"
block_lease="$(token_from_result "$result")"
[ -n "$block_lease" ]

result="$($COORD_BIN request --owner codex:zcutils-shm-tcp-smoke \
	--mode soft-exclusive --sensitivity high --priority 50 --ttl 600 \
	--resource "cpu=$TARGET_CPU,$LEAF_CPU;memory-bandwidth=*;port=$LEAF_PORT" \
	--note 'real TCP userspace fan-to-leaf correctness smoke')"
printf '%s\n' "$result" | tee -a "$OUTDIR/coordination.log"
host_lease="$(token_from_result "$result")"
[ -n "$host_lease" ]

printf 'classification=correctness-only client=block-edge target_cpu=%s fan_transport=shared-memory leaf_cpu=%s leaf_transport=tcp leaf=%s:%s lanes=1 writeback_batch=%s remote_recv_spins=%s remote_recv_policy=%s remote_recv_adaptive_spin_min=%s remote_recv_adaptive_spin_max=%s remote_recv_adaptive_wait_ns=%s remote_send_mode=%s remote_send_ring_entries=%s remote_send_zc_required=%s allow_unsafe_send_zc=%s\n' \
	"$TARGET_CPU" "$LEAF_CPU" "$LEAF_ADDR" "$LEAF_PORT" "$WRITEBACK_BATCH" \
	"$REMOTE_RECV_SPINS" "$REMOTE_RECV_POLICY" "$REMOTE_RECV_ADAPTIVE_SPIN_MIN" \
	"$REMOTE_RECV_ADAPTIVE_SPIN_MAX" "$REMOTE_RECV_ADAPTIVE_WAIT_NS" \
	"$REMOTE_SEND_MODE" "$REMOTE_SEND_RING_ENTRIES" "$REMOTE_SEND_ZC_REQUIRED" \
	"$ALLOW_UNSAFE_SEND_ZC" | tee "$OUTDIR/topology.log"

env URING_PLAY_PIN_CPU_LIST="$LEAF_CPU" URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
	URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
	"$LEAF_BIN" "zcmem:${SIZE_MIB}M" "$LEAF_ADDR" "$LEAF_PORT" 1 1 4096 1 true blocking \
	>"$OUTDIR/leaf.log" 2>&1 &
leaf_pid=$!
for _ in $(seq 1 100); do
	ss -H -ltn | awk -v port=":$LEAF_PORT" '$4 ~ port "$" { found=1 } END { exit !found }' && break
	[ -e "/proc/$leaf_pid" ] || {
		cat "$OUTDIR/leaf.log" >&2
		exit 1
	}
	sleep 0.05
done
ss -H -ltn | awk -v port=":$LEAF_PORT" '$4 ~ port "$" { found=1 } END { exit !found }'

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
	URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_SPINS="$REMOTE_RECV_SPINS" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_POLICY="$REMOTE_RECV_POLICY" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MIN="$REMOTE_RECV_ADAPTIVE_SPIN_MIN" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MAX="$REMOTE_RECV_ADAPTIVE_SPIN_MAX" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_WAIT_NS="$REMOTE_RECV_ADAPTIVE_WAIT_NS" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE="$REMOTE_SEND_MODE" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_RING_ENTRIES="$REMOTE_SEND_RING_ENTRIES" \
	URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_ZC_REQUIRED="$REMOTE_SEND_ZC_REQUIRED" \
	URING_PLAY_ALLOW_UNSAFE_SEND_ZC="$ALLOW_UNSAFE_SEND_ZC" \
	URING_PLAY_ZCNBLK_SHM_LEAF_ADDR="$LEAF_ADDR:$LEAF_PORT" \
	URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE="$pid_file" \
	"$TARGET_BIN" /dev/zcnblk-shmctl wal-tcp 128 "$TARGET_CPU" 1000 1000 10000 \
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

dd if=/dev/zero bs=4096 count=1 status=none >"$scratch/zero"
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' '\125' >"$scratch/pattern-a"
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' '\252' >"$scratch/pattern-b"

sudo -n dd if=/dev/zcnblk0 of="$scratch/read-cold" bs=4096 skip=7 count=1 \
	iflag=direct status=none
cmp "$scratch/zero" "$scratch/read-cold"

sudo -n dd if="$scratch/pattern-a" of=/dev/zcnblk0 bs=4096 seek=7 count=1 \
	oflag=direct conv=notrunc status=none
sudo -n dd if=/dev/zcnblk0 of="$scratch/read-a-dirty" bs=4096 skip=7 count=1 \
	iflag=direct status=none
cmp "$scratch/pattern-a" "$scratch/read-a-dirty"

sudo -n dd if="$scratch/pattern-b" of=/dev/zcnblk0 bs=4096 seek=7 count=1 \
	oflag=direct conv=notrunc status=none
sudo -n dd if=/dev/zcnblk0 of="$scratch/read-b-dirty" bs=4096 skip=7 count=1 \
	iflag=direct status=none
cmp "$scratch/pattern-b" "$scratch/read-b-dirty"

sudo -n dd if=/dev/zero of=/dev/zcnblk0 bs=4096 count=0 conv=fsync status=none
sudo -n dd if=/dev/zcnblk0 of="$scratch/read-b-remote" bs=4096 skip=7 count=1 \
	iflag=direct status=none
cmp "$scratch/pattern-b" "$scratch/read-b-remote"
sudo -n "$ORDER_BIN" /dev/zcnblk0 "$ORDER_PAIRS" | tee "$OUTDIR/order-smoke.log"

stop_target
for _ in $(seq 1 100); do
	[ ! -e "/proc/$leaf_pid" ] && break
	sleep 0.05
done
[ ! -e "/proc/$leaf_pid" ]
wait "$leaf_pid"
leaf_pid=""

grep 'zcnblk-shm-target-summary:' "$OUTDIR/target.log" | tee "$OUTDIR/summary.log"
grep 'zcnblk-shm-target-remote-leaf-summary:' "$OUTDIR/target.log" | tee -a "$OUTDIR/summary.log"
grep 'zcnblk-wal-leaf-summary:' "$OUTDIR/leaf.log" | tee -a "$OUTDIR/summary.log"
grep -q 'syncs=[1-9]' "$OUTDIR/summary.log"
grep -Eq 'read_records=([2-9]|[1-9][0-9]+)' "$OUTDIR/summary.log"
printf 'zcnblk-shm-tcp-leaf-smoke: PASS cold_read=remote dirty_read=true overwrite_order=true concurrent_order=true sync_hwm=true post_sync_read=remote artifact=%s\n' "$OUTDIR"
