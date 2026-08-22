#!/bin/sh
set -eu

PATH=/bin:/sbin:/usr/bin
export PATH

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /tmp

echo '[zcvhost-vm] loading virtio_blk for fio'
if [ -r /modules/virtio_blk.ko ]; then
    insmod /modules/virtio_blk.ko
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

jobs=4
qd=128
runtime=10
rw=randread
bs=4k
size=48M
hipri=0
for argument in $(cat /proc/cmdline); do
    case "$argument" in
        zc_fio_jobs=*) jobs=${argument#*=} ;;
        zc_fio_qd=*) qd=${argument#*=} ;;
        zc_fio_runtime=*) runtime=${argument#*=} ;;
        zc_fio_rw=*) rw=${argument#*=} ;;
        zc_fio_bs=*) bs=${argument#*=} ;;
        zc_fio_size=*) size=${argument#*=} ;;
        zc_fio_hipri=*) hipri=${argument#*=} ;;
    esac
done

last_cpu=$((jobs - 1))
echo "[zcvhost-vm] fio topology: jobs=$jobs per_worker_qd=$qd aggregate_outstanding_depth=$((jobs * qd)) guest_cpu_map=jobN:cpuN guest_cpus=0-$last_cpu rw=$rw bs=$bs hipri=$hipri"
for hctx in /sys/block/vda/mq/*; do
    [ -r "$hctx/cpu_list" ] || continue
    echo "[zcvhost-vm] hctx=${hctx##*/} guest_cpu_list=$(cat "$hctx/cpu_list")"
done
/usr/bin/fio \
    --name=zcvhost \
    --filename=/dev/vda \
    --ioengine=io_uring \
    --direct=1 \
    --rw="$rw" \
    --bs="$bs" \
    --iodepth="$qd" \
    --numjobs="$jobs" \
    --cpus_allowed="0-$last_cpu" \
    --cpus_allowed_policy=split \
    --time_based=1 \
    --runtime="$runtime" \
    --group_reporting=1 \
    --norandommap=1 \
    --randrepeat=0 \
    --size="$size" \
    --fixedbufs=1 \
    --registerfiles=1 \
    --hipri="$hipri"
sync

echo '[zcvhost-vm] PASS: fio completed through stock QEMU vhost-user-blk'
poweroff -f
