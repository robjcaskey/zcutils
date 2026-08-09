#!/bin/sh

set -u

export PATH=/usr/bin:/bin:/sbin

result=0
leaf_pid=""
target_pid=""
wal_lane_batch=0

fail()
{
	echo "[zcnblk-wal-vm] FAIL: $*"
	result=1
}

wait_for_path()
{
	path="$1"
	for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
		[ -e "$path" ] && return 0
		sleep 1
	done
	return 1
}

wait_for_log()
{
	path="$1"
	pattern="$2"
	for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
		grep -q "$pattern" "$path" 2>/dev/null && return 0
		sleep 1
	done
	return 1
}

stop_process()
{
	pid="$1"
	signal="$2"
	[ -n "$pid" ] || return 0
	[ -e "/proc/$pid" ] || return 0
	kill "-$signal" "$pid" 2>/dev/null || true
	for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
		[ ! -e "/proc/$pid" ] && return 0
		sleep 1
	done
	return 1
}

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /tmp
ip link set lo up

case " $(cat /proc/cmdline 2>/dev/null) " in
	*" zcnblk.wal_lane_batch=1 "*) wal_lane_batch=1 ;;
esac

echo "[zcnblk-wal-vm] uname"
uname -a

echo "[zcnblk-wal-vm] starting userspace memory leaf"
URING_PLAY_PIN_CPU_LIST=3 \
URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
	/zcnblk-wal-leaf zcmem:64M 127.0.0.1 29000 1 1 4096 1 true blocking \
	>/tmp/leaf.log 2>&1 &
leaf_pid=$!
sleep 1
if [ ! -e "/proc/$leaf_pid" ]; then
	cat /tmp/leaf.log
	fail "WAL leaf exited during startup"
fi

echo "[zcnblk-wal-vm] loading ABI-v5 client module inside guest"
if ! insmod /modules/zcnblk_client_mod.ko \
	transport=shm lanes=1 connections_per_lane=1 size_mib=64 \
	queues=1 queue_depth=128 max_frame_bytes=4096 pipeline_depth=128 \
	shm_ring_entries=128 shm_payload_entries=1024 shm_poll_us=50 \
	hctx_affinity=1 pin_threads=1 pin_base_cpu=1 pin_cpu_count=1 pin_stride=1; then
	fail "module load failed"
fi
wait_for_path /dev/zcnblk0 || fail "/dev/zcnblk0 did not appear"
wait_for_path /dev/zcnblk-shmctl || fail "/dev/zcnblk-shmctl did not appear"

if [ "$result" -eq 0 ]; then
	echo "[zcnblk-wal-vm] starting shared-memory onramp"
	export URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH=32
	export URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_POLICY=adaptive
	export URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MIN=0
	export URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MAX=1024
	export URING_PLAY_ZCNBLK_SHM_LEAF_ADDR=127.0.0.1:29000
	export URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE=/tmp/target.pid
	if [ "$wal_lane_batch" -eq 1 ]; then
		export URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1
		export URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS=1
	fi
	/zcnblk-shm-target /dev/zcnblk-shmctl wal-tcp 32 2 1000 1000 10000 \
		>/tmp/target.log 2>&1 &
	target_job_pid=$!
	wait_for_log /tmp/target.log '^zcnblk-shm-target:' || fail "SHM target did not become ready"
	if [ -s /tmp/target.pid ]; then
		target_pid="$(cat /tmp/target.pid)"
	else
		target_pid="$target_job_pid"
	fi
fi

