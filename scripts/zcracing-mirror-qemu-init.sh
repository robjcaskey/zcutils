#!/bin/sh
set -u
export PATH=/bin:/sbin:/usr/bin:/usr/sbin

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /tmp /mnt

fail()
{
	echo "RACING_MIRROR_QEMU_FAIL role=${role:-early} reason=$*"
	dmesg | tail -80
	poweroff -f
}

insmod /modules/failover.ko || fail failover-module
insmod /modules/net_failover.ko || fail net-failover-module
insmod /modules/virtio_net.ko || fail virtio-net-module
insmod /modules/virtio_blk.ko || fail virtio-blk-module
insmod /modules/mbcache.ko || fail mbcache-module
insmod /modules/jbd2.ko || fail jbd2-module
insmod /modules/crc16.ko || fail crc16-module
insmod /modules/ext4.ko || fail ext4-module

role=""
delay_ms=0
frames=8
resume_frames=4
window=4
for argument in $(cat /proc/cmdline); do
	case "$argument" in
		zcrm.role=*) role="${argument#zcrm.role=}" ;;
		zcrm.delay_ms=*) delay_ms="${argument#zcrm.delay_ms=}" ;;
		zcrm.frames=*) frames="${argument#zcrm.frames=}" ;;
		zcrm.resume_frames=*) resume_frames="${argument#zcrm.resume_frames=}" ;;
		zcrm.window=*) window="${argument#zcrm.window=}" ;;
	esac
done

case "$role" in
	client) ip_address=10.44.0.1 ;;
	first-hop) ip_address=10.44.0.2 ;;
	remote-leaf) ip_address=10.44.0.3 ;;
	*) fail unknown-role ;;
esac

ip link set eth0 up || fail link-up
ip address add "$ip_address/24" dev eth0 || fail address-add

echo "RACING_MIRROR_TOPOLOGY role=$role lane=0 worker=0 vcpu=0 lane_to_worker=0:0 lane_to_cpu=0:0 worker_qd=$window lanes=1 aggregate_outstanding=$window completion=both_terminal_fdatasync"
echo "RACING_MIRROR_TOPOLOGY_WARNING functional_qemu=true representative_benchmark=false hugetlb=absent memlock_headroom=unverified kthread_affinity=unverified hctx_affinity=unverified batching=window-$window io_uring_fast_path=unused raw_transport_rtt=not_measured theoretical_iops_ceiling=not_reported"
total_frames=$((frames + resume_frames))

case "$role" in
	remote-leaf)
		count=0
		while [ ! -b /dev/vda ] && [ "$count" -lt 100 ]; do
			sleep 0.05
			count=$((count + 1))
		done
		[ -b /dev/vda ] || fail missing-terminal-device
		mount -t ext4 -o noatime /dev/vda /mnt || fail mount-terminal
		/zcracing-mirror leaf 10.44.0.3:47031 /mnt/remote.log "$delay_ms" || fail leaf
		last_batch=$((frames % window))
		[ "$last_batch" -ne 0 ] || last_batch="$window"
		lag_hwm=$((frames - last_batch))
		truncate_bytes=$((lag_hwm * (64 + 4096)))
		truncate -s "$truncate_bytes" /mnt/remote.log || fail inject-remote-lag
		sync
		echo "RACING_MIRROR_QEMU_LAG_INJECTED role=$role durable_frames=$lag_hwm"
		echo "RACING_MIRROR_QEMU_RESTART role=$role recovered_frames=$frames"
		/zcracing-mirror leaf 10.44.0.3:47031 /mnt/remote.log "$delay_ms" || fail leaf-resume
		/zcracing-mirror verify /mnt/remote.log "$total_frames" || fail verify-remote
		sync
		umount /mnt || fail umount-remote
		;;
	first-hop)
		count=0
		while [ ! -b /dev/vda ] && [ "$count" -lt 100 ]; do
			sleep 0.05
			count=$((count + 1))
		done
		[ -b /dev/vda ] || fail missing-terminal-device
		mount -t ext4 -o noatime /dev/vda /mnt || fail mount-terminal
		/zcracing-mirror first-hop 10.44.0.2:47030 10.44.0.3:47031 /mnt/local.log || fail first-hop
		echo "RACING_MIRROR_QEMU_RESTART role=$role recovered_frames=$frames"
		/zcracing-mirror first-hop 10.44.0.2:47030 10.44.0.3:47031 /mnt/local.log || fail first-hop-resume
		/zcracing-mirror verify /mnt/local.log "$total_frames" || fail verify-local
		sync
		umount /mnt || fail umount-local
		;;
	client)
		/zcracing-mirror client 10.44.0.2:47030 "$frames" 4096 "$window" || fail client
		echo "RACING_MIRROR_QEMU_RESTART role=$role recovered_frames=$frames"
		/zcracing-mirror client 10.44.0.2:47030 "$resume_frames" 4096 "$window" || fail client-resume
		;;
esac

echo "RACING_MIRROR_QEMU_PASS role=$role"
poweroff -f
