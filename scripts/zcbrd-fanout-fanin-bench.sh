#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN:-$REPO_DIR/target/release/zcutils}"
LOG_DIR="${LOG_DIR:-$REPO_DIR/qemu-zcrx/zcbrd-fanout-fanin-$(date -u +%Y%m%dT%H%M%SZ)}"

COUNTS="${COUNTS:-1 2 4}"
DEVS="${DEVS:-/dev/zcbrd0 /dev/zcbrd1 /dev/zcbrd2 /dev/zcbrd3}"
BYTES_PER_TARGET="${BYTES_PER_TARGET:-64m}"
CHUNK_BYTES="${CHUNK_BYTES:-4k}"
PIPELINE="${PIPELINE:-64}"
RING="${RING:-512}"
BUFFER_MODE="${BUFFER_MODE:-small-pages}"
SLOT_BUFFER_LAYOUT="${SLOT_BUFFER_LAYOUT:-shared-buffer}"
USE_SUDO="${USE_SUDO:-true}"
RUN_TREE_SIM="${RUN_TREE_SIM:-true}"
TREE_BYTES="${TREE_BYTES:-16g}"
TREE_RECORD_BYTES="${TREE_RECORD_BYTES:-4k}"
TREE_SEGMENT_BYTES="${TREE_SEGMENT_BYTES:-384k}"
SETUP_DEVICES="${SETUP_DEVICES:-false}"
DEVICE_SIZE_MIB="${DEVICE_SIZE_MIB:-1024}"
DEVICE_BLOCKSIZE="${DEVICE_BLOCKSIZE:-4096}"
DEVICE_QUEUES="${DEVICE_QUEUES:-$(nproc)}"
DEVICE_QUEUE_DEPTH="${DEVICE_QUEUE_DEPTH:-1024}"
DEVICE_SHARDS="${DEVICE_SHARDS:-$(nproc)}"

usage() {
	cat <<'EOF'
usage: scripts/zcbrd-fanout-fanin-bench.sh

Runs real io-slot WAL fanout and fanin phases against RAM block devices. The
default shape is 1 -> 1, 1 -> 2, and 1 -> 4 over /dev/zcbrd0..3.

Environment:
  COUNTS              fanout widths to test, default: "1 2 4"
  DEVS                block devices, default: /dev/zcbrd0 /dev/zcbrd1 /dev/zcbrd2 /dev/zcbrd3
  BYTES_PER_TARGET    bytes written/read per device per phase, default: 64m
  CHUNK_BYTES         io-slot WAL chunk size, default: 4k
  PIPELINE            io-slot pipeline depth per device, default: 64
  RING                io_uring entries per device, default: 512
  BUFFER_MODE         small-pages|hugetlb, default: small-pages
  SLOT_BUFFER_LAYOUT  per-slot|shared-buffer, default: shared-buffer
  USE_SUDO            run child benches with sudo, default: true
  RUN_TREE_SIM        also run descriptor-only 1,2,4 tree simulation, default: true
  SETUP_DEVICES       recreate /dev/zcbrd0..max(COUNTS)-1 first, default: false
  DEVICE_SIZE_MIB     size per recreated zcbrd, default: 1024
  DEVICE_BLOCKSIZE    block size per recreated zcbrd, default: 4096
  DEVICE_QUEUES       blk-mq queues per recreated zcbrd, default: nproc
  DEVICE_QUEUE_DEPTH  queue depth per recreated zcbrd, default: 1024
  DEVICE_SHARDS       internal shards per recreated zcbrd, default: nproc
  LOG_DIR             output directory
EOF
}

log() {
	printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"
}

kv_from_line() {
	local line="$1"
	local key="$2"
	printf '%s\n' "$line" | tr ' ' '\n' | awk -F= -v key="$key" '$1 == key { print $2; exit }'
}

bench_prefix() {
	if [ "$USE_SUDO" = true ]; then
		printf 'sudo env URING_PLAY_SLOT_BUFFER_LAYOUT=%q ' "$SLOT_BUFFER_LAYOUT"
	else
		printf 'env URING_PLAY_SLOT_BUFFER_LAYOUT=%q ' "$SLOT_BUFFER_LAYOUT"
	fi
}

max_count() {
	local max=0 count
	for count in $COUNTS; do
		if [ "$count" -gt "$max" ]; then
			max="$count"
		fi
	done
	printf '%s\n' "$max"
}

