#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
REPEATS="${REPEATS:-3}"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-420}"
WORK_DIR="${WORK_DIR:-$ROOT/bench-results/zcvolume-scale-qemu-$(date -u +%Y%m%dT%H%M%SZ)}"
ROOTFS="$WORK_DIR/rootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
LOG_DIR="$WORK_DIR/logs"
ONLINE_MARKER="$WORK_DIR/storage-8.online"
network_tag="$(printf '%04x' $(( $$ % 65536 )))"
bridge="zvs${network_tag}b"

need()
{
	command -v "$1" >/dev/null || {
		printf 'missing required command: %s\n' "$1" >&2
		exit 1
	}
}

if [[ "${ZCVOLUME_SCALE_QEMU_COORDINATED:-0}" != 1 && -x "$COORD_BIN" ]]; then
	exec "$COORD_BIN" run \
		--owner codex:zcutils-volume-scale-qemu \
		--mode soft-exclusive --sensitivity high --priority 65 --ttl 900 \
		--resource 'cpu=*;memory-bandwidth=*;kvm=*' \
		--note 'seven storage plus three client QEMU virtual-volume scale and eighth-node admission proof' \
		-- env ZCVOLUME_SCALE_QEMU_COORDINATED=1 "$0" "$@"
fi

for command in awk cargo cpio ip ldd "$QEMU_BIN" readelf sudo taskset timeout xz; do need "$command"; done
[[ "$REPEATS" =~ ^[1-9][0-9]*$ ]] || { printf 'REPEATS must be positive\n' >&2; exit 2; }
[[ "$TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]] || { printf 'TIMEOUT_SECONDS must be positive\n' >&2; exit 2; }
[[ -r "$KERNEL" ]] || { printf 'kernel not readable: %s\n' "$KERNEL" >&2; exit 1; }
[[ -c /dev/kvm ]] || { printf '/dev/kvm is unavailable\n' >&2; exit 1; }
sudo -n true
[[ ! -e "$WORK_DIR" ]] || { printf 'refusing to overwrite %s\n' "$WORK_DIR" >&2; exit 1; }
mkdir -p "$ROOTFS/bin" "$ROOTFS/usr/bin" "$ROOTFS/lib" "$ROOTFS/lib64" \
	"$ROOTFS/modules" "$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/run" \
	"$ROOTFS/tmp" "$LOG_DIR"

printf 'WARNING: local QEMU scale results are shared-host, non-representative measurements. hugetlb is not configured; virtio IRQs share guest CPUs; KVM vCPU and emulator pinning is recorded. Do not compare these IOPS with hardware EFA or block records.\n' | tee "$WORK_DIR/preflight.log"
printf 'preflight kernel=%s host_cpus=%s host_mem_available_kib=%s hugetlb_total=%s memlock_host_kib=%s qemu_vms_initial=10 storage_nodes_initial=7 clients=3 volumes=1000 heavy_volumes=7 repeats=%s\n' \
	"$KERNEL_RELEASE" "$(nproc)" "$(awk '/MemAvailable/ {print $2}' /proc/meminfo)" \
	"$(awk '/HugePages_Total/ {print $2}' /proc/meminfo)" "$(ulimit -l)" "$REPEATS" >>"$WORK_DIR/preflight.log"
(( $(nproc) >= 32 )) || { printf 'need at least 32 host CPUs for the declared pin map\n' >&2; exit 1; }

cargo build --release --bin zcutils --bin zcvolume-capacity-scenario

copy_runtime_file()
{
	local source_path="$1"
	local destination_path="$2"
	mkdir -p "$ROOTFS$(dirname "$destination_path")"
	cp -L "$source_path" "$ROOTFS$destination_path"
}

copy_binary()
{
	local source_path="$1"
	local destination_path="$2"
	copy_runtime_file "$source_path" "$destination_path"
	while IFS= read -r library; do
		[[ -n "$library" ]] || continue
		copy_runtime_file "$library" "$library"
	done < <(ldd "$source_path" | awk '/=> \/.* \(/ {print $3; next} /^[[:space:]]*\/lib/ {print $1}' | sort -u)
}

copy_module()
{
	local module="$1"
	local source_path
	source_path="$(/usr/sbin/modinfo -k "$KERNEL_RELEASE" -n "$module")"
	case "$source_path" in
		*.xz) xz -dc -- "$source_path" >"$ROOTFS/modules/$module.ko" ;;
		*.zst) zstd -dc -- "$source_path" >"$ROOTFS/modules/$module.ko" ;;
		*.ko) cp "$source_path" "$ROOTFS/modules/$module.ko" ;;
		*) printf 'unsupported module compression: %s\n' "$source_path" >&2; return 1 ;;
	esac
}

