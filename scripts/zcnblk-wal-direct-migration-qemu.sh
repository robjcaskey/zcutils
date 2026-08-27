#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
KDIR="${KDIR:-/lib/modules/$KERNEL_RELEASE/build}"
RUN_TAG="$(date -u +%Y%m%dT%H%M%SZ)-$$"
WORK_DIR="${WORK_DIR:-/mnt/bulk_data/zcutils-qemu/zcnblk-direct-migration-$RUN_TAG}"
ROOTFS="$WORK_DIR/rootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
LOG_DIR="$WORK_DIR/logs"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-180}"
network_tag="$(printf '%04x' $(( $$ % 65536 )))"
bridge="zdm${network_tag}b"

need() { command -v "$1" >/dev/null || { printf 'missing required command: %s\n' "$1" >&2; exit 1; }; }

if [[ "${ZCNBLK_DIRECT_MIGRATION_QEMU_COORDINATED:-0}" != 1 ]]; then
	need "$COORD_BIN"
	exec "$COORD_BIN" run --owner codex:zcutils-direct-migration-qemu \
		--mode soft-exclusive --sensitivity high --priority 65 --ttl 1200 \
		--resource 'cpu=*;memory-bandwidth=*;kvm=*;block=zcnblk0' \
		--note 'three-VM proxy-free zcnblk direct-route migration correctness proof' \
		-- env ZCNBLK_DIRECT_MIGRATION_QEMU_COORDINATED=1 "$0" "$@"
fi

for command in cargo cpio ip ldd qemu-system-x86_64 readelf sudo timeout xz; do need "$command"; done
[[ -r "$KERNEL" && -d "$KDIR" ]] || { printf 'missing kernel or headers\n' >&2; exit 1; }
[[ -r "$ROOT/scripts/zcnblk-wal-direct-migration-qemu-init.sh" ]]

mkdir -p "$ROOTFS/bin" "$ROOTFS/lib" "$ROOTFS/lib64" "$ROOTFS/modules" \
	"$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/tmp" "$LOG_DIR"

cargo build --release --bin zcnblk-wal-leaf --bin zcnblk-shm-target \
	--bin zcnblk-edge-continuity --bin zcnblk-direct-migratectl

bins=(zcnblk-wal-leaf zcnblk-shm-target zcnblk-edge-continuity zcnblk-direct-migratectl)
for bin in "${bins[@]}"; do
	[[ -x "$ROOT/target/release/$bin" ]]
	readelf -h "$ROOT/target/release/$bin" >/dev/null
done

cp /usr/bin/busybox "$ROOTFS/bin/busybox"
for applet in cat dmesg echo grep insmod ip kill mkdir mount nc poweroff rmmod seq sh sleep sync taskset true uname; do
	ln -s busybox "$ROOTFS/bin/$applet"
done
cp "$ROOT/scripts/zcnblk-wal-direct-migration-qemu-init.sh" "$ROOTFS/init"
chmod +x "$ROOTFS/init"

copy_xz_module() { xz -dc -- "$1" >"$ROOTFS/modules/$2.ko"; }
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/net/core/failover.ko.xz" failover
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/drivers/net/net_failover.ko.xz" net_failover
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/drivers/net/virtio_net.ko.xz" virtio_net
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/crypto/aead.ko.xz" aead
cp "$ROOT/kmods/zcnblk_client_mod.ko" "$ROOTFS/modules/zcnblk_client_mod.ko"
for bin in "${bins[@]}"; do cp "$ROOT/target/release/$bin" "$ROOTFS/$bin"; done

while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$ROOTFS$(dirname "$library")"
	cp "$library" "$ROOTFS$library"
done < <(
	{
		for bin in "${bins[@]}"; do ldd "$ROOT/target/release/$bin"; done
		ldd /usr/bin/busybox
	} | awk '/=> \// { print $3; next } /^[[:space:]]*\/lib/ { print $1; next }' | sort -u
)
(
	cd "$ROOTFS"
	find . -print0 | cpio --null -o --format=newc >"$INITRAMFS"
)

roles=(source destination client)
declare -a qemu_pids=()
declare -a tap_devices=()

