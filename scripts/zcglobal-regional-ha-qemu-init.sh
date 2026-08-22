#!/bin/sh
set -u
export PATH=/bin:/sbin:/usr/bin:/usr/sbin

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /sys/kernel/debug /tmp /run /etc
mount -t debugfs debugfs /sys/kernel/debug
mount -t tmpfs tmpfs /run || true

role=""
operations=64
move_end=96
scenario=clean
loss_checkpoint=32
failure_suffix=a
expect_degraded=false
for argument in $(cat /proc/cmdline); do
	case "$argument" in
		zcgha.role=*) role="${argument#zcgha.role=}" ;;
		zcgha.operations=*) operations="${argument#zcgha.operations=}" ;;
		zcgha.move_end=*) move_end="${argument#zcgha.move_end=}" ;;
		zcgha.scenario=*) scenario="${argument#zcgha.scenario=}" ;;
		zcgha.loss_checkpoint=*) loss_checkpoint="${argument#zcgha.loss_checkpoint=}" ;;
		zcgha.failure_suffix=*) failure_suffix="${argument#zcgha.failure_suffix=}" ;;
	esac
done
export ZCGLOBAL_ROLE="$role"

fail()
{
	echo "ZCGLOBAL_REGIONAL_HA_QEMU_FAIL role=${role:-early} reason=$*"
	for log in /tmp/quorum.log /tmp/failover.log /tmp/us-route.log /tmp/eu-route.log /tmp/leaf.log /tmp/target.log /tmp/workload.log /tmp/grade.log; do
		[ ! -s "$log" ] || { echo "== $log =="; cat "$log"; }
	done
	dmesg | tail -60
	poweroff -f
}

wait_path()
{
	path="$1"
	count=0
	while [ ! -e "$path" ] && [ "$count" -lt 400 ]; do
		sleep 0.05
		count=$((count + 1))
	done
	[ -e "$path" ]
}

stop_pid()
{
	pid="$1"
	signal="$2"
	[ -n "$pid" ] || return 0
	[ -e "/proc/$pid" ] || return 0
	kill "-$signal" "$pid" 2>/dev/null || true
	count=0
	while [ -e "/proc/$pid" ] && [ "$count" -lt 200 ]; do
		sleep 0.05
		count=$((count + 1))
	done
	[ ! -e "/proc/$pid" ]
}

insmod /modules/failover.ko || fail failover-module
insmod /modules/net_failover.ko || fail net-failover-module
insmod /modules/virtio_net.ko || fail virtio-net-module

case "$role" in
	region-us) address=10.46.0.1 ;;
	gateway) address=10.46.0.2 ;;
	region-eu) address=10.46.0.3 ;;
	us-leaf-a) address=10.46.0.11; partuuid=46aa0011-01; region=us; leaf_connections=4; quorum_frontend=true ;;
	us-leaf-b) address=10.46.0.12; partuuid=46aa0012-01; region=us; leaf_connections=4; quorum_frontend=true ;;
	us-leaf-c) address=10.46.0.13; partuuid=46aa0013-01; region=us; leaf_connections=4; quorum_frontend=false ;;
	eu-leaf-a) address=10.46.0.21; partuuid=46aa0021-01; region=eu; leaf_connections=8; quorum_frontend=true ;;
	eu-leaf-b) address=10.46.0.22; partuuid=46aa0022-01; region=eu; leaf_connections=8; quorum_frontend=true ;;
	eu-leaf-c) address=10.46.0.23; partuuid=46aa0023-01; region=eu; leaf_connections=8; quorum_frontend=false ;;
	*) fail unknown-role ;;
esac

case "$role:$failure_suffix" in
	us-leaf-a:b|us-leaf-a:c|eu-leaf-a:b|eu-leaf-a:c|us-leaf-b:a|eu-leaf-b:a)
		expect_degraded=true
		;;
esac

hostname "$role" || fail hostname
ip link set lo up || fail loopback
ip link set eth0 up || fail link
ip address add "$address/24" dev eth0 || fail address

echo "ZCGLOBAL_REGIONAL_HA_TOPOLOGY role=$role qemu_l2_backend=tap-linux-bridge guest_storage_transport=tcp-unicast multicast_product_dependency=false rdma_emulation=false"
echo "ZCGLOBAL_REGIONAL_HA_TOPOLOGY_WARNING functional_qemu=true representative_benchmark=false hugetlb=absent memlock_headroom=unverified worker_qd=64 lanes=1 aggregate_outstanding=64 raw_transport_rtt=not_measured theoretical_iops_ceiling=not_reported"

