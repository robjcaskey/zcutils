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
final_sync=1
batch_submit=1
batch_complete_min=1
batch_complete_max=1
effective_qd=128
effective_aggregate_qd=512
virtqueue_size=512
expected_hctx_cpus=
nomerges=
for argument in $(cat /proc/cmdline); do
    case "$argument" in
        zc_fio_jobs=*) jobs=${argument#*=} ;;
        zc_fio_qd=*) qd=${argument#*=} ;;
        zc_fio_runtime=*) runtime=${argument#*=} ;;
        zc_fio_rw=*) rw=${argument#*=} ;;
        zc_fio_bs=*) bs=${argument#*=} ;;
        zc_fio_size=*) size=${argument#*=} ;;
        zc_fio_hipri=*) hipri=${argument#*=} ;;
        zc_fio_final_sync=*) final_sync=${argument#*=} ;;
        zc_fio_batch_submit=*) batch_submit=${argument#*=} ;;
        zc_fio_batch_complete_min=*) batch_complete_min=${argument#*=} ;;
        zc_fio_batch_complete_max=*) batch_complete_max=${argument#*=} ;;
        zc_fio_effective_qd=*) effective_qd=${argument#*=} ;;
        zc_fio_effective_aggregate_qd=*) effective_aggregate_qd=${argument#*=} ;;
        zc_virtqueue_size=*) virtqueue_size=${argument#*=} ;;
        zc_expected_hctx_cpus=*) expected_hctx_cpus=${argument#*=} ;;
        zc_fio_nomerges=*) nomerges=${argument#*=} ;;
    esac
done

if [ -n "$nomerges" ]; then
    nomerges_path=/sys/block/vda/queue/nomerges
    if [ ! -w "$nomerges_path" ]; then
        echo "[zcvhost-vm] FAIL: requested nomerges=$nomerges but $nomerges_path is not writable"
        poweroff -f
        exit 1
    fi
    echo "$nomerges" > "$nomerges_path"
    actual_nomerges=$(cat "$nomerges_path")
    if [ "$actual_nomerges" -ne "$nomerges" ]; then
        echo "[zcvhost-vm] FAIL: requested nomerges=$nomerges actual=$actual_nomerges"
        poweroff -f
        exit 1
    fi
    echo "[zcvhost-vm] block merge policy verified: nomerges=$actual_nomerges"
fi

last_cpu=$((jobs - 1))
echo "[zcvhost-vm] fio topology: jobs=$jobs requested_per_worker_qd=$qd effective_per_worker_qd=$effective_qd requested_aggregate_outstanding_depth=$((jobs * qd)) effective_aggregate_outstanding_depth=$effective_aggregate_qd virtqueue_size=$virtqueue_size guest_cpu_map=jobN:cpuN guest_cpus=0-$last_cpu rw=$rw bs=$bs hipri=$hipri final_sync=$final_sync batch_submit=$batch_submit batch_complete_min=$batch_complete_min batch_complete_max=$batch_complete_max"
hctx_count=0
for hctx in /sys/block/vda/mq/*; do
    [ -r "$hctx/cpu_list" ] || continue
    hctx_index=${hctx##*/}
    actual_hctx_cpu=$(cat "$hctx/cpu_list")
    echo "[zcvhost-vm] hctx=$hctx_index guest_cpu_list=$actual_hctx_cpu"
    if [ -n "$expected_hctx_cpus" ]; then
        expected_hctx_cpu=$(printf '%s\n' "$expected_hctx_cpus" | cut -d, -f$((hctx_index + 1)))
        if [ "$actual_hctx_cpu" != "$expected_hctx_cpu" ]; then
            echo "[zcvhost-vm] FAIL: hctx=$hctx_index expected_guest_cpu=$expected_hctx_cpu actual_guest_cpu_list=$actual_hctx_cpu"
            poweroff -f
            exit 1
        fi
    fi
    hctx_count=$((hctx_count + 1))
done
if [ -n "$expected_hctx_cpus" ]; then
    expected_hctx_count=$(printf '%s\n' "$expected_hctx_cpus" | awk -F, '{ print NF }')
    if [ "$hctx_count" -ne "$expected_hctx_count" ]; then
        echo "[zcvhost-vm] FAIL: expected_hctx_count=$expected_hctx_count actual_hctx_count=$hctx_count"
        poweroff -f
        exit 1
    fi
    echo "[zcvhost-vm] hctx topology verified: count=$hctx_count map=$expected_hctx_cpus"
fi
if [ "$hipri" -eq 1 ]; then
    io_poll_path=/sys/block/vda/queue/io_poll
    if [ ! -r "$io_poll_path" ] || [ "$(cat "$io_poll_path")" -ne 1 ]; then
        echo "[zcvhost-vm] FAIL: hipri=1 requires active block polling at $io_poll_path"
        poweroff -f
        exit 1
    fi
    echo "[zcvhost-vm] block polling verified: $io_poll_path=1"
fi
/usr/bin/fio \
    --name=zcvhost \
    --filename=/dev/vda \
    --ioengine=io_uring \
    --direct=1 \
    --rw="$rw" \
    --bs="$bs" \
    --iodepth="$qd" \
    --iodepth_batch_submit="$batch_submit" \
    --iodepth_batch_complete_min="$batch_complete_min" \
    --iodepth_batch_complete_max="$batch_complete_max" \
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
    --fsync_on_close="$final_sync" \
    --hipri="$hipri"

echo '[zcvhost-vm] PASS: fio completed through stock QEMU vhost-user-blk'
poweroff -f
