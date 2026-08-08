#!/usr/bin/env bash

set -euo pipefail

ROOT=/home/rob/zcutils
OUTDIR="$ROOT/bench-results/zcworkload-qemu-tap-2lane-qd64-20260713T0645Z"
KERNEL=/home/rob/src/linux-7.0.8-zcslots/arch/x86/boot/bzImage
INITRD="$OUTDIR/zcworkload-two-vm-initramfs.cpio"
BRIDGE=zcwlbr0
TARGET_TAP=zcwltgt0
CLIENT_TAP=zcwlcli0
COORD_BIN=/home/rob/.local/bin/agent-coord
TARGET_PIDFILE="$OUTDIR/target-qemu.pid"
CLIENT_PIDFILE="$OUTDIR/client-qemu.pid"
TARGET_JOB_PID=
CLIENT_JOB_PID=
COORD_TOKEN=$(sed -n 's/.*"token":"\([^"]*\)".*/\1/p' "$OUTDIR/coordination-request.json")

cleanup_one()
{
	label=$1
	pidfile=$2
	[ -s "$pidfile" ] || return 0
	pid=$(cat "$pidfile")
	case "$pid" in
		''|*[!0-9]*)
			echo "cleanup: refusing invalid $label pid=$pid"
			return 1
			;;
	esac
	[ -r "/proc/$pid/comm" ] || {
		echo "cleanup: $label pid=$pid already exited"
		return 0
	}
	comm=$(cat "/proc/$pid/comm")
	cmdline=$(tr '\0' ' ' < "/proc/$pid/cmdline")
	echo "cleanup: inspect label=$label pid=$pid comm=$comm cmdline=$cmdline"
	case "$comm:$cmdline" in
		qemu-system-x86*:*$INITRD*) ;;
		*)
			echo "cleanup: refusing to signal unverified $label pid=$pid"
			return 1
			;;
	esac
	kill -TERM "$pid"
	for _ in $(seq 1 100); do
		[ ! -e "/proc/$pid" ] && break
		sleep 0.1
	done
	if [ -e "/proc/$pid" ]; then
		echo "cleanup: escalating verified $label pid=$pid to KILL"
		kill -KILL "$pid"
	fi
}

cleanup()
{
	status=$?
	set +e
	{
		cleanup_one client "$CLIENT_PIDFILE"
		cleanup_one target "$TARGET_PIDFILE"
		[ -z "$CLIENT_JOB_PID" ] || wait "$CLIENT_JOB_PID"
		[ -z "$TARGET_JOB_PID" ] || wait "$TARGET_JOB_PID"
		if [ -n "$COORD_TOKEN" ]; then
			$COORD_BIN release "$COORD_TOKEN"
			COORD_TOKEN=
		fi
		sudo ip link del "$TARGET_TAP" 2>/dev/null || true
		sudo ip link del "$CLIENT_TAP" 2>/dev/null || true
		sudo ip link del "$BRIDGE" 2>/dev/null || true
		echo "cleanup: complete original_status=$status"
	} >> "$OUTDIR/cleanup.log" 2>&1
	exit "$status"
}

