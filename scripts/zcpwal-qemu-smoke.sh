#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zcpwal-smoke}"
ROOTFS="$WORK_DIR/rootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
LOG_DIR="$WORK_DIR/logs"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/qemu-zcpwal-cargo}"
BIN="$CARGO_TARGET_DIR/release/zcpwal-qemu-smoke"
JOURNAL_IMAGE="$WORK_DIR/journal.raw"
BASE_IMAGE="$WORK_DIR/base.raw"
FILES_IMAGE="$WORK_DIR/files.ext4"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-90}"

need()
{
	command -v "$1" >/dev/null || {
		printf 'missing required command: %s\n' "$1" >&2
		exit 1
	}
}

if [[ "${ZCPWAL_QEMU_COORDINATED:-0}" != 1 ]]; then
	need "$COORD_BIN"
	exec "$COORD_BIN" run \
		--owner codex:zcutils-zcpwal-qemu \
		--mode soft-exclusive --sensitivity high --priority 60 --ttl 1200 \
		--resource 'cpu=*;memory-bandwidth=*;kvm=*' \
		--note 'persistent WAL terminal virtio-blk and ext4 crash-recovery matrix' \
		-- env ZCPWAL_QEMU_COORDINATED=1 "$0" "$@"
fi

need cargo
need cpio
need ldd
need qemu-system-x86_64
need timeout
need xz
[[ -r "$KERNEL" ]] || {
	printf 'kernel is not readable: %s\n' "$KERNEL" >&2
	exit 1
}
[[ -x /sbin/mkfs.ext4 ]]

mkdir -p "$WORK_DIR" "$LOG_DIR"
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release --bin zcpwal-qemu-smoke

rm -rf -- "$ROOTFS"
mkdir -p "$ROOTFS/bin" "$ROOTFS/lib" "$ROOTFS/lib64" \
	"$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/tmp" "$ROOTFS/mnt"
cp /usr/bin/busybox "$ROOTFS/bin/busybox"
for applet in cat dmesg echo insmod mkdir mount poweroff sh sleep sync tail umount; do
	ln -s busybox "$ROOTFS/bin/$applet"
done
mkdir -p "$ROOTFS/modules"
xz -dc -- "/lib/modules/$KERNEL_RELEASE/kernel/drivers/block/virtio_blk.ko.xz" \
	> "$ROOTFS/modules/virtio_blk.ko"
for module in mbcache jbd2 crc16 ext4; do
	case "$module" in
		mbcache) source="/lib/modules/$KERNEL_RELEASE/kernel/fs/mbcache.ko.xz" ;;
		jbd2) source="/lib/modules/$KERNEL_RELEASE/kernel/fs/jbd2/jbd2.ko.xz" ;;
		crc16) source="/lib/modules/$KERNEL_RELEASE/kernel/lib/crc/crc16.ko.xz" ;;
		ext4) source="/lib/modules/$KERNEL_RELEASE/kernel/fs/ext4/ext4.ko.xz" ;;
	esac
	xz -dc -- "$source" > "$ROOTFS/modules/$module.ko"
done
cp "$ROOT/scripts/zcpwal-qemu-init.sh" "$ROOTFS/init"
chmod +x "$ROOTFS/init"
cp "$BIN" "$ROOTFS/zcpwal-qemu-smoke"

while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$ROOTFS$(dirname "$library")"
	cp "$library" "$ROOTFS$library"
