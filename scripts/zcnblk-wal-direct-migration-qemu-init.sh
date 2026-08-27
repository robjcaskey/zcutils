#!/bin/sh

set -u

export PATH=/usr/bin:/bin:/sbin

result=0
leaf_pid=""
target_pid=""
continuity_pid=""

fail()
{
	echo "ZCNBLK_DIRECT_MIGRATION_QEMU_FAIL role=$role reason=$*"
	result=1
}

wait_for_path()
{
	path="$1"
	for _ in $(seq 1 200); do
		[ -e "$path" ] && return 0
		sleep 0.05
	done
	return 1
}

wait_for_log()
{
	path="$1"
	pattern="$2"
	for _ in $(seq 1 400); do
		grep -q "$pattern" "$path" 2>/dev/null && return 0
		sleep 0.05
	done
	return 1
}

stop_exact()
{
	pid="$1"
	signal="$2"
	[ -n "$pid" ] && [ -e "/proc/$pid" ] || return 0
	kill "-$signal" "$pid" 2>/dev/null || true
	for _ in $(seq 1 200); do
		[ ! -e "/proc/$pid" ] && return 0
		sleep 0.02
	done
	return 1
}

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /tmp

role=unknown
case " $(cat /proc/cmdline) " in
	*" zcdm.role=source "*) role=source ;;
	*" zcdm.role=destination "*) role=destination ;;
	*" zcdm.role=client "*) role=client ;;
esac

insmod /modules/failover.ko || fail "failover-module"
insmod /modules/net_failover.ko || fail "net-failover-module"
insmod /modules/virtio_net.ko || fail "virtio-net-module"
ip link set lo up
ip link set eth0 up
case "$role" in
	source) address=10.83.0.2 ;;
	destination) address=10.83.0.3 ;;
	client) address=10.83.0.4 ;;
	*) fail "unknown-role"; address=10.83.0.254 ;;
esac
ip address add "$address/24" dev eth0 || fail "address-config"
echo "ZCNBLK_DIRECT_MIGRATION_QEMU_TOPOLOGY role=$role address=$address transport=tcp-unicast placement=userspace block_placement=false migration_proxy=false representative=false"

if [ "$role" = source ] || [ "$role" = destination ]; then
	URING_PLAY_PIN_CPU_LIST=0,1 \
	URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
	URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
	URING_PLAY_ZCNBLK_WAL_LEAF_DYNAMIC_ACCEPT=1 \
		/zcnblk-wal-leaf zcmem:64M "$address" 29600 1 2 4096 2 true blocking \
		>/tmp/leaf.log 2>&1 &
	leaf_pid=$!
	wait_for_log /tmp/leaf.log '^zcnblk-wal-leaf:' || fail "leaf-start"
	if [ "$result" -eq 0 ]; then
		echo "ZCNBLK_DIRECT_MIGRATION_QEMU_LEAF_READY role=$role"
		echo ready | nc -l -p 29998
		nc -l -p 29999 >/tmp/shutdown-request
	fi
	stop_exact "$leaf_pid" TERM || fail "leaf-stop"
	wait "$leaf_pid" 2>/dev/null || true
	cat /tmp/leaf.log
	if [ "$result" -eq 0 ]; then
		echo "ZCNBLK_DIRECT_MIGRATION_QEMU_PASS role=$role terminal_leaf=true"
	fi
