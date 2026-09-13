#!/bin/sh
set -eu
export PATH=/bin:/sbin:/usr/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
role=unknown
slow=middle
frontend=shared-arena
for arg in $(cat /proc/cmdline); do
    case "$arg" in zccl.role=*) role=${arg#zccl.role=} ;; zccl.slow=*) slow=${arg#zccl.slow=} ;; zccl.frontend=*) frontend=${arg#zccl.frontend=} ;; esac
done
fail() { echo "CLIENT_WAL_LIVE_QEMU_FAIL role=$role reason=$*"; sync; poweroff -f; exit 1; }
wait_log() {
    path=$1 pattern=$2
    for i in $(seq 1 800); do
        grep -q "$pattern" "$path" 2>/dev/null && return 0
        sleep 0.05
    done
    cat "$path" 2>/dev/null || true
    return 1
}
while read -r module; do insmod "$module" || fail "module-$module"; done </modules/load-order
mkdir -p /mnt/terminal
mount -t ext4 /dev/vda /mnt/terminal || fail terminal-mount
ip link set lo up
ip link set eth0 up
case "$role" in client) suffix=1 ;; middle) suffix=2 ;; tail) suffix=3 ;; replacement) suffix=4 ;; *) fail unknown-role ;; esac
ip addr add "10.63.73.$suffix/24" dev eth0
export ZCCUSAN_COMMUNITY_SURVEY_ENABLED=0
export URING_PLAY_PIN_CPU_LIST=0,1
export URING_PLAY_TOPOLOGY_STRICT=0
export URING_PLAY_TOPOLOGY_FATAL=0
echo "CLIENT_WAL_LIVE_TOPOLOGY role=$role slow=$slow transport=tcp-unicast persistent_media=ext4-virtio-final-files placement=userspace shared_host=true representative_performance=false"

if [ "$role" != client ]; then
    if [ "$role" = "$slow" ]; then
        export ZC_CUSTODY_TEST_COMMIT_DELAY_MS=100
        export ZC_CUSTODY_TEST_COMMIT_DELAY_AFTER=16
    fi
    if [ "$role" = replacement ]; then export ZC_CUSTODY_TEST_COPY_DELAY_MS=100; fi
    /zcutils zcnblk-wal-custody peer "/configs/live-$role.json" >/tmp/peer.log 2>&1 &
    peer_pid=$!
    wait_log /tmp/peer.log '^custody-peer-ready:' || fail peer-start
    tail -f /tmp/peer.log >/dev/console &
    log_pid=$!
    if [ "$role" != replacement ]; then echo ready | nc -l -p 29998; fi
    nc -l -p 29999 >/tmp/stop
    kill -TERM "$peer_pid"
    wait "$peer_pid" 2>/dev/null || true
    kill -TERM "$log_pid"
    wait "$log_pid" 2>/dev/null || true
    /zcutils zcnblk-wal-custody inspect "/configs/live-$role.json" || fail persisted-image-reopen
    echo "CLIENT_WAL_LIVE_QEMU_PASS role=$role reopened_persistent_image=true"
