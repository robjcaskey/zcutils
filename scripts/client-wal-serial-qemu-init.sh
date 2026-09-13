#!/bin/sh
# Correctness smoke only: shared host, one lane, two vCPUs/guest, no perf claim.
export PATH=/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
role=unknown
failure=0
batch=0
for argument in $(cat /proc/cmdline); do
    case "$argument" in zccw.role=*) role=${argument#zccw.role=} ;; zccw.failure=*) failure=${argument#zccw.failure=} ;; zccw.batch=*) batch=${argument#zccw.batch=} ;; esac
done
fail() { echo "CLIENT_WAL_SERIAL_QEMU_FAIL role=$role reason=$*"; poweroff -f; }
while read -r module; do insmod "$module" || fail "module $module"; done </modules/load-order
ip link set lo up
ip link set eth0 up || fail network
case "$role" in client) suffix=1 ;; middle) suffix=2 ;; tail) suffix=3 ;; replacement) suffix=4 ;; *) fail role ;; esac
ip addr add "10.63.72.$suffix/24" dev eth0 || fail address
mkdir -p /mnt/terminal
mount -t ext4 /dev/vda /mnt/terminal || fail terminal
export ZCCUSAN_COMMUNITY_SURVEY_ENABLED=0
export URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0,1
export URING_PLAY_RAID_MIRROR_ACK_WINDOW=1
export URING_PLAY_RAID_MIRROR_TERMINAL_BATCH=$batch
export URING_PLAY_ZCNBLK_WAL_LEAF_SUBMIT_MODE=blocking
export URING_PLAY_ZCNBLK_PWAL_INTEGRITY=frame
export URING_PLAY_SEND_PATTERN=fill URING_PLAY_SEND_FILL_BYTE=90
echo "CLIENT_WAL_SERIAL_QEMU_START role=$role kernel=$(uname -r) persistent_terminal=ext4 lane=0 worker=0 cpu=0 forward_cpu=1 per_worker_qd=1 aggregate_qd=1 representative_performance=false"
if [ "$failure" = 1 ]; then
    export URING_PLAY_RAID_MIRROR_IDLE_TIMEOUT_MS=5000
    case "$role" in
        client)
            sleep 3
            export URING_PLAY_RAID_MIRROR_CLIENT_WAL_CONFIG=/configs/failure-client.json
            # The interrupted benchmark must fail, even when its cold repair
            # succeeds: its partial throughput is not a representative result.
            /zcutils zcraid-mirror-send tcp 10.63.72.2 42000,43000 512K 4K 1 /configs/plan.json efa rdm true
            status=$?
            [ "$status" != 0 ] || fail fault_not_injected
            ;;
        middle)
            sleep 1
            /zcutils zcraid-mirror-hop /configs/failure-middle.json
            fail middle_was_not_power_cut
            ;;
        tail)
            export URING_PLAY_ZCNBLK_WAL_LEAF_WRITE_DELAY_US=5000
            /zcutils zcraid-mirror-recv tcp 0.0.0.0 44000 1 512K 4K 1 /configs/plan.json efa rdm true \
                zcpwal:/mnt/terminal/tail.wal,/mnt/terminal/tail.base,512K,4M
            [ "$?" != 0 ] || fail middle_connection_did_not_fail
            ;;
    esac
    if [ "$role" != client ]; then
        unset URING_PLAY_ZCNBLK_WAL_LEAF_WRITE_DELAY_US
        /zcutils zcraid-repair-terminal "/configs/repair-$role.json" >/tmp/repair.log 2>&1
        status=$?
        cat /tmp/repair.log
        [ "$status" = 0 ] || fail repair_terminal
        through=$(sed -n 's/.*client-wal-repair-terminal-complete:.*through=\([0-9][0-9]*\).*/\1/p' /tmp/repair.log)
        [ -n "$through" ] || fail no_repaired_hwm
        dd if=/dev/zero bs=4096 count="$through" 2>/dev/null | tr '\000' Z >/tmp/expected
        dd if=/dev/zero bs=4096 count=$((128-through)) 2>/dev/null >>/tmp/expected
        cmp /tmp/expected "/mnt/terminal/$role.base" || fail recovered_payload
        echo "CLIENT_WAL_SERIAL_PAYLOAD role=$role through=$through bytes=524288 sha256=$(sha256sum /tmp/expected)"
    fi
    sync
    umount /mnt/terminal || fail unmount
    echo "CLIENT_WAL_SERIAL_QEMU_PASS role=$role replacement_test=true"
    poweroff -f
fi
case "$role" in
    client)
        sleep 3
        export URING_PLAY_RAID_MIRROR_CLIENT_WAL_CONFIG=/configs/client.json
        /zcutils zcraid-mirror-send tcp 10.63.72.2 42000,43000 32K 4K 1 /configs/plan.json efa rdm true || fail sender
        ;;
    middle)
        sleep 1
        /zcutils zcraid-mirror-hop /configs/middle.json || fail relay
        ;;
    tail)
        export URING_PLAY_ZCNBLK_WAL_LEAF_WRITE_DELAY_US=20000
        /zcutils zcraid-mirror-recv tcp 0.0.0.0 44000 1 32K 4K 1 /configs/plan.json efa rdm true \
            zcpwal:/mnt/terminal/tail.wal,/mnt/terminal/tail.base,64K,512K || fail receiver
        ;;
esac
if [ "$role" != client ]; then
    # Expected nonzero data is generated without depending on the transport.
    dd if=/dev/zero bs=4096 count=8 2>/dev/null | tr '\000' Z >/tmp/expected
    dd if="/mnt/terminal/$role.base" bs=4096 count=8 2>/dev/null >/tmp/actual
    cmp /tmp/expected /tmp/actual || fail payload
    echo "CLIENT_WAL_SERIAL_PAYLOAD role=$role bytes=32768 sha256=$(sha256sum /tmp/actual)"
fi
sync
umount /mnt/terminal || fail unmount
echo "CLIENT_WAL_SERIAL_QEMU_PASS role=$role"
poweroff -f
