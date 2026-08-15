#!/bin/sh

set -u
export PATH=/bin:/sbin:/usr/bin:/usr/sbin

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /tmp /mnt
insmod /modules/virtio_blk.ko || {
	echo "ZCPWAL_QEMU_PHASE_FAIL phase=early reason=virtio-blk-module-load"
	poweroff -f
}
insmod /modules/mbcache.ko || fail_early=mbcache
insmod /modules/jbd2.ko || fail_early=jbd2
insmod /modules/crc16.ko || fail_early=crc16
insmod /modules/ext4.ko || fail_early=ext4
if [ -n "${fail_early:-}" ]; then
	echo "ZCPWAL_QEMU_PHASE_FAIL phase=early reason=$fail_early-module-load"
	poweroff -f
fi

phase=""
for argument in $(cat /proc/cmdline); do
	case "$argument" in
		zcpwal.phase=*) phase="${argument#zcpwal.phase=}" ;;
	esac
done

fail()
{
	echo "ZCPWAL_QEMU_PHASE_FAIL phase=$phase reason=$*"
	dmesg | tail -80
	poweroff -f
}

for device in /dev/vda /dev/vdb /dev/vdc; do
	count=0
	while [ ! -e "$device" ] && [ "$count" -lt 100 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ -e "$device" ] || fail "missing-$device"
done

echo "ZCPWAL_QEMU_PHASE_START phase=$phase"
case "$phase" in
	file-matrix)
		mount -t ext4 -o noatime /dev/vdc /mnt || fail "mount-ext4"
		/zcpwal-qemu-smoke file-matrix /mnt || fail "file-matrix"
		sync
		umount /mnt || fail "umount-ext4"
		;;
	block-init)
		/zcpwal-qemu-smoke block-init /dev/vda /dev/vdb || fail "block-init"
		;;
	direct-block)
		/zcpwal-qemu-smoke direct-block /dev/vda /dev/vdb || fail "direct-block"
		;;
	crash-before-publish)
		exec /zcpwal-qemu-smoke crash-before-publish /dev/vda /dev/vdb
		;;
	verify-before-publish)
		/zcpwal-qemu-smoke verify-before-publish /dev/vda /dev/vdb || fail "verify-before-publish"
		;;
	crash-after-commit)
		exec /zcpwal-qemu-smoke crash-after-commit /dev/vda /dev/vdb
		;;
	verify-after-commit)
		/zcpwal-qemu-smoke verify-after-commit /dev/vda /dev/vdb || fail "verify-after-commit"
		;;
	crash-unsynced)
		exec /zcpwal-qemu-smoke crash-unsynced /dev/vda /dev/vdb
		;;
	verify-unsynced)
		/zcpwal-qemu-smoke verify-unsynced /dev/vda /dev/vdb || fail "verify-unsynced"
		;;
	corrupt-block)
		/zcpwal-qemu-smoke corrupt-block /dev/vda /dev/vdb || fail "corrupt-block"
		;;
	*)
		fail "unknown-phase"
		;;
esac

echo "ZCPWAL_QEMU_PHASE_PASS phase=$phase"
poweroff -f