snapshot_threads()
{
	label=$1
	pid=$2
	out=$3
	{
		echo "snapshot=$label qemu_pid=$pid"
		for task in /proc/$pid/task/[0-9]*; do
			[ -r "$task/comm" ] || continue
			tid=${task##*/}
			comm=$(cat "$task/comm")
			allowed=$(sed -n 's/^Cpus_allowed_list:[[:space:]]*//p' "$task/status")
			voluntary=$(sed -n 's/^voluntary_ctxt_switches:[[:space:]]*//p' "$task/status")
			involuntary=$(sed -n 's/^nonvoluntary_ctxt_switches:[[:space:]]*//p' "$task/status")
			printf 'qemu-thread: vm=%s tid=%s comm=%s allowed=%s voluntary=%s involuntary=%s\n' \
				"$label" "$tid" "$comm" "$allowed" "$voluntary" "$involuntary"
		done
	} >> "$out"
}

pin_vm()
{
	label=$1
	pid=$2
	emulator_cpu=$3
	first_vcpu_host_cpu=$4
	vcpu_count=$5
	out=$6

	taskset -apc "$emulator_cpu" "$pid" >> "$out" 2>&1
	found=0
	for _ in $(seq 1 100); do
		found=0
		for task in /proc/$pid/task/[0-9]*; do
			[ -r "$task/comm" ] || continue
			tid=${task##*/}
			comm=$(cat "$task/comm")
			case "$comm" in
				CPU\ [0-9]*/KVM)
					vcpu=${comm#CPU }
					vcpu=${vcpu%/KVM}
					host_cpu=$((first_vcpu_host_cpu + vcpu))
					taskset -pc "$host_cpu" "$tid" >> "$out" 2>&1
					printf 'vcpu-map: vm=%s qemu_pid=%s tid=%s guest_vcpu=%s host_cpu=%s\n' \
						"$label" "$pid" "$tid" "$vcpu" "$host_cpu" >> "$out"
					found=$((found + 1))
					;;
			esac
		done
		[ "$found" -eq "$vcpu_count" ] && break
		sleep 0.1
	done
	[ "$found" -eq "$vcpu_count" ]
	printf 'emulator-map: vm=%s qemu_pid=%s host_cpu=%s\n' "$label" "$pid" "$emulator_cpu" >> "$out"
}

trap cleanup EXIT INT TERM

test -r "$KERNEL"
test -r "$INITRD"
test -c /dev/kvm
test -n "$COORD_TOKEN"

sudo ip link del "$TARGET_TAP" 2>/dev/null || true
sudo ip link del "$CLIENT_TAP" 2>/dev/null || true
sudo ip link del "$BRIDGE" 2>/dev/null || true
sudo ip link add "$BRIDGE" type bridge
sudo ip tuntap add dev "$TARGET_TAP" mode tap user "$(id -un)"
sudo ip tuntap add dev "$CLIENT_TAP" mode tap user "$(id -un)"
sudo ip link set "$TARGET_TAP" master "$BRIDGE"
sudo ip link set "$CLIENT_TAP" master "$BRIDGE"
sudo ip link set "$BRIDGE" mtu 9000 up
sudo ip link set "$TARGET_TAP" mtu 9000 up
sudo ip link set "$CLIENT_TAP" mtu 9000 up
{
	ip -details link show dev "$BRIDGE"
	ip -details link show dev "$TARGET_TAP"
	ip -details link show dev "$CLIENT_TAP"
} > "$OUTDIR/host-link-topology.log"

rm -f "$TARGET_PIDFILE" "$CLIENT_PIDFILE" "$OUTDIR/cleanup.log"

echo "launching target VM"
/usr/bin/time -v -o "$OUTDIR/target-qemu-time.log" \
	taskset -c 2,4-7 \
	qemu-system-x86_64 \
		-name guest=zcwl-target,debug-threads=on \
		-machine q35,accel=kvm -cpu host \
		-m 2048M -smp 4,sockets=1,cores=4,threads=1 \
		-display none -monitor none -serial "file:$OUTDIR/target-console.log" \
		-no-reboot -nodefaults -pidfile "$TARGET_PIDFILE" \
		-kernel "$KERNEL" -initrd "$INITRD" \
		-append 'console=ttyS0 panic=-1 oops=panic quiet zcworkload_role=target' \
		-netdev tap,id=link,ifname="$TARGET_TAP",script=no,downscript=no \
		-device virtio-net-pci,netdev=link,mac=52:54:00:72:00:02 &
TARGET_JOB_PID=$!

for _ in $(seq 1 100); do
	[ -s "$TARGET_PIDFILE" ] && break
	sleep 0.1
done
[ -s "$TARGET_PIDFILE" ]
TARGET_PID=$(cat "$TARGET_PIDFILE")

echo "launching client VM"
/usr/bin/time -v -o "$OUTDIR/client-qemu-time.log" \
	taskset -c 8-15 \
	qemu-system-x86_64 \
		-name guest=zcwl-client,debug-threads=on \
		-machine q35,accel=kvm -cpu host \
		-m 2048M -smp 7,sockets=1,cores=7,threads=1 \
		-display none -monitor none -serial "file:$OUTDIR/client-console.log" \
		-no-reboot -nodefaults -pidfile "$CLIENT_PIDFILE" \
		-kernel "$KERNEL" -initrd "$INITRD" \
		-append 'console=ttyS0 panic=-1 oops=panic quiet zcworkload_role=client' \
		-netdev tap,id=link,ifname="$CLIENT_TAP",script=no,downscript=no \
		-device virtio-net-pci,netdev=link,mac=52:54:00:72:00:01 &
CLIENT_JOB_PID=$!

for _ in $(seq 1 100); do
	[ -s "$CLIENT_PIDFILE" ] && break
	sleep 0.1
done
[ -s "$CLIENT_PIDFILE" ]
CLIENT_PID=$(cat "$CLIENT_PIDFILE")

: > "$OUTDIR/host-thread-map.log"
pin_vm target "$TARGET_PID" 2 4 4 "$OUTDIR/host-thread-map.log"
pin_vm client "$CLIENT_PID" 15 8 7 "$OUTDIR/host-thread-map.log"
snapshot_threads target "$TARGET_PID" "$OUTDIR/host-thread-context-before.log"
snapshot_threads client "$CLIENT_PID" "$OUTDIR/host-thread-context-before.log"

client_exited=false
for _ in $(seq 1 3000); do
	if [ ! -e "/proc/$CLIENT_PID" ]; then
		client_exited=true
		break
	fi
	sleep 0.1
done
[ "$client_exited" = true ]

if [ -e "/proc/$TARGET_PID" ]; then
	snapshot_threads target "$TARGET_PID" "$OUTDIR/host-thread-context-after-client.log"
fi

target_exited=false
for _ in $(seq 1 600); do
	if [ ! -e "/proc/$TARGET_PID" ]; then
		target_exited=true
		break
	fi
	sleep 0.1
done
[ "$target_exited" = true ]

set +e
wait "$CLIENT_JOB_PID"
client_wait_status=$?
CLIENT_JOB_PID=
wait "$TARGET_JOB_PID"
target_wait_status=$?
TARGET_JOB_PID=
set -e

echo "qemu-wait-status: client=$client_wait_status target=$target_wait_status" | tee "$OUTDIR/qemu-wait-status.log"
grep -q 'ZCWORKLOAD_GUEST_FINAL role=client status=0' "$OUTDIR/client-console.log"
grep -q 'ZCWORKLOAD_CLIENT_VALIDATION PASS' "$OUTDIR/client-console.log"
grep -q 'ZCWORKLOAD_GUEST_FINAL role=target status=0' "$OUTDIR/target-console.log"
grep -q '^zcnblk-wal-leaf-summary:' "$OUTDIR/target-console.log"
[ "$client_wait_status" -eq 0 ]
[ "$target_wait_status" -eq 0 ]

echo "host-harness-result: PASS two_vm_kvm=true tap_bridge=true lanes=2 direct_userspace_memory_leaf=true frame_bytes=4096 representative=false"
