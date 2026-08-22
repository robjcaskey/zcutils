#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
KDIR="${KDIR:-/lib/modules/$KERNEL_RELEASE/build}"
WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zccusan-crds}"
ROOTFS="$WORK_DIR/rootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
LOG_DIR="$WORK_DIR/logs"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/qemu-zccusan-crd-cargo}"
BIN_DIR="$CARGO_TARGET_DIR/release"
K3S_VERSION="${K3S_VERSION:-v1.36.1+k3s1}"
K3S_BIN="${K3S_BIN:-$ROOT/target/qemu-zcglobal-volume-failover/k3s-$K3S_VERSION}"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-360}"
VM_MEMORY="${VM_MEMORY:-2048M}"
network_tag="$(printf '%04x' $(( $$ % 65536 )))"
bridge="zcr${network_tag}b"

need()
{
	command -v "$1" >/dev/null || {
		printf 'missing required command: %s\n' "$1" >&2
		exit 1
	}
}

if [[ "${ZCCUSAN_CRD_QEMU_COORDINATED:-0}" != 1 && -x "$COORD_BIN" ]]; then
	exec "$COORD_BIN" run \
		--owner codex:zcutils-crd-qemu \
		--mode soft-exclusive --sensitivity high --priority 65 --ttl 1200 \
		--resource 'cpu=*;memory-bandwidth=*;kvm=*' \
		--note 'three-VM zccusan tier and encrypted cross-region CRD proof' \
		-- env ZCCUSAN_CRD_QEMU_COORDINATED=1 "$0" "$@"
fi

for command in cargo cpio ip ldd make podman qemu-system-x86_64 sed sudo timeout truncate xz; do
	need "$command"
done
[[ -r "$KERNEL" ]] || { printf 'kernel not readable: %s\n' "$KERNEL" >&2; exit 1; }
[[ -d "$KDIR" ]] || { printf 'kernel build directory missing: %s\n' "$KDIR" >&2; exit 1; }
[[ -x "$K3S_BIN" ]] || { printf 'verified k3s binary missing: %s\n' "$K3S_BIN" >&2; exit 1; }
[[ -x /usr/sbin/mkfs.ext4 ]] || { printf 'mkfs.ext4 missing\n' >&2; exit 1; }

mkdir -p "$WORK_DIR" "$LOG_DIR"
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release \
	--bin zccusan-operator --bin zcnblk-wal-leaf --bin zcnblk-wal-failover \
	--bin zcnblk-shm-target --bin zcrepl
make -C "$ROOT/kmods" KDIR="$KDIR"

image_context="$WORK_DIR/image-context"
rm -rf -- "$image_context"
mkdir -p "$image_context/root/usr/local/bin" "$image_context/root/bin" \
	"$image_context/root/lib" "$image_context/root/lib64" \
	"$image_context/root/etc"
for binary in zccusan-operator zcnblk-wal-leaf zcnblk-wal-failover zcnblk-shm-target zcrepl; do
	cp "$BIN_DIR/$binary" "$image_context/root/usr/local/bin/$binary"
done
cp /usr/bin/busybox "$image_context/root/bin/busybox"
for applet in dd grep sh sleep test wc; do
	ln -s busybox "$image_context/root/bin/$applet"
done
printf 'root:x:0:0:root:/root:/bin/sh\nnobody:x:65532:65532:nobody:/:/bin/false\n' >"$image_context/root/etc/passwd"
printf 'root:x:0:\nnobody:x:65532:\n' >"$image_context/root/etc/group"
while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$image_context/root$(dirname "$library")"
	cp "$library" "$image_context/root$library"
done < <(
	{
		for binary in zccusan-operator zcnblk-wal-leaf zcnblk-wal-failover zcnblk-shm-target zcrepl; do
			ldd "$BIN_DIR/$binary"
		done
		ldd /usr/bin/busybox
	} | awk '/=> \// { print $3; next } /^[[:space:]]*\/lib/ { print $1; next }' | sort -u
)
podman build --network none --pull=never -q \
	-t localhost/zccusan-crd-qemu:latest \
	-f "$ROOT/scripts/zccusan-crd-qemu.Containerfile" "$image_context" >/dev/null
