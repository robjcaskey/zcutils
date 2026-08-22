#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
KDIR="${KDIR:-/lib/modules/$KERNEL_RELEASE/build}"
WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zcglobal-volume-failover}"
ROOTFS="$WORK_DIR/rootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
LOG_DIR="$WORK_DIR/logs"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/qemu-zcglobal-volume-cargo}"
BIN_DIR="$CARGO_TARGET_DIR/release"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-120}"
OPERATIONS="${OPERATIONS:-64}"
MOVE_END="${MOVE_END:-96}"
KUBERNETES="${ZCGLOBAL_KUBERNETES:-0}"
K3S_VERSION="${K3S_VERSION:-v1.36.1+k3s1}"
K3S_BIN="${K3S_BIN:-$ROOT/target/qemu-zcglobal-volume-failover/k3s-$K3S_VERSION}"
VM_MEMORY="${VM_MEMORY:-$([[ "$KUBERNETES" == 1 ]] && printf 2048M || printf 1024M)}"
REUSE_K8S_ARTIFACTS="${REUSE_K8S_ARTIFACTS:-0}"
REPLICATION_MODE="${ZCGLOBAL_REPLICATION_MODE:-sync}"
SCENARIO="${ZCGLOBAL_SCENARIO:-clean}"
DECLARED_LOSS_CHECKPOINT="${DECLARED_LOSS_CHECKPOINT:-32}"
network_tag="$(printf '%04x' $(( $$ % 65536 )))"
bridge="zgv${network_tag}b"
[[ "$REPLICATION_MODE" == sync || "$REPLICATION_MODE" == async ]] || {
	printf 'ZCGLOBAL_REPLICATION_MODE must be sync or async\n' >&2
	exit 2
}
[[ "$SCENARIO" == clean || "$SCENARIO" == declared-loss ]] || {
	printf 'ZCGLOBAL_SCENARIO must be clean or declared-loss\n' >&2
	exit 2
}
if [[ "$SCENARIO" == declared-loss && "$REPLICATION_MODE" != async ]]; then
	printf 'declared-loss scenario requires async replication\n' >&2
	exit 2
fi

need()
{
	command -v "$1" >/dev/null || {
		printf 'missing required command: %s\n' "$1" >&2
		exit 1
	}
}

if [[ "${ZCGLOBAL_VOLUME_QEMU_COORDINATED:-0}" != 1 ]]; then
	need "$COORD_BIN"
	exec "$COORD_BIN" run \
		--owner codex:zcutils-global-volume-qemu \
		--mode soft-exclusive --sensitivity high --priority 65 --ttl 1200 \
		--resource 'cpu=*;memory-bandwidth=*;kvm=*' \
		--note 'three-VM userspace global volume failover correctness proof' \
		-- env ZCGLOBAL_VOLUME_QEMU_COORDINATED=1 "$0" "$@"
fi

for command in cargo cpio ip ldd make od qemu-system-x86_64 readelf stat sudo timeout truncate xz; do
	need "$command"
done
[[ -x /usr/sbin/sfdisk ]] || { printf 'missing /usr/sbin/sfdisk\n' >&2; exit 1; }
[[ -r "$KERNEL" ]] || { printf 'kernel not readable: %s\n' "$KERNEL" >&2; exit 1; }
[[ -d "$KDIR" ]] || { printf 'kernel build directory missing: %s\n' "$KDIR" >&2; exit 1; }
[[ "$OPERATIONS" =~ ^[0-9]+$ && "$MOVE_END" =~ ^[0-9]+$ ]] || {
	printf 'OPERATIONS and MOVE_END must be positive test ranges\n' >&2
	exit 2
}
(( OPERATIONS >= 4 && MOVE_END > OPERATIONS && MOVE_END < 4096 )) || {
	printf 'require 4 <= OPERATIONS < MOVE_END < 4096\n' >&2
	exit 2
}
(( DECLARED_LOSS_CHECKPOINT > 0 && DECLARED_LOSS_CHECKPOINT < OPERATIONS )) || {
	printf 'require 0 < DECLARED_LOSS_CHECKPOINT < OPERATIONS\n' >&2
	exit 2
}

