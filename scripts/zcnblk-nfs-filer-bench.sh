#!/usr/bin/env bash
set -euo pipefail

PATH="/usr/sbin:/usr/bin:/sbin:/bin:$PATH"
export PATH

usage() {
	cat <<'EOF'
Usage: zcnblk-nfs-filer-bench.sh RESULT_DIR DATA_ROOT

Exports a directory below DATA_ROOT with kernel NFSv4.2, mounts it over
loopback, verifies data, and runs metadata, synchronous-write, random mixed,
and sequential fio phases. The script never edits /etc/exports.

Optional environment:
  SERVER_CPUS=4-15 CLIENT_CPUS=0,16-23 NFS_THREADS=16 NCONNECT=8
  METADATA_FILES=1000 WORKING_SET=256m STREAM_SIZE=512m
  SYNC_SECONDS=3 RANDOM_SECONDS=5 RECORD_SIZE=4096
  CLIENT_MOUNT=/mnt/zc-nfs-client-bench
  EXPECT_ZCNBLK=0 COORDINATION_RESULT=unknown
EOF
}

[ "$#" -eq 2 ] || { usage >&2; exit 2; }
RESULT_DIR="$1"
DATA_ROOT="$2"
SERVER_CPUS="${SERVER_CPUS:-4-15}"
CLIENT_CPUS="${CLIENT_CPUS:-0,16-23}"
NFS_THREADS="${NFS_THREADS:-16}"
NCONNECT="${NCONNECT:-8}"
METADATA_FILES="${METADATA_FILES:-1000}"
WORKING_SET="${WORKING_SET:-256m}"
STREAM_SIZE="${STREAM_SIZE:-512m}"
SYNC_SECONDS="${SYNC_SECONDS:-3}"
RANDOM_SECONDS="${RANDOM_SECONDS:-5}"
RECORD_SIZE="${RECORD_SIZE:-4096}"
CLIENT_MOUNT="${CLIENT_MOUNT:-/mnt/zc-nfs-client-bench}"
EXPECT_ZCNBLK="${EXPECT_ZCNBLK:-0}"
COORDINATION_RESULT="${COORDINATION_RESULT:-unknown}"
EXPORT_OPTIONS="rw,sync,no_subtree_check,no_root_squash,fsid=0"
MOUNT_OPTIONS="vers=4.2,proto=tcp,port=2049,nconnect=$NCONNECT,hard,noatime"

die() { printf 'zcnblk-nfs-filer-bench: ERROR: %s\n' "$*" >&2; exit 1; }
for tool in exportfs mount.nfs nfsstat fio taskset systemctl; do
	command -v "$tool" >/dev/null 2>&1 || die "required executable not found: $tool"
done
command -v sudo >/dev/null || die 'sudo is required'
sudo -n true || die 'passwordless sudo is required'
[ -d "$DATA_ROOT" ] && [ -w "$DATA_ROOT" ] ||
	die "DATA_ROOT must be an existing writable directory: $DATA_ROOT"

mkdir -p "$RESULT_DIR"
RESULT_DIR="$(realpath "$RESULT_DIR")"
DATA_ROOT="$(realpath "$DATA_ROOT")"
if [ "$EXPECT_ZCNBLK" = 1 ]; then
	mount_source="$(findmnt -T "$DATA_ROOT" -n -o SOURCE)"
	[ "$mount_source" = /dev/zcnblk0 ] ||
		die "EXPECT_ZCNBLK=1 but DATA_ROOT is mounted from $mount_source"