tee_attr() {
	local path="$1"
	local value="$2"
	printf '%s\n' "$value" | sudo tee "$path" >/dev/null
}

wait_for_path() {
	local path="$1"
	local tries="${2:-200}"
	for _ in $(seq 1 "$tries"); do
		[ -e "$path" ] && return 0
		sleep 0.1
	done
	return 1
}

load_modules() {
	sudo modprobe configfs
	if ! mountpoint -q /sys/kernel/config; then
		sudo mount -t configfs configfs /sys/kernel/config
	fi
	if ! lsmod | awk '{print $1}' | grep -qx zcbrd_mod; then
		sudo insmod "$REPO_DIR/kmods/zcbrd_mod.ko"
	fi
}

destroy_configfs_family() {
	local family="$1"
	local dir
	for dir in "/sys/kernel/config/$family"/*; do
		[ -d "$dir" ] || continue
		if [ -e "$dir/power" ]; then
			tee_attr "$dir/power" 0 || true
		fi
		sudo rmdir "$dir" 2>/dev/null || true
	done
}

setup_devices() {
	local count idx dir
	count="$(max_count)"
	if [ "$count" -le 0 ]; then
		printf 'COUNTS must contain at least one positive count\n' >&2
		exit 1
	fi
	log "setup uniform zcbrd devices count=$count size_mib=$DEVICE_SIZE_MIB blocksize=$DEVICE_BLOCKSIZE queues=$DEVICE_QUEUES depth=$DEVICE_QUEUE_DEPTH shards=$DEVICE_SHARDS"
	load_modules
	destroy_configfs_family zcstripe
	destroy_configfs_family zcbrd
	for idx in $(seq 0 $((count - 1))); do
		dir="/sys/kernel/config/zcbrd/zcbrd$idx"
		sudo mkdir "$dir"
		tee_attr "$dir/size_mib" "$DEVICE_SIZE_MIB"
		tee_attr "$dir/blocksize" "$DEVICE_BLOCKSIZE"
		tee_attr "$dir/queues" "$DEVICE_QUEUES"
		tee_attr "$dir/queue_depth" "$DEVICE_QUEUE_DEPTH"
		tee_attr "$dir/shards" "$DEVICE_SHARDS"
		tee_attr "$dir/descriptor_mode" advertise
		tee_attr "$dir/power" 1
		wait_for_path "/dev/zcbrd$idx"
	done
	sudo chmod a+rw /dev/zcbrd* 2>/dev/null || true
	lsblk -b -o NAME,SIZE,LOG-SEC,PHY-SEC,ROTA,TYPE /dev/zcbrd* |
		tee "$LOG_DIR/zcbrd-devices.log"
}

require_devices() {
	local count="$1"
	local idx=0 dev
	for dev in $DEVS; do
		if [ "$idx" -ge "$count" ]; then
			return 0
		fi
		if [ ! -b "$dev" ]; then
			printf 'missing block device for count=%s: %s\n' "$count" "$dev" >&2
			return 1
		fi
		idx=$((idx + 1))
	done
	if [ "$idx" -ge "$count" ]; then
		return 0
	fi
	printf 'count=%s needs %s devices, but DEVS only provided %s\n' "$count" "$count" "$idx" >&2
	return 1
}

run_one_phase() {
	local count="$1"
	local phase="$2"
	local mode="$3"
	local case_dir="$LOG_DIR/real-${phase}-n${count}"
	local start_ns end_ns status=0 idx=0 dev log_file cmd_prefix
	local -a pids=()

	require_devices "$count"
	mkdir -p "$case_dir"
	cmd_prefix="$(bench_prefix)"
	log "real $phase topology=1->$count devices=$count bytes_per_target=$BYTES_PER_TARGET chunk=$CHUNK_BYTES pipeline=$PIPELINE"
	start_ns="$(date +%s%N)"
	for dev in $DEVS; do
		if [ "$idx" -ge "$count" ]; then
			break
		fi
		log_file="$case_dir/$(basename "$dev").log"
		# shellcheck disable=SC2086
		(
			eval "$cmd_prefix" "\"$BIN\"" slot-wal-bench "\"$dev\"" \
				"\"$BYTES_PER_TARGET\"" "\"$CHUNK_BYTES\"" "\"$PIPELINE\"" "\"$RING\"" \
				"\"$mode\"" "\"$BUFFER_MODE\""
		) >"$log_file" 2>&1 &
		pids+=("$!")
		idx=$((idx + 1))
	done

	for idx in "${!pids[@]}"; do
		if ! wait "${pids[$idx]}"; then
			status=1
		fi
	done
	end_ns="$(date +%s%N)"

	emit_phase_result "$count" "$phase" "$mode" "$start_ns" "$end_ns" "$case_dir" "$status"
	return "$status"
}

emit_phase_result() {
	local count="$1"
	local phase="$2"
	local mode="$3"
	local start_ns="$4"
	local end_ns="$5"
	local case_dir="$6"
	local status="$7"
	local summary_file="$LOG_DIR/results.log"

	python3 - "$count" "$phase" "$mode" "$start_ns" "$end_ns" "$case_dir" "$status" <<'PY' | tee -a "$summary_file"
import glob
import sys

count, phase, mode, start_ns, end_ns, case_dir, status = sys.argv[1:]
elapsed = max((int(end_ns) - int(start_ns)) / 1_000_000_000.0, 1e-12)
total_bytes = 0
total_ops = 0
child_mibps = 0.0
child_iops = 0.0
logs = sorted(glob.glob(case_dir + "/*.log"))
missing = []
for path in logs:
    summary = None
    with open(path, "r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            if line.startswith("slot-wal-bench:"):
                summary = line.strip()
    if summary is None:
        missing.append(path)
        continue
    fields = {}
    for part in summary.split():
        if "=" in part:
            key, value = part.split("=", 1)
            fields[key] = value
    total_bytes += int(fields.get("total_bytes", "0"))
    total_ops += int(fields.get("ops", "0"))
    child_mibps += float(fields.get("MiBps", "0"))
    child_iops += float(fields.get("ops_per_sec", "0"))

mibps = total_bytes / 1024.0 / 1024.0 / elapsed
iops = total_ops / elapsed
print(
    "zcbrd-fanout-fanin-result "
    f"topology=1->{count} phase={phase} mode={mode} devices={len(logs)} "
    f"status={status} missing_summaries={len(missing)} total_bytes={total_bytes} "
    f"ops={total_ops} chunk_iops={iops:.0f} MiBps={mibps:.2f} "
    f"child_sum_iops={child_iops:.0f} child_sum_MiBps={child_mibps:.2f} "
    f"wall_seconds={elapsed:.6f} log_dir={case_dir}"
)
if missing:
    for path in missing:
        print(f"zcbrd-fanout-fanin-missing-summary log={path}")
PY
}

run_tree_sim() {
	if [ "$RUN_TREE_SIM" != true ]; then
		return 0
	fi
	if [ ! -x "$REPO_DIR/target/release/zcraidd" ]; then
		log "skip tree-sim: target/release/zcraidd is missing"
		return 0
	fi
	log "descriptor tree-sim topology=1,2,4"
	"$REPO_DIR/target/release/zcraidd" tree-sim --levels 1,2,4 \
		--bytes "$TREE_BYTES" \
		--record-bytes "$TREE_RECORD_BYTES" \
		--segment-bytes "$TREE_SEGMENT_BYTES" \
		2>&1 | tee "$LOG_DIR/tree-sim-1-2-4.log"
}

main() {
	if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ] || [ "${1:-}" = "help" ]; then
		usage
		exit 0
	fi
	if [ ! -x "$BIN" ]; then
		printf 'missing executable: %s\n' "$BIN" >&2
		printf 'run: cargo build --release --bin zcutils\n' >&2
		exit 1
	fi

	mkdir -p "$LOG_DIR"
	printf 'zcbrd-fanout-fanin-config counts=%q devs=%q bytes_per_target=%q chunk_bytes=%q pipeline=%q ring=%q buffer_mode=%q slot_buffer_layout=%q use_sudo=%q log_dir=%q\n' \
		"$COUNTS" "$DEVS" "$BYTES_PER_TARGET" "$CHUNK_BYTES" "$PIPELINE" "$RING" \
		"$BUFFER_MODE" "$SLOT_BUFFER_LAYOUT" "$USE_SUDO" "$LOG_DIR" |
		tee "$LOG_DIR/results.log"

	if [ "$SETUP_DEVICES" = true ]; then
		setup_devices
	fi
	run_tree_sim
	local count
	for count in $COUNTS; do
		run_one_phase "$count" fanout write
		run_one_phase "$count" fanin read
	done
}

main "$@"