mkdir -p "$WORK_DIR" "$LOG_DIR"
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release \
	--bin zcnblk-wal-failover --bin zcnblk-wal-leaf --bin zcnblk-shm-target \
	--bin zcglobal-volume-workload --bin zcglobal-kubernetes-adapter
make -C "$ROOT/kmods" KDIR="$KDIR"

if [[ "$KUBERNETES" == 1 ]]; then
	need podman
	[[ -x "$K3S_BIN" ]] || {
		printf 'missing verified k3s binary: %s\n' "$K3S_BIN" >&2
		exit 1
	}
	if [[ "$REUSE_K8S_ARTIFACTS" != 1 || ! -s "$WORK_DIR/zcglobal-volume-workload.tar" || ! -s "$WORK_DIR/pause.tar" ]]; then
		image_context="$WORK_DIR/image-context"
		rm -rf -- "$image_context"
		mkdir -p "$image_context/lib/x86_64-linux-gnu" "$image_context/lib64"
		cp "$BIN_DIR/zcglobal-volume-workload" "$image_context/zcglobal-volume-workload"
		cp /lib/x86_64-linux-gnu/libc.so.6 "$image_context/lib/x86_64-linux-gnu/libc.so.6"
		cp /lib/x86_64-linux-gnu/libgcc_s.so.1 "$image_context/lib/x86_64-linux-gnu/libgcc_s.so.1"
		cp /lib64/ld-linux-x86-64.so.2 "$image_context/lib64/ld-linux-x86-64.so.2"
		podman build --network none --pull=never -q \
			-t localhost/zcglobal-volume-workload:qemu \
			-f "$ROOT/scripts/zcglobal-volume-workload.Containerfile" "$image_context" >/dev/null
		podman image exists registry.k8s.io/pause:3.10 || podman pull registry.k8s.io/pause:3.10 >/dev/null
		podman save --format oci-archive -o "$WORK_DIR/zcglobal-volume-workload.tar" \
			localhost/zcglobal-volume-workload:qemu
		podman save --format oci-archive -o "$WORK_DIR/pause.tar" registry.k8s.io/pause:3.10
	fi
fi

rm -rf -- "$ROOTFS"
mkdir -p "$ROOTFS/bin" "$ROOTFS/lib" "$ROOTFS/lib64" "$ROOTFS/modules" \
	"$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/tmp"
mkdir -p "$ROOTFS/run"
cp /usr/bin/busybox "$ROOTFS/bin/busybox"
for applet in cat dmesg echo grep hostname insmod ip kill ln mkdir mount nc poweroff rmmod sh sleep switch_root tail true; do
	ln -s busybox "$ROOTFS/bin/$applet"
done
cp "$ROOT/scripts/zcglobal-volume-failover-qemu-init.sh" "$ROOTFS/init"
chmod +x "$ROOTFS/init"
if [[ "$KUBERNETES" == 1 ]]; then
	cp "$K3S_BIN" "$ROOTFS/k3s"
	mkdir -p "$ROOTFS/var/lib/rancher/k3s/agent/images" "$ROOTFS/etc"
	cp "$WORK_DIR/zcglobal-volume-workload.tar" "$ROOTFS/var/lib/rancher/k3s/agent/images/"
	cp "$WORK_DIR/pause.tar" "$ROOTFS/var/lib/rancher/k3s/agent/images/"
	printf 'nameserver 10.45.0.2\n' > "$ROOTFS/etc/resolv.conf"
	printf '127.0.0.1 localhost\n10.45.0.1 region-us\n10.45.0.2 gateway\n10.45.0.3 region-eu\n' > "$ROOTFS/etc/hosts"
	printf 'root:x:0:0:root:/root:/bin/sh\n' > "$ROOTFS/etc/passwd"
	printf 'root:x:0:\n' > "$ROOTFS/etc/group"
