#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-/home/rob/.local/bin/agent-coord}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zcracing-mirror}"
ROOTFS="$WORK_DIR/rootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
LOG_DIR="$WORK_DIR/logs"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/qemu-zcracing-cargo}"
BIN="$CARGO_TARGET_DIR/release/zcracing-mirror"
LOCAL_IMAGE="$WORK_DIR/first-hop-terminal.ext4"
REMOTE_IMAGE="$WORK_DIR/remote-terminal.ext4"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-120}"
FRAMES="${FRAMES:-8}"
RESUME_FRAMES="${RESUME_FRAMES:-4}"
REMOTE_ACK_DELAY_MS="${REMOTE_ACK_DELAY_MS:-50}"
MCAST_PORT="${MCAST_PORT:-47040}"

need()
{
	command -v "$1" >/dev/null || {
		printf 'missing required command: %s\n' "$1" >&2
		exit 1
	}
}

if [[ "${ZCRACING_QEMU_COORDINATED:-0}" != 1 ]]; then
	[[ -x "$COORD_BIN" ]] || {
		printf 'missing coordinator: %s\n' "$COORD_BIN" >&2
		exit 1
	}
	exec "$COORD_BIN" run \
		--owner codex:zcutils-zcracing-qemu \
		--mode soft-exclusive --sensitivity high --priority 60 --ttl 1200 \
		--resource 'cpu=*;memory-bandwidth=*;kvm=*' \
		--note 'three-VM userspace racing high-water mirror correctness proof' \
		-- env ZCRACING_QEMU_COORDINATED=1 "$0" "$@"
fi

need cargo
need cpio
need ldd
need qemu-system-x86_64
need xz
[[ -r "$KERNEL" ]] || { printf 'kernel is not readable: %s\n' "$KERNEL" >&2; exit 1; }
[[ -x /sbin/mkfs.ext4 ]] || { printf 'missing /sbin/mkfs.ext4\n' >&2; exit 1; }

mkdir -p "$WORK_DIR" "$LOG_DIR"
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release --bin zcracing-mirror

rm -rf -- "$ROOTFS"
mkdir -p "$ROOTFS/bin" "$ROOTFS/lib" "$ROOTFS/lib64" "$ROOTFS/proc" \
	"$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/tmp" "$ROOTFS/mnt" "$ROOTFS/modules"
cp /usr/bin/busybox "$ROOTFS/bin/busybox"
for applet in awk cat dmesg echo grep insmod ip mkdir mount poweroff sh sleep sync tail truncate umount; do
	ln -s busybox "$ROOTFS/bin/$applet"
done

module_copy()
{
	local source="$1"
	local output="$2"
	xz -dc -- "$source" > "$ROOTFS/modules/$output.ko"
}
module_copy "/lib/modules/$KERNEL_RELEASE/kernel/net/core/failover.ko.xz" failover
module_copy "/lib/modules/$KERNEL_RELEASE/kernel/drivers/net/net_failover.ko.xz" net_failover
module_copy "/lib/modules/$KERNEL_RELEASE/kernel/drivers/net/virtio_net.ko.xz" virtio_net
module_copy "/lib/modules/$KERNEL_RELEASE/kernel/drivers/block/virtio_blk.ko.xz" virtio_blk
module_copy "/lib/modules/$KERNEL_RELEASE/kernel/fs/mbcache.ko.xz" mbcache
module_copy "/lib/modules/$KERNEL_RELEASE/kernel/fs/jbd2/jbd2.ko.xz" jbd2
module_copy "/lib/modules/$KERNEL_RELEASE/kernel/lib/crc/crc16.ko.xz" crc16
module_copy "/lib/modules/$KERNEL_RELEASE/kernel/fs/ext4/ext4.ko.xz" ext4

cp "$ROOT/scripts/zcracing-mirror-qemu-init.sh" "$ROOTFS/init"
chmod +x "$ROOTFS/init"
cp "$BIN" "$ROOTFS/zcracing-mirror"
while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$ROOTFS$(dirname "$library")"
	cp "$library" "$ROOTFS$library"
done < <(
	{
		ldd "$BIN"
		ldd /usr/bin/busybox
	} | awk '/=> \// { print $3; next } /^[[:space:]]*\/lib/ { print $1; next }' | sort -u
)

(
	cd "$ROOTFS"
	find . -print0 | cpio --null -o --format=newc > "$INITRAMFS"
)

truncate -s 128M "$LOCAL_IMAGE"
truncate -s 128M "$REMOTE_IMAGE"
/sbin/mkfs.ext4 -F -q "$LOCAL_IMAGE"
/sbin/mkfs.ext4 -F -q "$REMOTE_IMAGE"

declare -a qemu_pids=()
cleanup()
{
	local pid
	for pid in "${qemu_pids[@]}"; do
		if kill -0 "$pid" 2>/dev/null; then
			kill -TERM "$pid" 2>/dev/null || true
		fi
	done
}
trap cleanup EXIT

