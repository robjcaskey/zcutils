#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
K3S_VERSION="${K3S_VERSION:-v1.36.1+k3s1}"
K3S_BIN="${K3S_BIN:-$ROOT/target/qemu-zcglobal-volume-failover/k3s-$K3S_VERSION}"
WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zccusan-local-regions}"
ROOTFS="$WORK_DIR/rootfs"
BOOTFS="$WORK_DIR/bootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
SYSTEM_IMAGE="$WORK_DIR/system.ext4"
LOG_DIR="$WORK_DIR/logs"
IMAGE_DIR="$WORK_DIR/images"
PODMAN_ROOT="${PODMAN_ROOT:-$WORK_DIR/podman-root}"
PODMAN_RUNROOT="${PODMAN_RUNROOT:-/tmp/zccusan-local-regions-podman-run}"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-1200}"
VM_MEMORY="${VM_MEMORY:-8192M}"
VM_CPUS="${VM_CPUS:-8}"
REUSE_IMAGES="${REUSE_IMAGES:-1}"

need()
{
	command -v "$1" >/dev/null || {
		printf 'missing required command: %s\n' "$1" >&2
		exit 1
	}
}

if [[ "${ZCCUSAN_LOCAL_REGIONS_QEMU_COORDINATED:-0}" != 1 && -x "$COORD_BIN" ]]; then
	exec "$COORD_BIN" run \
		--owner codex:zcutils-local-regions-qemu \
		--mode soft-exclusive --sensitivity high --priority 65 --ttl 1800 \
		--resource 'cpu=*;memory-bandwidth=*;kvm=*' \
		--note 'single-QEMU-cluster three-version CSI replication and failover proof' \
		-- env ZCCUSAN_LOCAL_REGIONS_QEMU_COORDINATED=1 "$0" "$@"
fi

for command in cpio curl find jq ldd podman qemu-system-x86_64 tar timeout truncate xz; do
	need "$command"
done
[[ -r "$KERNEL" ]] || { printf 'kernel not readable: %s\n' "$KERNEL" >&2; exit 1; }
[[ -x "$K3S_BIN" ]] || { printf 'verified k3s binary missing: %s\n' "$K3S_BIN" >&2; exit 1; }
[[ -x /usr/sbin/mkfs.ext4 ]] || { printf 'mkfs.ext4 is required\n' >&2; exit 1; }
[[ -r "/lib/modules/$KERNEL_RELEASE/modules.dep" ]] || {
	printf 'kernel module tree is incomplete for %s\n' "$KERNEL_RELEASE" >&2
	exit 1
}

mkdir -p "$WORK_DIR" "$LOG_DIR" "$IMAGE_DIR" "$PODMAN_ROOT" "$PODMAN_RUNROOT"
podman_cmd=(podman --root "$PODMAN_ROOT" --runroot "$PODMAN_RUNROOT")
images=(
	docker.io/robjcaskey/zcblock-csi:0.1.4
	docker.io/robjcaskey/zcblock-csi:0.1.5
	docker.io/robjcaskey/zcblock-csi:0.1.6
	registry.k8s.io/sig-storage/csi-provisioner:v5.3.0
	registry.k8s.io/sig-storage/csi-snapshotter:v8.3.0
	registry.k8s.io/sig-storage/csi-node-driver-registrar:v2.16.0
	registry.k8s.io/pause:3.10
	docker.io/library/debian:bookworm-slim
)

for image in "${images[@]}"; do
	archive_name="${image//[\/:]/-}.tar"
	archive="$IMAGE_DIR/$archive_name"
	if [[ "$REUSE_IMAGES" != 1 || ! -s "$archive" ]]; then
		"${podman_cmd[@]}" pull "$image"
		"${podman_cmd[@]}" save --format oci-archive -o "$archive" "$image"
	fi
done