verified_stop_qemu()
{
	local pid="$1" role="$2" comm cmdline
	[[ "$pid" =~ ^[0-9]+$ ]] && [[ -r "/proc/$pid/comm" ]] || return 0
	comm="$(cat "/proc/$pid/comm")"
	cmdline="$(tr '\0' ' ' <"/proc/$pid/cmdline")"
	if [[ "$comm" == qemu-system-* && "$cmdline" == *"$INITRAMFS"*"zcdm.role=$role"* ]]; then
		kill -TERM "$pid" 2>/dev/null || true
	else
		printf 'refusing to stop unexpected pid=%s comm=%s role=%s\n' "$pid" "$comm" "$role" >&2
		return 1
	fi
}

cleanup()
{
	local index
	set +e
	for index in "${!qemu_pids[@]}"; do verified_stop_qemu "${qemu_pids[$index]}" "${roles[$index]}"; done
	for tap in "${tap_devices[@]:-}"; do [[ -z "$tap" ]] || sudo -n ip link del "$tap" 2>/dev/null || true; done
	sudo -n ip link del "$bridge" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

! ip link show dev "$bridge" >/dev/null 2>&1 || { printf 'bridge already exists: %s\n' "$bridge" >&2; exit 1; }
sudo -n ip link add "$bridge" type bridge
sudo -n ip link set "$bridge" type bridge stp_state 0
sudo -n ip link set "$bridge" up

launch_vm()
{
	local role="$1" suffix memory smp tap log
	case "$role" in
		source) suffix=02; memory=512M; smp=2 ;;
		destination) suffix=03; memory=512M; smp=2 ;;
		client) suffix=04; memory=1536M; smp=6 ;;
	esac
	tap="zdm${network_tag}${suffix}"
	log="$LOG_DIR/$role.log"
	: >"$log"
	[[ ${#tap} -le 15 ]]
	sudo -n ip tuntap add dev "$tap" mode tap user "$(id -un)"
	sudo -n ip link set "$tap" master "$bridge"
	sudo -n ip link set "$tap" up
	tap_devices+=("$tap")
	qemu-system-x86_64 -machine accel=kvm -cpu host -m "$memory" -smp "$smp" \
		-nographic -no-reboot -nodefaults -serial "file:$log" \
		-kernel "$KERNEL" -initrd "$INITRAMFS" \
		-append "console=ttyS0 panic=-1 oops=panic quiet net.ifnames=0 rootfstype=tmpfs zcdm.role=$role" \
		-netdev "tap,id=net0,ifname=$tap,script=no,downscript=no" \
		-device "virtio-net-pci,netdev=net0,mac=52:54:83:00:00:$suffix" \
		>/dev/null 2>>"$log" &
	qemu_pids+=("$!")
}

launch_vm source
launch_vm destination
launch_vm client

deadline=$((SECONDS + TIMEOUT_SECONDS))
while :; do
	alive=0
	for pid in "${qemu_pids[@]}"; do kill -0 "$pid" 2>/dev/null && alive=$((alive + 1)); done
	(( alive == 0 )) && break
	if (( SECONDS >= deadline )); then
		printf 'direct migration QEMU test timed out\n' >&2
		for log in "$LOG_DIR"/*.log; do printf '\n== %s ==\n' "$log"; tail -160 "$log"; done
		exit 1
	fi
	sleep 0.1
done

status=0
for pid in "${qemu_pids[@]}"; do wait "$pid" || status=1; done
cleanup
trap - EXIT
(( status == 0 )) || { printf 'one or more QEMU guests failed\n' >&2; exit 1; }

for role in source destination client; do
	grep -q "ZCNBLK_DIRECT_MIGRATION_QEMU_PASS role=$role" "$LOG_DIR/$role.log" || {
		cat "$LOG_DIR/$role.log"; printf 'missing pass marker for %s\n' "$role" >&2; exit 1;
	}
	if grep -Eq 'ZCNBLK_DIRECT_MIGRATION_QEMU_FAIL|BUG:|Oops:|KASAN:|general protection fault|kernel panic' "$LOG_DIR/$role.log"; then
		cat "$LOG_DIR/$role.log"; printf 'failure marker for %s\n' "$role" >&2; exit 1
	fi
done

control_line="$(grep '^OK active_destination=true ' "$LOG_DIR/client.log" | tail -1)"
printf 'ZCNBLK_WAL_DIRECT_MIGRATION_QEMU_PASS machines=3 transport=tcp-unicast client_block_edge=/dev/zcnblk0 placement=userspace foreground_hops=1 migration_proxy=false stable_descriptor=true reconnects=0 control=%q artifact=%s\n' \
	"$control_line" "$WORK_DIR"