launch_vm()
{
	local role="$1"
	local drive_image="${2:-}"
	local log="$LOG_DIR/$role.log"
	local -a drive_args=()
	: > "$log"
	if [[ -n "$drive_image" ]]; then
		drive_args=(
			-drive "if=none,id=terminal,file=$drive_image,format=raw,cache=none,aio=threads"
			-device virtio-blk-pci,drive=terminal
		)
	fi
	qemu-system-x86_64 \
		-machine accel=kvm -cpu host -m 512M -smp 1 -nographic -no-reboot -nodefaults \
		-serial "file:$log" -kernel "$KERNEL" -initrd "$INITRAMFS" \
		-append "console=ttyS0 panic=-1 oops=panic quiet net.ifnames=0 zcrm.role=$role zcrm.frames=$FRAMES zcrm.resume_frames=$RESUME_FRAMES zcrm.delay_ms=$REMOTE_ACK_DELAY_MS" \
		-netdev "socket,id=net0,mcast=230.44.0.1:$MCAST_PORT" \
		-device virtio-net-pci,netdev=net0 \
		"${drive_args[@]}" >/dev/null 2>>"$log" &
	qemu_pids+=("$!")
}

launch_vm remote-leaf "$REMOTE_IMAGE"
launch_vm first-hop "$LOCAL_IMAGE"
launch_vm client

deadline=$((SECONDS + TIMEOUT_SECONDS))
while :; do
	alive=0
	for pid in "${qemu_pids[@]}"; do
		kill -0 "$pid" 2>/dev/null && alive=$((alive + 1))
	done
	(( alive == 0 )) && break
	if (( SECONDS >= deadline )); then
		printf 'three-VM mirror timed out; logs follow\n' >&2
		for log in "$LOG_DIR"/*.log; do cat "$log"; done
		exit 1
	fi
	sleep 0.1
done

for pid in "${qemu_pids[@]}"; do wait "$pid"; done
trap - EXIT

for role in client first-hop remote-leaf; do
	log="$LOG_DIR/$role.log"
	if ! grep -q "RACING_MIRROR_QEMU_PASS role=$role" "$log"; then
		cat "$log"
		printf 'missing pass marker for %s\n' "$role" >&2
		exit 1
	fi
	if grep -Eq 'RACING_MIRROR_QEMU_FAIL|BUG:|Oops:|general protection fault|kernel panic' "$log"; then
		cat "$log"
		printf 'failure marker in %s\n' "$role" >&2
		exit 1
	fi
done

grep -q 'payload_userspace_copy_bytes=0' "$LOG_DIR/first-hop.log"
grep -q 'payload_userspace_copy_bytes=0' "$LOG_DIR/remote-leaf.log"
grep -q 'RACING_MIRROR_VERIFY_PASS' "$LOG_DIR/first-hop.log"
grep -q 'RACING_MIRROR_VERIFY_PASS' "$LOG_DIR/remote-leaf.log"
grep -q 'RACING_MIRROR_QEMU_RESTART' "$LOG_DIR/client.log"
grep -q 'RACING_MIRROR_QEMU_RESTART' "$LOG_DIR/first-hop.log"
grep -q 'RACING_MIRROR_QEMU_RESTART' "$LOG_DIR/remote-leaf.log"
grep -q 'RACING_MIRROR_QEMU_LAG_INJECTED' "$LOG_DIR/remote-leaf.log"
grep -q "RACING_MIRROR_REPLAY_PASS from_hwm=$((FRAMES - 1)) to_hwm=$FRAMES payload_userspace_copy_bytes=0" "$LOG_DIR/first-hop.log"

client_elapsed="$(awk -F'elapsed_s=' '/RACING_MIRROR_CLIENT_PASS/ { split($2, value, " "); total += value[1] } END { printf "%.6f", total }' "$LOG_DIR/client.log")"
minimum_delay="$(awk -v frames="$FRAMES" -v resume="$RESUME_FRAMES" -v delay="$REMOTE_ACK_DELAY_MS" 'BEGIN { printf "%.6f", (frames + resume) * delay / 1000.0 * 0.80 }')"
awk -v actual="$client_elapsed" -v minimum="$minimum_delay" 'BEGIN { exit !(actual + 0 >= minimum + 0) }' || {
	printf 'client durable HWM advanced too early: elapsed=%s minimum=%s\n' "$client_elapsed" "$minimum_delay" >&2
	exit 1
}

printf 'RACING_MIRROR_QEMU_MATRIX_PASS machines=3 placement=userspace local_terminal=virtio-blk-ext4 remote_terminal=virtio-blk-ext4 payload_relay=splice-tee first_hop_payload_userspace_copy_bytes=0 remote_payload_userspace_copy_bytes=0 durable_hwm=min-contiguous-legs process_restart_resume=true lagging_remote_suffix_replay=true delayed_leg_ms=%s elapsed_s=%s logs=%s\n' \
	"$REMOTE_ACK_DELAY_MS" "$client_elapsed" "$LOG_DIR"