copy_binary /usr/bin/busybox /bin/busybox
for applet in awk cat date env grep hostname insmod ln mkdir mount ping poweroff sed sh sleep sync taskset true uname wc; do
	ln -s busybox "$ROOTFS/bin/$applet"
done
copy_binary /usr/bin/ip /usr/bin/ip
copy_binary "$ROOT/target/release/zcutils" /uring-play
copy_module failover
copy_module net_failover
copy_module virtio_net
cp "$ROOT/scripts/zcvolume-scale-qemu-init.sh" "$ROOTFS/init"
chmod 0755 "$ROOTFS/init" "$ROOTFS/uring-play"
(
	cd "$ROOTFS"
	find . -print0 | cpio --null -o --format=newc >"$INITRAMFS" 2>"$WORK_DIR/cpio.log"
)

declare -a qemu_pids=()
declare -a tap_devices=()
declare -A vm_pid=()
declare -A vm_log=()
scheduler_pid=""
network_created=0

verified_stop_qemu()
{
	local pid="$1"
	[[ "$pid" =~ ^[0-9]+$ && -r "/proc/$pid/comm" ]] || return 0
	local comm cmdline
	comm="$(<"/proc/$pid/comm")"
	cmdline="$(tr '\0' ' ' <"/proc/$pid/cmdline")"
	if [[ "$comm" == qemu-system-* && "$cmdline" == *"$INITRAMFS"*zcscale.role=* ]]; then
		kill -TERM "$pid" 2>/dev/null || true
	else
		printf 'cleanup refused unverified pid=%s comm=%s cmdline=%s\n' "$pid" "$comm" "$cmdline" >&2
	fi
}

