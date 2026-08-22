#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
HOST_ARCH="${HOST_ARCH:-$(uname -m)}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
VIRTIO_BLK_MODULE="${VIRTIO_BLK_MODULE:-}"
WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zcvhost-user-blk-smoke}"
ROOTFS="$WORK_DIR/rootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
LEAF="$WORK_DIR/terminal-leaf.raw"
SOCKET="$WORK_DIR/vhost-user-blk.sock"
BACKEND_LOG="$WORK_DIR/backend.log"
QEMU_LOG="$WORK_DIR/qemu.log"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/qemu-zcvhost-cargo}"
BACKEND="$CARGO_TARGET_DIR/release/zcvhost-user-blk"
QEMU_MEM="${QEMU_MEM:-512M}"
QEMU_SMP="${QEMU_SMP:-4}"
QUEUES="${QUEUES:-4}"
QUEUE_SIZE="${QUEUE_SIZE:-256}"
QUEUE_CPUS="${QUEUE_CPUS:-}"
TIMEOUT="${TIMEOUT:-120s}"
BUILD="${BUILD:-1}"
ARENA_SOCKET="${ARENA_SOCKET:-}"
ZCNBLK_DEVICE="${ZCNBLK_DEVICE:-/dev/zcnblk0}"

need() {
    command -v "$1" >/dev/null || {
        printf 'missing required command: %s\n' "$1" >&2
        exit 1
    }
}

if [[ "${ZCVHOST_QEMU_COORDINATED:-0}" != 1 ]]; then
    need "$COORD_BIN"
    exec "$COORD_BIN" run \
        --owner codex:zcutils-vhost-user-blk-qemu \
        --mode soft-exclusive --sensitivity high --priority 60 --ttl 600 \
        --resource 'cpu=*;memory-bandwidth=*;kvm=*' \
        --note 'stock-QEMU vhost-user-blk protocol smoke against terminal file leaf' \
        -- env ZCVHOST_QEMU_COORDINATED=1 "$0" "$@"
fi

case "$HOST_ARCH" in
    x86_64) QEMU_BIN=qemu-system-x86_64; QEMU_MACHINE=q35; QEMU_CPU_HOST=host; GUEST_CONSOLE=ttyS0 ;;
    aarch64|arm64) QEMU_BIN=qemu-system-aarch64; QEMU_MACHINE=virt; QEMU_CPU_HOST=max; GUEST_CONSOLE=ttyAMA0 ;;
    *) printf 'unsupported QEMU host architecture: %s\n' "$HOST_ARCH" >&2; exit 1 ;;
esac
if [[ -c /dev/kvm && -r /dev/kvm && -w /dev/kvm ]]; then
    QEMU_ACCEL=kvm
    QEMU_CPU="$QEMU_CPU_HOST"
else
    QEMU_ACCEL=tcg
    QEMU_CPU=max
fi

if [[ -z "$VIRTIO_BLK_MODULE" ]]; then
    for candidate in \
        "/lib/modules/$KERNEL_RELEASE/kernel/drivers/block/virtio_blk.ko" \
        "/lib/modules/$KERNEL_RELEASE/kernel/drivers/block/virtio_blk.ko.xz" \
        "/lib/modules/$KERNEL_RELEASE/kernel/drivers/block/virtio_blk.ko.zst"; do
        [[ -r "$candidate" ]] || continue
        VIRTIO_BLK_MODULE="$candidate"
        break
    done
fi

for command in cpio ldd "$QEMU_BIN" timeout; do
    need "$command"
done
if [[ "$BUILD" == 1 ]]; then
    need cargo
fi
[[ -r "$KERNEL" ]] || { printf 'kernel is not readable: %s\n' "$KERNEL" >&2; exit 1; }
if [[ -z "$VIRTIO_BLK_MODULE" ]]; then
    grep -q '^CONFIG_VIRTIO_BLK=y$' "/boot/config-$KERNEL_RELEASE" || {
        printf 'virtio_blk is neither built in nor available as a module\n' >&2
        exit 1
    }
elif [[ ! -r "$VIRTIO_BLK_MODULE" ]]; then
    printf 'virtio_blk module is not readable: %s\n' "$VIRTIO_BLK_MODULE" >&2
    exit 1
fi
[[ -r "$ROOT/scripts/zcvhost-user-blk-qemu-init.sh" ]]

if [[ "$BUILD" == 1 ]]; then
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release --bin zcvhost-user-blk
fi
[[ -x "$BACKEND" ]] || { printf 'backend is not executable: %s\n' "$BACKEND" >&2; exit 1; }

mkdir -p "$WORK_DIR"
if [[ -d "$ROOTFS" ]]; then
    rm -rf -- "$ROOTFS"
fi
mkdir -p "$ROOTFS/bin" "$ROOTFS/lib" "$ROOTFS/lib64" "$ROOTFS/modules" \
    "$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/tmp"