# Tags alone are not evidence of a version-skew test. Prove that the three
# pinned regional images resolve to three distinct OCI manifests before booting
# the guest. This check reads the saved archives and does not depend on Podman's
# mutable local database after the pull step.
declare -A seen_release_manifests=()
release_manifest_proofs=()
for image in \
	docker.io/robjcaskey/zcblock-csi:0.1.4 \
	docker.io/robjcaskey/zcblock-csi:0.1.5 \
	docker.io/robjcaskey/zcblock-csi:0.1.6; do
	archive_name="${image//[\/:]/-}.tar"
	archive="$IMAGE_DIR/$archive_name"
	manifest_digest="$(tar -xOf "$archive" index.json | jq -er '.manifests[0].digest')"
	[[ "$manifest_digest" =~ ^sha256:[0-9a-f]{64}$ ]] || {
		printf 'invalid OCI manifest digest for %s: %s\n' "$image" "$manifest_digest" >&2
		exit 1
	}
	if [[ -n "${seen_release_manifests[$manifest_digest]:-}" ]]; then
		printf 'version-skew images %s and %s resolve to the same OCI manifest %s\n' \
			"${seen_release_manifests[$manifest_digest]}" "$image" "$manifest_digest" >&2
		exit 1
	fi
	seen_release_manifests[$manifest_digest]="$image"
	release_manifest_proofs+=("$image@$manifest_digest")
done
printf 'ZCCUSAN_LOCAL_REGIONS_IMAGE_SKEW_PASS manifests=%s\n' \
	"$(IFS=,; printf '%s' "${release_manifest_proofs[*]}")"

snapshot_crds="$WORK_DIR/snapshot-crds-v8.3.0.yaml"
if [[ ! -s "$snapshot_crds" ]]; then
	: >"$snapshot_crds"
	for crd in volumesnapshotclasses volumesnapshotcontents volumesnapshots; do
		curl --fail --silent --show-error --location \
			"https://raw.githubusercontent.com/kubernetes-csi/external-snapshotter/v8.3.0/client/config/crd/snapshot.storage.k8s.io_${crd}.yaml" \
			>>"$snapshot_crds"
		printf '\n---\n' >>"$snapshot_crds"
	done
fi

rm -rf -- "$ROOTFS" "$BOOTFS"
mkdir -p "$ROOTFS/bin" "$ROOTFS/sbin" "$ROOTFS/lib/modules" "$ROOTFS/lib64" \
	"$ROOTFS/modules" "$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" \
	"$ROOTFS/tmp" "$ROOTFS/run" "$ROOTFS/etc" "$ROOTFS/usr/bin" \
	"$ROOTFS/usr/local/bin" "$ROOTFS/var/lib/rancher/k3s/agent/images"
cp /usr/bin/busybox "$ROOTFS/bin/busybox"
for applet in awk cat chmod cp cut date dd dmesg echo find grep hostname insmod install ip kill ln ls \
	mkdir modprobe mount nc poweroff readlink rm sed seq sh sleep sort switch_root tail test \
	touch tr umount uname wc; do
	ln -s busybox "$ROOTFS/bin/$applet"
done
ln -s ../bin/busybox "$ROOTFS/sbin/modprobe"
cp "$K3S_BIN" "$ROOTFS/k3s"
ln -s /k3s "$ROOTFS/usr/local/bin/kubectl"
cp /usr/bin/jq "$ROOTFS/usr/bin/jq"
cp -a "/lib/modules/$KERNEL_RELEASE" "$ROOTFS/lib/modules/"
cp "$ROOT/scripts/zccusan-local-regions-qemu-init.sh" "$ROOTFS/init"
cp "$ROOT/zccusan/deploy/zcblock-csi/test-local-regions-failover.sh" \
	"$ROOTFS/test-local-regions-failover.sh"