case "$role" in
*-leaf-*)
	insmod /modules/virtio_blk.ko || fail virtio-blk
	wait_path /dev/vda1 || fail terminal-partition
	mkdir -p /dev/disk/by-partuuid
	ln -s ../../vda1 "/dev/disk/by-partuuid/$partuuid"
	echo "PARTUUID=$partuuid" >/tmp/raw-partitions.allow
	export URING_PLAY_RAW_PARTITION_ALLOWLIST=/tmp/raw-partitions.allow
	export URING_PLAY_ALLOW_RAW_BLOCK_WRITE=1
	export URING_PLAY_RAW_TARGET_PARTUUID="$partuuid"
	URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
		/zcnblk-wal-leaf "PARTUUID=$partuuid" "$address" 31000 1 "$leaf_connections" 4096 "$leaf_connections" false blocking \
		>/tmp/leaf.log 2>&1 &
	leaf_pid=$!
	quorum_pid=""
	if [ "$quorum_frontend" = true ]; then
		case "$region" in
			us) leaves=10.46.0.11:31000,10.46.0.12:31000,10.46.0.13:31000 ;;
			eu) leaves=10.46.0.21:31000,10.46.0.22:31000,10.46.0.23:31000 ;;
		esac
		export ZCNBLK_WAL_QUORUM_IO_TIMEOUT_MS=1000
		/zcnblk-wal-quorum "$address:30000" "$leaves" 1 2 >/tmp/quorum.log 2>&1 &
		quorum_pid=$!
	fi
	nc -l -p 29996 -e /leaf-fail >/dev/null 2>&1 &
	failure_listener=$!
	# Storage services are intentionally long-lived. The gateway sends this
	# functional-test teardown barrier only after both clients have closed and
	# the global path is stopped; daemon lifetime is not an EOF contract.
	nc -l -p 29997 -e /bin/true || fail completion-barrier
	stop_pid "$failure_listener" TERM || true
	stop_pid "$quorum_pid" TERM || fail quorum-stop
	stop_pid "$leaf_pid" TERM || fail leaf-stop
	half=$((operations / 2))
	case "$region" in
		us) /zcglobal-volume-workload grade /dev/vda1 "$half" $((half + 1)) "$move_end" >/tmp/grade.log 2>&1 || fail source-grade ;;
		eu) /zcglobal-volume-workload grade /dev/vda1 "$move_end" >/tmp/grade.log 2>&1 || fail target-grade ;;
	esac
	cat /tmp/grade.log
	[ ! -s /tmp/quorum.log ] || cat /tmp/quorum.log
	cat /tmp/leaf.log
	if [ "$expect_degraded" = true ]; then
		grep -q 'zcnblk-wal-quorum-leaf-degraded' /tmp/quorum.log || fail no-degraded-leaf-observed
	fi
	echo "ZCGLOBAL_REGIONAL_HA_QEMU_PASS role=$role terminal=virtio-blk userspace_placement=true regional_frontend=$quorum_frontend graded=true"
	poweroff -f
	;;
esac

