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

role=""
full_replicas=0
for word in $(cat /proc/cmdline); do
	case "$word" in
		zcglobal_role=*) role="${word#zcglobal_role=}" ;;
		zcglobal_full_replicas=*) full_replicas="${word#zcglobal_full_replicas=}" ;;
	esac
done

case "$role:$full_replicas" in
	a1:*) ip=10.241.0.11; region=region-a ;;
	a2:0) ip=10.241.0.12; region=region-a ;;
	a2:1) ip=10.241.0.12; region=region-b ;;
	b1:0) ip=10.241.0.21; region=region-b ;;
	b1:1) ip=10.241.0.21; region=region-c ;;
	*) echo "invalid zcglobal_role=$role"; poweroff -f ;;
esac

ip link set eth0 up
ip addr add "$ip/24" dev eth0
echo "GLOBAL_QEMU_GUEST_READY node=$role region=$region ip=$ip"
if [ "$full_replicas" = 1 ]; then
	peers='a1@10.241.0.11:9910#leader,a2@10.241.0.12:9910#leader,b1@10.241.0.21:9910#leader'
else
	peers='a1@10.241.0.11:9910#leader,a2@10.241.0.12:9910#leader,b1@10.241.0.21:9910#voter'
fi
exec /zcglobal-policy-node serve qemu-default "$role" "$region" 0.0.0.0:9910 \
	/run/global-raft.json "$peers" /etc/zcglobal-admin-token