fi
DATA_DIR="$(mktemp -d "$DATA_ROOT/zcutils-nfs-data.XXXXXX")"
EXPORT_DIR="$DATA_DIR/export"
SERVER_WAS_ACTIVE=0
SERVER_STARTED=0
NFS_THREADS_BEFORE=0
EXPORTED=0
CLIENT_MOUNTED=0
SERVER_PIDS=()
declare -A SERVER_AFFINITY_BEFORE=()
declare -A SERVER_COMM_BEFORE=()
NFS_RELATED_UNITS=(
	nfs-server.service
	nfs-mountd.service
	nfs-idmapd.service
	nfsdcld.service
	rpc-statd-notify.service
	rpc-statd.service
	rpcbind.target
	rpcbind.service
	rpcbind.socket
	rpc_pipefs.target
	run-rpc_pipefs.mount
	proc-fs-nfsd.mount
	auth-rpcgss-module.service
	gssproxy.service
	rpc-gssd.service
	rpc-svcgssd.service
)
declare -A UNIT_ACTIVE_BEFORE=()
for unit in "${NFS_RELATED_UNITS[@]}"; do
	if systemctl is-active --quiet "$unit"; then
		UNIT_ACTIVE_BEFORE[$unit]=1
	else
		UNIT_ACTIVE_BEFORE[$unit]=0
	fi
done

unmount_client() {
	[ "$CLIENT_MOUNTED" -ne 0 ] || return 0
	for _ in $(seq 1 100); do
		if sudo -n umount "$CLIENT_MOUNT" >>"$RESULT_DIR/cleanup.log" 2>&1; then
			CLIENT_MOUNTED=0
			return 0
		fi
		sleep 0.1
	done
	return 1
}

cleanup() {
	local status=$?
	trap - EXIT INT TERM
	set +e
	unmount_client
	if [ "$EXPORTED" -ne 0 ]; then
		sudo -n exportfs -u "127.0.0.1:$EXPORT_DIR" >>"$RESULT_DIR/cleanup.log" 2>&1
	fi
	if [ "$SERVER_WAS_ACTIVE" -ne 0 ]; then
		for pid in "${!SERVER_AFFINITY_BEFORE[@]}"; do
			[ -r "/proc/$pid/comm" ] || continue
			[ "$(cat "/proc/$pid/comm")" = "${SERVER_COMM_BEFORE[$pid]}" ] || continue
			sudo -n taskset -pc "${SERVER_AFFINITY_BEFORE[$pid]}" "$pid" \
				>>"$RESULT_DIR/cleanup.log" 2>&1
		done
	fi
	if [ "$SERVER_STARTED" -ne 0 ]; then
		sudo -n systemctl stop nfs-server.service >>"$RESULT_DIR/cleanup.log" 2>&1
		for unit in "${NFS_RELATED_UNITS[@]:1}"; do
			[ "${UNIT_ACTIVE_BEFORE[$unit]}" -eq 0 ] || continue
			systemctl is-active --quiet "$unit" || continue
			sudo -n systemctl stop "$unit" >>"$RESULT_DIR/cleanup.log" 2>&1
		done
	fi
	sudo -n rmdir "$CLIENT_MOUNT" >>"$RESULT_DIR/cleanup.log" 2>&1
	rm -rf -- "$DATA_DIR"
	exit "$status"
}
trap cleanup EXIT INT TERM

context_snapshot() {
	local output="$1" pid status
	: >"$output"
	for pid in "${SERVER_PIDS[@]}"; do
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
	done
}

record_cmd() {
	printf '%q ' "$@" >>"$RESULT_DIR/commands.sh"
	printf '\n' >>"$RESULT_DIR/commands.sh"
}

run_fio() {
	local phase="$1"
	shift
	context_snapshot "$RESULT_DIR/context-$phase-before.txt"
	record_cmd taskset -c "$CLIENT_CPUS" fio "$@"
	/usr/bin/time \
		-f 'elapsed_seconds=%e\nuser_seconds=%U\nsystem_seconds=%S\nmax_rss_kb=%M\nvoluntary_context_switches=%w\ninvoluntary_context_switches=%c' \
		-o "$RESULT_DIR/$phase.time" \
		taskset -c "$CLIENT_CPUS" fio --output="$RESULT_DIR/$phase.json" \
		--output-format=json "$@" >"$RESULT_DIR/$phase.console" 2>&1
	context_snapshot "$RESULT_DIR/context-$phase-after.txt"
}

[ ! -e "$CLIENT_MOUNT" ] || sudo -n rmdir "$CLIENT_MOUNT" ||
	die "client mountpoint already exists and is not empty: $CLIENT_MOUNT"
