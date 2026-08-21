#!/bin/sh
set -eu

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /run /tmp
mount -t tmpfs -o size=64m tmpfs /run
insmod /modules/failover.ko
insmod /modules/net_failover.ko
insmod /modules/virtio_net.ko
ip link set lo up

role=""
for word in $(cat /proc/cmdline); do
	case "$word" in zcglobal_role=*) role="${word#zcglobal_role=}" ;; esac
done

case "$role" in
	use) ip=10.242.0.11; region=us-east ;;
	usw) ip=10.242.0.12; region=us-west ;;
	uk) ip=10.242.0.21; region=uk ;;
	pe) ip=10.242.0.31; region=pottsylvania-east ;;
	pw) ip=10.242.0.32; region=pottsylvania-west ;;
	*) echo "invalid zcglobal_role=$role"; poweroff -f ;;
esac

ip link set eth0 up
ip addr add "$ip/24" dev eth0
echo "GLOBAL_MULTI_QEMU_GUEST_READY node=$role region=$region ip=$ip"

atlas='use@10.242.0.11:9921#leader,usw@10.242.0.12:9921#leader,uk@10.242.0.21:9921#voter'
borealis='pe@10.242.0.31:9922#leader,pw@10.242.0.32:9922#leader,uk@10.242.0.21:9922#voter'
concord='usw@10.242.0.12:9923#leader,uk@10.242.0.21:9923#leader,pe@10.242.0.31:9923#voter'

case "$role" in
	use)
		/zcglobal-policy-node serve atlas "$role" "$region" 0.0.0.0:9921 /run/atlas.json "$atlas" /etc/atlas.token &
		;;
	usw)
		/zcglobal-policy-node serve atlas "$role" "$region" 0.0.0.0:9921 /run/atlas.json "$atlas" /etc/atlas.token &
		/zcglobal-policy-node serve concord "$role" "$region" 0.0.0.0:9923 /run/concord.json "$concord" /etc/concord.token &
		;;
	uk)
		/zcglobal-policy-node serve atlas "$role" "$region" 0.0.0.0:9921 /run/atlas.json "$atlas" /etc/atlas.token &
		/zcglobal-policy-node serve borealis "$role" "$region" 0.0.0.0:9922 /run/borealis.json "$borealis" /etc/borealis.token &
		/zcglobal-policy-node serve concord "$role" "$region" 0.0.0.0:9923 /run/concord.json "$concord" /etc/concord.token &
		;;
	pe)
		/zcglobal-policy-node serve borealis "$role" "$region" 0.0.0.0:9922 /run/borealis.json "$borealis" /etc/borealis.token &
		/zcglobal-policy-node serve concord "$role" "$region" 0.0.0.0:9923 /run/concord.json "$concord" /etc/concord.token &
		;;
	pw)
		/zcglobal-policy-node serve borealis "$role" "$region" 0.0.0.0:9922 /run/borealis.json "$borealis" /etc/borealis.token &
		;;
esac
wait
