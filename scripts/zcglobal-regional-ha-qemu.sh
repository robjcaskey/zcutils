#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
KDIR="${KDIR:-/lib/modules/$KERNEL_RELEASE/build}"
WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zcglobal-regional-ha}"
ROOTFS="$WORK_DIR/rootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
LOG_DIR="$WORK_DIR/logs"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/qemu-zcglobal-volume-cargo}"
BIN_DIR="$CARGO_TARGET_DIR/release"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-180}"
OPERATIONS="${OPERATIONS:-64}"
MOVE_END="${MOVE_END:-96}"
SCENARIO="${ZCGLOBAL_SCENARIO:-clean}"
DECLARED_LOSS_CHECKPOINT="${DECLARED_LOSS_CHECKPOINT:-32}"
FAILURE_SUFFIX="${ZCGLOBAL_REGIONAL_FAILURE_SUFFIX:-a}"
network_tag="$(printf '%04x' $(( $$ % 65536 )))"
bridge="zgr${network_tag}b"

need() { command -v "$1" >/dev/null || { printf 'missing required command: %s\n' "$1" >&2; exit 1; }; }

if [[ "${ZCGLOBAL_REGIONAL_HA_COORDINATED:-0}" != 1 ]]; then
	need "$COORD_BIN"
	exec "$COORD_BIN" run --owner codex:zcutils-global-regional-ha-qemu \
		--mode soft-exclusive --sensitivity high --priority 65 --ttl 1800 \
		--resource 'cpu=*;memory-bandwidth=*;kvm=*' \
		--note 'nine-VM two-region userspace 2-of-3 data quorum correctness proof' \
		-- env ZCGLOBAL_REGIONAL_HA_COORDINATED=1 "$0" "$@"
fi

for command in cargo cpio ip ldd make qemu-system-x86_64 readelf sudo timeout truncate xz; do need "$command"; done
[[ -x /usr/sbin/sfdisk ]] || { printf 'missing sfdisk\n' >&2; exit 1; }
[[ -r "$KERNEL" && -d "$KDIR" ]] || { printf 'missing kernel or headers\n' >&2; exit 1; }
(( OPERATIONS >= 8 && OPERATIONS % 4 == 0 && MOVE_END > OPERATIONS && MOVE_END < 4096 )) || {
	printf 'require OPERATIONS >= 8 divisible by four and OPERATIONS < MOVE_END < 4096\n' >&2; exit 2;
}
[[ "$SCENARIO" == clean || "$SCENARIO" == declared-loss ]] || {
	printf 'ZCGLOBAL_SCENARIO must be clean or declared-loss\n' >&2; exit 2;
}
[[ "$FAILURE_SUFFIX" == a || "$FAILURE_SUFFIX" == b || "$FAILURE_SUFFIX" == c ]] || {
	printf 'ZCGLOBAL_REGIONAL_FAILURE_SUFFIX must be a, b, or c\n' >&2; exit 2;
}
if [[ "$SCENARIO" == declared-loss ]]; then
	(( DECLARED_LOSS_CHECKPOINT >= 8 && DECLARED_LOSS_CHECKPOINT % 4 == 0 && DECLARED_LOSS_CHECKPOINT < OPERATIONS )) || {
		printf 'require declared-loss checkpoint divisible by four and 8 <= checkpoint < OPERATIONS\n' >&2; exit 2;
	}
fi

mkdir -p "$WORK_DIR" "$LOG_DIR"
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release \
	--bin zcnblk-wal-failover --bin zcnblk-wal-ha-route --bin zcnblk-wal-quorum --bin zcnblk-wal-leaf \
	--bin zcnblk-shm-target --bin zcglobal-volume-workload
make -C "$ROOT/kmods" KDIR="$KDIR"

rm -rf -- "$ROOTFS"
mkdir -p "$ROOTFS/bin" "$ROOTFS/lib" "$ROOTFS/lib64" "$ROOTFS/modules" \
	"$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/tmp" "$ROOTFS/run" "$ROOTFS/etc"
