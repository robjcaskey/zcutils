#!/bin/sh
set -eu

PATH=/bin:/sbin
export PATH

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /tmp

echo '[zcvhost-vm] loading virtio_blk'
if [ -r /modules/virtio_blk.ko ]; then
    insmod /modules/virtio_blk.ko
else
    echo '[zcvhost-vm] virtio_blk is built into the guest kernel'
fi

attempt=0
while [ ! -b /dev/vda ] && [ "$attempt" -lt 100 ]; do
    attempt=$((attempt + 1))
    sleep 0.05
done
if [ ! -b /dev/vda ]; then
    echo '[zcvhost-vm] FAIL: /dev/vda did not appear'
    poweroff -f
    exit 1
fi

# Exercise guest reads, writes, and an explicit durability barrier.  BusyBox is
# deterministic input already resident in the initramfs; comparison verifies
# that data traversed the vhost-user edge in both directions.
dd if=/bin/busybox of=/dev/vda bs=4096 count=1 seek=8 conv=fsync
dd if=/dev/vda of=/tmp/actual bs=4096 count=1 skip=8
dd if=/bin/busybox of=/tmp/expected bs=4096 count=1
cmp /tmp/expected /tmp/actual
sync

echo '[zcvhost-vm] PASS: read write flush through stock QEMU vhost-user-blk'
poweroff -f