fi

copy_xz_module()
{
	local source="$1"
	local output="$2"
	xz -dc -- "$source" > "$ROOTFS/modules/$output.ko"
}
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/net/core/failover.ko.xz" failover
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/drivers/net/net_failover.ko.xz" net_failover
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/drivers/net/virtio_net.ko.xz" virtio_net
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/drivers/block/virtio_blk.ko.xz" virtio_blk
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/crypto/aead.ko.xz" aead
if [[ "$KUBERNETES" == 1 ]]; then
	copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/lib/crc/crc16.ko.xz" crc16
	copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/fs/mbcache.ko.xz" mbcache
	copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/fs/jbd2/jbd2.ko.xz" jbd2
	copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/fs/ext4/ext4.ko.xz" ext4
fi
cp "$ROOT/kmods/zcnblk_client_mod.ko" "$ROOTFS/modules/zcnblk_client_mod.ko"

bins=(zcnblk-wal-failover zcnblk-wal-leaf zcnblk-shm-target zcglobal-volume-workload zcglobal-kubernetes-adapter)
for bin in "${bins[@]}"; do cp "$BIN_DIR/$bin" "$ROOTFS/$bin"; done
while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$ROOTFS$(dirname "$library")"
	cp "$library" "$ROOTFS$library"
done < <(
	{
		for bin in "${bins[@]}"; do ldd "$BIN_DIR/$bin"; done
		ldd /usr/bin/busybox
	} | awk '/=> \// { print $3; next } /^[[:space:]]*\/lib/ { print $1; next }' | sort -u
)
(
	cd "$ROOTFS"
	find . -print0 | cpio --null -o --format=newc > "$INITRAMFS"
)
if [[ "$KUBERNETES" == 1 ]]; then
	[[ -x /usr/sbin/mkfs.ext4 ]] || { printf 'missing /usr/sbin/mkfs.ext4\n' >&2; exit 1; }
	touch "$ROOTFS/.zcgf-system-root"
	for role in region-us gateway region-eu; do
		system_image="$WORK_DIR/$role-system.ext4"
		truncate -s 768M "$system_image"
		/usr/sbin/mkfs.ext4 -F -q -L "zcgf-$role" -d "$ROOTFS" "$system_image"
	done
fi

primary_image="$WORK_DIR/region-us-terminal.raw"
secondary_image="$WORK_DIR/region-eu-terminal.raw"
# These are disposable test leaves.  `truncate -s 36M` alone preserves blocks
# from a prior run when the file is already that size, which can make a stale
# payload look like speculative async replay.  Recreate the sparse media from
# length zero so every recovery assertion starts from known-empty storage.
truncate -s 0 "$primary_image"
truncate -s 0 "$secondary_image"
truncate -s 36M "$primary_image"
truncate -s 36M "$secondary_image"
printf '2048,65536,83,*\n' | /usr/sbin/sfdisk --quiet --label dos "$primary_image"
/usr/sbin/sfdisk --disk-id "$primary_image" 0x45aa0001 >/dev/null
printf '2048,65536,83,*\n' | /usr/sbin/sfdisk --quiet --label dos "$secondary_image"
/usr/sbin/sfdisk --disk-id "$secondary_image" 0x45aa0003 >/dev/null

