#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${TARGET:-$ROOT/target/release/zciscsi-target}"
ARENA_SOCKET="${ARENA_SOCKET:?set ARENA_SOCKET to the established userspace-stage arena}"
ZCNBLK_DEVICE="${ZCNBLK_DEVICE:-/dev/zcnblk0}"
LISTEN="${LISTEN:-127.0.0.1:3260}"
PORT="${LISTEN##*:}"
IQN="${IQN:-iqn.2026-08.local.zcutils:fio-volume}"
LANE_CPUS="${LANE_CPUS:?set the complete arena lane-to-CPU list}"
FIO_CPUS="${FIO_CPUS:-99,111,123,135,147,159,171,183,3,15,27,39,51,63,75,87}"
WORK_DIR="${WORK_DIR:-/tmp/zciscsi-openiscsi-fio}"
RUNTIME="${RUNTIME:-10}"
RAMP_TIME="${RAMP_TIME:-2}"
NUMJOBS="${NUMJOBS:-16}"
IODEPTH="${IODEPTH:-64}"
RW="${RW:-randread}"
TEST_SIZE="${TEST_SIZE:-1G}"
ISCSI_CMDS_MAX="${ISCSI_CMDS_MAX:-2048}"
ISCSI_QUEUE_DEPTH="${ISCSI_QUEUE_DEPTH:-1024}"
ISCSI_NR_SESSIONS="${ISCSI_NR_SESSIONS:-16}"
LOG="$WORK_DIR/target.log"

for command in fio iscsiadm jq lsblk sg_sync ss; do
    command -v "$command" >/dev/null || {
        printf 'missing required command: %s\n' "$command" >&2
        exit 1
    }
done
[[ -x "$TARGET" ]]
[[ -S "$ARENA_SOCKET" ]]
[[ -b "$ZCNBLK_DEVICE" ]]
mkdir -p "$WORK_DIR"
rm -f -- "$LOG" "$WORK_DIR/fio.json" "$WORK_DIR/discovery.log" \
    "$WORK_DIR/login.log" "$WORK_DIR/logout.log" "$WORK_DIR/lsblk.log"

target_pid=""
logged_in=0
cleanup() {
    local status=$?
    if [[ "$logged_in" == 1 ]]; then
        sudo -n iscsiadm -m node -T "$IQN" -p "$LISTEN" --logout >/dev/null 2>&1 || true
    fi
    sudo -n iscsiadm -m node -o delete -T "$IQN" -p "$LISTEN" >/dev/null 2>&1 || true
    if [[ -n "$target_pid" ]] && sudo -n test -d "/proc/$target_pid"; then
        sudo -n kill -TERM "$target_pid" 2>/dev/null || true
        for _ in $(seq 1 50); do
            sudo -n test ! -d "/proc/$target_pid" && break
            sleep 0.05
        done
        sudo -n test ! -d "/proc/$target_pid" || sudo -n kill -KILL "$target_pid" 2>/dev/null || true
    fi
    wait "$target_pid" 2>/dev/null || true
    exit "$status"
}
trap cleanup EXIT INT TERM

sudo -n "$TARGET" \
    --listen "$LISTEN" \
    --target "$IQN" \
    --arena-socket "$ARENA_SOCKET" \
    --zcnblk-device "$ZCNBLK_DEVICE" \
    --block-size 4096 \
    --lane-cpus "$LANE_CPUS" >"$LOG" 2>&1 &
target_pid=$!

for _ in $(seq 1 100); do
    ss -ltnH "sport = :$PORT" | grep -q . && break
    sudo -n test -d "/proc/$target_pid" || {
        sed -n '1,200p' "$LOG" >&2
        exit 1
    }
    sleep 0.05
done
ss -ltnH "sport = :$PORT" | grep -q .

sudo -n iscsiadm -m discovery -t sendtargets -p "$LISTEN" >"$WORK_DIR/discovery.log"
grep -Fq "$IQN" "$WORK_DIR/discovery.log"
sudo -n iscsiadm -m node -T "$IQN" -p "$LISTEN" -o update \
    -n node.session.cmds_max -v "$ISCSI_CMDS_MAX"
sudo -n iscsiadm -m node -T "$IQN" -p "$LISTEN" -o update \
    -n node.session.queue_depth -v "$ISCSI_QUEUE_DEPTH"
sudo -n iscsiadm -m node -T "$IQN" -p "$LISTEN" -o update \
    -n node.session.nr_sessions -v "$ISCSI_NR_SESSIONS"
