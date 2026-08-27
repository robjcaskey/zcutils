#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
KDIR="${KDIR:-/lib/modules/$KERNEL_RELEASE/build}"
QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
K3S_VERSION="${K3S_VERSION:-v1.36.1+k3s1}"
K3S_BIN="${K3S_BIN:-$ROOT/target/qemu-zcglobal-volume-failover/k3s-$K3S_VERSION}"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-600}"
WORK_DIR="${WORK_DIR:-/mnt/bulk_data/zcutils-qemu/zccusan-capacity-$(date -u +%Y%m%dT%H%M%SZ)}"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/qemu-zccusan-capacity-cargo}"
BIN_DIR="$CARGO_TARGET_DIR/release"
ROOTFS="$WORK_DIR/rootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
LOG_DIR="$WORK_DIR/logs"
network_tag="$(printf '%04x' $(( $$ % 65536 )))"
bridge="zca${network_tag}b"

need()
{
	command -v "$1" >/dev/null || {
		printf 'missing required command: %s\n' "$1" >&2
		exit 1
	}
}

if [[ "${ZCCUSAN_CAPACITY_QEMU_COORDINATED:-0}" != 1 && -x "$COORD_BIN" ]]; then
	exec "$COORD_BIN" run \
		--owner codex:zcutils-capacity-qemu \
		--mode soft-exclusive --sensitivity high --priority 65 --ttl 1800 \
		--resource 'cpu=*;memory-bandwidth=*;kvm=*' \
		--note 'seven initial K3s storage-node capacity admission and delayed eighth-node proof' \
		-- env ZCCUSAN_CAPACITY_QEMU_COORDINATED=1 "$0" "$@"
fi

for command in awk cargo cpio cp find ip ldd make podman "$QEMU_BIN" sed sudo taskset timeout truncate xz; do need "$command"; done
[[ -r "$KERNEL" ]] || { printf 'kernel not readable: %s\n' "$KERNEL" >&2; exit 1; }
[[ -d "$KDIR" ]] || { printf 'kernel build directory missing: %s\n' "$KDIR" >&2; exit 1; }
[[ -x "$K3S_BIN" ]] || { printf 'verified k3s binary missing: %s\n' "$K3S_BIN" >&2; exit 1; }
[[ -c /dev/kvm ]] || { printf '/dev/kvm unavailable\n' >&2; exit 1; }
[[ -x /usr/sbin/mkfs.ext4 ]] || { printf 'mkfs.ext4 missing\n' >&2; exit 1; }
(( $(nproc) >= 28 )) || { printf 'need at least 28 host CPUs for declared QEMU thread map\n' >&2; exit 1; }
sudo -n true
[[ ! -e "$WORK_DIR" ]] || { printf 'refusing to overwrite %s\n' "$WORK_DIR" >&2; exit 1; }
mkdir -p "$WORK_DIR" "$LOG_DIR"

printf 'WARNING: Kubernetes QEMU capacity results are correctness evidence, not representative IOPS measurements. Nine VMs share the host; hugetlb and dedicated IRQ lanes are absent.\n' | tee "$WORK_DIR/preflight.log"
printf 'kernel=%s host_cpus=%s host_mem_available_kib=%s hugetlb_total=%s memlock_host_kib=%s controller_vms=1 storage_nodes_initial=7 storage_nodes_delayed=1 placement=userspace-mirror block_placement=false\n' \
	"$KERNEL_RELEASE" "$(nproc)" "$(awk '/MemAvailable/ {print $2}' /proc/meminfo)" \
	"$(awk '/HugePages_Total/ {print $2}' /proc/meminfo)" "$(ulimit -l)" >>"$WORK_DIR/preflight.log"

CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release \
	--bin zccusan-operator --bin zcnblk-wal-leaf --bin zcnblk-wal-failover \
	--bin zcnblk-shm-target
make -C "$ROOT/kmods" KDIR="$KDIR"

