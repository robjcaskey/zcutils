#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${TARGET:-$ROOT/target/release/zciscsi-target}"
ARENA_SOCKET="${ARENA_SOCKET:-}"
ZCNBLK_DEVICE="${ZCNBLK_DEVICE:-/dev/zcnblk0}"
FAN_ADDRS="${FAN_ADDRS:-}"
CAPACITY_BYTES="${CAPACITY_BYTES:-}"
FAN_WINDOW="${FAN_WINDOW:-64}"
LISTEN="${LISTEN:-127.0.0.1:3260}"
HOST="${LISTEN%:*}"
PORT="${LISTEN##*:}"
IQN="${IQN:-iqn.2026-08.local.zcutils:fio-volume}"
LANE_CPUS="${LANE_CPUS:?set the complete arena lane-to-CPU list}"
RX_CPUS="${RX_CPUS:?set the complete iSCSI RX-to-CPU list}"
TX_CPUS="${TX_CPUS:?set the complete iSCSI TX-to-CPU list}"
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
SESSIONS_PER_LANE="${SESSIONS_PER_LANE:-1}"
LOG="$WORK_DIR/target.log"

for command in fio iscsiadm jq lsblk sg_sync ss timeout; do
    command -v "$command" >/dev/null || {
        printf 'missing required command: %s\n' "$command" >&2
        exit 1
    }
done
[[ -x "$TARGET" ]]
if [[ -n "$FAN_ADDRS" ]]; then
    [[ -z "$ARENA_SOCKET" ]]
    [[ "$CAPACITY_BYTES" =~ ^[0-9]+$ ]]
    (( CAPACITY_BYTES > 0 && CAPACITY_BYTES % 4096 == 0 ))
    [[ "$FAN_WINDOW" =~ ^[0-9]+$ ]]
    (( FAN_WINDOW > 0 && FAN_WINDOW <= 4096 ))
    backend_args=(--fan-addrs "$FAN_ADDRS" --capacity-bytes "$CAPACITY_BYTES" --fan-window "$FAN_WINDOW")
    backend_label="direct-userspace-raid-stage"
    endpoint="$FAN_ADDRS"
else
    [[ -n "$ARENA_SOCKET" && -S "$ARENA_SOCKET" ]]
    [[ -b "$ZCNBLK_DEVICE" ]]
    backend_args=(--arena-socket "$ARENA_SOCKET" --zcnblk-device "$ZCNBLK_DEVICE")
    backend_label="shared-arena-block-edge"
    endpoint="$ARENA_SOCKET"
fi
mkdir -p "$WORK_DIR"
rm -f -- "$LOG" "$WORK_DIR/fio.json" "$WORK_DIR/discovery.log" \
    "$WORK_DIR/login.log" "$WORK_DIR/logout.log" "$WORK_DIR/lsblk.log"

target_pid=""
logged_in=0
cleanup() {
    local status=$?
    if [[ "$logged_in" == 1 ]]; then
        sudo -n timeout -k 1 5 iscsiadm -m node -T "$IQN" -p "$LISTEN" --logout >/dev/null 2>&1 || true
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
    "${backend_args[@]}" \
    --block-size 4096 \
    --lane-cpus "$LANE_CPUS" \
    --rx-cpus "$RX_CPUS" \
    --tx-cpus "$TX_CPUS" \
    --sessions-per-lane "$SESSIONS_PER_LANE" >"$LOG" 2>&1 &
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
    for session_path in /sys/class/iscsi_session/session*; do
        [[ -r "$session_path/targetname" ]] || continue
        IFS= read -r session_target < "$session_path/targetname"
        [[ "$session_target" == "$IQN" ]] || continue
        session_id="${session_path##*session}"
        portal_match=0
        for connection_path in /sys/class/iscsi_connection/connection"$session_id":*; do
            [[ -r "$connection_path/persistent_address" && -r "$connection_path/persistent_port" ]] || continue
            IFS= read -r session_address < "$connection_path/persistent_address"
            IFS= read -r session_port < "$connection_path/persistent_port"
            if [[ "$session_address" == "$HOST" && "$session_port" == "$PORT" ]]; then
                portal_match=1
                break
            fi
        done
        (( portal_match == 1 )) || continue
        for block_path in "$session_path"/device/target*/*/block/*; do
            [[ -e "$block_path" ]] || continue
            devices+=("/dev/${block_path##*/}")
        done
    done
    if (( ${#devices[@]} != 0 )); then
        mapfile -t devices < <(printf '%s\n' "${devices[@]}" | sort -u)
    fi
    devices_ready=1
    for device in "${devices[@]}"; do
        if [[ ! -b "$device" ]] || ! sudo -n blockdev --getsize64 "$device" >/dev/null 2>&1; then
            devices_ready=0
            break
        fi
    done
    (( ${#devices[@]} >= ISCSI_NR_SESSIONS && devices_ready == 1 )) && break
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
printf 'ZCISCSI_FIO_TOPOLOGY devices=%s sessions=%s sessions_per_lane=%s capacity_bytes_each=%s logical_block=%s physical_block=%s lane_to_cpu=%s rx_to_cpu=%s tx_to_cpu=%s fio_cpus=%s role_cpu_sharing=within-role-session-shards-only iscsi_cmds_max=%s iscsi_queue_depth=%s transport=iscsi-tcp-loopback backend=%s endpoint=%s frontend_placement=no\n' \
    "$filename" "$ISCSI_NR_SESSIONS" "$SESSIONS_PER_LANE" "$size_bytes" "$logical_block" "$physical_block" "$LANE_CPUS" "$RX_CPUS" "$TX_CPUS" "$FIO_CPUS" "$ISCSI_CMDS_MAX" "$ISCSI_QUEUE_DEPTH" "$backend_label" "$endpoint"
