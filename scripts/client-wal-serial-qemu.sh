#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ ${CLIENT_WAL_QEMU_COORDINATED:-0} != 1 ]]; then
    exec /home/rob/.local/bin/agent-coord run --owner codex:zcutils-client-wal-qemu \
        --mode shared --sensitivity normal --priority 40 --ttl 1200 \
        --resource 'cpu=*;memory-bandwidth=*;kvm=*' \
        --note 'three-guest serial persistent client WAL correctness, not performance' \
        -- env CLIENT_WAL_QEMU_COORDINATED=1 bash "$0" "$@"
fi
KERNEL_RELEASE=${KERNEL_RELEASE:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/mnt/bulk_data/zcutils-cargo-target}
export CARGO_TARGET_DIR CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} CARGO_PROFILE_DEV_DEBUG=0
work_parent=${CLIENT_WAL_QEMU_WORK_PARENT:-/mnt/bulk_data/zcutils-qemu}
mkdir -p "$work_parent"
work=$(mktemp -d "$work_parent/client-wal-serial.XXXXXXXX")
rootfs="$work/rootfs"
initramfs="$work/initramfs.cpio"
mkdir -p "$rootfs"/{bin,proc,sys,dev,tmp,mnt,modules,configs} "$work/logs"
printf 'CLIENT_WAL_SERIAL_ARTIFACT=%s\n' "$work"
cargo build --manifest-path "$ROOT/Cargo.toml" --bin zcutils
cp "$CARGO_TARGET_DIR/debug/zcutils" "$rootfs/zcutils"
cp /usr/bin/busybox "$rootfs/bin/busybox"
for applet in cat cmp dd echo insmod ip mkdir mount poweroff sed sha256sum sh sleep sync tr umount uname; do
    ln -s busybox "$rootfs/bin/$applet"