sudo -n mkdir -p "$CLIENT_MOUNT"
mkdir -p "$EXPORT_DIR"
if sudo -n exportfs -v | grep -q '[^[:space:]]'; then
	die 'existing NFS exports make an isolated filer benchmark unsafe'
fi
if systemctl is-active --quiet nfs-server.service; then
	SERVER_WAS_ACTIVE=1
else
	sudo -n systemctl start nfs-server.service
	SERVER_STARTED=1
fi
NFS_THREADS_BEFORE=$(sudo -n cat /proc/fs/nfsd/threads)
if [ "$NFS_THREADS_BEFORE" != "$NFS_THREADS" ]; then
	[ "$SERVER_WAS_ACTIVE" -eq 0 ] ||
		die "active nfsd has $NFS_THREADS_BEFORE workers; use NFS_THREADS=$NFS_THREADS_BEFORE or stop it before retuning"
	sudo -n /usr/sbin/nfsdctl threads "$NFS_THREADS"
fi
NFS_THREADS_ACTUAL=$(sudo -n cat /proc/fs/nfsd/threads)
[ "$NFS_THREADS_ACTUAL" = "$NFS_THREADS" ] ||
	die "requested $NFS_THREADS nfsd workers but kernel reports $NFS_THREADS_ACTUAL"

record_cmd sudo exportfs -i -o "$EXPORT_OPTIONS" "127.0.0.1:$EXPORT_DIR"
sudo -n exportfs -i -o "$EXPORT_OPTIONS" "127.0.0.1:$EXPORT_DIR"
EXPORTED=1
sudo -n exportfs -v >"$RESULT_DIR/exports.txt"

record_cmd sudo mount -t nfs4 -o "$MOUNT_OPTIONS" 127.0.0.1:/ "$CLIENT_MOUNT"
sudo -n mount -t nfs4 -o "$MOUNT_OPTIONS" 127.0.0.1:/ "$CLIENT_MOUNT"
CLIENT_MOUNTED=1
sudo -n chown "$(id -u):$(id -g)" "$EXPORT_DIR"
mkdir -p "$CLIENT_MOUNT/metadata"

mapfile -t SERVER_PIDS < <(ps -e -o pid=,comm= | awk '$2 == "nfsd" || $2 == "rpc.mountd" {print $1}')
[ "${#SERVER_PIDS[@]}" -gt 0 ] || die 'NFS server exposed no accountable worker processes'
for pid in "${SERVER_PIDS[@]}"; do
	SERVER_COMM_BEFORE[$pid]="$(cat "/proc/$pid/comm")"
	affinity_output="$(taskset -pc "$pid")"
	SERVER_AFFINITY_BEFORE[$pid]="${affinity_output##*: }"
	sudo -n taskset -pc "$SERVER_CPUS" "$pid" >>"$RESULT_DIR/server-affinity.txt" 2>&1
done

{
	printf 'timestamp_utc=%s\nhostname=%s\nkernel=%s\n' \
		"$(date -u +%FT%TZ)" "$(hostname)" "$(uname -srvo)"
	printf 'data_root=%s\ndata_mount=%s\n' "$DATA_ROOT" "$(findmnt -T "$DATA_ROOT" -n -o SOURCE,TARGET,FSTYPE,OPTIONS)"
	printf 'client_mount=%s\nclient_mount_info=%s\n' "$CLIENT_MOUNT" "$(findmnt -T "$CLIENT_MOUNT" -n -o SOURCE,TARGET,FSTYPE,OPTIONS)"
	printf 'nfs_version=4.2\nexport_options=%s\nmount_options=%s\n' "$EXPORT_OPTIONS" "$MOUNT_OPTIONS"
	printf 'server_cpus=%s\nclient_cpus=%s\nnfs_threads_before=%s\nnfs_threads_requested=%s\nnfs_threads_actual=%s\n' \
		"$SERVER_CPUS" "$CLIENT_CPUS" "$NFS_THREADS_BEFORE" "$NFS_THREADS" "$NFS_THREADS_ACTUAL"
	printf 'qd1_workers=1\nqd1_per_worker_depth=1\nqd1_aggregate_depth=1\n'
	printf 'qd16_workers=4\nqd16_per_worker_depth=4\nqd16_aggregate_depth=16\n'
	printf 'sync_write_completion=client-fsync-to-NFS-COMMIT-on-sync-export\n'
	printf 'stream_read_cache_state=warm-or-shared-system-unknown;global-caches-not-dropped\n'
	printf 'coordination=%s\nserver_was_active=%s\n' "$COORDINATION_RESULT" "$SERVER_WAS_ACTIVE"
	printf 'zcnblk_device=%s\n' "$(test -b /dev/zcnblk0 && printf present || printf absent)"
	lscpu
} >"$RESULT_DIR/topology.txt"
fio --version >"$RESULT_DIR/fio-version.txt"
nfsstat --version >"$RESULT_DIR/nfsstat-version.txt" 2>&1 || true
nfsstat -c -s >"$RESULT_DIR/nfsstat-before.txt" 2>&1
cat /proc/net/dev >"$RESULT_DIR/net-dev-before.txt"