else
    for address in 10.63.73.2 10.63.73.3; do
        ready=0
        for i in $(seq 1 100); do
            if [ "$(nc -w 1 "$address" 29998 2>/dev/null)" = ready ]; then ready=1; break; fi
            sleep 0.05
        done
        [ "$ready" = 1 ] || fail peer-unreachable
    done
    client_log=/tmp/target.log
    if [ "$frontend" = tcp-onramp ]; then
        client_log=/tmp/client.log
        /zcutils zcnblk-wal-custody client /configs/live-client.json >"$client_log" 2>&1 &
        client_pid=$!
        wait_log "$client_log" '^custody-live-ready:' || fail custody-start
        tail -f "$client_log" >/dev/console &
        log_pid=$!
    else
        [ "$frontend" = shared-arena ] || fail unknown-frontend
        export URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=custody-tcp
        export URING_PLAY_ZCNBLK_SHM_CLIENT_WAL_CONFIG=/configs/live-client.json
    fi
    insmod /modules/zcnblk_client_mod.ko transport=shm lanes=1 connections_per_lane=1 \
        size_mib=1 queues=1 queue_depth=32 max_frame_bytes=4096 pipeline_depth=32 \
        shm_ring_entries=128 shm_payload_entries=256 shm_poll_us=1000 pin_threads=0 || fail block-module
    export URING_PLAY_ZCNBLK_SHM_LEAF_ADDR=127.0.0.1:29600
    export URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1
    export URING_PLAY_ZCNBLK_SHM_WAL_COMPACT_WRITES=0
    export URING_PLAY_ZCNBLK_SHM_REMOTE_RESULT_RANGES=0
    export URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE=blocking
    export URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_ZC_REQUIRED=0
    /zcnblk-shm-target /dev/zcnblk-shmctl wal-tcp 1 1 1000 1000 10000 >/tmp/target.log 2>&1 &
    target_pid=$!
    wait_log /tmp/target.log '^zcnblk-shm-target:' || fail target-start
    tail -f /tmp/target.log >/dev/console &
    target_log_pid=$!
    sleep 1
    export ZCNBLK_EDGE_CONTINUITY_VOLUME_BYTES=1048576
    export ZCNBLK_EDGE_CONTINUITY_FUA_EVERY=4
    export ZCNBLK_EDGE_CONTINUITY_SYNC_CONTRACT=client-persistent-wal-plus-either-remote-durable-prefix
    /zcnblk-edge-continuity /dev/zcnblk0 0 32 1000 8 >/tmp/continuity.log 2>&1 &
    workload_pid=$!
    if ! wait_log /tmp/continuity.log '^zcnblk-edge-continuity-start:'; then
        cat /tmp/target.log
        cat "$client_log"
        fail workload-start
    fi
    echo 'CLIENT_WAL_LIVE_WORKLOAD_STARTED block_descriptor_open=true'
    if ! wait_log "$client_log" '^custody-live-rebuilt:'; then
        cat /tmp/target.log
        cat /tmp/continuity.log
        fail mirror-not-rebuilt
    fi
    # Keep changing and checking the SAME descriptor after returning to C->M->S.
    sleep 3
    kill -TERM "$workload_pid"
    wait "$workload_pid" || fail workload-failed
    cat /tmp/continuity.log
    grep -q '^ZCNBLK_EDGE_CONTINUITY_PASS .*open_descriptor_replaced=false .*mismatches=0 ' /tmp/continuity.log || fail stable-descriptor-proof
    kill -INT "$target_pid"
    wait "$target_pid" || fail target-stop
    kill -TERM "$target_log_pid"
    wait "$target_log_pid" 2>/dev/null || true
    if [ "$frontend" = tcp-onramp ]; then
        wait "$client_pid" || { cat "$client_log"; fail custody-final-drain; }
        kill -TERM "$log_pid"
        wait "$log_pid" 2>/dev/null || true
        cat "$client_log"
    fi
    cat /tmp/target.log
    grep -q '^custody-live-complete:.*rebuilt=true .*frontend_reconnects=0' "$client_log" || fail final-redundancy-proof
    for address in 10.63.73.3 10.63.73.4; do echo stop | nc -w 2 "$address" 29999 || fail peer-stop; done
    rmmod zcnblk_client_mod || fail block-module-unload
    dmesg >/tmp/dmesg.log
    if grep -Eq 'BUG:|Oops:|KASAN:|general protection fault|Kernel panic' /tmp/dmesg.log; then cat /tmp/dmesg.log; fail kernel-fault; fi
    echo 'CLIENT_WAL_LIVE_QEMU_PASS role=client same_open_block_descriptor=true client_reconnects=0'
fi
sync
umount /mnt/terminal || fail terminal-unmount
poweroff -f
