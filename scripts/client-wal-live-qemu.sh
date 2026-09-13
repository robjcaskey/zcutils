#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ ${CLIENT_WAL_LIVE_COORDINATED:-0} != 1 ]]; then
    exec /home/rob/.local/bin/agent-coord run --owner codex:zcutils-live-custody-qemu \
        --mode shared --sensitivity normal --priority 40 --ttl 1200 \
        --resource 'cpu=*;memory-bandwidth=*;kvm=*' \
        --note 'isolated QEMU block-client continuity during serial mirror loss and live rebuild; not representative performance' \
        -- env CLIENT_WAL_LIVE_COORDINATED=1 bash "$0" "$@"
fi
slow=${CLIENT_WAL_LIVE_SLOW:-middle}
[[ "$slow" == middle || "$slow" == tail ]]
frontend=${CLIENT_WAL_LIVE_FRONTEND:-shared-arena}
[[ "$frontend" == shared-arena || "$frontend" == tcp-onramp ]]
kernel_release=${KERNEL_RELEASE:-$(uname -r)}
kernel=${KERNEL:-/boot/vmlinuz-$kernel_release}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/mnt/bulk_data/zcutils-cargo-target}
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} CARGO_PROFILE_DEV_DEBUG=0
work=$(mktemp -d /mnt/bulk_data/zcutils-qemu/client-wal-live.XXXXXXXX)
rootfs="$work/rootfs"
initramfs="$work/initramfs.cpio"
mkdir -p "$rootfs"/{bin,proc,sys,dev,tmp,mnt,modules,configs} "$work/logs"
printf 'CLIENT_WAL_LIVE_ARTIFACT=%s slow=%s frontend=%s\n' "$work" "$slow" "$frontend"
cargo build --manifest-path "$ROOT/Cargo.toml" --bin zcutils --bin zcnblk-shm-target --bin zcnblk-edge-continuity
for bin in zcutils zcnblk-shm-target zcnblk-edge-continuity; do cp "$CARGO_TARGET_DIR/debug/$bin" "$rootfs/$bin"; done
cp /usr/bin/busybox "$rootfs/bin/busybox"
for applet in cat dmesg echo grep insmod ip kill mkdir mount nc poweroff rmmod seq sh sleep sync tail umount; do ln -s busybox "$rootfs/bin/$applet"; done
cp "$ROOT/scripts/client-wal-live-qemu-init.sh" "$rootfs/init"
chmod +x "$rootfs/init"
cp "$ROOT"/tests/fixtures/client-wal/live-*.json "$rootfs/configs/"
while read -r library; do
    mkdir -p "$rootfs$(dirname "$library")"
    cp "$library" "$rootfs$library"
done < <({ for bin in zcutils zcnblk-shm-target zcnblk-edge-continuity; do ldd "$rootfs/$bin"; done; ldd /usr/bin/busybox; } |
    awk '/=> \// {print $3; next} /^[[:space:]]*\/lib/ {print $1}' | sort -u)
while read -r module; do
    name=$(basename "${module%.xz}")
    if [[ "$module" == *.xz ]]; then xz -dc "$module" >"$rootfs/modules/$name"; else cp "$module" "$rootfs/modules/$name"; fi
    printf '/modules/%s\n' "$name" >>"$rootfs/modules/load-order"
done < <(for module in ext4 virtio_net virtio_blk aead; do /sbin/modprobe --set-version "$kernel_release" --show-depends "$module"; done |
    awk '$1=="insmod" && !seen[$2]++ {print $2}')
