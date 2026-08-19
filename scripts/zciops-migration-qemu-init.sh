#!/bin/sh
set -eu
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /run /tmp
mount -t tmpfs -o size=64m tmpfs /run
for module in failover net_failover virtio_net virtio_blk; do insmod "/modules/$module.ko"; done
ip link set lo up
role=""
for word in $(cat /proc/cmdline); do case "$word" in zciops_role=*) role="${word#zciops_role=}" ;; esac; done
case "$role" in controller) ip=10.241.0.10 ;; fast) ip=10.241.0.11 ;; slow) ip=10.241.0.12 ;; *) poweroff -f ;; esac
ip link set eth0 up
ip addr add "$ip/24" dev eth0
echo "ZCIOPS_GUEST_READY role=$role ip=$ip"
case "$role" in
fast|slow) /zciops-migration-emu leaf 0.0.0.0:9910 /dev/vda 8192 0 ;;
controller)
	/zciops-migration-emu scenario 10.241.0.11:9910 10.241.0.12:9910 /run/results.log 8192
	status=$?
	cat /run/results.log
	echo "ZCIOPS_CONTROLLER_STOP status=$status"
	sync
	poweroff -f
	exit "$status"
	;;
esac
