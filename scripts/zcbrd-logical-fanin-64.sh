#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN:-$REPO_DIR/target/release/zcutils}"
LOG_DIR="${LOG_DIR:-$REPO_DIR/qemu-zcrx/zcbrd-logical-fanin-$(date -u +%Y%m%dT%H%M%SZ)}"

DEVICE_COUNT="${DEVICE_COUNT:-64}"
DEVICE_SIZE_MIB="${DEVICE_SIZE_MIB:-128}"
DEVICE_BLOCKSIZE="${DEVICE_BLOCKSIZE:-4096}"
DEVICE_QUEUE_DEPTH="${DEVICE_QUEUE_DEPTH:-1024}"
DEVICE_QUEUES_PER_DEV="${DEVICE_QUEUES_PER_DEV:-1}"
DEVICE_SHARDS_PER_DEV="${DEVICE_SHARDS_PER_DEV:-1}"
DESCRIPTOR_MODE="${DESCRIPTOR_MODE:-advertise}"
BYTES_PER_DEVICE="${BYTES_PER_DEVICE:-64m}"
CHUNK_BYTES="${CHUNK_BYTES:-64k}"
PIPELINE="${PIPELINE:-64}"
RING="${RING:-512}"
BUFFER_MODE="${BUFFER_MODE:-small-pages}"
SLOT_BUFFER_LAYOUT="${SLOT_BUFFER_LAYOUT:-shared-buffer}"
CPU_LIST="${CPU_LIST:-}"
USE_NUMACTL="${USE_NUMACTL:-auto}"
PHASES="${PHASES:-write read}"
DESTROY_EXISTING="${DESTROY_EXISTING:-true}"

usage() {
	cat <<'EOF'
usage: scripts/zcbrd-logical-fanin-64.sh

Creates many topologically aligned zcbrd RAM block devices, then runs one
slot-WAL lane per edge device so aggregate logical IOPS are spread across
processors. This is an edge-media benchmark; userspace remains responsible for
fanout/fanin topology and RAID policy.

Environment:
  DEVICE_COUNT          zcbrd device count, default: 64
  DEVICE_SIZE_MIB       size per zcbrd, default: 128
  DEVICE_QUEUES_PER_DEV blk-mq queues per zcbrd, default: 1
  DEVICE_SHARDS_PER_DEV internal shards per zcbrd, default: 1
  BYTES_PER_DEVICE      bytes per device per phase, default: 64m
  CHUNK_BYTES           IO extent size, default: 64k
  PIPELINE              io_uring pipeline per device lane, default: 64
  RING                  io_uring entries per device lane, default: 512
  CPU_LIST              optional CPU list, e.g. 0-63 or 0-31,96-127
  USE_NUMACTL           auto|true|false; bind zcbrd power-on and workers to NUMA
  PHASES                phases to run: "write read", "write", or "read"
  DESTROY_EXISTING      recreate zcbrd configfs entries, default: true
  LOG_DIR               output directory
EOF
}

log() {
	printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"
}

die() {
	printf 'error: %s\n' "$*" >&2
	exit 1
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

parse_cpu_list() {
	local input="$1"
	local part start end cpu
	input="${input// /}"
	[ -n "$input" ] || return 0
	IFS=',' read -ra parts <<<"$input"
	for part in "${parts[@]}"; do
		[ -n "$part" ] || continue
		if [[ "$part" == *-* ]]; then
			start="${part%-*}"
			end="${part#*-}"
			for cpu in $(seq "$start" "$end"); do
				printf '%s\n' "$cpu"
			done
		else
			printf '%s\n' "$part"
		fi
	done
}

online_cpus() {
	if [ -n "$CPU_LIST" ]; then
		parse_cpu_list "$CPU_LIST"
	else
		parse_cpu_list "$(cat /sys/devices/system/cpu/online)"
	fi
}

numa_node_for_cpu() {
	local cpu="$1"
	local path base
	for path in "/sys/devices/system/cpu/cpu${cpu}"/node*; do
		[ -e "$path" ] || continue
		base="$(basename "$path")"
		case "$base" in
			node*) printf '%s\n' "${base#node}"; return ;;
		esac
	done
	printf '0\n'
}