if [ "$role" = gateway ]; then
	# This is a correctness matrix on a shared nine-VM host, not a detector
	# latency benchmark. Avoid mistaking multi-second guest descheduling for a
	# failed regional frontend.
	export ZCNBLK_WAL_HA_ROUTE_IO_TIMEOUT_MS=5000
	if [ "$failure_suffix" = b ]; then
		us_frontends=10.46.0.12:30000,10.46.0.11:30000
		eu_frontends=10.46.0.22:30000,10.46.0.21:30000
	else
		us_frontends=10.46.0.11:30000,10.46.0.12:30000
		eu_frontends=10.46.0.21:30000,10.46.0.22:30000
	fi
	/zcnblk-wal-ha-route 10.46.0.2:30000 "$us_frontends" 1 \
		>/dev/console 2>&1 &
	us_route_pid=$!
	/zcnblk-wal-ha-route 10.46.0.2:30100 "$eu_frontends" 1 \
		>/dev/console 2>&1 &
	eu_route_pid=$!
	export ZCNBLK_WAL_FAILOVER_MODE=async
	export ZCNBLK_WAL_FAILOVER_FENCE_SOURCE_IP=10.46.0.1
	/zcnblk-wal-failover 10.46.0.2:29000 10.46.0.2:30000 10.46.0.2:30100 10.46.0.2:29110 1 \
		>/tmp/failover.log 2>&1 &
	failover_pid=$!
	if [ "$scenario" = declared-loss ]; then
		nc -l -p 29113 -e /bin/true || fail source-region-destroyed
		loss_response="$(echo "secondary accept-loss $loss_checkpoint godzilla-destroyed-entire-source-region" | nc 10.46.0.2 29110)"
		echo "ZCGLOBAL_REGIONAL_DECLARED_LOSS_CONTROL_RESPONSE $loss_response"
		echo "$loss_response" | grep -q 'declared_loss=true' || fail declared-loss-promotion
		count=0
		while ! echo move-loss | nc 10.46.0.3 29999 2>/dev/null && [ "$count" -lt 400 ]; do sleep 0.05; count=$((count + 1)); done
		[ "$count" -lt 400 ] || fail target-loss-signal
		nc -l -p 29112 -e /bin/true || fail target-done
	else
		nc -l -p 29111 -e /bin/true || fail source-done
		nc -l -p 29112 -e /bin/true || fail target-done
	fi
	cat /tmp/failover.log
	grep -q 'placement_epoch=2' /tmp/failover.log || fail no-global-cut
	stop_pid "$failover_pid" TERM || fail failover-stop
	sleep 0.2
	stop_pid "$us_route_pid" TERM || fail us-route-stop
	stop_pid "$eu_route_pid" TERM || fail eu-route-stop
	case "$failure_suffix" in
		a)
			us_survivors="10.46.0.12 10.46.0.13"
			eu_survivors="10.46.0.22 10.46.0.23"
			;;
		b)
			us_survivors="10.46.0.11 10.46.0.13"
			eu_survivors="10.46.0.21 10.46.0.23"
			;;
		c)
			us_survivors="10.46.0.11 10.46.0.12"
			eu_survivors="10.46.0.21 10.46.0.22"
			;;
		*) fail "invalid-failure-suffix-$failure_suffix" ;;
	esac
	if [ "$scenario" = declared-loss ]; then survivors="$eu_survivors"; else survivors="$us_survivors $eu_survivors"; fi
	for survivor in $survivors; do
		count=0
		while ! echo done | nc "$survivor" 29997 2>/dev/null && [ "$count" -lt 200 ]; do
			sleep 0.05
			count=$((count + 1))
		done
		[ "$count" -lt 200 ] || fail "leaf-completion-signal-$survivor"
	done
	echo "ZCGLOBAL_REGIONAL_HA_QEMU_PASS role=gateway global_cut=$scenario async_replication=true regional_frontends=active-standby frontend_node_failures=one-per-region source_region_destroyed=$([ "$scenario" = declared-loss ] && echo true || echo false)"
	poweroff -f
fi

insmod /modules/aead.ko || fail aead
insmod /modules/zcnblk_client_mod.ko \
	transport=shm lanes=1 connections_per_lane=1 size_mib=32 queues=1 queue_depth=64 \
	max_frame_bytes=4096 pipeline_depth=64 shm_ring_entries=64 shm_payload_entries=256 \
	shm_poll_us=50 hctx_affinity=1 pin_threads=1 pin_base_cpu=1 pin_cpu_count=1 pin_stride=1 \
	shm_bio_arena_zero_copy=0 || fail zcnblk-module
wait_path /dev/zcnblk0 || fail zcnblk-device
wait_path /dev/zcnblk-shmctl || fail zcnblk-control
export URING_PLAY_ZCNBLK_SHM_LEAF_ADDR=10.46.0.2:29000
export URING_PLAY_ZCNBLK_SHM_REMOTE_CONNECT_RETRY_MS=30000
export URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1
export URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS=1
export URING_PLAY_ZCNBLK_SHM_ARENA_BACKING=vmalloc
export URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE=/tmp/target.pid
/zcnblk-shm-target /dev/zcnblk-shmctl wal-tcp 32 2 1000 1000 10000 >/tmp/target.log 2>&1 &
target_job_pid=$!
count=0
while ! grep -q '^zcnblk-shm-target:' /tmp/target.log 2>/dev/null && [ "$count" -lt 800 ]; do
	sleep 0.05
	count=$((count + 1))