sudo -n iscsiadm -m node -T "$IQN" -p "$LISTEN" --login >"$WORK_DIR/login.log"
logged_in=1

devices=()
for _ in $(seq 1 200); do
    devices=()
    for block_path in /sys/class/iscsi_session/session*/device/target*/*/block/*; do
        [[ -e "$block_path" ]] || continue
        devices+=("/dev/${block_path##*/}")
    done
    if (( ${#devices[@]} != 0 )); then
        mapfile -t devices < <(printf '%s\n' "${devices[@]}" | sort -u)
    fi
    (( ${#devices[@]} >= ISCSI_NR_SESSIONS )) && break
    sleep 0.05
done
(( ${#devices[@]} == ISCSI_NR_SESSIONS ))
size_bytes=""
for device in "${devices[@]}"; do
    [[ -b "$device" ]]
    logical_block="$(sudo -n blockdev --getss "$device")"
    physical_block="$(sudo -n blockdev --getpbsz "$device")"
    device_size="$(sudo -n blockdev --getsize64 "$device")"
    [[ "$logical_block" == 4096 && "$physical_block" == 4096 ]]
    (( device_size % 4096 == 0 ))
    if [[ -z "$size_bytes" ]]; then
        size_bytes="$device_size"
    else
        [[ "$device_size" == "$size_bytes" ]]
    fi
done
lsblk -b -o NAME,TYPE,SIZE,LOG-SEC,PHY-SEC "${devices[@]}" >"$WORK_DIR/lsblk.log"
filename="$(IFS=:; printf '%s' "${devices[*]}")"

# Offset, size, and every I/O are 4 KiB aligned. libaio needs direct=1 for
# genuinely asynchronous Linux I/O. Random-read is nondestructive and its
# completion means the remote payload has returned through the userspace stage.
sudo -n fio \
    --name=zciscsi-4k \
    --filename="$filename" \
    --file_service_type=roundrobin:1 \
    --rw="$RW" \
    --bs=4096 \
    --offset=4194304 \
    --size="$TEST_SIZE" \
    --direct=1 \
    --ioengine=libaio \
    --iodepth="$IODEPTH" \
    --numjobs="$NUMJOBS" \
    --time_based=1 \
    --runtime="$RUNTIME" \
    --ramp_time="$RAMP_TIME" \
    --randrepeat=1 \
    --norandommap=1 \
    --group_reporting=1 \
    --cpus_allowed="$FIO_CPUS" \
    --cpus_allowed_policy=split \
    --percentile_list=50:90:95:99:99.5:99.9:99.99 \
    --output-format=json+ \
    --output="$WORK_DIR/fio.json"

if [[ "$RW" == *write* ]]; then
    for device in "${devices[@]}"; do
        sudo -n sg_sync --sync-nv "$device"
    done
fi
sudo -n iscsiadm -m node -T "$IQN" -p "$LISTEN" --logout >"$WORK_DIR/logout.log"
logged_in=0

jq -r --arg rw "$RW" --argjson jobs "$NUMJOBS" --argjson qd "$IODEPTH" '
    .jobs[0] as $j
    | ($j[$rw] // (if ($rw | contains("read")) then $j.read else $j.write end)) as $io
    | "ZCISCSI_FIO_RESULT rw=\($rw) block_size=4096 numjobs=\($jobs) per_job_qd=\($qd) aggregate_outstanding=\($jobs * $qd) iops=\($io.iops) bandwidth_KiBps=\($io.bw) mean_clat_ns=\($io.clat_ns.mean) p99_ns=\($io.clat_ns.percentile["99.000000"]) p99_5_ns=\($io.clat_ns.percentile["99.500000"]) p99_9_ns=\($io.clat_ns.percentile["99.900000"])"' \
    "$WORK_DIR/fio.json"
printf 'ZCISCSI_FIO_TOPOLOGY devices=%s sessions=%s capacity_bytes_each=%s logical_block=%s physical_block=%s lane_to_cpu=%s fio_cpus=%s iscsi_cmds_max=%s iscsi_queue_depth=%s transport=iscsi-tcp-loopback backend=userspace-stage frontend_placement=no\n' \
    "$filename" "$ISCSI_NR_SESSIONS" "$size_bytes" "$logical_block" "$physical_block" "$LANE_CPUS" "$FIO_CPUS" "$ISCSI_CMDS_MAX" "$ISCSI_QUEUE_DEPTH"
