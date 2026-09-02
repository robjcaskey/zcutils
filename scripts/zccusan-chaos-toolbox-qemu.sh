#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
K3S_VERSION="${K3S_VERSION:-v1.36.1+k3s1}"
K3S_BIN="${K3S_BIN:-$ROOT/target/qemu-zcglobal-volume-failover/k3s-$K3S_VERSION}"
WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zccusan-chaos-toolbox}"
ROOTFS="$WORK_DIR/rootfs"
BOOTFS="$WORK_DIR/bootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
SYSTEM_IMAGE="$WORK_DIR/system.ext4"
LOG_DIR="$WORK_DIR/logs"
IMAGE_DIR="$WORK_DIR/images"
CHART_REF="${CHAOS_CHART_REF:-$ROOT/zccusan/charts/zccusan-chaos-toolbox}"
CHART_VERSION="${CHAOS_CHART_VERSION:-}"
IMAGE_REF="${CHAOS_IMAGE_REF:-localhost/zccusan-chaos-toolbox:dev}"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-600}"
VM_MEMORY="${VM_MEMORY:-3072M}"
VM_CPUS="${VM_CPUS:-4}"

need()
{
	command -v "$1" >/dev/null || {
		printf 'missing required command: %s\n' "$1" >&2
		exit 1
	}
}

if [[ "${ZCCUSAN_CHAOS_QEMU_COORDINATED:-0}" != 1 && -x "$COORD_BIN" ]]; then
	exec "$COORD_BIN" run \
		--owner codex:zcutils-chaos-qemu \
		--mode soft-exclusive --sensitivity high --priority 65 --ttl 1200 \
		--resource 'cpu=*;memory-bandwidth=*;kvm=*' \
		--note 'QEMU k3s proof of packaged chaos chart fault primitives' \
		-- env ZCCUSAN_CHAOS_QEMU_COORDINATED=1 "$0" "$@"
fi

for command in cpio find helm ldd podman qemu-system-x86_64 sha256sum tar timeout truncate xz; do
	need "$command"
done
[[ -r "$KERNEL" ]] || { printf 'kernel not readable: %s\n' "$KERNEL" >&2; exit 1; }
[[ -x "$K3S_BIN" ]] || { printf 'verified k3s binary missing: %s\n' "$K3S_BIN" >&2; exit 1; }
[[ -x /bin/kmod ]] || { printf 'the standard kmod loader is required at /bin/kmod\n' >&2; exit 1; }
[[ -x /usr/sbin/mkfs.ext4 ]] || { printf 'mkfs.ext4 is required\n' >&2; exit 1; }
[[ -r "/lib/modules/$KERNEL_RELEASE/modules.dep" ]] || {
	printf 'kernel module tree is incomplete for %s\n' "$KERNEL_RELEASE" >&2
	exit 1
}

mkdir -p "$WORK_DIR" "$LOG_DIR" "$IMAGE_DIR"