cp /usr/bin/busybox "$ROOTFS/bin/busybox"
for applet in cmp dd echo insmod mkdir mount poweroff sh sleep sync; do
    ln -s busybox "$ROOTFS/bin/$applet"
done
cp "$ROOT/scripts/zcvhost-user-blk-qemu-init.sh" "$ROOTFS/init"
chmod +x "$ROOTFS/init"
if [[ -n "$VIRTIO_BLK_MODULE" ]]; then
    case "$VIRTIO_BLK_MODULE" in
        *.xz) xz -dc -- "$VIRTIO_BLK_MODULE" > "$ROOTFS/modules/virtio_blk.ko" ;;
        *.zst) zstd -dc -- "$VIRTIO_BLK_MODULE" > "$ROOTFS/modules/virtio_blk.ko" ;;
        *) cp "$VIRTIO_BLK_MODULE" "$ROOTFS/modules/virtio_blk.ko" ;;
    esac
fi

while IFS= read -r library; do
    [[ -n "$library" ]] || continue
    mkdir -p "$ROOTFS$(dirname "$library")"
    cp "$library" "$ROOTFS$library"
done < <(
    ldd /usr/bin/busybox | awk '
        /=> \// { print $3; next }
        /^[[:space:]]*\/lib/ { print $1; next }
    ' | sort -u
)

(
    cd "$ROOTFS"
    find . -print0 | cpio --null -o --format=newc > "$INITRAMFS"
)
if [[ -z "$ARENA_SOCKET" ]]; then
    truncate -s 64M "$LEAF"
fi
rm -f -- "$SOCKET" "$BACKEND_LOG" "$QEMU_LOG"

backend_pid=''
cleanup() {
    if [[ -n "$backend_pid" ]] && kill -0 "$backend_pid" 2>/dev/null; then
        kill "$backend_pid"
        wait "$backend_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT

backend_args=(--socket "$SOCKET" --queues "$QUEUES" --queue-size "$QUEUE_SIZE")
if [[ -n "$QUEUE_CPUS" ]]; then
    backend_args+=(--queue-cpus "$QUEUE_CPUS")
fi
if [[ -n "$ARENA_SOCKET" ]]; then
    backend_args+=(--arena-socket "$ARENA_SOCKET" --zcnblk-device "$ZCNBLK_DEVICE")
else
    backend_args+=(--leaf-file "$LEAF")
fi
"$BACKEND" "${backend_args[@]}" >"$BACKEND_LOG" 2>&1 &
backend_pid=$!

for _ in $(seq 1 100); do
    [[ -S "$SOCKET" ]] && break
    kill -0 "$backend_pid" 2>/dev/null || {
        printf 'backend exited before creating its socket\n' >&2
        sed -n '1,240p' "$BACKEND_LOG" >&2
        exit 1
    }
    sleep 0.05
done
[[ -S "$SOCKET" ]] || { printf 'backend socket was not created\n' >&2; exit 1; }

set +e
timeout "$TIMEOUT" "$QEMU_BIN" \
    -machine "$QEMU_MACHINE,accel=$QEMU_ACCEL" \
    -cpu "$QEMU_CPU" \
    -m "$QEMU_MEM" \
    -smp "$QEMU_SMP" \
    -object "memory-backend-memfd,id=guestmem,size=$QEMU_MEM,share=on" \
    -numa node,memdev=guestmem \
    -nodefaults \
    -nographic \
    -no-reboot \
    -serial mon:stdio \
    -kernel "$KERNEL" \
    -initrd "$INITRAMFS" \
    -append "console=$GUEST_CONSOLE panic=-1 oops=panic quiet" \
    -chardev "socket,id=zcblk,path=$SOCKET" \
    -device "vhost-user-blk-pci,chardev=zcblk,num-queues=$QUEUES,queue-size=$QUEUE_SIZE" \
    | tee "$QEMU_LOG"
qemu_status=${PIPESTATUS[0]}
set -e

# Reap the backend directly.  `kill -0` also succeeds for an exited child that
# is waiting to be reaped, which made the earlier lifecycle check misclassify
# a clean disconnect as a live daemon.
wait "$backend_pid"
backend_pid=''

if [[ "$qemu_status" -ne 0 ]]; then
    printf 'QEMU exited with status %s\n' "$qemu_status" >&2
    exit "$qemu_status"
fi
grep -Fq '[zcvhost-vm] PASS:' "$QEMU_LOG"
grep -Eq 'zcvhost-user-blk-summary:.*reads=[1-9].*writes=[1-9].*flushes=[1-9].*io_errors=0' "$BACKEND_LOG"
if grep -Eq 'BUG:|Oops:|KASAN:|general protection fault|kernel panic' "$QEMU_LOG"; then
    printf 'guest kernel failure detected\n' >&2
    exit 1
fi

printf 'zcvhost-user-blk QEMU smoke passed\n'
printf 'guest_log=%s\nbackend_log=%s\n' "$QEMU_LOG" "$BACKEND_LOG"