should_use_numactl() {
	case "$USE_NUMACTL" in
		true|yes|1) return 0 ;;
		false|no|0) return 1 ;;
		auto) command -v numactl >/dev/null 2>&1 ;;
		*) die "USE_NUMACTL must be auto, true, or false" ;;
	esac
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

destroy_family() {
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

create_devices() {
	local -n cpus_ref="$1"
	local idx dir cpu node

	load_modules
	if [ "$DESTROY_EXISTING" = true ]; then
		destroy_family zcbrd
	fi

	for idx in $(seq 0 $((DEVICE_COUNT - 1))); do
		cpu="${cpus_ref[$((idx % ${#cpus_ref[@]}))]}"
		node="$(numa_node_for_cpu "$cpu")"
		dir="/sys/kernel/config/zcbrd/zcbrd$idx"
		if [ "$DESTROY_EXISTING" != true ] && [ -d "$dir" ] && [ "$(cat "$dir/power" 2>/dev/null || printf 0)" = 1 ]; then
			log "reuse powered zcbrd$idx cpu=$cpu numa_node=$node"
			wait_for_path "/dev/zcbrd$idx" || die "timed out waiting for /dev/zcbrd$idx"
			continue
		fi
		if [ ! -d "$dir" ]; then
			sudo mkdir "$dir"
		fi
		tee_attr "$dir/size_mib" "$DEVICE_SIZE_MIB"
		tee_attr "$dir/blocksize" "$DEVICE_BLOCKSIZE"
		tee_attr "$dir/queues" "$DEVICE_QUEUES_PER_DEV"
		tee_attr "$dir/queue_depth" "$DEVICE_QUEUE_DEPTH"
		tee_attr "$dir/shards" "$DEVICE_SHARDS_PER_DEV"
		tee_attr "$dir/descriptor_mode" "$DESCRIPTOR_MODE"
		log "power zcbrd$idx cpu=$cpu numa_node=$node"
		if should_use_numactl; then
			sudo numactl --cpunodebind="$node" --membind="$node" \
				sh -c "printf '1\n' > '$dir/power'"
		else
			tee_attr "$dir/power" 1
		fi
		wait_for_path "/dev/zcbrd$idx" || die "timed out waiting for /dev/zcbrd$idx"
	done
	sudo chmod a+rw /dev/zcbrd* 2>/dev/null || true
	lsblk -b -o NAME,SIZE,LOG-SEC,PHY-SEC,ROTA,TYPE /dev/zcbrd* |
		tee "$LOG_DIR/devices.log"
}

run_lane() {
	local phase="$1"
	local idx="$2"
	local cpu="$3"
	local node="$4"
	local dev="/dev/zcbrd$idx"
	local log_file="$LOG_DIR/${phase}/zcbrd${idx}.log"
	local cmd=(env
		URING_PLAY_SLOT_BUFFER_LAYOUT="$SLOT_BUFFER_LAYOUT"
		URING_PLAY_PIN_CPUS=1
		URING_PLAY_PIN_CPU_LIST="$cpu"
		"$BIN" slot-wal-bench "$dev" "$BYTES_PER_DEVICE" "$CHUNK_BYTES" "$PIPELINE" "$RING" "$phase" "$BUFFER_MODE")

	mkdir -p "$(dirname "$log_file")"
	if should_use_numactl; then
		sudo numactl --physcpubind="$cpu" --membind="$node" "${cmd[@]}" >"$log_file" 2>&1
	else
		taskset -c "$cpu" sudo "${cmd[@]}" >"$log_file" 2>&1
	fi
}

run_phase() {
	local phase="$1"
	local -n cpus_ref="$2"
	local idx cpu node start_ns end_ns status=0
	local -a pids=()

	mkdir -p "$LOG_DIR/$phase"
	log "run phase=$phase devices=$DEVICE_COUNT bytes_per_device=$BYTES_PER_DEVICE chunk=$CHUNK_BYTES pipeline=$PIPELINE"
	start_ns="$(date +%s%N)"
	for idx in $(seq 0 $((DEVICE_COUNT - 1))); do
		cpu="${cpus_ref[$((idx % ${#cpus_ref[@]}))]}"
		node="$(numa_node_for_cpu "$cpu")"
		run_lane "$phase" "$idx" "$cpu" "$node" &
		pids+=("$!")
	done
	for idx in "${!pids[@]}"; do
		if ! wait "${pids[$idx]}"; then
			status=1
		fi
	done
	end_ns="$(date +%s%N)"
	emit_summary "$phase" "$start_ns" "$end_ns" "$status"
	return "$status"
}

emit_summary() {
	local phase="$1"
	local start_ns="$2"
	local end_ns="$3"
	local status="$4"
	python3 - "$LOG_DIR/$phase" "$phase" "$DEVICE_COUNT" "$CHUNK_BYTES" "$start_ns" "$end_ns" "$status" <<'PY' |
import glob
import sys

case_dir, phase, device_count, chunk_bytes, start_ns, end_ns, status = sys.argv[1:]
def parse_size(value):
    value = str(value).strip().lower()
    mult = 1
    for suffix, factor in (("kib", 1024), ("kb", 1000), ("k", 1024), ("mib", 1024**2), ("mb", 1000**2), ("m", 1024**2), ("gib", 1024**3), ("gb", 1000**3), ("g", 1024**3)):
        if value.endswith(suffix):
            value = value[:-len(suffix)]
            mult = factor
            break
    return int(value) * mult
chunk_bytes = parse_size(sys.argv[4])
elapsed = max((int(end_ns) - int(start_ns)) / 1_000_000_000.0, 1e-12)
total_bytes = 0
total_ops = 0
child_iops = 0.0
missing = []
for path in sorted(glob.glob(case_dir + "/*.log")):
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
    child_iops += float(fields.get("ops_per_sec", "0"))
chunk_iops = total_ops / elapsed
logical_4k_iops = total_bytes / 4096.0 / elapsed
mibps = total_bytes / 1024.0 / 1024.0 / elapsed
gbitps = total_bytes * 8.0 / 1_000_000_000.0 / elapsed
print(
    "zcbrd-logical-fanin-result "
    f"phase={phase} devices={device_count} status={status} missing_summaries={len(missing)} "
    f"chunk_bytes={chunk_bytes} total_bytes={total_bytes} chunk_ops={total_ops} "
    f"chunk_iops={chunk_iops:.0f} logical_4k_iops={logical_4k_iops:.0f} "
    f"MiBps={mibps:.2f} Gbitps={gbitps:.3f} child_sum_iops={child_iops:.0f} "
    f"wall_seconds={elapsed:.6f} log_dir={case_dir}"
)
for path in missing:
    print(f"zcbrd-logical-fanin-missing-summary log={path}")
PY
	tee -a "$LOG_DIR/results.log"
}

main() {
	if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ] || [ "${1:-}" = "help" ]; then
		usage
		exit 0
	fi
	[ -x "$BIN" ] || die "missing executable $BIN; run cargo build --release --bin zcutils"
	mkdir -p "$LOG_DIR"
	mapfile -t cpus < <(online_cpus)
	[ "${#cpus[@]}" -gt 0 ] || die "no online CPUs discovered"

	printf 'zcbrd-logical-fanin-config devices=%s device_size_mib=%s blocksize=%s queues_per_dev=%s queue_depth=%s shards_per_dev=%s bytes_per_device=%q chunk_bytes=%q pipeline=%s ring=%s buffer_mode=%s slot_buffer_layout=%s cpu_count=%s cpu_list=%q use_numactl=%s log_dir=%q\n' \
		"$DEVICE_COUNT" "$DEVICE_SIZE_MIB" "$DEVICE_BLOCKSIZE" "$DEVICE_QUEUES_PER_DEV" \
		"$DEVICE_QUEUE_DEPTH" "$DEVICE_SHARDS_PER_DEV" "$BYTES_PER_DEVICE" "$CHUNK_BYTES" \
		"$PIPELINE" "$RING" "$BUFFER_MODE" "$SLOT_BUFFER_LAYOUT" "${#cpus[@]}" \
		"${CPU_LIST:-$(cat /sys/devices/system/cpu/online)}" "$USE_NUMACTL" "$LOG_DIR" |
		tee "$LOG_DIR/results.log"

	create_devices cpus
	local phase
	for phase in $PHASES; do
		case "$phase" in
			write|read) run_phase "$phase" cpus ;;
			*) die "unknown phase $phase; use write and/or read" ;;
		esac
	done
}

main "$@"