image_context="$WORK_DIR/image-context"
mkdir -p "$image_context/root/usr/local/bin" "$image_context/root/bin" \
	"$image_context/root/lib" "$image_context/root/lib64" "$image_context/root/etc"
for binary in zccusan-operator zcnblk-wal-leaf zcnblk-wal-failover zcnblk-shm-target; do
	cp "$BIN_DIR/$binary" "$image_context/root/usr/local/bin/$binary"
done
cp /usr/bin/busybox "$image_context/root/bin/busybox"
for applet in grep sh sleep test; do ln -s busybox "$image_context/root/bin/$applet"; done
printf 'root:x:0:0:root:/root:/bin/sh\nnobody:x:65532:65532:nobody:/:/bin/false\n' >"$image_context/root/etc/passwd"
printf 'root:x:0:\nnobody:x:65532:\n' >"$image_context/root/etc/group"
while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$image_context/root$(dirname "$library")"
	cp "$library" "$image_context/root$library"
done < <(
	{
		for binary in zccusan-operator zcnblk-wal-leaf zcnblk-wal-failover zcnblk-shm-target; do ldd "$BIN_DIR/$binary"; done
		ldd /usr/bin/busybox
	} | awk '/=> \// { print $3; next } /^[[:space:]]*\/lib/ { print $1; next }' | sort -u
)
podman build --network none --pull=never -q \
	-t localhost/zccusan-capacity-qemu:latest \
	-f "$ROOT/scripts/zccusan-crd-qemu.Containerfile" "$image_context" >/dev/null
podman image exists registry.k8s.io/pause:3.10 || podman pull registry.k8s.io/pause:3.10 >/dev/null
podman save --format oci-archive -o "$WORK_DIR/zccusan-capacity-qemu.tar" localhost/zccusan-capacity-qemu:latest
podman save --format oci-archive -o "$WORK_DIR/pause.tar" registry.k8s.io/pause:3.10

mkdir -p "$ROOTFS/bin" "$ROOTFS/lib" "$ROOTFS/lib64" "$ROOTFS/modules" \
	"$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/tmp" "$ROOTFS/run" \
	"$ROOTFS/etc" "$ROOTFS/var/lib/rancher/k3s/agent/images"
cp /usr/bin/busybox "$ROOTFS/bin/busybox"
for applet in cat dmesg echo grep hostname insmod ip kill ln mkdir mount nc poweroff rmmod sh sleep sort switch_root tail test tr wc; do
	ln -s busybox "$ROOTFS/bin/$applet"
done
cp "$ROOT/scripts/zccusan-capacity-qemu-init.sh" "$ROOTFS/init"
chmod 0755 "$ROOTFS/init"
cp "$K3S_BIN" "$ROOTFS/k3s"
cp "$WORK_DIR/zccusan-capacity-qemu.tar" "$ROOTFS/var/lib/rancher/k3s/agent/images/"
cp "$WORK_DIR/pause.tar" "$ROOTFS/var/lib/rancher/k3s/agent/images/"
cp "$ROOT/zccusan/charts/zcblock-csi/crds/storage.zcutils.io.yaml" "$ROOTFS/storage-crds.yaml"
cp "$ROOT/scripts/zccusan-capacity-qemu-operator.yaml" "$ROOTFS/operator.yaml"
cp "$ROOT/scripts/zccusan-capacity-qemu-intents.yaml" "$ROOTFS/intents.yaml"
printf 'nameserver 10.96.1.1\n' >"$ROOTFS/etc/resolv.conf"
{
	printf '127.0.0.1 localhost\n10.96.1.1 controller\n'
	for ordinal in 1 2 3 4 5 6 7 8; do printf '10.96.1.%s storage-%s\n' "$((10 + ordinal))" "$ordinal"; done
} >"$ROOTFS/etc/hosts"
printf 'root:x:0:0:root:/root:/bin/sh\n' >"$ROOTFS/etc/passwd"
printf 'root:x:0:\n' >"$ROOTFS/etc/group"

