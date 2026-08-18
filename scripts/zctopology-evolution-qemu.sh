#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KREL="${KREL:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-${KREL}}"
QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
OUTDIR="${OUTDIR:-${ROOT}/bench-results/zctopology-evolution-qemu-$(date -u +%Y%m%dT%H%M%SZ)}"
ROOTFS="${OUTDIR}/rootfs"
INITRD="${OUTDIR}/zctopology-initramfs.cpio"
tag="$(printf '%04x' $(( $$ % 65536 )))"
BRIDGE="zte${tag}b"
roles=(node-a node-b node-c-cold node-b-repl node-c-hot controller)
taps=()
pidfiles=()
jobs=()
NETWORK_CREATED=0

log() { printf '[zctopology-qemu] %s\n' "$*"; }

copy_runtime_file() {
	local source_path="$1" dest_path="$2" dest="${ROOTFS}${2}"
	mkdir -p "$(dirname "$dest")"
	cp -L "$source_path" "$dest"
}

copy_binary() {
	local source_path="$1" dest_path="$2" library
	copy_runtime_file "$source_path" "$dest_path"
	while read -r library; do
		[ -n "$library" ] || continue
		copy_runtime_file "$library" "$library"
	done < <(ldd "$source_path" | awk '/=> \// {print $3; next} /^[[:space:]]*\/lib/ {print $1}')
}

copy_module() {
	local name="$1" source_path
	source_path="$(/usr/sbin/modinfo -k "$KREL" -n "$name")"
	case "$source_path" in
		*.xz) xz -dc "$source_path" > "$ROOTFS/modules/$name.ko" ;;
		*.zst) zstd -dc "$source_path" > "$ROOTFS/modules/$name.ko" ;;
		*.ko) cp "$source_path" "$ROOTFS/modules/$name.ko" ;;
		*) printf 'unsupported module path: %s\n' "$source_path" >&2; return 1 ;;
	esac
}

verified_stop_qemu() {
	local role="$1" pidfile="$2" pid comm cmdline
	[ -s "$pidfile" ] || return 0
	pid="$(cat "$pidfile")"
	case "$pid" in ''|*[!0-9]*) return 1 ;; esac
	[ -r "/proc/$pid/comm" ] || return 0
	comm="$(cat "/proc/$pid/comm")"
	cmdline="$(tr '\0' ' ' < "/proc/$pid/cmdline")"
	printf 'cleanup-inspect: role=%s pid=%s comm=%s cmdline=%s\n' "$role" "$pid" "$comm" "$cmdline"
	case "$comm:$cmdline" in
		qemu-system-x86*:*$INITRD*"zctopo_role=$role"*) ;;
		*) printf 'refusing to signal unverified pid=%s role=%s\n' "$pid" "$role" >&2; return 1 ;;
	esac
	kill -TERM "$pid"
	for _ in $(seq 1 50); do [ ! -e "/proc/$pid" ] && return 0; sleep 0.1; done
	[ ! -e "/proc/$pid" ] || kill -KILL "$pid"
}

cleanup() {
	local status=$? i
	trap - EXIT INT TERM
	set +e
	for i in "${!roles[@]}"; do verified_stop_qemu "${roles[$i]}" "${pidfiles[$i]:-}"; done
	for job in "${jobs[@]:-}"; do [ -z "$job" ] || wait "$job"; done
	if [ "$NETWORK_CREATED" -eq 1 ]; then
		for tap in "${taps[@]:-}"; do [ -z "$tap" ] || sudo -n ip link del "$tap" 2>/dev/null || true; done
		sudo -n ip link del "$BRIDGE" 2>/dev/null || true
	fi
	exit "$status"
}
trap cleanup EXIT INT TERM

[ -r "$KERNEL" ]
[ -c /dev/kvm ]
command -v "$QEMU_BIN" >/dev/null
sudo -n true
[ ! -e "$OUTDIR" ] || { echo "refusing existing OUTDIR=$OUTDIR" >&2; exit 1; }
mkdir -p "$ROOTFS"/{bin,usr/bin,proc,sys,dev,run,tmp,modules} "$OUTDIR"

log 'building topology emulator'
cargo build --release --manifest-path "$ROOT/Cargo.toml" --bin zctopology-emu
copy_binary /bin/busybox /bin/busybox
for applet in sh mount umount poweroff sync cat sleep seq ping mkdir insmod; do ln -s busybox "$ROOTFS/bin/$applet"; done
copy_binary /usr/bin/ip /usr/bin/ip
copy_binary "$ROOT/target/release/zctopology-emu" /zctopology-emu
for module in failover net_failover virtio_net; do copy_module "$module"; done
cp "$ROOT/scripts/zctopology-evolution-qemu-init.sh" "$ROOTFS/init"
chmod 0755 "$ROOTFS/init" "$ROOTFS/zctopology-emu"
(
	cd "$ROOTFS"
	find . -print0 | cpio --null -o --format=newc > "$INITRD" 2> "$OUTDIR/cpio.log"
)

