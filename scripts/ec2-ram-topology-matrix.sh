#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN:-$REPO_DIR/target/release/zcutils}"
LOG_DIR="${LOG_DIR:-$REPO_DIR/qemu-zcrx/ec2-ram-topology-$(date -u +%Y%m%dT%H%M%SZ)}"

CPUS="$(nproc)"
COUNTS="${COUNTS:-1 2 4 8 16 32 64}"
CHUNKS="${CHUNKS:-4096 65536}"
WORKERS="${WORKERS:-$CPUS}"
BYTES_PER_WORKER="${BYTES_PER_WORKER:-268435456}"
PIPELINE="${PIPELINE:-64}"
PIPELINES="${PIPELINES:-$PIPELINE}"
RING="${RING:-512}"
BUFFER_MODE="${BUFFER_MODE:-small-pages}"
WRITE_MODE="${WRITE_MODE:-fixed}"
WRITE_MODES="${WRITE_MODES:-$WRITE_MODE}"
QUEUES="${QUEUES:-$CPUS}"
QUEUE_DEPTH="${QUEUE_DEPTH:-256}"
SHARDS="${SHARDS:-$CPUS}"
BLOCKSIZE="${BLOCKSIZE:-4096}"
ZCBRD_NUMA_POWER="${ZCBRD_NUMA_POWER:-align}"
PIN_WORKERS="${PIN_WORKERS:-true}"
COMPLETION_BATCH="${COMPLETION_BATCH:-64}"

usage() {
	cat <<'EOF'
usage: scripts/ec2-ram-topology-matrix.sh

Environment:
  COUNTS             ram disk counts, default: 1 2 4 8 16 32 64
  CHUNKS             IO sizes, default: 4096 65536
  WORKERS            aggregate workers, default: nproc
  BYTES_PER_WORKER   bytes per worker for each run, default: 256MiB
  PIPELINE           io_uring pipeline per worker, default: 64
  PIPELINES          pipeline values to sweep after each setup, default: PIPELINE
  RING               io_uring ring entries per worker, default: 512
  WRITE_MODE         write|fixed|fixed-file, default: fixed
  WRITE_MODES        write modes to sweep after each setup, default: WRITE_MODE
  QUEUE_DEPTH        blk-mq queue depth for created devices, default: 256
  ZCBRD_NUMA_POWER   align|off; align powers each disk under its worker NUMA node
EOF
}