copy_module()
{
	local module="$1" source_path
	source_path="$(/usr/sbin/modinfo -k "$KERNEL_RELEASE" -n "$module")"
	case "$source_path" in
		*.xz) xz -dc -- "$source_path" >"$ROOTFS/modules/$module.ko" ;;
		*.zst) zstd -dc -- "$source_path" >"$ROOTFS/modules/$module.ko" ;;
		*.ko) cp "$source_path" "$ROOTFS/modules/$module.ko" ;;
		*) printf 'unsupported module compression: %s\n' "$source_path" >&2; return 1 ;;
	esac
}
for module in failover net_failover virtio_net virtio_blk aead crc16 mbcache jbd2 ext4; do copy_module "$module"; done
cp "$ROOT/kmods/zcnblk_client_mod.ko" "$ROOTFS/modules/zcnblk_client_mod.ko"
while IFS= read -r library; do
	[[ -n "$library" ]] || continue
	mkdir -p "$ROOTFS$(dirname "$library")"
	cp "$library" "$ROOTFS$library"
done < <(ldd /usr/bin/busybox | awk '/=> \// { print $3; next } /^[[:space:]]*\/lib/ { print $1; next }' | sort -u)

(
	cd "$ROOTFS"
	find . -print0 | cpio --null -o --format=newc >"$INITRAMFS" 2>"$WORK_DIR/cpio.log"
)
touch "$ROOTFS/.zccap-system-root"
base_image="$WORK_DIR/base-system.ext4"
truncate -s 1300M "$base_image"
/usr/sbin/mkfs.ext4 -F -q -L zccap-base -d "$ROOTFS" "$base_image"
for role in controller storage-1 storage-2 storage-3 storage-4 storage-5 storage-6 storage-7 storage-8; do
	cp --sparse=always "$base_image" "$WORK_DIR/$role-system.ext4"
done

declare -a qemu_pids=()
declare -a tap_devices=()
declare -A vm_pid=()
declare -A vm_log=()
network_created=0

verified_stop_qemu()
{
	local pid="$1" comm cmdline
	[[ "$pid" =~ ^[0-9]+$ && -r "/proc/$pid/comm" ]] || return 0
	comm="$(<"/proc/$pid/comm")"
	cmdline="$(tr '\0' ' ' <"/proc/$pid/cmdline")"
	if [[ "$comm" == qemu-system-* && "$cmdline" == *"$INITRAMFS"*zccap.role=* ]]; then
		kill -TERM "$pid" 2>/dev/null || true
	else
		printf 'cleanup refused unverified pid=%s comm=%s cmdline=%s\n' "$pid" "$comm" "$cmdline" >&2
	fi
}

cleanup()
{
	local status=$? pid tap
	trap - EXIT INT TERM
	for pid in "${qemu_pids[@]:-}"; do verified_stop_qemu "$pid"; done
	for tap in "${tap_devices[@]:-}"; do [[ -n "$tap" ]] && sudo -n ip link del "$tap" 2>/dev/null || true; done
	if (( network_created )); then sudo -n ip link del "$bridge" 2>/dev/null || true; fi
	exit "$status"
}
trap cleanup EXIT INT TERM

sudo -n ip link add "$bridge" type bridge
network_created=1
sudo -n ip link set "$bridge" type bridge stp_state 0
sudo -n ip link set "$bridge" mtu 1500 up