cp /usr/bin/busybox "$ROOTFS/bin/busybox"
for applet in cat dmesg echo grep hostname insmod ip kill ln mkdir mount nc poweroff rmmod sh sleep tail true; do
	ln -s busybox "$ROOTFS/bin/$applet"
done
cp "$ROOT/scripts/zcglobal-regional-ha-qemu-init.sh" "$ROOTFS/init"
cp "$ROOT/scripts/zcglobal-regional-ha-qemu-leaf-fail.sh" "$ROOTFS/leaf-fail"
chmod +x "$ROOTFS/init" "$ROOTFS/leaf-fail"

copy_xz_module() { xz -dc -- "$1" >"$ROOTFS/modules/$2.ko"; }
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/net/core/failover.ko.xz" failover
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/drivers/net/net_failover.ko.xz" net_failover
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/drivers/net/virtio_net.ko.xz" virtio_net
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/drivers/block/virtio_blk.ko.xz" virtio_blk
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/crypto/aead.ko.xz" aead
cp "$ROOT/kmods/zcnblk_client_mod.ko" "$ROOTFS/modules/zcnblk_client_mod.ko"

bins=(zcnblk-wal-failover zcnblk-wal-ha-route zcnblk-wal-quorum zcnblk-wal-leaf zcnblk-shm-target zcglobal-volume-workload)
for bin in "${bins[@]}"; do cp "$BIN_DIR/$bin" "$ROOTFS/$bin"; done
while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$ROOTFS$(dirname "$library")"
	cp "$library" "$ROOTFS$library"
done < <({ for bin in "${bins[@]}"; do ldd "$BIN_DIR/$bin"; done; ldd /usr/bin/busybox; } |
	awk '/=> \// { print $3; next } /^[[:space:]]*\/lib/ { print $1; next }' | sort -u)
(
	cd "$ROOTFS"
	find . -print0 | cpio --null -o --format=newc >"$INITRAMFS"
)

declare -A disk_ids=(
	[us-leaf-a]=46aa0011 [us-leaf-b]=46aa0012 [us-leaf-c]=46aa0013
	[eu-leaf-a]=46aa0021 [eu-leaf-b]=46aa0022 [eu-leaf-c]=46aa0023
)
for role in "${!disk_ids[@]}"; do
	image="$WORK_DIR/$role-terminal.raw"
	truncate -s 0 "$image"
	truncate -s 36M "$image"
	printf '2048,65536,83,*\n' | /usr/sbin/sfdisk --quiet --label dos "$image"
	/usr/sbin/sfdisk --disk-id "$image" "0x${disk_ids[$role]}" >/dev/null
done

declare -a qemu_pids=()
declare -a tap_devices=()
cleanup()
{
	for pid in "${qemu_pids[@]}"; do
		if kill -0 "$pid" 2>/dev/null; then kill -TERM "$pid" 2>/dev/null || true; fi
	done
	for tap in "${tap_devices[@]:-}"; do
		[[ -z "$tap" ]] || sudo -n ip link del "$tap" 2>/dev/null || true
	done
	sudo -n ip link del "$bridge" 2>/dev/null || true
}
trap cleanup EXIT

! ip link show dev "$bridge" >/dev/null 2>&1 || {
	printf 'QEMU bridge already exists: %s\n' "$bridge" >&2
	exit 1
}
sudo -n ip link add "$bridge" type bridge
sudo -n ip link set "$bridge" type bridge stp_state 0
sudo -n ip link set "$bridge" up