[[ $(/sbin/modinfo -F vermagic "$ROOT/kmods/zcnblk_client_mod.ko") == "$kernel_release "* ]]
cp "$ROOT/kmods/zcnblk_client_mod.ko" "$rootfs/modules/"
(cd "$rootfs"; find . -print0 | cpio --null -o --format=newc >"$initramfs")
network_tag=$(printf '%04x' "$(( $$ % 65536 ))")
bridge="zcl${network_tag}b"
pids=() taps=() roles=(tail middle client)
bridge_owned=0
cleanup() {
    local index pid cmdline
    set +e
    for index in "${!pids[@]}"; do
        pid=${pids[$index]}
        [[ -r /proc/$pid/cmdline ]] || continue
        cmdline=$(tr '\0' ' ' <"/proc/$pid/cmdline")
        [[ -n "$cmdline" ]] || { wait "$pid" 2>/dev/null; continue; }
        if [[ "$cmdline" == *qemu-system-x86_64*"$initramfs"*"zccl.role=${roles[$index]}"* ]]; then
            kill -TERM "$pid"
            wait "$pid" 2>/dev/null
        else printf 'REFUSED unexpected cleanup pid=%s\n' "$pid" >&2; fi
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
    local role=$1 suffix tap cpus=2
    case "$role" in client) suffix=01; cpus=4 ;; middle) suffix=02 ;; tail) suffix=03 ;; replacement) suffix=04 ;; esac
    tap="zcl${network_tag}${suffix}"
    sudo -n ip tuntap add dev "$tap" mode tap user "$(id -un)"
    taps+=("$tap")
    sudo -n ip link set "$tap" master "$bridge"
    sudo -n ip link set "$tap" up
    truncate -s 256M "$work/$role.img"
    /sbin/mkfs.ext4 -q -F "$work/$role.img"
    qemu-system-x86_64 -machine accel=kvm -cpu host -m 768M -smp "$cpus" \
        -nographic -no-reboot -nodefaults -serial "file:$work/logs/$role.log" \
        -kernel "$kernel" -initrd "$initramfs" \
        -append "console=ttyS0 panic=-1 oops=panic quiet net.ifnames=0 zccl.role=$role zccl.slow=$slow zccl.frontend=$frontend" \
        -netdev "tap,id=net0,ifname=$tap,script=no,downscript=no" \
        -device "virtio-net-pci,netdev=net0,mac=52:54:90:${network_tag:0:2}:${network_tag:2:2}:$suffix" \
        -drive "file=$work/$role.img,format=raw,if=virtio,cache=none" \
        >"$work/logs/$role.launch.log" 2>&1 &
    pids+=("$!")
}
for role in "${roles[@]}"; do launch_vm "$role"; done
deadline=$((SECONDS + ${TIMEOUT_SECONDS:-150}))
faulted=0 replacement_started=0
while :; do
    if ((!faulted)) && [[ -f "$work/logs/client.log" ]] && rg -q '^CLIENT_WAL_LIVE_WORKLOAD_STARTED' "$work/logs/client.log"; then
        middle_pid=${pids[1]}
        cmdline=$(tr '\0' ' ' <"/proc/$middle_pid/cmdline")
        [[ "$cmdline" == *qemu-system-x86_64*"$initramfs"*'zccl.role=middle'* ]]
        kill -KILL "$middle_pid"
        wait "$middle_pid" 2>/dev/null || true
        faulted=1
        printf 'CLIENT_WAL_LIVE_POWER_CUT role=middle pid=%s block_workload_active=true\n' "$middle_pid"
    fi
    if ((faulted && !replacement_started)) && rg -q '^custody-live-replacement-staged:' "$work/logs/client.log"; then
        # The harness executes provisioning only. Rust detects/fences the loss,
        # selects the admitted replacement and gates activation on durable HWM.
        sleep 3
        roles+=(replacement)
        launch_vm replacement
        replacement_started=1
        printf 'CLIENT_WAL_LIVE_PROVISIONED role=replacement empty_media=true\n'
    fi
    alive=0
    for pid in "${pids[@]}"; do kill -0 "$pid" 2>/dev/null && alive=$((alive + 1)); done
    ((alive == 0)) && break
    if rg -q '^CLIENT_WAL_LIVE_QEMU_FAIL|Kernel panic|panicked at' "$work"/logs/*.log; then
        for role in "${roles[@]}"; do tail -50 "$work/logs/$role.log"; done
        exit 1
    fi
    if ((SECONDS >= deadline)); then
        for role in "${roles[@]}"; do tail -70 "$work/logs/$role.log"; done
        printf 'live custody QEMU timeout\n' >&2
        exit 1
    fi
    sleep 0.1
done
((faulted && replacement_started))
for role in client tail replacement; do rg -q "^CLIENT_WAL_LIVE_QEMU_PASS role=$role" "$work/logs/$role.log"; done
expected=$(sed -n 's/.*ZCNBLK_EDGE_CONTINUITY_DIGEST .*expected=\([a-f0-9]*\).*/\1/p' "$work/logs/client.log" | tail -1)
[[ ${#expected} == 64 ]]
for role in tail replacement; do
    rg -q "^custody-image:.*sha256=$expected " "$work/logs/$role.log"
done
if [[ "$slow" == middle ]]; then counter=early_third; else counter=early_middle; fi
rg -q "^custody-live-complete:.*${counter}=[1-9][0-9]* .*retained_reads=[1-9][0-9]* .*writes_during_copy=[1-9][0-9]* " "$work/logs/client.log"
if [[ "$slow" == middle ]]; then winner=third; else winner=middle; fi
awk -v winner="$winner" '
    $1 == "custody-live-race:" && $2 == "generation=1" && $3 == "winner=" winner {
        split($4, hwm, "=")
        if (hwm[2] > 16 && /lagging_sync_pending=true/) proved = 1
    }
    END { exit !proved }
' "$work/logs/client.log"
rg -q '^ZCNBLK_EDGE_CONTINUITY_PASS .*fua_writes=[1-9][0-9]* ' "$work/logs/client.log"
if [[ "$frontend" == shared-arena ]]; then
    rg -q '^custody-live-ready: frontend=shared-arena-direct .*client_payload_rebuffer_bytes=0 local_socket_hops=0 ' "$work/logs/client.log"
fi
printf 'CLIENT_WAL_LIVE_QEMU_PASS slow=%s frontend=%s same_block_descriptor=true frontend_reconnects=0 replicas_sha256=%s artifacts=%s\n' "$slow" "$frontend" "$expected" "$work"