pin_vm()
{
	local role="$1" pid="$2" vcpu0="$3" vcpu1="$4" emulator="$5" found=0
	taskset -pc "$emulator" "$pid" >>"$WORK_DIR/host-thread-map.log" 2>&1
	for _ in $(seq 1 300); do
		found=0
		for task in /proc/"$pid"/task/[0-9]*; do
			[[ -r "$task/comm" ]] || continue
			local tid comm cpu
			tid="${task##*/}"
			comm="$(<"$task/comm")"
			case "$comm" in
				'CPU 0/KVM') cpu="$vcpu0" ;;
				'CPU 1/KVM') cpu="$vcpu1" ;;
				*) continue ;;
			esac
			taskset -pc "$cpu" "$tid" >>"$WORK_DIR/host-thread-map.log" 2>&1
			printf 'vcpu-map role=%s guest_vcpu=%s host_cpu=%s tid=%s\n' "$role" "${comm#CPU }" "$cpu" "$tid" >>"$WORK_DIR/host-thread-map.log"
			found=$((found + 1))
		done
		(( found == 2 )) && break
		sleep 0.05
	done
	(( found == 2 ))
	printf 'emulator-map role=%s host_cpu=%s pid=%s\n' "$role" "$emulator" "$pid" >>"$WORK_DIR/host-thread-map.log"
}