cleanup()
{
	local status=$?
	trap - EXIT INT TERM
	for pid in "${qemu_pids[@]:-}"; do verified_stop_qemu "$pid"; done
	if [[ "$scheduler_pid" =~ ^[0-9]+$ ]] && kill -0 "$scheduler_pid" 2>/dev/null; then
		kill -TERM "$scheduler_pid" 2>/dev/null || true
	fi
	for tap in "${tap_devices[@]:-}"; do
		[[ -n "$tap" ]] && sudo -n ip link del "$tap" 2>/dev/null || true
	done
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
	local role="$1" pid="$2" vcpu0="$3" vcpu1="$4" emulator="$5"
	local found=0
	taskset -pc "$emulator" "$pid" >>"$WORK_DIR/host-thread-map.log" 2>&1
	for _ in $(seq 1 200); do
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
	local role="$1" address_suffix="$2" vcpu0="$3" vcpu1="$4" emulator="$5" start_epoch="$6"
	local tap="zv${network_tag}${address_suffix}"
	local log="$LOG_DIR/$role.log"
	local pidfile="$WORK_DIR/$role.pid"
	[[ ${#tap} -le 15 ]] || { printf 'tap name too long: %s\n' "$tap" >&2; exit 1; }
	: >"$log"
	sudo -n ip tuntap add dev "$tap" mode tap user "$(id -un)"
	sudo -n ip link set "$tap" master "$bridge"
	sudo -n ip link set "$tap" mtu 1500 up
	tap_devices+=("$tap")
	taskset -c "$emulator" "$QEMU_BIN" \
		-name "guest=zcvolume-$role,debug-threads=on" \
		-machine q35,accel=kvm -cpu host -m 384M -smp 2,sockets=1,cores=2,threads=1 \
		-display none -monitor none -serial "file:$log" -no-reboot -nodefaults -pidfile "$pidfile" \
		-kernel "$KERNEL" -initrd "$INITRAMFS" \
		-append "console=ttyS0 panic=-1 oops=panic quiet net.ifnames=0 zcscale.role=$role zcscale.start_epoch=$start_epoch zcscale.repeats=$REPEATS" \
		-netdev "tap,id=net0,ifname=$tap,script=no,downscript=no" \
		-device "virtio-net-pci,netdev=net0,mac=52:54:5f:00:00:$address_suffix" \
		>/dev/null 2>>"$log" &
	local job_pid=$!
	for _ in $(seq 1 100); do [[ -s "$pidfile" ]] && break; sleep 0.05; done
	[[ -s "$pidfile" ]]
	local pid
	pid="$(<"$pidfile")"
	[[ "$pid" =~ ^[0-9]+$ ]]
	qemu_pids+=("$pid")
	vm_pid["$role"]="$pid"
	vm_log["$role"]="$log"
	pin_vm "$role" "$pid" "$vcpu0" "$vcpu1" "$emulator"
	printf 'launch role=%s qemu_job_pid=%s qemu_pid=%s tap=%s guest_vcpu0_host_cpu=%s guest_vcpu1_host_cpu=%s emulator_host_cpu=%s\n' \
		"$role" "$job_pid" "$pid" "$tap" "$vcpu0" "$vcpu1" "$emulator" >>"$WORK_DIR/topology.log"
}

{
	printf 'classification=shared-host-qemu representative=false storage_nodes_initial=7 clients=3 virtual_volumes=1000 heavy_volumes=7 heavy_share_pct=80 repetitions=%s\n' "$REPEATS"
	printf 'data_path=client-userspace-WAL-frame->virtio-net->tap->linux-bridge->tap->virtio-net->storage-userspace-volatile-WAL-HWM placement=userspace-lane-flow mirror=false stripe=false block_device=false\n'
	printf 'lanes_per_flow=1 workers_per_flow=1 per_worker_qd=64 concurrent_flows=14 aggregate_outstanding_depth=896 completion=remote-application-ack raw_transport_rtt=guest-ping theoretical_iops_ceiling=not-computed actual_theoretical_efficiency=not-reported reason=shared-host-qemu-nonrepresentative\n'
	printf 'guest_memlock=unlimited-attempt hugetlb=not-configured virtio_irq_affinity=guest-default worker_cpu_map=storage:cold0,hot1;client:server-ordinal-mod2 hctx_affinity=not-applicable-no-block-edge\n'
} >"$WORK_DIR/topology.log"

"$ROOT/target/release/zcvolume-capacity-scenario" "$ONLINE_MARKER" >"$WORK_DIR/capacity-scheduler.log" 2>&1 &
scheduler_pid=$!
start_epoch=$(( $(date +%s) + 20 ))
for ordinal in $(seq 1 7); do
	launch_vm "storage-$ordinal" "$(printf '%02x' "$ordinal")" "$(( (ordinal - 1) * 2 ))" "$(( (ordinal - 1) * 2 + 1 ))" "$((19 + ordinal))" "$start_epoch"
done
for client in 0 1 2; do
	launch_vm "client-$client" "$(printf '%02x' "$((21 + client))")" "$((14 + client * 2))" "$((15 + client * 2))" "$((27 + client))" "$start_epoch"
done

initial_roles=(storage-1 storage-2 storage-3 storage-4 storage-5 storage-6 storage-7 client-0 client-1 client-2)
deadline=$((SECONDS + TIMEOUT_SECONDS))
while :; do
	ready=0
	for role in "${initial_roles[@]}"; do
		grep -q "ZCVOLUME_SCALE_INITIAL_PASS role=$role" "${vm_log[$role]}" && ready=$((ready + 1))
	done
	(( ready == ${#initial_roles[@]} )) && break
	if (( SECONDS >= deadline )); then
		printf 'timed out waiting for initial QEMU workload; logs retained at %s\n' "$LOG_DIR" >&2
		for role in "${initial_roles[@]}"; do printf '\n== %s ==\n' "$role"; tail -80 "${vm_log[$role]}"; done
		exit 1
	fi
	for role in "${initial_roles[@]}"; do
		pid="${vm_pid[$role]}"
		if ! kill -0 "$pid" 2>/dev/null && ! grep -q "ZCVOLUME_SCALE_INITIAL_PASS role=$role" "${vm_log[$role]}"; then
			printf 'VM %s exited before its initial pass marker\n' "$role" >&2
			tail -160 "${vm_log[$role]}"
			exit 1
		fi
	done
	sleep 0.1
done

launch_vm storage-8 18 0 1 20 "$start_epoch"
for _ in $(seq 1 1200); do
	grep -q 'ZCVOLUME_SCALE_STORAGE8_READY' "${vm_log[storage-8]}" && break
	sleep 0.05
done
grep -q 'ZCVOLUME_SCALE_STORAGE8_READY' "${vm_log[storage-8]}"
touch "$ONLINE_MARKER"

deadline=$((SECONDS + TIMEOUT_SECONDS))
while :; do
	if grep -q 'ZCVOLUME_SCALE_CLIENT8_DATA_PASS' "${vm_log[client-0]}" && \
		grep -q 'ZCVOLUME_SCALE_STORAGE8_DATA_PASS' "${vm_log[storage-8]}" && \
		! kill -0 "$scheduler_pid" 2>/dev/null; then
		break
	fi
	(( SECONDS < deadline )) || { printf 'timed out waiting for eighth-node admission\n' >&2; exit 1; }
	sleep 0.1
done
wait "$scheduler_pid"
scheduler_pid=""

for role in storage-1 storage-2 storage-3 storage-4 storage-5 storage-6 storage-7 client-0 client-1 client-2 storage-8; do
	grep -q "ZCVOLUME_SCALE_GUEST_PASS role=$role" "${vm_log[$role]}"
	if grep -Eq 'BUG:|Oops:|general protection fault|kernel panic|ZCVOLUME_SCALE_GUEST_FINAL.*status=[1-9]' "${vm_log[$role]}"; then
		printf 'failure marker in %s\n' "${vm_log[$role]}" >&2
		exit 1
	fi
done
for role in storage-1 storage-2 storage-3 storage-4 storage-5 storage-6 storage-7 client-0 client-1 client-2 storage-8; do
	pid="${vm_pid[$role]}"
	qemu_status=0
	wait "$pid" || qemu_status=$?
	(( qemu_status == 0 )) || {
		printf 'QEMU role=%s exited status=%s\n' "$role" "$qemu_status" >&2
		exit 1
	}
done
qemu_pids=()
grep -q 'ZCVOLUME_CAPACITY_REJECT_PASS' "$WORK_DIR/capacity-scheduler.log"
grep -q 'ZCVOLUME_CAPACITY_ADD_NODE_PASS.*selected_host=storage-8' "$WORK_DIR/capacity-scheduler.log"

awk '
/ZCVOLUME_SCALE_CLIENT_WAVE/ {
	rep=ops=elapsed=0
	for (i=1; i<=NF; i++) {
		split($i, f, "=")
		if (f[1] == "repeat") rep=f[2]+0
		if (f[1] == "total_ops") ops=f[2]+0
		if (f[1] == "elapsed_us") elapsed=f[2]+0
	}
	total[rep] += ops
	if (elapsed > slowest[rep]) slowest[rep]=elapsed
}
END {
	for (rep=1; rep<100; rep++) if (total[rep] > 0) {
		iops=total[rep]*1000000/slowest[rep]
		printf "repeat=%d completed_ops=%d synchronized_elapsed_us=%d conservative_iops=%.0f payload_Gbitps=%.3f hot_share_pct=80\n", rep, total[rep], slowest[rep], iops, iops*4096*8/1e9
	}
}' "$LOG_DIR"/client-*.log | tee "$WORK_DIR/throughput-summary.log"
rg 'zcofi-wal-send-latency-summary:' "$LOG_DIR"/client-*.log >"$WORK_DIR/latency-flows.log" || true
rg 'zcofi-wal-(send|recv)-virtual-volumes:' "$LOG_DIR"/*.log >"$WORK_DIR/volume-fairness.log"

cleanup_status=0
for tap in "${tap_devices[@]}"; do sudo -n ip link del "$tap" 2>/dev/null || cleanup_status=1; done
tap_devices=()
sudo -n ip link del "$bridge"
network_created=0
trap - EXIT INT TERM

printf 'ZCVOLUME_SCALE_QEMU_PASS storage_nodes_initial=7 storage_nodes_final=8 clients=3 initial_virtual_volumes=1000 heavy_volumes=7 initial_remote_ops_per_repeat=143360 hot_share_pct=80 repetitions=%s completion=remote-application-ack capacity_fill_volumes=8 rejected_volume=needs-storage-8 rejection_preserved_state=true admitted_after_node_online=true admitted_host=storage-8 admitted_data_ops=1024 userspace_placement=true block_placement=false transport=ofi-sockets-over-virtio-tcp shared_host=true representative=false cleanup_status=%s artifacts=%s\n' \
	"$REPEATS" "$cleanup_status" "$WORK_DIR" | tee "$WORK_DIR/result.log"