podman image exists registry.k8s.io/pause:3.10 || podman pull registry.k8s.io/pause:3.10 >/dev/null
podman save --format oci-archive -o "$WORK_DIR/zccusan-crd-qemu.tar" localhost/zccusan-crd-qemu:latest
podman save --format oci-archive -o "$WORK_DIR/pause.tar" registry.k8s.io/pause:3.10

rm -rf -- "$ROOTFS"
mkdir -p "$ROOTFS/bin" "$ROOTFS/lib" "$ROOTFS/lib64" "$ROOTFS/modules" \
	"$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/tmp" "$ROOTFS/run" \
	"$ROOTFS/etc" "$ROOTFS/var/lib/rancher/k3s/agent/images"
cp /usr/bin/busybox "$ROOTFS/bin/busybox"
for applet in cat dd dmesg echo grep hostname insmod ip kill ln mkdir mount nc poweroff rmmod sh sleep switch_root tail test wc; do
	ln -s busybox "$ROOTFS/bin/$applet"
done
cp "$ROOT/scripts/zccusan-crd-qemu-init.sh" "$ROOTFS/init"
chmod +x "$ROOTFS/init"
cp "$K3S_BIN" "$ROOTFS/k3s"
cp "$WORK_DIR/zccusan-crd-qemu.tar" "$ROOTFS/var/lib/rancher/k3s/agent/images/"
cp "$WORK_DIR/pause.tar" "$ROOTFS/var/lib/rancher/k3s/agent/images/"
cp "$ROOT/zccusan/charts/zcblock-csi/crds/storage.zcutils.io.yaml" "$ROOTFS/storage-crds.yaml"
cp "$ROOT/scripts/zccusan-crd-qemu-operator.yaml" "$ROOTFS/operator.yaml"
ephemeral_token="$($BIN_DIR/zcrepl token)"
sed "s|ZCCUSAN_QEMU_EPHEMERAL_TOKEN|$ephemeral_token|" \
	"$ROOT/scripts/zccusan-crd-qemu-test-intents.yaml" >"$ROOTFS/test-intents.yaml"
cp "$ROOT/scripts/zccusan-crd-qemu-tier-writer.yaml" "$ROOTFS/tier-writer.yaml"
cp "$ROOT/scripts/zccusan-crd-qemu-tier-verify.yaml" "$ROOTFS/tier-verify.yaml"
cp "$ROOT/scripts/zccusan-crd-qemu-cross-verify.yaml" "$ROOTFS/cross-verify.yaml"
cp "$ROOT/scripts/zccusan-crd-qemu-cross-fail-closed.yaml" "$ROOTFS/cross-fail-closed.yaml"
printf 'nameserver 10.46.0.1\n' >"$ROOTFS/etc/resolv.conf"
printf '127.0.0.1 localhost\n10.46.0.1 controller\n10.46.0.2 region-us\n10.46.0.3 region-uk\n' >"$ROOTFS/etc/hosts"
printf 'root:x:0:0:root:/root:/bin/sh\n' >"$ROOTFS/etc/passwd"
printf 'root:x:0:\n' >"$ROOTFS/etc/group"

copy_xz_module()
{
	local source="$1"
	local output="$2"
	xz -dc -- "$source" >"$ROOTFS/modules/$output.ko"
}
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/net/core/failover.ko.xz" failover
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/drivers/net/net_failover.ko.xz" net_failover
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/drivers/net/virtio_net.ko.xz" virtio_net
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/drivers/block/virtio_blk.ko.xz" virtio_blk
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/crypto/aead.ko.xz" aead
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/lib/crc/crc16.ko.xz" crc16
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/fs/mbcache.ko.xz" mbcache
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/fs/jbd2/jbd2.ko.xz" jbd2
copy_xz_module "/lib/modules/$KERNEL_RELEASE/kernel/fs/ext4/ext4.ko.xz" ext4
cp "$ROOT/kmods/zcnblk_client_mod.ko" "$ROOTFS/modules/zcnblk_client_mod.ko"

while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$ROOTFS$(dirname "$library")"
	cp "$library" "$ROOTFS$library"
