#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
KDIR="${KDIR:-/lib/modules/$KERNEL_RELEASE/build}"
AEAD_MODULE="${AEAD_MODULE:-/lib/modules/$KERNEL_RELEASE/kernel/crypto/aead.ko.xz}"
WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zcnblk-wal-smoke}"
ROOTFS="$WORK_DIR/rootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
LOG="${LOG:-$WORK_DIR/qemu.log}"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/qemu-zcnblk-cargo}"
BIN_DIR="$CARGO_TARGET_DIR/release"
QEMU_MEM="${QEMU_MEM:-2048M}"
QEMU_SMP="${QEMU_SMP:-4}"
TIMEOUT="${TIMEOUT:-180s}"
QEMU_WAL_LANE_BATCH="${QEMU_WAL_LANE_BATCH:-0}"
QEMU_SHM_ARENA_BACKING="${QEMU_SHM_ARENA_BACKING:-hugetlb}"
QEMU_HUGEPAGE_SIZE="${QEMU_HUGEPAGE_SIZE:-2M}"
QEMU_HUGEPAGES="${QEMU_HUGEPAGES:-16}"
QEMU_CARGO_CLEAN="${QEMU_CARGO_CLEAN:-0}"

case "$QEMU_WAL_LANE_BATCH" in
	0|1) ;;
	*)
		printf 'QEMU_WAL_LANE_BATCH must be 0 or 1, got %s\n' "$QEMU_WAL_LANE_BATCH" >&2
		exit 2
		;;
esac

case "$QEMU_SHM_ARENA_BACKING" in
	hugetlb|vmalloc) ;;
	*)
		printf 'QEMU_SHM_ARENA_BACKING must be hugetlb or vmalloc, got %s\n' \
			"$QEMU_SHM_ARENA_BACKING" >&2
		exit 2
		;;
esac

case "$QEMU_CARGO_CLEAN" in
	0|1) ;;
	*)
		printf 'QEMU_CARGO_CLEAN must be 0 or 1, got %s\n' "$QEMU_CARGO_CLEAN" >&2
		exit 2
		;;
esac

if [[ "$QEMU_SHM_ARENA_BACKING" == hugetlb ]] &&
	! [[ "$QEMU_HUGEPAGES" =~ ^[1-9][0-9]*$ ]]; then
	printf 'QEMU_HUGEPAGES must be a positive integer, got %s\n' "$QEMU_HUGEPAGES" >&2
	exit 2
fi

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
		--note 'ABI-v6 zcnblk external-HugeTLB arena and WAL correctness smoke in QEMU only' \
		-- env ZCNBLK_QEMU_COORDINATED=1 "$0" "$@"
fi

need cargo
need cpio
need ldd
need make
need od
need qemu-system-x86_64
need readelf
need stat
need timeout
need xz

[[ -r "$KERNEL" ]] || {
	printf 'kernel image is not readable: %s\n' "$KERNEL" >&2
	exit 1
}
[[ -d "$KDIR" ]] || {
	printf 'kernel build directory is missing: %s\n' "$KDIR" >&2
	exit 1
}
[[ -r "$AEAD_MODULE" ]] || {
	printf 'kernel AEAD module is not readable: %s\n' "$AEAD_MODULE" >&2
	exit 1
}
[[ -r "$ROOT/scripts/zcnblk-wal-qemu-init.sh" ]]

if [[ "$QEMU_CARGO_CLEAN" == 1 ]]; then
	cargo clean --target-dir "$CARGO_TARGET_DIR"
fi
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release \
	--bin zcnblk-shm-target \
	--bin zcnblk-wal-leaf \
	--bin zcnblk-order-smoke \
	--bin zcnblk-contract-smoke
make -C "$ROOT/kmods" KDIR="$KDIR"

verify_elf() {
	local path="$1"
	local blocks magic

	[[ -x "$path" ]] || {
		printf 'Cargo output is missing or not executable: %s\n' "$path" >&2
		return 1
	}
	magic="$(od -An -tx1 -N4 -- "$path" | tr -d '[:space:]')"
	[[ "$magic" == 7f454c46 ]] || {
		printf 'Cargo output is not ELF (magic=%s): %s\n' "$magic" "$path" >&2
		return 1
	}
	blocks="$(stat -c %b -- "$path")"
	(( blocks > 0 )) || {
		printf 'Cargo output has no allocated blocks (interrupted sparse link): %s\n' \
			"$path" >&2
		return 1
	}
	readelf -h -- "$path" >/dev/null
}

bins=(zcnblk-shm-target zcnblk-wal-leaf zcnblk-order-smoke zcnblk-contract-smoke)
for bin in "${bins[@]}"; do
	verify_elf "$BIN_DIR/$bin"
done

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
xz -dc -- "$AEAD_MODULE" > "$ROOTFS/modules/aead.ko"
cp "$ROOT/kmods/zcnblk_client_mod.ko" "$ROOTFS/modules/zcnblk_client_mod.ko"

for bin in "${bins[@]}"; do
	cp "$BIN_DIR/$bin" "$ROOTFS/$bin"
done

while IFS= read -r lib; do
	[[ -n "$lib" ]] || continue
	mkdir -p "$ROOTFS$(dirname "$lib")"
	cp "$lib" "$ROOTFS$lib"
done < <(
	{
		for bin in "${bins[@]}"; do
			ldd "$BIN_DIR/$bin"
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
qemu_append="console=ttyS0 panic=-1 oops=panic quiet zcnblk.wal_lane_batch=$QEMU_WAL_LANE_BATCH zcnblk.shm_arena_backing=$QEMU_SHM_ARENA_BACKING"
if [[ "$QEMU_SHM_ARENA_BACKING" == hugetlb ]]; then
	qemu_append+=" hugepagesz=$QEMU_HUGEPAGE_SIZE hugepages=$QEMU_HUGEPAGES"
fi
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
	-append "$qemu_append" | tee "$LOG"
qemu_status=${PIPESTATUS[0]}
set -e

if [[ "$qemu_status" -ne 0 ]]; then
	printf 'QEMU exited with status %s; log: %s\n' "$qemu_status" "$LOG" >&2
	exit "$qemu_status"
fi
if ! grep -q "\[zcnblk-wal-vm\] PASS:.*abi-v6.*arena=$QEMU_SHM_ARENA_BACKING.*lane_batch=$QEMU_WAL_LANE_BATCH" "$LOG"; then
	printf 'zcnblk WAL QEMU smoke failed; log: %s\n' "$LOG" >&2
	exit 1
fi
if grep -Eq 'BUG:|Oops:|KASAN:|general protection fault|kernel panic' "$LOG"; then
	printf 'zcnblk WAL QEMU smoke found a kernel failure; log: %s\n' "$LOG" >&2
	exit 1
fi

printf 'zcnblk WAL QEMU smoke passed; log: %s\n' "$LOG"