launch_vm()
{
	local role="$1"
	local image="${2:-}"
	local memory=384M
	local smp=2
	[[ "$role" == region-* ]] && { memory=1024M; smp=4; }
	[[ "$role" == gateway ]] && memory=512M
	local mac_suffix
	case "$role" in
		region-us) mac_suffix=01 ;; gateway) mac_suffix=02 ;; region-eu) mac_suffix=03 ;;
		us-leaf-a) mac_suffix=11 ;; us-leaf-b) mac_suffix=12 ;; us-leaf-c) mac_suffix=13 ;;
		eu-leaf-a) mac_suffix=21 ;; eu-leaf-b) mac_suffix=22 ;; eu-leaf-c) mac_suffix=23 ;;
	esac
	local log="$LOG_DIR/$role.log"
	local tap="zgr${network_tag}${mac_suffix}"
	local -a drive=()
	: >"$log"
	[[ ${#tap} -le 15 ]] || { printf 'TAP name too long: %s\n' "$tap" >&2; exit 1; }
	! ip link show dev "$tap" >/dev/null 2>&1 || {
		printf 'QEMU TAP already exists: %s\n' "$tap" >&2
		exit 1
	}
	sudo -n ip tuntap add dev "$tap" mode tap user "$(id -un)"
	sudo -n ip link set "$tap" master "$bridge"
	sudo -n ip link set "$tap" up
	tap_devices+=("$tap")
	if [[ -n "$image" ]]; then
		drive=(-drive "if=none,id=terminal,file=$image,format=raw,cache=none,aio=threads" -device virtio-blk-pci,drive=terminal)
	fi
	qemu-system-x86_64 -machine accel=kvm -cpu host -m "$memory" -smp "$smp" -nographic -no-reboot -nodefaults \
		-serial "file:$log" -kernel "$KERNEL" -initrd "$INITRAMFS" \
		-append "console=ttyS0 panic=-1 oops=panic quiet net.ifnames=0 rootfstype=tmpfs zcgha.role=$role zcgha.operations=$OPERATIONS zcgha.move_end=$MOVE_END zcgha.scenario=$SCENARIO zcgha.loss_checkpoint=$DECLARED_LOSS_CHECKPOINT zcgha.failure_suffix=$FAILURE_SUFFIX" \
		-netdev "tap,id=net0,ifname=$tap,script=no,downscript=no" -device "virtio-net-pci,netdev=net0,mac=52:54:46:00:00:$mac_suffix" \
		"${drive[@]}" >/dev/null 2>>"$log" &
	qemu_pids+=("$!")
}

for role in us-leaf-a us-leaf-b us-leaf-c eu-leaf-a eu-leaf-b eu-leaf-c; do
	launch_vm "$role" "$WORK_DIR/$role-terminal.raw"
done
launch_vm region-us
launch_vm region-eu
launch_vm gateway

deadline=$((SECONDS + TIMEOUT_SECONDS))
while :; do
	alive=0
	for pid in "${qemu_pids[@]}"; do kill -0 "$pid" 2>/dev/null && alive=$((alive + 1)); done
	(( alive == 0 )) && break
	if (( SECONDS >= deadline )); then
		printf 'regional HA QEMU test timed out\n' >&2
		for log in "$LOG_DIR"/*.log; do printf '\n== %s ==\n' "$log"; tail -160 "$log"; done
		exit 1
	fi
	sleep 0.1
done
for pid in "${qemu_pids[@]}"; do wait "$pid"; done
cleanup
trap - EXIT

us_failed="us-leaf-$FAILURE_SUFFIX"
eu_failed="eu-leaf-$FAILURE_SUFFIX"
pass_roles=(gateway region-us region-eu)
failure_roles=()
for suffix in a b c; do
	role="eu-leaf-$suffix"
	if [[ "$role" == "$eu_failed" ]]; then failure_roles+=("$role"); else pass_roles+=("$role"); fi
done
if [[ "$SCENARIO" == clean ]]; then
	for suffix in a b c; do
		role="us-leaf-$suffix"
		if [[ "$role" == "$us_failed" ]]; then failure_roles+=("$role"); else pass_roles+=("$role"); fi
	done
else
	failure_roles+=(us-leaf-a us-leaf-b us-leaf-c)
fi
for role in "${pass_roles[@]}"; do
	grep -q "ZCGLOBAL_REGIONAL_HA_QEMU_PASS role=$role" "$LOG_DIR/$role.log" || {
		cat "$LOG_DIR/$role.log"; printf 'missing pass marker for %s\n' "$role" >&2; exit 1;
	}
done
for role in "${failure_roles[@]}"; do
	grep -q 'ZCGLOBAL_REGIONAL_LEAF_FAILURE' "$LOG_DIR/$role.log" || {
		cat "$LOG_DIR/$role.log"; printf 'missing intentional failure marker for %s\n' "$role" >&2; exit 1;
	}
done
for log in "$LOG_DIR"/*.log; do
	if grep -Eq 'ZCGLOBAL_REGIONAL_HA_QEMU_FAIL|BUG:|Oops:|general protection fault|kernel panic' "$log"; then
		cat "$log"; printf 'failure marker in %s\n' "$log" >&2; exit 1
	fi
done
if [[ "$SCENARIO" == clean ]]; then
	grep -q 'ZCGLOBAL_VOLUME_STAY_HA_PASS.*regional_replication=2-of-3.*source_leaf_failures=1.*target_leaf_failures=1' "$LOG_DIR/region-us.log"
	grep -q 'ZCGLOBAL_VOLUME_MOVE_PASS.*pod_data_loss=0' "$LOG_DIR/region-eu.log"
	acknowledged_data_loss=0
	source_terminal_grading=surviving-two
else
	grep -q 'ZCGLOBAL_VOLUME_DISASTER_SOURCE_READY.*regional_replication=2-of-3.*source_leaf_failures=1.*target_leaf_failures=1' "$LOG_DIR/region-us.log"
	grep -q 'ZCGLOBAL_VOLUME_DECLARED_LOSS_MOVE_PASS.*booked_missing=' "$LOG_DIR/region-eu.log"
	grep -q 'fence=declared-loss.*first_missing=Some(' "$LOG_DIR/gateway.log"
	acknowledged_data_loss="booked-$((DECLARED_LOSS_CHECKPOINT + 1))..$OPERATIONS"
	source_terminal_grading=unavailable-region-destroyed
fi
if [[ "$FAILURE_SUFFIX" == b ]]; then surviving_frontend=a; else surviving_frontend=b; fi
[[ "$FAILURE_SUFFIX" != c ]] || surviving_frontend=a
grep -q 'zcnblk-wal-quorum-leaf-degraded' "$LOG_DIR/eu-leaf-$surviving_frontend.log"
if [[ "$SCENARIO" == clean ]]; then
	grep -q 'zcnblk-wal-quorum-leaf-degraded' "$LOG_DIR/us-leaf-$surviving_frontend.log"
fi
if [[ "$FAILURE_SUFFIX" != c ]]; then
	grep -q 'zcnblk-wal-ha-route-failover:.*from=0 to=1' "$LOG_DIR/gateway.log"
	frontend_failure=true
else
	frontend_failure=false
fi

printf 'ZCGLOBAL_REGIONAL_HA_QEMU_MATRIX_PASS machines=9 regions=2 scenario=%s regional_servers=3 regional_replicas=3 regional_quorum=2 regional_frontends=2 frontend_failover=transparent single_storage_vm_failures=one-per-region failed_vm_suffix=%s failed_vm_includes_frontend=%s client_reconnects=0 acknowledged_data_loss=%s global_replication=async clean_cut=%s source_region_destroyed=%s userspace_placement=true block_raid=false source_terminal_grading=%s surviving_target_terminal_leaves_graded=2 failed_leaves_require_rebuild=true qemu_l2_backend=tap-linux-bridge guest_storage_transport=tcp-unicast multicast_product_dependency=false rdma_emulation=false logs=%s\n' \
	"$SCENARIO" "$FAILURE_SUFFIX" "$frontend_failure" "$acknowledged_data_loss" "$([[ "$SCENARIO" == clean ]] && printf true || printf false)" "$([[ "$SCENARIO" == declared-loss ]] && printf true || printf false)" "$source_terminal_grading" "$LOG_DIR"