done
grep -q '^zcnblk-shm-target:' /tmp/target.log || fail target-ready
target_pid="$target_job_pid"
[ ! -s /tmp/target.pid ] || target_pid="$(cat /tmp/target.pid)"

case "$role" in
	region-us)
		case "$failure_suffix" in
			a) us_failure_ip=10.46.0.11; eu_failure_ip=10.46.0.21 ;;
			b) us_failure_ip=10.46.0.12; eu_failure_ip=10.46.0.22 ;;
			c) us_failure_ip=10.46.0.13; eu_failure_ip=10.46.0.23 ;;
		esac
		if [ "$scenario" = declared-loss ]; then
			/zcglobal-volume-workload disaster-source-ha /dev/zcnblk0 10.46.0.2:29110 "$loss_checkpoint" "$operations" "$us_failure_ip:29996" "$eu_failure_ip:29996" \
				>/tmp/workload.log 2>&1 || fail disaster-source-ha-workload
			for doomed in 10.46.0.11 10.46.0.12 10.46.0.13; do
				[ "$doomed" != "$us_failure_ip" ] || continue
				count=0
				while ! echo destroy-region | nc "$doomed" 29996 2>/dev/null && [ "$count" -lt 200 ]; do sleep 0.05; count=$((count + 1)); done
				[ "$count" -lt 200 ] || fail "destroy-source-region-leaf-$doomed"
			done
			count=0
			while ! echo destroyed | nc 10.46.0.2 29113 2>/dev/null && [ "$count" -lt 200 ]; do sleep 0.05; count=$((count + 1)); done
			[ "$count" -lt 200 ] || fail source-region-destroyed-signal
			cat /tmp/workload.log
			echo "ZCGLOBAL_REGIONAL_HA_QEMU_PASS role=$role source_region_destroyed=true regional_quorum_before_loss=2-of-3 acknowledged_through=$operations remote_checkpoint=$loss_checkpoint"
			sleep 0.2
			poweroff -f
		fi
		nc -l -p 29998 -e /bin/true &
		destination_done_pid=$!
		/zcglobal-volume-workload stay-ha /dev/zcnblk0 10.46.0.2:29110 "$operations" "$us_failure_ip:29996" "$eu_failure_ip:29996" \
			>/tmp/workload.log 2>&1 || fail source-workload
		count=0
		while ! echo move | nc 10.46.0.3 29999 2>/dev/null && [ "$count" -lt 200 ]; do sleep 0.05; count=$((count + 1)); done
		[ "$count" -lt 200 ] || fail target-signal
		wait "$destination_done_pid" || fail destination-done
		;;
	region-eu)
		nc -l -p 29999 -e /bin/true || fail source-signal
		if [ "$scenario" = declared-loss ]; then
			/zcglobal-volume-workload move-loss /dev/zcnblk0 "$loss_checkpoint" "$operations" "$move_end" >/tmp/workload.log 2>&1 || fail target-loss-workload
		else
			/zcglobal-volume-workload move /dev/zcnblk0 "$operations" "$move_end" >/tmp/workload.log 2>&1 || fail target-workload
		fi
		if [ "$scenario" != declared-loss ]; then
			count=0
			while ! echo done | nc 10.46.0.1 29998 2>/dev/null && [ "$count" -lt 200 ]; do sleep 0.05; count=$((count + 1)); done
			[ "$count" -lt 200 ] || fail source-done-signal
		fi
		;;
esac

stop_pid "$target_pid" INT || fail target-stop
wait "$target_job_pid" 2>/dev/null || true
sleep 0.5
cat /tmp/workload.log
cat /tmp/target.log
grep -q 'ZCGLOBAL_VOLUME_.*_PASS' /tmp/workload.log || fail workload-marker
rmmod zcnblk_client_mod || fail zcnblk-unload
rmmod aead || fail aead-unload
case "$role" in
	region-us) port=29111 ;;
	region-eu) port=29112 ;;
esac
count=0
while ! echo "$role" | nc 10.46.0.2 "$port" 2>/dev/null && [ "$count" -lt 400 ]; do sleep 0.05; count=$((count + 1)); done
[ "$count" -lt 400 ] || fail gateway-final-signal
echo "ZCGLOBAL_REGIONAL_HA_QEMU_PASS role=$role regional_quorum=2-of-3 frontend_failover=transparent client_reconnects=0"
poweroff -f
