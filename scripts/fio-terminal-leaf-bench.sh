#!/usr/bin/env bash
set -euo pipefail

# Benchmark exactly one terminal leaf through a pre-sized file. This script does
# not perform placement, mirroring, striping, spill, or tier decisions.

file="${FIO_LEAF_FILE:-/mnt/zc-fio-leaf/terminal-leaf.bin}"
device="${FIO_LEAF_DEVICE:-/dev/nvme0n1}"
outdir="${FIO_LEAF_OUTDIR:-$PWD/fio-terminal-leaf-results}"
runtime="${FIO_RUNTIME:-10}"
ramp="${FIO_RAMP_TIME:-2}"
repeats="${FIO_REPEATS:-3}"
strict="${URING_PLAY_TOPOLOGY_STRICT:-0}"
fatal="${URING_PLAY_TOPOLOGY_FATAL:-0}"

die() { printf 'fio-terminal-leaf: fatal: %s\n' "$*" >&2; exit 1; }
warn() { printf 'fio-terminal-leaf: WARNING: %s\n' "$*" >&2; }
problem() {
	if [ "$strict" = 1 ] || [ "$fatal" = 1 ]; then die "$*"; else warn "$*"; fi
}

[ -f "$file" ] || die "missing pre-sized leaf file: $file"
[ -b "$device" ] || die "missing terminal leaf device: $device"
mount_source="$(findmnt -nro SOURCE --target "$file")"
[ "$mount_source" = "$device" ] || die "$file resolves to $mount_source, expected $device"
device_model="$(lsblk -dn -o MODEL "$device" | xargs)"
device_numa="$(cat "/sys/block/${device##*/}/device/numa_node")"
if [ "$device_numa" -eq 0 ]; then
	low_cpu=0
	high_cpu_first=0
	high_cpu_last=31
else
	low_cpu=96
	high_cpu_first=96
	high_cpu_last=127
fi
high_cpu_list="${high_cpu_first}-${high_cpu_last}"

file_size="$(stat -c %s "$file")"
[ "$file_size" -ge 274877906944 ] || problem "leaf file is smaller than 256 GiB ($file_size bytes)"
hugepages="$(awk '/HugePages_Total/ {print $2}' /proc/meminfo)"
[ "$hugepages" -ge 1024 ] || problem "HugeTLB pool is $hugepages pages; require at least 1024"
memlock_kb="$(ulimit -l)"
if [ "$memlock_kb" != unlimited ] && [ "$memlock_kb" -lt 1048576 ]; then
	problem "memlock headroom is ${memlock_kb} KiB; require at least 1 GiB"
fi
command -v fio >/dev/null || die "fio is not installed"
fio --enghelp=io_uring 2>&1 | grep -qw fixedbufs || problem "fio io_uring fixed-buffer support is missing"
fio --enghelp=io_uring 2>&1 | grep -qw registerfiles || problem "fio io_uring registered-file support is missing"

low_hctx=
for queue in "/sys/block/${device##*/}/mq/"*; do
	while IFS= read -r cpu; do
		if [ "$cpu" = "$low_cpu" ]; then low_hctx="${queue##*/}"; fi
	done < <(cat "$queue/cpu_list" | tr ', ' '\n\n' | awk 'NF == 1 && $1 !~ /-/ {print $1}')
done
[ -n "$low_hctx" ] || problem "worker CPU $low_cpu has no matching $device hardware context"

mkdir -p "$outdir"
topology="$outdir/topology.txt"
{
	printf 'representative=yes shared_system=yes topology_strict=%s topology_fatal=%s\n' "$strict" "$fatal"
	printf 'placement=single-userspace-writer-to-single-terminal-leaf mirror=no stripe=no spill=no\n'
	printf 'device=%s model=%s device_numa=%s filesystem_source=%s file=%s file_bytes=%s\n' \
		"$device" "$device_model" "$device_numa" "$mount_source" "$file" "$file_size"
	printf 'hugepages=%s memlock_kb=%s ioengine=io_uring direct=1 fixedbufs=1 registerfiles=1\n' \
		"$hugepages" "$memlock_kb"
	printf 'low_qd_lane=0 worker=0 cpu=%s hctx=%s worker_count=1 lane_count=1\n' "$low_cpu" "$low_hctx"
	printf 'saturation_workers=32 lanes=32 cpu_list=%s per_worker_region=8GiB aggregate_region=256GiB\n' "$high_cpu_list"
	printf 'raw_transport=local-pcie raw_transport_rtt=not-applicable network_rtt_ceiling=not-applicable\n'
	printf 'read_completion=direct-read-data-visible-to-userspace\n'
	printf 'write_completion=direct-write-completed-without-durability-barrier\n'
	printf 'sync_completion=each-direct-write-followed-by-fsync-drain-via-psync-engine\n'
	printf 'fio_version=%s kernel=%s host=%s\n' "$(fio --version)" "$(uname -r)" "$(hostname)"
	for queue in "/sys/block/${device##*/}/mq/"*; do
		printf 'hctx=%s cpus=%s\n' "${queue##*/}" "$(cat "$queue/cpu_list")"
	done
} > "$topology"
cat "$topology"

common=(
	--filename="$file" --ioengine=io_uring --direct=1 --bs=4k
	--fixedbufs=1 --registerfiles=1 --time_based=1 --group_reporting=1
	--randrepeat=0 --norandommap=1 --ramp_time="$ramp" --runtime="$runtime"
	--output-format=json
)

if [ "${FIO_SKIP_PRECONDITION:-0}" != 1 ]; then
	printf 'precondition: sequential direct write of 256 GiB\n'
	fio --name=precondition --filename="$file" --ioengine=io_uring --direct=1 \
		--rw=write --bs=1M --size=256G --iodepth=64 --iodepth_batch_submit=16 \
		--iodepth_batch_complete_min=16 --fixedbufs=1 --registerfiles=1 \
		--cpus_allowed="$low_cpu" --cpus_allowed_policy=shared \
		--output-format=json --output="$outdir/precondition.json"
	# Keep the io_uring data-path measurement separate from the durability
	# barrier. Some kernel/filesystem pairs reject fio's io_uring end_fsync.
	sync -f "$file"
fi

for repeat in $(seq 1 "$repeats"); do
	for qd in 1 2 4 8 16; do
		for mode in randread randwrite; do
			printf 'low-qd repeat=%s mode=%s per_worker_qd=%s workers=1 lanes=1 aggregate_outstanding=%s cpu=%s hctx=%s\n' \
				"$repeat" "$mode" "$qd" "$qd" "$low_cpu" "$low_hctx"
			fio --name="low-${mode}-q${qd}-r${repeat}" "${common[@]}" \
				--rw="$mode" --size=256G --iodepth="$qd" --iodepth_batch_submit=1 \
				--iodepth_batch_complete_min=1 --iodepth_batch_complete_max="$qd" \
				--numjobs=1 --cpus_allowed="$low_cpu" --cpus_allowed_policy=shared \
				--output="$outdir/low-${mode}-q${qd}-r${repeat}.json"
		done
	done
done

for repeat in $(seq 1 "$repeats"); do
	printf 'sync-drain repeat=%s per_worker_qd=1 workers=1 lanes=1 aggregate_outstanding=1 cpu=%s hctx=%s\n' \
		"$repeat" "$low_cpu" "$low_hctx"
	fio --name="sync-r${repeat}" --filename="$file" --ioengine=psync --direct=1 \
		--bs=4k --rw=randwrite --size=256G --time_based=1 --group_reporting=1 \
		--randrepeat=0 --norandommap=1 --ramp_time="$ramp" --runtime="$runtime" \
		--fsync=1 --numjobs=1 --cpus_allowed="$low_cpu" \
		--cpus_allowed_policy=shared --output-format=json \
		--output="$outdir/sync-r${repeat}.json"
done

for repeat in $(seq 1 "$repeats"); do
	for mode in randread randwrite; do
		printf 'saturation repeat=%s mode=%s per_worker_qd=64 workers=32 lanes=32 aggregate_outstanding=2048 cpu_list=%s\n' \
			"$repeat" "$mode" "$high_cpu_list"
		fio --name="sat-${mode}-r${repeat}" "${common[@]}" --rw="$mode" \
			--size=8G --offset_increment=8G --iodepth=64 --iodepth_batch_submit=16 \
			--iodepth_batch_complete_min=16 --iodepth_batch_complete_max=64 \
			--numjobs=32 --cpus_allowed="$high_cpu_list" --cpus_allowed_policy=split \
			--output="$outdir/sat-${mode}-r${repeat}.json"
	done
done

printf 'fio-terminal-leaf: complete outdir=%s\n' "$outdir"