run_fio verify --name=verify --filename="$CLIENT_MOUNT/verify.bin" --rw=write \
	--bs=4k --size=16m --ioengine=sync --direct=0 --verify=crc32c \
	--do_verify=1 --verify_fatal=1 --end_fsync=1 --group_reporting=1

run_fio metadata-create --name=metadata --directory="$CLIENT_MOUNT/metadata" \
	--rw=write --bs=4k --size="$((METADATA_FILES * 4096))" --nrfiles="$METADATA_FILES" \
	--filesize=4k --openfiles=64 --file_service_type=roundrobin --ioengine=sync \
	--direct=0 --fsync_on_close=1 --create_on_open=1 --unlink=1 --numjobs=1 \
	--group_reporting=1

run_fio sync-write-qd1 --name=sync-write --filename="$CLIENT_MOUNT/sync.bin" \
	--rw=randwrite --bs="$RECORD_SIZE" --size=64m --time_based=1 --runtime="$SYNC_SECONDS" \
	--ioengine=sync --direct=0 --fsync=1 --iodepth=1 --numjobs=1 --group_reporting=1

run_fio seed-random --name=seed --filename="$CLIENT_MOUNT/random.bin" --rw=write \
	--bs=1m --size="$WORKING_SET" --ioengine=io_uring --direct=1 --iodepth=16 \
	--numjobs=1 --end_fsync=1 --group_reporting=1

run_fio random-mixed-qd1 --name=random-mixed-qd1 --filename="$CLIENT_MOUNT/random.bin" \
	--rw=randrw --rwmixread=70 --bs="$RECORD_SIZE" --size="$WORKING_SET" \
	--time_based=1 --runtime="$RANDOM_SECONDS" --ioengine=io_uring --direct=1 \
	--iodepth=1 --numjobs=1 --norandommap=1 --randrepeat=1 --group_reporting=1

run_fio random-mixed-qd16 --name=random-mixed-qd16 --filename="$CLIENT_MOUNT/random.bin" \
	--rw=randrw --rwmixread=70 --bs="$RECORD_SIZE" --size="$WORKING_SET" \
	--time_based=1 --runtime="$RANDOM_SECONDS" --ioengine=io_uring --direct=1 \
	--iodepth=4 --numjobs=4 --norandommap=1 --randrepeat=1 --group_reporting=1

run_fio stream-write --name=stream-write --filename="$CLIENT_MOUNT/stream.bin" \
	--rw=write --bs=1m --size="$STREAM_SIZE" --ioengine=io_uring --direct=1 \
	--iodepth=16 --numjobs=1 --end_fsync=1 --group_reporting=1

run_fio stream-read-warm --name=stream-read --filename="$CLIENT_MOUNT/stream.bin" \
	--rw=read --bs=1m --size="$STREAM_SIZE" --ioengine=io_uring --direct=1 \
	--iodepth=16 --numjobs=1 --group_reporting=1

nfsstat -c -s >"$RESULT_DIR/nfsstat-after.txt" 2>&1
cat /proc/net/dev >"$RESULT_DIR/net-dev-after.txt"