if [ "$result" -eq 0 ]; then
	/bin/busybox dd if=/dev/zero bs=4096 count=1 status=none > /tmp/zero
	/bin/busybox tr '\000' '\125' < /tmp/zero > /tmp/pattern-a
	/bin/busybox tr '\000' '\252' < /tmp/zero > /tmp/pattern-b

	/bin/busybox dd if=/dev/zcnblk0 of=/tmp/read-cold bs=4096 skip=7 count=1 \
		iflag=direct status=none || fail "cold read failed"
	/bin/busybox cmp /tmp/zero /tmp/read-cold || fail "cold read was not zero"

	/bin/busybox dd if=/tmp/pattern-a of=/dev/zcnblk0 bs=4096 seek=7 count=1 \
		oflag=direct conv=notrunc status=none || fail "ordinary direct write failed"
	/bin/busybox dd if=/dev/zcnblk0 of=/tmp/read-a bs=4096 skip=7 count=1 \
		iflag=direct status=none || fail "dirty read failed"
	/bin/busybox cmp /tmp/pattern-a /tmp/read-a || fail "dirty read mismatch"

	/zcnblk-contract-smoke /dev/zcnblk0 7 >/tmp/contract.log 2>&1 || \
		fail "FUA/ioprio/write-lifetime write failed"
	/bin/busybox dd if=/dev/zcnblk0 of=/tmp/read-b-dirty bs=4096 skip=7 count=1 \
		iflag=direct status=none || fail "post-FUA dirty read failed"
	/bin/busybox cmp /tmp/pattern-b /tmp/read-b-dirty || fail "post-FUA dirty read mismatch"

	/bin/busybox dd if=/dev/zero of=/dev/zcnblk0 bs=4096 count=0 \
		conv=fsync status=none || fail "global sync failed"
	/bin/busybox dd if=/dev/zcnblk0 of=/tmp/read-b-remote bs=4096 skip=7 count=1 \
		iflag=direct status=none || fail "post-sync read failed"
	/bin/busybox cmp /tmp/pattern-b /tmp/read-b-remote || fail "post-sync read mismatch"

	/zcnblk-order-smoke /dev/zcnblk0 16 >/tmp/order.log 2>&1 || fail "ordered read/write smoke failed"
fi

if ! stop_process "$target_pid" INT; then
	fail "SHM target did not stop"
fi
if [ -n "${target_job_pid:-}" ]; then
	wait "$target_job_pid" 2>/dev/null || true
fi
if ! stop_process "$leaf_pid" TERM; then
	fail "WAL leaf did not stop"
fi
wait "$leaf_pid" 2>/dev/null || true

echo "[zcnblk-wal-vm] target log"
cat /tmp/target.log 2>/dev/null || true
echo "[zcnblk-wal-vm] leaf log"
cat /tmp/leaf.log 2>/dev/null || true
echo "[zcnblk-wal-vm] order log"
cat /tmp/order.log 2>/dev/null || true
echo "[zcnblk-wal-vm] contract log"
cat /tmp/contract.log 2>/dev/null || true

grep -q 'negotiated=0x7f' /tmp/target.log 2>/dev/null || fail "target did not negotiate all seven WAL features"
grep -q 'negotiated=0x7f' /tmp/leaf.log 2>/dev/null || fail "leaf did not negotiate all seven WAL features"
grep -q 'syncs=[1-9]' /tmp/target.log 2>/dev/null || fail "target did not complete a sync"
grep -q 'fua_requests=[1-9]' /tmp/target.log 2>/dev/null || fail "target did not receive native FUA"
grep -q 'ioprio_requests=[1-9]' /tmp/target.log 2>/dev/null || fail "target did not receive I/O priority"
grep -q 'write_lifetime_requests=[1-9]' /tmp/target.log 2>/dev/null || fail "target did not receive write-lifetime hints"
if [ "$wal_lane_batch" -eq 1 ]; then
	grep -q 'registered_lease_requests=[1-9]' /tmp/target.log 2>/dev/null || fail "target did not receive registered leases"
	grep -q 'payload_ownership=submit-sequence-token-transfer' /tmp/target.log 2>/dev/null || fail "target did not activate transferred payload ownership"
else
	grep -q 'registered_lease_requests=0' /tmp/target.log 2>/dev/null || fail "default target unexpectedly transferred payload ownership"
fi

dmesg > /tmp/dmesg.log
if grep -Eq 'BUG:|Oops:|KASAN:|general protection fault|kernel panic' /tmp/dmesg.log; then
	fail "kernel log contains a fatal diagnostic"
fi

rmmod zcnblk_client_mod 2>/dev/null || fail "guest module unload failed"
echo "[zcnblk-wal-vm] recent kernel log"
dmesg | tail -80

if [ "$result" -eq 0 ]; then
	echo "[zcnblk-wal-vm] PASS: abi-v5 read-write-fua-sync-order contract=0x7f lane_batch=$wal_lane_batch"
else
	echo "[zcnblk-wal-vm] FAILED"
fi

sync
poweroff -f