log() {
	printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"
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

size_mib_for_count() {
	local count="$1"
	if [ "$count" -le 2 ]; then
		printf '65536\n'
	elif [ "$count" -le 4 ]; then
		printf '32768\n'
	elif [ "$count" -le 16 ]; then
		printf '16384\n'
	else
		printf '4096\n'
	fi
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

destroy_zcbrds() {
	local dir
	for dir in /sys/kernel/config/zcbrd/*; do
		[ -d "$dir" ] || continue
		tee_attr "$dir/power" 0 || true
		sudo rmdir "$dir" 2>/dev/null || true
	done
}

create_zcbrds() {
	local count="$1"
	local size_mib="$2"
	local idx dir workers start_cpu node

	destroy_zcbrds

	start_cpu=0
	for idx in $(seq 0 $((count - 1))); do
		dir="/sys/kernel/config/zcbrd/zcbrd$idx"
		sudo mkdir "$dir"
		tee_attr "$dir/size_mib" "$size_mib"
		tee_attr "$dir/blocksize" "$BLOCKSIZE"
		tee_attr "$dir/queues" "$QUEUES"
		tee_attr "$dir/queue_depth" "$QUEUE_DEPTH"
		tee_attr "$dir/shards" "$SHARDS"
		tee_attr "$dir/descriptor_mode" advertise
		workers="$(workers_for_shard "$idx" "$count")"
		node="$(numa_node_for_cpu "$start_cpu")"
		if [ "$ZCBRD_NUMA_POWER" = align ] && command -v numactl >/dev/null 2>&1; then
			log "power zcbrd$idx numa_node=$node cpu_start=$start_cpu workers=$workers"
			sudo numactl --cpunodebind="$node" --membind="$node" sh -c "printf '1\n' > '$dir/power'"
		else
			tee_attr "$dir/power" 1
		fi
		wait_for_path "/dev/zcbrd$idx"
		start_cpu=$((start_cpu + workers))
	done
	sudo chmod a+rw /dev/zcbrd* 2>/dev/null || true
}

workers_for_shard() {
	local shard="$1"
	local count="$2"
	local base=$((WORKERS / count))
	local rem=$((WORKERS % count))
	if [ "$shard" -lt "$rem" ]; then
		printf '%s\n' $((base + 1))
	else
		printf '%s\n' "$base"
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

run_sharded_direct() {
	local count="$1"
	local chunk="$2"
	local case_dir="$LOG_DIR/sharded-direct-n${count}-chunk${chunk}"
	local start_ns end_ns status=0 idx workers start_cpu log_file total_bytes total_ops
	local -a pids=()

	mkdir -p "$case_dir"
	log "run topology=sharded-direct count=$count chunk=$chunk"
	start_ns="$(date +%s%N)"
	start_cpu=0
	for idx in $(seq 0 $((count - 1))); do
		workers="$(workers_for_shard "$idx" "$count")"
		if [ "$workers" -eq 0 ]; then
			continue
		fi
		log_file="$case_dir/zcbrd${idx}.log"
		(
			sudo env \
				URING_PLAY_PIN_BASE_CPU="$start_cpu" \
				URING_PLAY_PIN_CPU_COUNT="$workers" \
				URING_PLAY_URING_WRITE_COMPLETION_BATCH="$COMPLETION_BATCH" \
				"$BIN" uring-write-bench "/dev/zcbrd$idx" "$workers" "$BYTES_PER_WORKER" \
				"$chunk" "$PIPELINE" "$RING" "$BUFFER_MODE" "$WRITE_MODE" "$PIN_WORKERS"
		) >"$log_file" 2>&1 &
		pids+=("$!")
		start_cpu=$((start_cpu + workers))
	done

	for idx in "${!pids[@]}"; do
		if ! wait "${pids[$idx]}"; then
			status=1
		fi
	done
	end_ns="$(date +%s%N)"

	total_bytes=$((WORKERS * BYTES_PER_WORKER))
	total_ops=$((total_bytes / chunk))
	python3 - "$count" "$chunk" "$WORKERS" "$PIPELINE" "$RING" "$WRITE_MODE" "$total_bytes" "$total_ops" "$start_ns" "$end_ns" "$case_dir" "$status" <<'PY' | tee -a "$LOG_DIR/matrix-results.log"
import sys

count, chunk, workers, pipeline, ring, write_mode, total_bytes, total_ops, start_ns, end_ns, case_dir, status = sys.argv[1:]
total_bytes = int(total_bytes)
total_ops = int(total_ops)
elapsed = max((int(end_ns) - int(start_ns)) / 1_000_000_000.0, 1e-12)
iops = total_ops / elapsed
mibps = total_bytes / 1024.0 / 1024.0 / elapsed
gbitps = total_bytes * 8.0 / 1_000_000_000.0 / elapsed
print(
    "matrix-result "
    f"topology=sharded-direct count={count} chunk={chunk} "
    f"workers={workers} pipeline={pipeline} ring={ring} write_mode={write_mode} status={status} "
    f"ops_per_sec={iops:.0f} MiBps={mibps:.2f} Gbitps={gbitps:.3f} "
    f"seconds={elapsed:.6f} bytes={total_bytes} log_dir={case_dir}"
)
PY
	if [ "$status" -ne 0 ]; then
		return "$status"
	fi
}

main() {
	if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
		usage
		exit 0
	fi

	mkdir -p "$LOG_DIR"
	load_modules
	printf 'matrix-config counts=%q chunks=%q workers=%s bytes_per_worker=%s pipeline=%s ring=%s write_mode=%s queue_depth=%s zcbrd_numa_power=%s\n' \
		"$COUNTS" "$CHUNKS" "$WORKERS" "$BYTES_PER_WORKER" \
		"$PIPELINES" "$RING" "$WRITE_MODES" "$QUEUE_DEPTH" "$ZCBRD_NUMA_POWER" | tee "$LOG_DIR/matrix-results.log"

	local count size_mib chunk mode pipeline_value
	for count in $COUNTS; do
		size_mib="$(size_mib_for_count "$count")"
		log "setup count=$count size_mib_each=$size_mib queues=$QUEUES depth=$QUEUE_DEPTH"
		create_zcbrds "$count" "$size_mib"
		lsblk -b -o NAME,SIZE,LOG-SEC,PHY-SEC,ROTA,TYPE /dev/zcbrd* |
			tee "$LOG_DIR/lsblk-n${count}.log"

		for mode in $WRITE_MODES; do
			WRITE_MODE="$mode"
			for pipeline_value in $PIPELINES; do
				PIPELINE="$pipeline_value"
				for chunk in $CHUNKS; do
					run_sharded_direct "$count" "$chunk"
				done
			done
		done
	done
}

main "$@"