chart_input="$CHART_REF"
chart_proof="source=local-directory"
if [[ "$CHART_REF" == oci://* ]]; then
	[[ -n "$CHART_VERSION" ]] || { printf 'CHAOS_CHART_VERSION is required for an OCI chart\n' >&2; exit 1; }
	download_dir="$WORK_DIR/published-chart"
	rm -rf -- "$download_dir"
	mkdir -p "$download_dir"
	helm pull "$CHART_REF" --version "$CHART_VERSION" --destination "$download_dir"
	chart_input="$(find "$download_dir" -maxdepth 1 -name '*.tgz' -print -quit)"
	[[ -s "$chart_input" ]] || { printf 'published chart was not downloaded\n' >&2; exit 1; }
	chart_proof="source=oci ref=$CHART_REF version=$CHART_VERSION sha256=$(sha256sum "$chart_input" | awk '{print $1}')"
fi

render_args=(template zccusan-chaos "$chart_input" --namespace zccusan-chaos
	--set fullnameOverride=zccusan-chaos
	--set faults.nodePoweroff.enabled=true
	--set faults.nodePoweroff.acknowledgeRisk=true)
if [[ "$CHART_REF" != oci://* ]]; then
	image_repository="${IMAGE_REF%:*}"
	image_tag="${IMAGE_REF##*:}"
	render_args+=(--set "image.repository=$image_repository" --set "image.tag=$image_tag")
fi
helm "${render_args[@]}" >"$WORK_DIR/chaos-chart.yaml"
rendered_image="$(awk '$1 == "image:" {gsub(/\"/, "", $2); print $2; exit}' "$WORK_DIR/chaos-chart.yaml")"
[[ -n "$rendered_image" ]] || { printf 'could not resolve image from rendered chart\n' >&2; exit 1; }
if [[ "$CHART_REF" == oci://* ]]; then
	IMAGE_REF="$rendered_image"
fi

images=(
	"$IMAGE_REF"
	registry.k8s.io/pause:3.10
	docker.io/library/alpine:3.22
)
for image in "${images[@]}"; do
	archive="$IMAGE_DIR/${image//[\/:@]/-}.tar"
	podman image exists "$image" || podman pull "$image"
	podman save --format oci-archive -o "$archive" "$image"
done

rm -rf -- "$ROOTFS" "$BOOTFS"
mkdir -p "$ROOTFS/bin" "$ROOTFS/sbin" "$ROOTFS/lib/modules" "$ROOTFS/lib64" \
	"$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/tmp" "$ROOTFS/run" \
	"$ROOTFS/etc" "$ROOTFS/usr/bin" "$ROOTFS/usr/local/bin" \
	"$ROOTFS/var/lib/rancher/k3s/agent/images" \
	"$BOOTFS/bin" "$BOOTFS/sbin" "$BOOTFS/lib/modules" "$BOOTFS/lib64" \
	"$BOOTFS/proc" "$BOOTFS/sys" "$BOOTFS/dev" "$BOOTFS/tmp" "$BOOTFS/run"
cp /usr/bin/busybox "$ROOTFS/bin/busybox"
for applet in awk cat chmod cp cut date dd dmesg echo find grep head hostname insmod ip kill ln ls \
	mkdir modprobe mount mv nc poweroff readlink rm sed seq sh sleep sort switch_root tail test \
	touch tr umount uname wc; do
	ln -s busybox "$ROOTFS/bin/$applet"
done
cp /bin/kmod "$ROOTFS/bin/kmod"
rm "$ROOTFS/bin/modprobe"
ln -s kmod "$ROOTFS/bin/modprobe"
ln -s ../bin/kmod "$ROOTFS/sbin/modprobe"
cp "$K3S_BIN" "$ROOTFS/k3s"
cp -a "/lib/modules/$KERNEL_RELEASE" "$ROOTFS/lib/modules/"
cp "$ROOT/scripts/zccusan-chaos-toolbox-qemu-init.sh" "$ROOTFS/init"
cp "$ROOT/scripts/zccusan-chaos-toolbox-qemu-victims.yaml" "$ROOTFS/chaos-victims.yaml"
cp "$WORK_DIR/chaos-chart.yaml" "$ROOTFS/chaos-chart.yaml"
printf '%s image=%s\n' "$chart_proof" "$IMAGE_REF" >"$ROOTFS/chart-proof.txt"
chmod +x "$ROOTFS/init"
for archive in "$IMAGE_DIR"/*.tar; do cp "$archive" "$ROOTFS/var/lib/rancher/k3s/agent/images/"; done
printf 'root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/:/bin/false\n' >"$ROOTFS/etc/passwd"
printf 'root:x:0:\nnobody:x:65534:\n' >"$ROOTFS/etc/group"
printf '127.0.0.1 localhost qemu-chaos\n10.52.0.1 qemu-chaos\n' >"$ROOTFS/etc/hosts"
printf 'nameserver 10.52.0.1\n' >"$ROOTFS/etc/resolv.conf"
printf 'NAME="zccusan chaos QEMU test appliance"\nID=zccusan-chaos-qemu\n' >"$ROOTFS/etc/os-release"

while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$ROOTFS$(dirname "$library")"
	cp -L "$library" "$ROOTFS$library"
done < <(
	{ ldd /usr/bin/busybox 2>/dev/null || true; ldd /bin/kmod; } \
		| awk '/=> \// { print $3; next } /^[[:space:]]*\/lib/ { print $1; next }' | sort -u
)

cp /usr/bin/busybox "$BOOTFS/bin/busybox"
for applet in cat dmesg echo grep insmod kill ln mkdir modprobe mount poweroff sh sleep switch_root tail test; do
	ln -s busybox "$BOOTFS/bin/$applet"
done
cp /bin/kmod "$BOOTFS/bin/kmod"
rm "$BOOTFS/bin/modprobe"
ln -s kmod "$BOOTFS/bin/modprobe"
ln -s ../bin/kmod "$BOOTFS/sbin/modprobe"
cp "$ROOT/scripts/zccusan-chaos-toolbox-qemu-init.sh" "$BOOTFS/init"
chmod +x "$BOOTFS/init"
cp -a "/lib/modules/$KERNEL_RELEASE" "$BOOTFS/lib/modules/"
while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$BOOTFS$(dirname "$library")"
	cp -L "$library" "$BOOTFS$library"
done < <(
	{ ldd /usr/bin/busybox 2>/dev/null || true; ldd /bin/kmod; } \
		| awk '/=> \// { print $3; next } /^[[:space:]]*\/lib/ { print $1; next }' | sort -u
)

(
	cd "$BOOTFS"
	find . -print0 | cpio --null -o --format=newc >"$INITRAMFS"
)
touch "$ROOTFS/.zccusan-chaos-system-root"
truncate -s 0 "$SYSTEM_IMAGE"
truncate -s 3G "$SYSTEM_IMAGE"
/usr/sbin/mkfs.ext4 -F -q -L zccusan-chaos -d "$ROOTFS" "$SYSTEM_IMAGE"

log="$LOG_DIR/qemu-chaos.log"
: >"$log"
accel="${QEMU_ACCEL:-kvm}"
cpu="host"
[[ "$accel" = kvm ]] || cpu=max
qemu-system-x86_64 \
	-machine "accel=$accel" -cpu "$cpu" -m "$VM_MEMORY" -smp "$VM_CPUS" \
	-nographic -no-reboot -nodefaults -serial "file:$log" \
	-kernel "$KERNEL" -initrd "$INITRAMFS" \
	-append 'console=ttyS0 panic=-1 oops=panic quiet net.ifnames=0 rootfstype=tmpfs' \
	-netdev user,id=net0 -device virtio-net-pci,netdev=net0,mac=52:54:52:00:00:01 \
	-drive "if=none,id=system,file=$SYSTEM_IMAGE,format=raw,cache=none,aio=threads" \
	-device virtio-blk-pci,drive=system,serial=zccusan-chaos \
	>/dev/null 2>>"$log" &
qemu_pid=$!

cleanup()
{
	if kill -0 "$qemu_pid" 2>/dev/null; then kill -TERM "$qemu_pid" 2>/dev/null || true; fi
}
trap cleanup EXIT

deadline=$((SECONDS + TIMEOUT_SECONDS))
while kill -0 "$qemu_pid" 2>/dev/null; do
	if (( SECONDS >= deadline )); then
		printf 'chaos-toolbox QEMU test timed out; guest log follows\n' >&2
		tail -500 "$log" >&2
		exit 1
	fi
	sleep 0.2
done
wait "$qemu_pid"
trap - EXIT

for marker in ZCCUSAN_CHAOS_QEMU_PROCESS_PASS ZCCUSAN_CHAOS_QEMU_NETWORK_PASS ZCCUSAN_CHAOS_QEMU_PASS; do
	grep -q "$marker" "$log" || { tail -600 "$log" >&2; printf 'missing %s\n' "$marker" >&2; exit 1; }
done
grep -q '"event":"node_poweroff_requested".*"node":"qemu-chaos"' "$log" \
	|| { tail -600 "$log" >&2; printf 'node poweroff was not observed\n' >&2; exit 1; }
if grep -Eq 'ZCCUSAN_CHAOS_QEMU_FAIL|BUG:|Oops:|general protection fault|kernel panic' "$log"; then
	tail -600 "$log" >&2
	printf 'chaos QEMU log contains a failure marker\n' >&2
	exit 1
fi
grep 'ZCCUSAN_CHAOS_QEMU_.*PASS\|ZCCUSAN_CHAOS_QEMU_ARTIFACT' "$log"