done < <(
	{
		ldd "$BIN"
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

truncate -s 0 "$JOURNAL_IMAGE"
truncate -s 0 "$BASE_IMAGE"
truncate -s 0 "$FILES_IMAGE"
truncate -s 64M "$JOURNAL_IMAGE"
truncate -s 64M "$BASE_IMAGE"
truncate -s 384M "$FILES_IMAGE"
/sbin/mkfs.ext4 -F -q "$FILES_IMAGE"

boot_phase()
{
	local phase="$1"
	local marker="$2"
	local crash="$3"
	local log="$LOG_DIR/$phase.log"
	local pid deadline

	: > "$log"
	qemu-system-x86_64 \
		-machine accel=kvm \
		-cpu host \
		-m 1024M \
		-smp 2 \
		-nographic \
		-no-reboot \
		-nodefaults \
		-serial "file:$log" \
		-kernel "$KERNEL" \
		-initrd "$INITRAMFS" \
		-append "console=ttyS0 panic=-1 oops=panic quiet zcpwal.phase=$phase" \
		-drive "if=none,id=journal,file=$JOURNAL_IMAGE,format=raw,cache=none,aio=threads" \
		-device virtio-blk-pci,drive=journal \
		-drive "if=none,id=base,file=$BASE_IMAGE,format=raw,cache=none,aio=threads" \
		-device virtio-blk-pci,drive=base \
		-drive "if=none,id=files,file=$FILES_IMAGE,format=raw,cache=none,aio=threads" \
		-device virtio-blk-pci,drive=files \
		>/dev/null 2>>"$log" &
	pid=$!
	deadline=$((SECONDS + TIMEOUT_SECONDS))
	while ! grep -q "$marker" "$log" 2>/dev/null; do
		if ! kill -0 "$pid" 2>/dev/null; then
			wait "$pid" || true
			cat "$log"
			printf 'QEMU phase %s exited before marker %s\n' "$phase" "$marker" >&2
			return 1
		fi
		if (( SECONDS >= deadline )); then
			kill -KILL "$pid" 2>/dev/null || true
			wait "$pid" 2>/dev/null || true
			cat "$log"
			printf 'QEMU phase %s timed out waiting for %s\n' "$phase" "$marker" >&2
			return 1
		fi
		sleep 0.1
	done
	if [[ "$crash" == 1 ]]; then
		kill -KILL "$pid"
		wait "$pid" 2>/dev/null || true
	else
		deadline=$((SECONDS + TIMEOUT_SECONDS))
		while kill -0 "$pid" 2>/dev/null; do
			if (( SECONDS >= deadline )); then
				kill -KILL "$pid" 2>/dev/null || true
				wait "$pid" 2>/dev/null || true
				cat "$log"
				printf 'QEMU phase %s did not power off\n' "$phase" >&2
				return 1
			fi
			sleep 0.1
		done
		wait "$pid"
	fi
	if grep -Eq 'BUG:|Oops:|general protection fault|kernel panic|ZCPWAL_QEMU_PHASE_FAIL' "$log"; then
		cat "$log"
		printf 'QEMU phase %s reported a failure\n' "$phase" >&2
		return 1
	fi
	printf 'zcpwal-qemu-phase: phase=%s crash=%s marker=%s log=%s\n' \
		"$phase" "$crash" "$marker" "$log"
}

boot_phase file-matrix 'ZCPWAL_QEMU_PHASE_PASS phase=file-matrix' 0
boot_phase direct-block 'ZCPWAL_QEMU_DIRECT_BLOCK_PASS' 0
boot_phase block-init 'ZCPWAL_QEMU_PHASE_PASS phase=block-init' 0
boot_phase crash-before-publish 'ZCPWAL_CRASH_PAYLOAD_DURABLE' 1
boot_phase verify-before-publish 'ZCPWAL_VERIFY_OLD_PREFIX_PASS' 0
boot_phase crash-after-commit 'ZCPWAL_CRASH_COMMIT_DURABLE' 1
boot_phase verify-after-commit 'ZCPWAL_VERIFY_NEW_PREFIX_PASS' 0
boot_phase crash-unsynced 'ZCPWAL_CRASH_UNSYNCED_APPENDED' 1
boot_phase verify-unsynced 'ZCPWAL_VERIFY_UNSYNCED_IGNORED_PASS' 0
boot_phase corrupt-block 'ZCPWAL_QEMU_BLOCK_CORRUPTION_PASS' 0

printf 'ZCPWAL_QEMU_MATRIX_PASS logs=%s journal=%s base=%s files=%s\n' \
	"$LOG_DIR" "$JOURNAL_IMAGE" "$BASE_IMAGE" "$FILES_IMAGE"
