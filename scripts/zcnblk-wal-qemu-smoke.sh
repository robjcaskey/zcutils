#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
KDIR="${KDIR:-/lib/modules/$KERNEL_RELEASE/build}"
WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zcnblk-wal-smoke}"
ROOTFS="$WORK_DIR/rootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
LOG="${LOG:-$WORK_DIR/qemu.log}"
QEMU_MEM="${QEMU_MEM:-2048M}"
QEMU_SMP="${QEMU_SMP:-4}"
TIMEOUT="${TIMEOUT:-180s}"
QEMU_WAL_LANE_BATCH="${QEMU_WAL_LANE_BATCH:-0}"

case "$QEMU_WAL_LANE_BATCH" in
	0|1) ;;
	*)
		printf 'QEMU_WAL_LANE_BATCH must be 0 or 1, got %s\n' "$QEMU_WAL_LANE_BATCH" >&2
		exit 2
		;;
esac

need() {
	command -v "$1" >/dev/null || {
		printf 'missing required command: %s\n' "$1" >&2
		exit 1
	}
}

if [[ "${ZCNBLK_QEMU_COORDINATED:-0}" != 1 ]]; then
	need "$COORD_BIN"
	exec "$COORD_BIN" run \
		--owner codex:zcutils-zcnblk-wal-qemu \
		--mode soft-exclusive --sensitivity high --priority 60 --ttl 900 \
		--resource 'cpu=*;memory-bandwidth=*;kvm=*' \
		--note 'ABI-v5 zcnblk module and WAL correctness smoke in QEMU only' \
		-- env ZCNBLK_QEMU_COORDINATED=1 "$0" "$@"
fi

need cargo
need cpio
need ldd
need make
need qemu-system-x86_64
need timeout

[[ -r "$KERNEL" ]] || {
	printf 'kernel image is not readable: %s\n' "$KERNEL" >&2
	exit 1
}
[[ -d "$KDIR" ]] || {
	printf 'kernel build directory is missing: %s\n' "$KDIR" >&2
	exit 1
}
[[ -r "$ROOT/scripts/zcnblk-wal-qemu-init.sh" ]]

cargo build --release \
	--bin zcnblk-shm-target \
	--bin zcnblk-wal-leaf \
	--bin zcnblk-order-smoke \
	--bin zcnblk-contract-smoke
make -C "$ROOT/kmods" KDIR="$KDIR"

rm -rf -- "$ROOTFS"
mkdir -p "$ROOTFS/bin" "$ROOTFS/lib" "$ROOTFS/lib64" \
	"$ROOTFS/modules" "$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/tmp"
cp /usr/bin/busybox "$ROOTFS/bin/busybox"
for applet in cat cmp dd dmesg echo grep insmod ionice ip kill mkdir mount poweroff \
	rmmod sh sleep sync tail tr true uname; do
	ln -s busybox "$ROOTFS/bin/$applet"
done
ln -s bin/busybox "$ROOTFS/init-shell"
cp "$ROOT/scripts/zcnblk-wal-qemu-init.sh" "$ROOTFS/init"
chmod +x "$ROOTFS/init"
cp "$ROOT/kmods/zcnblk_client_mod.ko" "$ROOTFS/modules/zcnblk_client_mod.ko"

bins=(zcnblk-shm-target zcnblk-wal-leaf zcnblk-order-smoke zcnblk-contract-smoke)
for bin in "${bins[@]}"; do
	cp "$ROOT/target/release/$bin" "$ROOTFS/$bin"
done

while IFS= read -r lib; do
	[[ -n "$lib" ]] || continue
	mkdir -p "$ROOTFS$(dirname "$lib")"
	cp "$lib" "$ROOTFS$lib"
done < <(
	{
		for bin in "${bins[@]}"; do
			ldd "$ROOT/target/release/$bin"
		done
		ldd /usr/bin/busybox
	} | awk '
		/=> \// { print $3; next }
		/^[[:space:]]*\/lib/ { print $1; next }
	' | sort -u
)

(
	cd "$ROOTFS"
	find . -print0 | cpio --null -o --format=newc > "$INITRAMFS"
)

mkdir -p "$(dirname "$LOG")"
set +e
timeout "$TIMEOUT" qemu-system-x86_64 \
	-machine accel=kvm \
	-cpu host \
	-m "$QEMU_MEM" \
	-smp "$QEMU_SMP" \
	-nographic \
	-no-reboot \
	-nodefaults \
	-serial mon:stdio \
	-kernel "$KERNEL" \
	-initrd "$INITRAMFS" \
	-append "console=ttyS0 panic=-1 oops=panic quiet zcnblk.wal_lane_batch=$QEMU_WAL_LANE_BATCH" | tee "$LOG"
qemu_status=${PIPESTATUS[0]}
set -e

if [[ "$qemu_status" -ne 0 ]]; then
	printf 'QEMU exited with status %s; log: %s\n' "$qemu_status" "$LOG" >&2
	exit "$qemu_status"
fi
if ! grep -q "\[zcnblk-wal-vm\] PASS:.*lane_batch=$QEMU_WAL_LANE_BATCH" "$LOG"; then
	printf 'zcnblk WAL QEMU smoke failed; log: %s\n' "$LOG" >&2
	exit 1
fi
if grep -Eq 'BUG:|Oops:|KASAN:|general protection fault|kernel panic' "$LOG"; then
	printf 'zcnblk WAL QEMU smoke found a kernel failure; log: %s\n' "$LOG" >&2
	exit 1
fi

printf 'zcnblk WAL QEMU smoke passed; log: %s\n' "$LOG"
