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
PORT="${LISTEN##*:}"
IQN="${IQN:-iqn.2026-08.local.zcutils:arena-volume}"
LANE_CPUS="${LANE_CPUS:?set the complete arena lane-to-CPU list}"
RX_CPUS="${RX_CPUS:?set the complete iSCSI RX-to-CPU list}"
TX_CPUS="${TX_CPUS:?set the complete iSCSI TX-to-CPU list}"
WORK_DIR="${WORK_DIR:-/tmp/zciscsi-openiscsi-smoke}"
LOG="$WORK_DIR/target.log"
ISCSI_CMDS_MAX="${ISCSI_CMDS_MAX:-2048}"
ISCSI_QUEUE_DEPTH="${ISCSI_QUEUE_DEPTH:-1024}"
SESSIONS_PER_LANE="${SESSIONS_PER_LANE:-1}"

for command in iscsiadm lsblk cmp dd sg_sync ss; do
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
else
    [[ -n "$ARENA_SOCKET" && -S "$ARENA_SOCKET" ]]
    [[ -b "$ZCNBLK_DEVICE" ]]
    backend_args=(--arena-socket "$ARENA_SOCKET" --zcnblk-device "$ZCNBLK_DEVICE")
    backend_label="shared-arena-block-edge"
fi
mkdir -p "$WORK_DIR"
rm -f -- "$LOG" "$WORK_DIR/expected" "$WORK_DIR/actual"

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
ss -ltnH "sport = :$PORT" | grep -q . || {
    printf 'iSCSI target did not listen on %s\n' "$LISTEN" >&2
    exit 1
}

sudo -n iscsiadm -m discovery -t sendtargets -p "$LISTEN" >"$WORK_DIR/discovery.log"
grep -Fq "$IQN" "$WORK_DIR/discovery.log"
sudo -n iscsiadm -m node -T "$IQN" -p "$LISTEN" -o update \
    -n node.session.cmds_max -v "$ISCSI_CMDS_MAX"
sudo -n iscsiadm -m node -T "$IQN" -p "$LISTEN" -o update \
    -n node.session.queue_depth -v "$ISCSI_QUEUE_DEPTH"
sudo -n iscsiadm -m node -T "$IQN" -p "$LISTEN" --login >"$WORK_DIR/login.log"
logged_in=1

device_link="/dev/disk/by-path/ip-$LISTEN-iscsi-$IQN-lun-0"
for _ in $(seq 1 200); do
    [[ -b "$device_link" ]] && break
    sleep 0.05
done
[[ -b "$device_link" ]] || {
    printf 'open-iscsi did not publish %s\n' "$device_link" >&2
    exit 1
}
device="$(readlink -f "$device_link")"
sudo -n blockdev --getss "$device" | grep -qx 4096
sudo -n blockdev --getpbsz "$device" | grep -qx 4096
size_bytes="$(sudo -n blockdev --getsize64 "$device")"
(( size_bytes % 4096 == 0 ))
lsblk -b -o NAME,TYPE,SIZE,LOG-SEC,PHY-SEC "$device" >"$WORK_DIR/lsblk.log"

dd if=/bin/busybox of="$WORK_DIR/expected" bs=4096 count=1 status=none
sudo -n dd if="$WORK_DIR/expected" of="$device" bs=4096 count=1 seek=32 \
    oflag=direct conv=fsync status=none
# Issue the SCSI command explicitly. Linux may otherwise elide a generic
# block-layer flush when a target advertises no volatile write cache.
sudo -n sg_sync --sync-nv "$device"
sudo -n dd if="$device" of="$WORK_DIR/actual" bs=4096 count=1 skip=32 \
    iflag=direct status=none
cmp "$WORK_DIR/expected" "$WORK_DIR/actual"

sudo -n iscsiadm -m node -T "$IQN" -p "$LISTEN" --logout >"$WORK_DIR/logout.log"
logged_in=0
grep -q 'implementation=zcutils-rfc7143-from-scratch external_iscsi_library=no' "$LOG"
grep -q 'placement_owner=downstream-userspace-raid frontend_placement=no mirror_primitive=no' "$LOG"

printf 'ZCISCSI_OPENISCSI_PASS target=%s device=%s capacity_bytes=%s logical_block=4096 physical_block=4096 io_offset=131072 io_bytes=4096 flush=sync-cache readback=match backend=%s endpoint=%s lane_to_cpu=%s frontend_placement=no\n' \
    "$IQN" "$device" "$size_bytes" "$backend_label" "${FAN_ADDRS:-$ARENA_SOCKET}" "${LANE_CPUS:-unmapped-nonstrict}"
