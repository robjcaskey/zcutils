#!/bin/sh
set -eu

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /run /tmp
mount -t tmpfs -o size=48m tmpfs /run
insmod /modules/failover.ko
insmod /modules/net_failover.ko
insmod /modules/virtio_net.ko
ip link set lo up

cmdline="$(cat /proc/cmdline)"
role=""
for word in $cmdline; do
	case "$word" in
		zctopo_role=*) role="${word#zctopo_role=}" ;;
	esac
done

case "$role" in
	controller) ip=10.231.0.10 ;;
	node-a) ip=10.231.0.11 ;;
	node-b) ip=10.231.0.12 ;;
	node-c-cold) ip=10.231.0.13 ;;
	node-b-repl) ip=10.231.0.14 ;;
	node-c-hot) ip=10.231.0.15 ;;
	*) echo "invalid zctopo_role=$role"; poweroff -f ;;
esac

ip link set eth0 up
ip addr add "$ip/24" dev eth0
echo "ZCTOPO_GUEST_READY role=$role ip=$ip"

case "$role" in
	controller)
		for peer in 11 12 13 14 15; do
			for attempt in $(seq 1 100); do
				ping -c 1 -W 1 "10.231.0.$peer" >/dev/null 2>&1 && break
				sleep 0.05
			done
		done
		/zctopology-emu scenario /run/controller.log \
			10.231.0.11:9900 10.231.0.12:9900 10.231.0.13:9900 \
			10.231.0.14:9900 10.231.0.15:9900
		;;
	node-a)
		/zctopology-emu node 0.0.0.0:9900 node-a region-a region-a-az1 hot 9 /run/node.json
		;;
	node-b)
		/zctopology-emu node 0.0.0.0:9900 node-b region-b region-b-az1 cold 1 /run/node.json
		;;
	node-c-cold)
		/zctopology-emu node 0.0.0.0:9900 node-c-cold region-c region-c-az1 cold 1 /run/node.json
		;;
	node-b-repl)
		/zctopology-emu node 0.0.0.0:9900 node-b-repl region-b region-b-az2 warm 3 /run/node.json
		;;
	node-c-hot)
		/zctopology-emu node 0.0.0.0:9900 node-c-hot region-c region-c-az2 hot 9 /run/node.json
		;;
esac

status=$?
echo "ZCTOPO_GUEST_STOP role=$role status=$status"
sync
poweroff -f
exit "$status"