python3 - "$RESULT_DIR" <<'PY'
import csv
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
phases = ["metadata-create", "sync-write-qd1", "random-mixed-qd1",
          "random-mixed-qd16", "stream-write", "stream-read-warm"]

def percentile(io, latency_key, key):
    values = io.get(latency_key, {}).get("percentile", {})
    value = values.get(key, 0)
    return float(value) / 1000.0

def weighted_mean_us(stats, latency_key):
    samples = sum(int(item.get(latency_key, {}).get("N", 0)) for item in stats)
    if not samples:
        return 0.0
    total = sum(float(item.get(latency_key, {}).get("mean", 0))
                * int(item.get(latency_key, {}).get("N", 0)) for item in stats)
    return total / samples / 1000.0

rows = []
for phase in phases:
    payload = json.loads((root / f"{phase}.json").read_text())
    jobs = payload["jobs"]
    read_iops = sum(float(job["read"].get("iops", 0)) for job in jobs)
    write_iops = sum(float(job["write"].get("iops", 0)) for job in jobs)
    read_bw = sum(int(job["read"].get("bw_bytes", 0)) for job in jobs)
    write_bw = sum(int(job["write"].get("bw_bytes", 0)) for job in jobs)
    read_mean = sum(float(job["read"].get("clat_ns", {}).get("mean", 0)) for job in jobs) / max(len(jobs), 1) / 1000
    write_mean = sum(float(job["write"].get("clat_ns", {}).get("mean", 0)) for job in jobs) / max(len(jobs), 1) / 1000
    sync_stats = [job.get("sync", {}) for job in jobs]
    sync_ios = sum(int(sync.get("total_ios", 0)) for sync in sync_stats)
    sync_mean = weighted_mean_us(sync_stats, "lat_ns")
    errors = sum(int(job.get("error", 0)) for job in jobs)
    rows.append([phase, read_iops, write_iops, read_bw, write_bw, read_mean,
                 write_mean,
                 max((percentile(job["read"], "clat_ns", "95.000000") for job in jobs), default=0),
                 max((percentile(job["write"], "clat_ns", "95.000000") for job in jobs), default=0),
                 max((percentile(job["read"], "clat_ns", "99.000000") for job in jobs), default=0),
                 max((percentile(job["write"], "clat_ns", "99.000000") for job in jobs), default=0),
                 sync_ios, sync_mean,
                 max((percentile(sync, "lat_ns", "95.000000") for sync in sync_stats), default=0),
                 max((percentile(sync, "lat_ns", "99.000000") for sync in sync_stats), default=0),
                 errors])

with (root / "summary.csv").open("w", newline="") as handle:
    writer = csv.writer(handle)
    writer.writerow(["phase", "read_iops", "write_iops", "read_bytes_per_second",
                     "write_bytes_per_second", "read_clat_mean_us", "write_clat_mean_us",
                     "read_clat_p95_us", "write_clat_p95_us", "read_clat_p99_us",
                     "write_clat_p99_us", "sync_ios", "sync_lat_mean_us",
                     "sync_lat_p95_us", "sync_lat_p99_us", "errors"])
    writer.writerows(rows)
PY

{
	printf 'phase,voluntary_context_switches,nonvoluntary_context_switches\n'
	for phase in metadata-create sync-write-qd1 random-mixed-qd1 random-mixed-qd16 stream-write stream-read-warm; do
		awk -v phase="$phase" '
			NR == FNR { voluntary[$1]=$3; involuntary[$1]=$4; next }
			($1 in voluntary) { v += $3-voluntary[$1]; iv += $4-involuntary[$1] }
			END { printf "%s,%d,%d\n", phase, v, iv }
		' "$RESULT_DIR/context-$phase-before.txt" "$RESULT_DIR/context-$phase-after.txt"
	done
} >"$RESULT_DIR/context-switches.csv"

printf 'results=%s\n' "$RESULT_DIR"
cat "$RESULT_DIR/summary.csv"
cat "$RESULT_DIR/context-switches.csv"