for i in "${!roles[@]}"; do
	tap="zte${tag}${i}"
	[ ${#tap} -le 15 ]
	! ip link show dev "$tap" >/dev/null 2>&1
	taps+=("$tap")
	pidfiles+=("$OUTDIR/${roles[$i]}.pid")
done
! ip link show dev "$BRIDGE" >/dev/null 2>&1
sudo -n ip link add "$BRIDGE" type bridge
NETWORK_CREATED=1
sudo -n ip link set "$BRIDGE" type bridge stp_state 0
sudo -n ip link set "$BRIDGE" up
for tap in "${taps[@]}"; do
	sudo -n ip tuntap add dev "$tap" mode tap user "$(id -un)"
	sudo -n ip link set "$tap" master "$BRIDGE"
	sudo -n ip link set "$tap" up
done

{
	printf 'classification=correctness-only representative=false\n'
	printf 'controller-map=controller-vm:single-vcpu metadata-path=durable-change-log\n'
	printf 'data-map=userspace-stage-per-vm transport=tcp lanes=4 kernel-placement=none block-mirror=none\n'
	printf 'failure-domains=region-a,region-b,region-c injection=physical-tap-down supervisor-voters=node-c-cold,node-b-repl,node-c-hot\n'
	printf 'pitr-path=userspace-snapshot-plus-versioned-wal recovery-verification=sequence-plus-sha256\n'
	printf 'hugetlb=not-applicable memlock=not-applicable hctx-affinity=not-applicable benchmark_numbers=none\n'
} > "$OUTDIR/topology.log"

launch_vm() {
	local i="$1" role="${roles[$1]}" tap="${taps[$1]}" pidfile="${pidfiles[$1]}" mac
	mac="52:54:00:e7:${tag:0:2}:$(printf '%02x' $((i + 1)))"
	"$QEMU_BIN" -name "guest=zctopo-$role,debug-threads=on" \
		-machine q35,accel=kvm -cpu host -m 192M -smp 1 \
		-display none -monitor none -serial "file:$OUTDIR/$role-console.log" \
		-no-reboot -nodefaults -pidfile "$pidfile" \
		-kernel "$KERNEL" -initrd "$INITRD" \
		-append "console=ttyS0 panic=-1 oops=panic zctopo_role=$role" \
		-netdev "tap,id=link0,ifname=$tap,script=no,downscript=no" \
		-device "virtio-net-pci,netdev=link0,mac=$mac" &
	jobs+=("$!")
}

for i in 0 1 2 3 4; do launch_vm "$i"; done
for i in 0 1 2 3 4; do
	for _ in $(seq 1 200); do grep -q 'TOPOLOGY_NODE_READY' "$OUTDIR/${roles[$i]}-console.log" 2>/dev/null && break; sleep 0.05; done
	grep -q 'TOPOLOGY_NODE_READY' "$OUTDIR/${roles[$i]}-console.log"
done
launch_vm 5

wait_marker() {
	local marker="$1"
	for _ in $(seq 1 400); do
		grep -q "$marker" "$OUTDIR/controller-console.log" 2>/dev/null && return 0
		sleep 0.05
	done
	return 1
}

log 'injecting isolated supervisor quorum loss'
wait_marker 'EVOLUTION_REQUEST_SUPERVISOR_QUORUM_LOSS phase=isolated'
sudo -n ip link set "${taps[3]}" down
sudo -n ip link set "${taps[4]}" down
printf 'failure_injected=supervisor-quorum phase=isolated taps=%s,%s action=link-down\n' "${taps[3]}" "${taps[4]}" > "$OUTDIR/failure-injection.log"
wait_marker 'EVOLUTION_REQUEST_SUPERVISOR_QUORUM_RESTORE phase=isolated'
sudo -n ip link set "${taps[3]}" up
sudo -n ip link set "${taps[4]}" up
printf 'failure_restored=supervisor-quorum phase=isolated taps=%s,%s action=link-up\n' "${taps[3]}" "${taps[4]}" >> "$OUTDIR/failure-injection.log"

log 'injecting data-region failure'
wait_marker 'EVOLUTION_REQUEST_REGION_FAILURE'
sudo -n ip link set "${taps[0]}" down
printf 'failure_injected=data-region-a tap=%s action=link-down\n' "${taps[0]}" >> "$OUTDIR/failure-injection.log"

log 'injecting overlapping data and supervisor loss'
wait_marker 'EVOLUTION_REQUEST_OVERLAP_FAILURE'
sudo -n ip link set "${taps[1]}" down
sudo -n ip link set "${taps[3]}" down
sudo -n ip link set "${taps[4]}" down
printf 'failure_injected=overlap data_taps=%s,%s supervisor_taps=%s,%s action=link-down\n' "${taps[0]}" "${taps[1]}" "${taps[3]}" "${taps[4]}" >> "$OUTDIR/failure-injection.log"
wait_marker 'EVOLUTION_REQUEST_OVERLAP_RESTORE'
sudo -n ip link set "${taps[1]}" up
sudo -n ip link set "${taps[3]}" up
sudo -n ip link set "${taps[4]}" up
printf 'failure_restored=overlap data_tap=%s supervisor_taps=%s,%s action=link-up region-a-remains-down=true\n' "${taps[1]}" "${taps[3]}" "${taps[4]}" >> "$OUTDIR/failure-injection.log"

controller_pid="$(cat "${pidfiles[5]}")"
for _ in $(seq 1 1200); do [ ! -e "/proc/$controller_pid" ] && break; sleep 0.05; done
[ ! -e "/proc/$controller_pid" ]
grep -q 'ZCTOPOLOGY_EVOLUTION_PASS' "$OUTDIR/controller-console.log"
grep -q 'EVOLUTION_PHASE_PASS phase=pitr-metadata-replay' "$OUTDIR/controller-console.log"
grep -q 'EVOLUTION_PHASE_PASS phase=overlap-failure' "$OUTDIR/controller-console.log"
grep -E 'EVOLUTION_|ZCTOPOLOGY_' "$OUTDIR/controller-console.log" > "$OUTDIR/validation-summary.log"
printf 'ZCTOPOLOGY_QEMU_EVOLUTION_PASS vms=6 memory_per_vm=192MiB pitr=snapshot-plus-wal failures=supervisor,data,overlap physical_failure=tap-down policies=5 artifact=%s\n' "$OUTDIR" | tee "$OUTDIR/result.log"