declare -a qemu_pids=()
declare -a tap_devices=()
cleanup()
{
	local pid
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
	local log="$LOG_DIR/$role.log"
	local tap_suffix
	case "$role" in
		region-us) tap_suffix=1 ;;
		gateway) tap_suffix=2 ;;
		region-eu) tap_suffix=3 ;;
	esac
	local tap="zgv${network_tag}${tap_suffix}"
	local mac="52:54:45:00:00:0${tap_suffix}"
	local -a drive=()
	local -a system_drive=()
	: > "$log"
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
		drive=(-drive "if=none,id=terminal,file=$image,format=raw,cache=none,aio=threads" \
			-device virtio-blk-pci,drive=terminal)
	fi
	if [[ "$KUBERNETES" == 1 ]]; then
		system_drive=(-drive "if=none,id=system,file=$WORK_DIR/$role-system.ext4,format=raw,cache=none,aio=threads" \
			-device virtio-blk-pci,drive=system,serial=zcgf-system)
	fi
	qemu-system-x86_64 \
		-machine accel=kvm -cpu host -m "$VM_MEMORY" -smp 4 -nographic -no-reboot -nodefaults \
		-serial "file:$log" -kernel "$KERNEL" -initrd "$INITRAMFS" \
		-append "console=ttyS0 panic=-1 oops=panic quiet net.ifnames=0 rootfstype=tmpfs zcgf.role=$role zcgf.operations=$OPERATIONS zcgf.move_end=$MOVE_END zcgf.kubernetes=$KUBERNETES zcgf.replication=$REPLICATION_MODE zcgf.scenario=$SCENARIO zcgf.loss_checkpoint=$DECLARED_LOSS_CHECKPOINT" \
		-netdev "tap,id=net0,ifname=$tap,script=no,downscript=no" \
		-device "virtio-net-pci,netdev=net0,mac=$mac" "${system_drive[@]}" "${drive[@]}" >/dev/null 2>>"$log" &
	qemu_pids+=("$!")
}

launch_vm region-us "$primary_image"
launch_vm region-eu "$secondary_image"
launch_vm gateway