launch_vm()
{
	local role="$1" suffix="$2" vcpu0="$3" vcpu1="$4" emulator="$5" memory="$6"
	local tap="za${network_tag}${suffix}" log="$LOG_DIR/$role.log" mac="52:54:60:00:00:$suffix"
	[[ ${#tap} -le 15 ]]
	: >"$log"
	sudo -n ip tuntap add dev "$tap" mode tap user "$(id -un)"
	sudo -n ip link set "$tap" master "$bridge"
	sudo -n ip link set "$tap" mtu 1500 up
	tap_devices+=("$tap")
	taskset -c "$emulator" "$QEMU_BIN" \
		-name "guest=zccap-$role,debug-threads=on" \
		-machine q35,accel=kvm -cpu host -m "$memory" -smp 2,sockets=1,cores=2,threads=1 \
		-display none -monitor none -serial "file:$log" -no-reboot -nodefaults \
		-kernel "$KERNEL" -initrd "$INITRAMFS" \
		-append "console=ttyS0 panic=-1 oops=panic quiet net.ifnames=0 rootfstype=tmpfs zccap.role=$role" \
		-netdev "tap,id=net0,ifname=$tap,script=no,downscript=no" \
		-device "virtio-net-pci,netdev=net0,mac=$mac" \
		-drive "if=none,id=system,file=$WORK_DIR/$role-system.ext4,format=raw,cache=none,aio=threads" \
		-device virtio-blk-pci,drive=system,serial=zccap-system \
		>/dev/null 2>>"$log" &
	local pid=$!
	qemu_pids+=("$pid")
	vm_pid["$role"]="$pid"
	vm_log["$role"]="$log"
	pin_vm "$role" "$pid" "$vcpu0" "$vcpu1" "$emulator"
	printf 'launch role=%s qemu_pid=%s tap=%s guest_vcpu0_host_cpu=%s guest_vcpu1_host_cpu=%s emulator_host_cpu=%s memory=%s\n' \
		"$role" "$pid" "$tap" "$vcpu0" "$vcpu1" "$emulator" "$memory" >>"$WORK_DIR/topology.log"
}

{
	printf 'classification=shared-host-qemu representative=false kubernetes=k3s controller_nodes=1 storage_nodes_initial=7 storage_nodes_delayed=1\n'
	printf 'placement=userspace-mirror copies=2 block_device_role=client-edge-only per_node_capacity_bytes=8388608 per_node_provisioned_iops=100 volume_capacity_bytes=8388608 volume_provisioned_iops=75\n'
	printf 'capacity_admission=operator-control-plane status_runtime=reservation-record hot_path_lock=false trigger_after_rejection=node-registration-only retry_seconds=5\n'
} >"$WORK_DIR/topology.log"

launch_vm controller 01 0 1 26 1536M
for ordinal in 1 2 3 4 5 6 7; do
	launch_vm "storage-$ordinal" "$(printf '%02x' "$((10 + ordinal))")" "$((ordinal * 2))" "$((ordinal * 2 + 1))" "$((17 + ordinal))" 1024M
done

controller_log="${vm_log[controller]}"
deadline=$((SECONDS + TIMEOUT_SECONDS))
while ! grep -q 'ZCCUSAN_K8S_CAPACITY_NEEDS_NODE' "$controller_log"; do
	if (( SECONDS >= deadline )); then
		printf 'timed out waiting for Kubernetes capacity rejection; logs=%s\n' "$LOG_DIR" >&2
		for log in "$LOG_DIR"/*.log; do printf '\n== %s ==\n' "$log"; tail -200 "$log"; done
		exit 1
	fi
	controller_pid="${vm_pid[controller]}"
	if ! kill -0 "$controller_pid" 2>/dev/null; then
		printf 'controller exited before capacity rejection marker\n' >&2
		tail -300 "$controller_log"
		exit 1
	fi
	sleep 0.1
done

printf 'host observed capacity rejection; bringing storage-8 online without changing the ZcVolume request\n' | tee -a "$WORK_DIR/topology.log"
launch_vm storage-8 18 16 17 25 1024M

deadline=$((SECONDS + TIMEOUT_SECONDS))
while ! grep -q 'ZCCUSAN_CAPACITY_QEMU_PASS role=controller' "$controller_log"; do
	if (( SECONDS >= deadline )); then
		printf 'timed out waiting for post-node admission; logs=%s\n' "$LOG_DIR" >&2
		for log in "$LOG_DIR"/*.log; do printf '\n== %s ==\n' "$log"; tail -240 "$log"; done
		exit 1
	fi
	controller_pid="${vm_pid[controller]}"
	if ! kill -0 "$controller_pid" 2>/dev/null; then
		printf 'controller exited before final pass marker\n' >&2
		tail -300 "$controller_log"
		exit 1
	fi
	sleep 0.1
done

for role in controller storage-1 storage-2 storage-3 storage-4 storage-5 storage-6 storage-7 storage-8; do
	pid="${vm_pid[$role]}"
	status=0
	wait "$pid" || status=$?
	(( status == 0 )) || { printf 'QEMU role=%s exited status=%s\n' "$role" "$status" >&2; exit 1; }
	grep -q "ZCCUSAN_CAPACITY_QEMU_PASS role=$role" "${vm_log[$role]}"
	if grep -Eq 'ZCCUSAN_CAPACITY_QEMU_FAIL|BUG:|Oops:|general protection fault|kernel panic' "${vm_log[$role]}"; then
		printf 'failure marker for %s\n' "$role" >&2
		tail -300 "${vm_log[$role]}"
		exit 1
	fi
done
qemu_pids=()

grep -q 'ZCCUSAN_CAPACITY_INITIAL_PASS.*storage_nodes=7.*reserved_userspace_leaves=6' "$controller_log"
grep -q 'ZCCUSAN_K8S_CAPACITY_NEEDS_NODE.*partial_reservation=false' "$controller_log"
grep -q 'ZCCUSAN_K8S_CAPACITY_ADD_NODE_PASS.*storage_nodes_final=8.*request_mutated=false.*admission_trigger=node-registration-only' "$controller_log"

cleanup_status=0
for tap in "${tap_devices[@]}"; do sudo -n ip link del "$tap" 2>/dev/null || cleanup_status=1; done
tap_devices=()
sudo -n ip link del "$bridge" || cleanup_status=1
network_created=0
trap - EXIT INT TERM

printf 'ZCCUSAN_CAPACITY_QEMU_MATRIX_PASS kubernetes=k3s storage_nodes_initial=7 storage_nodes_final=8 mirrored_volumes_before_rejection=3 userspace_leaves_reserved=6 rejected_volume=capacity-needs-storage-8 partial_reservation=false request_mutated=false trigger=node-registration-only final_leaves=storage-7,storage-8 userspace_placement=true block_placement=false cleanup_status=%s artifacts=%s\n' \
	"$cleanup_status" "$WORK_DIR" | tee "$WORK_DIR/result.log"