chmod +x "$ROOTFS/init" "$ROOTFS/test-local-regions-failover.sh"
cp "$snapshot_crds" "$ROOTFS/snapshot-crds.yaml"
for archive in "$IMAGE_DIR"/*.tar; do
	cp "$archive" "$ROOTFS/var/lib/rancher/k3s/agent/images/"
done

IMAGE=docker.io/robjcaskey/zcblock-csi:0.1.4 \
	"$ROOT/zccusan/deploy/zcblock-csi/render-region-install.sh" a >"$ROOTFS/region-a.yaml"
IMAGE=docker.io/robjcaskey/zcblock-csi:0.1.5 \
	"$ROOT/zccusan/deploy/zcblock-csi/render-region-install.sh" b >"$ROOTFS/region-b.yaml"
IMAGE=docker.io/robjcaskey/zcblock-csi:0.1.6 \
	"$ROOT/zccusan/deploy/zcblock-csi/render-region-install.sh" c >"$ROOTFS/region-c.yaml"

printf 'root:x:0:0:root:/root:/bin/sh\nnobody:x:65532:65532:nobody:/:/bin/false\n' \
	>"$ROOTFS/etc/passwd"
printf 'root:x:0:\nnobody:x:65532:\n' >"$ROOTFS/etc/group"
printf '127.0.0.1 localhost qemu-local-regions\n10.49.0.1 qemu-local-regions\n' \
	>"$ROOTFS/etc/hosts"
printf 'nameserver 10.49.0.1\n' >"$ROOTFS/etc/resolv.conf"
printf 'NAME="zccusan QEMU test appliance"\nID=zccusan-qemu\n' >"$ROOTFS/etc/os-release"

while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$ROOTFS$(dirname "$library")"
	cp -L "$library" "$ROOTFS$library"
done < <(
	{
		ldd /usr/bin/busybox
		ldd /usr/bin/jq
	} | awk '/=> \// { print $3; next } /^[[:space:]]*\/lib/ { print $1; next }' | sort -u
)

mkdir -p "$BOOTFS/bin" "$BOOTFS/sbin" "$BOOTFS/lib/modules" "$BOOTFS/lib64" \
	"$BOOTFS/proc" "$BOOTFS/sys" "$BOOTFS/dev" "$BOOTFS/tmp" "$BOOTFS/run"
cp /usr/bin/busybox "$BOOTFS/bin/busybox"
for applet in cat dmesg echo grep insmod kill ln mkdir modprobe mount poweroff sh sleep \
	switch_root tail test; do
	ln -s busybox "$BOOTFS/bin/$applet"
done
ln -s ../bin/busybox "$BOOTFS/sbin/modprobe"
cp "$ROOT/scripts/zccusan-local-regions-qemu-init.sh" "$BOOTFS/init"
chmod +x "$BOOTFS/init"
cp -a "/lib/modules/$KERNEL_RELEASE" "$BOOTFS/lib/modules/"
while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$BOOTFS$(dirname "$library")"
	cp -L "$library" "$BOOTFS$library"
done < <(ldd /usr/bin/busybox | awk '/=> \// { print $3; next } /^[[:space:]]*\/lib/ { print $1; next }' | sort -u)

(
	cd "$BOOTFS"
	find . -print0 | cpio --null -o --format=newc >"$INITRAMFS"
)
touch "$ROOTFS/.zclocal-regions-system-root"
truncate -s 0 "$SYSTEM_IMAGE"
truncate -s 8G "$SYSTEM_IMAGE"
/usr/sbin/mkfs.ext4 -F -q -L zccusan-local-regions -d "$ROOTFS" "$SYSTEM_IMAGE"

log="$LOG_DIR/qemu-local-regions.log"
: >"$log"
qemu-system-x86_64 \
	-machine accel=kvm -cpu host -m "$VM_MEMORY" -smp "$VM_CPUS" \
	-nographic -no-reboot -nodefaults -serial "file:$log" \
	-kernel "$KERNEL" -initrd "$INITRAMFS" \
	-append 'console=ttyS0 panic=-1 oops=panic quiet net.ifnames=0 rootfstype=tmpfs' \
	-netdev user,id=net0 -device virtio-net-pci,netdev=net0,mac=52:54:49:00:00:01 \
	-drive "if=none,id=system,file=$SYSTEM_IMAGE,format=raw,cache=none,aio=threads" \
	-device virtio-blk-pci,drive=system,serial=zccusan-local-regions \
	>/dev/null 2>>"$log" &
qemu_pid=$!

cleanup()
{
	if kill -0 "$qemu_pid" 2>/dev/null; then
		kill -TERM "$qemu_pid" 2>/dev/null || true
	fi
}
trap cleanup EXIT

deadline=$((SECONDS + TIMEOUT_SECONDS))
while kill -0 "$qemu_pid" 2>/dev/null; do
	if (( SECONDS >= deadline )); then
		printf 'local-regions QEMU test timed out; guest log follows\n' >&2
		tail -600 "$log" >&2
		exit 1
	fi
	sleep 0.2
done
wait "$qemu_pid"
trap - EXIT

grep -q 'ZCCUSAN_LOCAL_REGIONS_QEMU_PASS.*instances=3.*versions=0.1.4,0.1.5,0.1.6.*cross_region_replication=pass.*planned_failover=a-to-b-to-c' "$log" || {
	tail -800 "$log" >&2
	printf 'missing complete local-regions QEMU proof marker\n' >&2
	exit 1
}
if grep -Eq 'ZCCUSAN_LOCAL_REGIONS_QEMU_FAIL|BUG:|Oops:|general protection fault|kernel panic' "$log"; then
	tail -800 "$log" >&2
	printf 'local-regions QEMU log contains a failure marker\n' >&2
	exit 1
fi
grep 'ZCCUSAN_LOCAL_REGIONS_.*PASS' "$log"