else
	for peer in 10.83.0.2 10.83.0.3; do
		ready=0
		for _ in $(seq 1 200); do
			if [ "$(nc -w 1 "$peer" 29998 2>/dev/null)" = ready ]; then
				ready=1
				break
			fi
			sleep 0.05
		done
		[ "$ready" -eq 1 ] || fail "leaf-unreachable-$peer"
	done

	insmod /modules/aead.ko || fail "aead-module"
	if [ "$result" -eq 0 ]; then
		insmod /modules/zcnblk_client_mod.ko transport=shm lanes=1 connections_per_lane=1 \
			size_mib=64 queues=1 queue_depth=128 max_frame_bytes=4096 pipeline_depth=128 \
			shm_ring_entries=128 shm_payload_entries=4096 shm_poll_us=1000 pin_threads=0 || \
			fail "client-module"
	fi
	wait_for_path /dev/zcnblk0 || fail "block-device"
	wait_for_path /dev/zcnblk-shmctl || fail "shm-control"

	if [ "$result" -eq 0 ]; then
		URING_PLAY_ZCNBLK_SHM_LEAF_ADDR=10.83.0.2:29600 \
		URING_PLAY_ZCNBLK_SHM_MIGRATION_SOURCE_ADDR=10.83.0.2:29600 \
		URING_PLAY_ZCNBLK_SHM_MIGRATION_DEST_ADDR=10.83.0.3:29600 \
		URING_PLAY_ZCNBLK_SHM_MIGRATION_CONTROL_SOCKET=/tmp/direct-migration.sock \
		URING_PLAY_ZCNBLK_SHM_MIGRATION_TCP_COPY_METHOD=splice \
		URING_PLAY_ZCNBLK_SHM_MIGRATION_CATCHUP_PASSES=2 \
		URING_PLAY_ZCNBLK_SHM_REMOTE_RESULT_RANGES=1 \
		URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_ZC_REQUIRED=0 \
		URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1 \
		URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS=1 \
		URING_PLAY_ZCNBLK_SHM_OWNER_COUNT=1 \
		URING_PLAY_ZCNBLK_SHM_OWNER_CPU_LIST=2 \
		URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE=/tmp/target.pid \
			taskset -c 3 /zcnblk-shm-target /dev/zcnblk-shmctl wal-tcp 128 1 1000 1000 10000 \
			>/tmp/target.log 2>&1 &
		wait_for_path /tmp/direct-migration.sock || fail "migration-control"
		if [ -s /tmp/target.pid ]; then target_pid="$(cat /tmp/target.pid)"; fi
	fi

	if [ "$result" -eq 0 ]; then
		ZCNBLK_EDGE_CONTINUITY_PID_FILE=/tmp/continuity.pid \
			taskset -c 4 /zcnblk-edge-continuity /dev/zcnblk0 0 128 0 64 \
			>/tmp/continuity.log 2>&1 &
		wait_for_log /tmp/continuity.log '^zcnblk-edge-continuity-start:' || fail "continuity-start"
		if [ -s /tmp/continuity.pid ]; then continuity_pid="$(cat /tmp/continuity.pid)"; fi
	fi

	if [ "$result" -eq 0 ]; then
		/zcnblk-direct-migratectl /tmp/direct-migration.sock \
			migrate 2 67108864 1048576 4096 >/tmp/migration-control.log || fail "migration-command"
		grep -q '^OK active_destination=true ' /tmp/migration-control.log || fail "migration-result"
		grep -q 'foreground_hops=1 foreground_payload_rebuffer_copies=0' \
			/tmp/migration-control.log || fail "foreground-proxy-elimination"
		grep -q 'copy_payload_userspace_buffers=0 copy_method=Splice' \
			/tmp/migration-control.log || fail "splice-zero-buffer"
	fi

	stop_exact "$continuity_pid" TERM || fail "continuity-stop"
	if [ -n "$continuity_pid" ]; then wait "$continuity_pid" 2>/dev/null || true; fi
	grep -q '^ZCNBLK_EDGE_CONTINUITY_PASS .*identity_stable=true .*open_descriptor_replaced=false .*mismatches=0 ' \
		/tmp/continuity.log || fail "continuity-proof"
	grep -q '^zcnblk-shm-target-direct-route-cutover: .*foreground_hops=1 payload_rebuffer_copies=0 client_block_reconnect=false$' \
		/tmp/target.log || fail "route-cutover-proof"

	stop_exact "$target_pid" INT || fail "target-stop"
	if [ -n "$target_pid" ]; then wait "$target_pid" 2>/dev/null || true; fi
	cat /tmp/migration-control.log 2>/dev/null || true
	cat /tmp/continuity.log 2>/dev/null || true
	cat /tmp/target.log 2>/dev/null || true

	for peer in 10.83.0.2 10.83.0.3; do
		echo shutdown | nc -w 2 "$peer" 29999 || fail "leaf-shutdown-$peer"
	done
	dmesg >/tmp/dmesg.log
	if grep -Eq 'BUG:|Oops:|KASAN:|general protection fault|kernel panic' /tmp/dmesg.log; then
		fail "kernel-diagnostic"
	fi
	rmmod zcnblk_client_mod 2>/dev/null || fail "client-module-unload"
	rmmod aead 2>/dev/null || fail "aead-module-unload"
	if [ "$result" -eq 0 ]; then
		echo "ZCNBLK_DIRECT_MIGRATION_QEMU_PASS role=client stable_block_descriptor=true reconnects=0 foreground_hops=1 foreground_payload_rebuffer_copies=0"
	fi
fi

sync
poweroff -f