done
cp "$ROOT/scripts/client-wal-serial-qemu-init.sh" "$rootfs/init"
chmod +x "$rootfs/init"
cp "$ROOT"/tests/fixtures/client-wal/*.json "$rootfs/configs/"
while read -r library; do
    mkdir -p "$rootfs$(dirname "$library")"
    cp "$library" "$rootfs$library"
done < <({ ldd "$rootfs/zcutils"; ldd /usr/bin/busybox; } |
    awk '/=> \// {print $3; next} /^[[:space:]]*\/lib/ {print $1}' | sort -u)
while read -r module; do
    name=$(basename "${module%.xz}")
    if [[ "$module" == *.xz ]]; then xz -dc "$module" >"$rootfs/modules/$name"
    else cp "$module" "$rootfs/modules/$name"; fi
    printf '/modules/%s\n' "$name" >>"$rootfs/modules/load-order"
done < <(for module in ext4 virtio_net virtio_blk; do
    /sbin/modprobe --set-version "$KERNEL_RELEASE" --show-depends "$module"
done | awk '$1=="insmod" && !seen[$2]++ {print $2}')
(cd "$rootfs"; find . -print0 | cpio --null -o --format=newc >"$initramfs")

network_tag=$(printf '%04x' "$(( $$ % 65536 ))")
bridge="zcw${network_tag}b"
pids=() taps=() roles=(tail middle client)
bridge_owned=0
cleanup() {
    local index pid cmdline
    set +e
    for index in "${!pids[@]}"; do
        pid=${pids[$index]}
        [[ -r /proc/$pid/cmdline ]] || continue
        cmdline=$(tr '\0' ' ' <"/proc/$pid/cmdline")
        if [[ "$cmdline" == *qemu-system-x86_64*"$initramfs"*"zccw.role=${roles[$index]}"* ]]; then
            kill -TERM "$pid"
            wait "$pid" 2>/dev/null
        else
            printf 'REFUSED unexpected cleanup pid=%s\n' "$pid" >&2
        fi
    done
    for tap in "${taps[@]}"; do sudo -n ip link del "$tap"; done
    if ((bridge_owned)); then sudo -n ip link del "$bridge"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
sudo -n ip link add "$bridge" type bridge
bridge_owned=1
sudo -n ip link set "$bridge" type bridge stp_state 0
sudo -n ip link set "$bridge" up
launch_vm() {
    local role=$1 suffix tap
    case "$role" in client) suffix=01 ;; middle) suffix=02 ;; tail) suffix=03 ;; replacement) suffix=04 ;; esac
    tap="zcw${network_tag}${suffix}"
    sudo -n ip tuntap add dev "$tap" mode tap user "$(id -un)"
    taps+=("$tap")
    sudo -n ip link set "$tap" master "$bridge"
    sudo -n ip link set "$tap" up
    # Newly created files are terminal media, never mirror/stripe primitives.
    truncate -s 256M "$work/$role.img"
    /sbin/mkfs.ext4 -q -F "$work/$role.img"
    qemu-system-x86_64 -machine accel=kvm -cpu host -m 768M -smp 2 \
        -nographic -no-reboot -nodefaults -serial "file:$work/logs/$role.log" \
        -kernel "$KERNEL" -initrd "$initramfs" \
        -append "console=ttyS0 panic=-1 oops=panic quiet net.ifnames=0 zccw.role=$role zccw.failure=${CLIENT_WAL_QEMU_FAILURE:-0} zccw.batch=${CLIENT_WAL_QEMU_BATCH:-0}" \
        -netdev "tap,id=net0,ifname=$tap,script=no,downscript=no" \
        -device "virtio-net-pci,netdev=net0,mac=52:54:89:${network_tag:0:2}:${network_tag:2:2}:$suffix" \
        -drive "file=$work/$role.img,format=raw,if=virtio,cache=none" \
        >"$work/logs/$role.launch.log" 2>&1 &
    pids+=("$!")
}
for role in "${roles[@]}"; do launch_vm "$role"; done
deadline=$((SECONDS + ${TIMEOUT_SECONDS:-120}))
faulted=0 replacement_started=0
while :; do
    if [[ ${CLIENT_WAL_QEMU_FAILURE:-0} = 1 ]]; then
        if (( !faulted )) && [[ -f "$work/logs/client.log" ]] && rg -q 'client-wal-custody-progress:.*released_hwm=[1-9][0-9]*' "$work/logs/client.log"; then
            # Exact child PID and command-line ownership check, never patterns.
            middle_pid=${pids[1]}
            cmdline=$(tr '\0' ' ' <"/proc/$middle_pid/cmdline")
            [[ "$cmdline" == *qemu-system-x86_64*"$initramfs"*'zccw.role=middle'* ]]
            kill -KILL "$middle_pid"
            wait "$middle_pid" 2>/dev/null || true
            faulted=1
            printf 'CLIENT_WAL_POWER_CUT role=middle pid=%s after_reclaimed_prefix=true\n' "$middle_pid"
        fi
        if ((faulted && !replacement_started)) && rg -q 'client-wal-repair-staged:' "$work/logs/client.log"; then
            roles+=(replacement)
            launch_vm replacement
            replacement_started=1
        fi
    fi
    alive=0
    for pid in "${pids[@]}"; do kill -0 "$pid" 2>/dev/null && alive=$((alive+1)); done
    ((alive)) || break
    if ((SECONDS >= deadline)); then
        printf 'serial WAL QEMU timeout: %s\n' "$work" >&2
        tail -n 60 "$work"/logs/*.log
        exit 1
    fi
    sleep 0.02
done
for index in "${!pids[@]}"; do
    if ((faulted && index == 1)); then continue; fi
    wait "${pids[$index]}"
done
for role in "${roles[@]}"; do
    if [[ "$role" = middle && "$faulted" = 1 ]]; then continue; fi
    if ! rg -q "CLIENT_WAL_SERIAL_QEMU_PASS role=$role" "$work/logs/$role.log" ||
        rg -q 'CLIENT_WAL_SERIAL_QEMU_FAIL|BUG:|Oops:|Kernel panic' "$work/logs/$role.log"; then
        tail -n 90 "$work/logs/$role.log"
        exit 1
    fi
done
if [[ ${CLIENT_WAL_QEMU_FAILURE:-0} = 1 ]]; then
    ((faulted && replacement_started))
    rg -q 'client-wal-repair-complete:' "$work/logs/client.log"
    rg 'client-wal-repair-(staged|complete)' "$work/logs/client.log"
fi
rg 'client-wal.*(early|drain)|CLIENT_WAL_SERIAL_(PAYLOAD|QEMU_PASS)|zcraid-serial-complete' "$work"/logs/*.log
printf 'CLIENT_WAL_SERIAL_QEMU_PASS initial_machines=3 serial=true transport=tcp persistence=ext4 source_copy=0 relay_copy=0 replacement_test=%s representative_performance=false artifact=%s\n' "$faulted" "$work"