done < <(ldd /usr/bin/busybox | awk '/=> \// { print $3; next } /^[[:space:]]*\/lib/ { print $1; next }' | sort -u)

(
	cd "$ROOTFS"
	find . -print0 | cpio --null -o --format=newc >"$INITRAMFS"
)
touch "$ROOTFS/.zccrd-system-root"
for role in controller region-us region-uk; do
	image="$WORK_DIR/$role-system.ext4"
	truncate -s 0 "$image"
	truncate -s 1200M "$image"
	/usr/sbin/mkfs.ext4 -F -q -L "zccrd-$role" -d "$ROOTFS" "$image"
done

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

! ip link show dev "$bridge" >/dev/null 2>&1 || { printf 'bridge already exists: %s\n' "$bridge" >&2; exit 1; }
sudo -n ip link add "$bridge" type bridge
sudo -n ip link set "$bridge" type bridge stp_state 0
sudo -n ip link set "$bridge" up

launch_vm()
{
	local role="$1"
	local index="$2"
	local tap="zcr${network_tag}${index}"
	local log="$LOG_DIR/$role.log"
	local mac="52:54:46:00:00:0${index}"
	: >"$log"
	sudo -n ip tuntap add dev "$tap" mode tap user "$(id -un)"
	sudo -n ip link set "$tap" master "$bridge"
	sudo -n ip link set "$tap" up
	tap_devices+=("$tap")
	qemu-system-x86_64 \
		-machine accel=kvm -cpu host -m "$VM_MEMORY" -smp 4 -nographic -no-reboot -nodefaults \
		-serial "file:$log" -kernel "$KERNEL" -initrd "$INITRAMFS" \
		-append "console=ttyS0 panic=-1 oops=panic quiet net.ifnames=0 rootfstype=tmpfs zccrd.role=$role" \
		-netdev "tap,id=net0,ifname=$tap,script=no,downscript=no" \
		-device "virtio-net-pci,netdev=net0,mac=$mac" \
		-drive "if=none,id=system,file=$WORK_DIR/$role-system.ext4,format=raw,cache=none,aio=threads" \
		-device virtio-blk-pci,drive=system,serial=zccrd-system >/dev/null 2>>"$log" &
	qemu_pids+=("$!")
}

launch_vm controller 1
launch_vm region-us 2
launch_vm region-uk 3

deadline=$((SECONDS + TIMEOUT_SECONDS))
while :; do
	alive=0
	for pid in "${qemu_pids[@]}"; do kill -0 "$pid" 2>/dev/null && alive=$((alive + 1)); done
	(( alive == 0 )) && break
	if (( SECONDS >= deadline )); then
		printf 'zccusan CRD QEMU test timed out; logs follow\n' >&2
		for log in "$LOG_DIR"/*.log; do printf '\n== %s ==\n' "$log"; tail -300 "$log"; done
		exit 1
	fi
	sleep 0.1
done
for pid in "${qemu_pids[@]}"; do wait "$pid"; done
cleanup
trap - EXIT

for role in controller region-us region-uk; do
	log="$LOG_DIR/$role.log"
	grep -q "ZCCUSAN_CRD_QEMU_PASS role=$role" "$log" || {
		cat "$log"
		printf 'missing pass marker for %s\n' "$role" >&2
		exit 1
	}
	if grep -Eq 'ZCCUSAN_CRD_QEMU_FAIL|BUG:|Oops:|general protection fault|kernel panic' "$log"; then
		cat "$log"
		printf 'failure marker for %s\n' "$role" >&2
		exit 1
	fi
done
grep -q 'ZCCUSAN_CRD_TIER_PASS' "$LOG_DIR/controller.log" || { cat "$LOG_DIR/controller.log"; exit 1; }
grep -q 'ZCCUSAN_CRD_CROSS_REGION_PASS' "$LOG_DIR/controller.log" || { cat "$LOG_DIR/controller.log"; exit 1; }
printf 'ZCCUSAN_CRD_QEMU_MATRIX_PASS machines=3 tiering_crd=true tier_runtime=true cross_region_crd=true encrypted_checkpoint=true remote_durable_sync=true automatic_failover_fail_closed=true multicast=false logs=%s\n' "$LOG_DIR"