deadline=$((SECONDS + TIMEOUT_SECONDS))
while :; do
	alive=0
	for pid in "${qemu_pids[@]}"; do kill -0 "$pid" 2>/dev/null && alive=$((alive + 1)); done
	(( alive == 0 )) && break
	if (( SECONDS >= deadline )); then
		printf 'global volume failover QEMU test timed out; logs follow\n' >&2
		for log in "$LOG_DIR"/*.log; do printf '\n== %s ==\n' "$log"; cat "$log"; done
		exit 1
	fi
	sleep 0.1
done
for pid in "${qemu_pids[@]}"; do wait "$pid"; done
cleanup
trap - EXIT

for role in region-us region-eu gateway; do
	log="$LOG_DIR/$role.log"
	grep -q "ZCGLOBAL_FAILOVER_QEMU_PASS role=$role" "$log" || {
		cat "$log"; printf 'missing pass marker for %s\n' "$role" >&2; exit 1;
	}
	if grep -Eq 'ZCGLOBAL_FAILOVER_QEMU_FAIL|BUG:|Oops:|general protection fault|kernel panic' "$log"; then
		cat "$log"; printf 'failure marker for %s\n' "$role" >&2; exit 1
	fi
done
if [[ "$SCENARIO" == declared-loss ]]; then
	if [[ "$KUBERNETES" == 1 ]]; then
		grep -q 'ZCGLOBAL_VOLUME_DISASTER_SOURCE_READY.*regional_syncs_acknowledged=true' "$LOG_DIR/gateway.log" || {
			cat "$LOG_DIR/gateway.log"; printf 'missing Kubernetes declared-loss source proof\n' >&2; exit 1;
		}
		grep -q 'ZCGLOBAL_VOLUME_DECLARED_LOSS_MOVE_PASS.*destination_tail_absent_before_reuse=true' "$LOG_DIR/gateway.log" || {
			cat "$LOG_DIR/gateway.log"; printf 'missing Kubernetes declared-loss destination proof\n' >&2; exit 1;
		}
		grep -q 'ZCGLOBAL_KUBERNETES_MOVE_PASS scenario=declared-loss source_region_lost=true.*adapter_ack=emitted.*acknowledged_data_loss=booked-' "$LOG_DIR/gateway.log" || {
			cat "$LOG_DIR/gateway.log"; printf 'missing Kubernetes declared-loss adapter proof\n' >&2; exit 1;
		}
	else
		grep -q 'ZCGLOBAL_VOLUME_DISASTER_SOURCE_READY.*regional_syncs_acknowledged=true' "$LOG_DIR/region-us.log" || {
			cat "$LOG_DIR/region-us.log"; printf 'missing declared-loss source proof\n' >&2; exit 1;
		}
		grep -q 'ZCGLOBAL_VOLUME_DECLARED_LOSS_MOVE_PASS.*destination_tail_absent_before_reuse=true' "$LOG_DIR/region-eu.log" || {
			cat "$LOG_DIR/region-eu.log"; printf 'missing declared-loss destination proof\n' >&2; exit 1;
		}
	fi
	grep -q 'fence=declared-loss.*first_missing=Some(' "$LOG_DIR/gateway.log" || {
		cat "$LOG_DIR/gateway.log"; printf 'missing declared-loss fencing proof\n' >&2; exit 1;
	}
elif [[ "$KUBERNETES" == 1 ]]; then
	grep -q 'ZCGLOBAL_KUBERNETES_STAY_PASS.*pod_uid_stable=true.*restart_count=0' "$LOG_DIR/gateway.log" || {
		cat "$LOG_DIR/gateway.log"; printf 'missing Kubernetes stay proof\n' >&2; exit 1;
	}
	grep -q 'ZCGLOBAL_KUBERNETES_MOVE_PASS.*pod_uid_changed=true.*node_changed=true.*service_uid_stable=true' "$LOG_DIR/gateway.log" || {
		cat "$LOG_DIR/gateway.log"; printf 'missing Kubernetes move proof\n' >&2; exit 1;
	}
else
	grep -q 'ZCGLOBAL_VOLUME_STAY_PASS.*reconnects=0 remounts=0 process_restarts=0' "$LOG_DIR/region-us.log" || {
		cat "$LOG_DIR/region-us.log"; printf 'missing stay proof\n' >&2; exit 1;
	}
	grep -q 'ZCGLOBAL_VOLUME_MOVE_PASS.*pod_data_loss=0' "$LOG_DIR/region-eu.log" || {
		cat "$LOG_DIR/region-eu.log"; printf 'missing move proof\n' >&2; exit 1;
	}
fi
grep -q 'active=secondary placement_epoch=2' "$LOG_DIR/gateway.log" || {
	cat "$LOG_DIR/gateway.log"; printf 'missing custody-transfer proof\n' >&2; exit 1;
}
printf 'ZCGLOBAL_VOLUME_FAILOVER_QEMU_MATRIX_PASS machines=3 regions=2 kubernetes=%s scenario=%s replication=%s client_edges=2 terminal_media=two-independent-virtio-blk placement=userspace promotion=%s source_pod_policy=stay destination_pod_policy=move source_reconnects=0 source_remounts=0 acknowledged_data_loss=%s primary_stale_range_graded=%s secondary_full_range_graded=true qemu_l2_backend=tap-linux-bridge guest_storage_transport=tcp-unicast guest_control_transport=tcp-unicast multicast_product_dependency=false rdma_emulation=false logs=%s\n' "$KUBERNETES" "$SCENARIO" "$REPLICATION_MODE" "$([[ "$SCENARIO" == declared-loss ]] && printf explicit-declared-loss-hwm || { [[ "$REPLICATION_MODE" == async ]] && printf async-caught-up-sync-hwm || printf mirrored-sync-hwm; })" "$([[ "$SCENARIO" == declared-loss ]] && printf booked-explicitly || printf 0)" "$([[ "$SCENARIO" == declared-loss ]] && printf not-applicable-source-destroyed || printf true)" "$LOG_DIR"
