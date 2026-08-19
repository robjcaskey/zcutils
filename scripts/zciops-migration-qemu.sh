#!/usr/bin/env bash
set -Eeuo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KREL="${KREL:-$(uname -r)}"; KERNEL="${KERNEL:-/boot/vmlinuz-${KREL}}"; QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
OUTDIR="${OUTDIR:-$ROOT/bench-results/zciops-migration-qemu-$(date -u +%Y%m%dT%H%M%SZ)}"
ROOTFS="$OUTDIR/rootfs"; INITRD="$OUTDIR/initramfs.cpio"; tag="$(printf '%04x' $(( $$ % 65536 )))"; bridge="zim${tag}b"
roles=(fast slow controller); taps=(); pidfiles=(); jobs=(); network_created=0

copy_file() { mkdir -p "$ROOTFS$(dirname "$2")"; cp -L "$1" "$ROOTFS$2"; }
copy_binary() { copy_file "$1" "$2"; while read -r library; do [ -z "$library" ] || copy_file "$library" "$library"; done < <(ldd "$1" | awk '/=> \// {print $3; next} /^[[:space:]]*\/lib/ {print $1}'); }
copy_module() { local path; path="$(/usr/sbin/modinfo -k "$KREL" -n "$1")"; case "$path" in *.xz) xz -dc "$path" > "$ROOTFS/modules/$1.ko" ;; *.zst) zstd -dc "$path" > "$ROOTFS/modules/$1.ko" ;; *) cp "$path" "$ROOTFS/modules/$1.ko" ;; esac; }
stop_vm() {
	local role="$1" pidfile="$2" pid comm cmdline; [ -s "$pidfile" ] || return 0; pid="$(cat "$pidfile")"
	case "$pid" in ''|*[!0-9]*) return 1 ;; esac; [ -r "/proc/$pid/comm" ] || return 0
	comm="$(cat "/proc/$pid/comm")"; cmdline="$(tr '\0' ' ' < "/proc/$pid/cmdline")"
	case "$comm:$cmdline" in qemu-system-x86*:*$INITRD*"zciops_role=$role"*) ;; *) echo "refusing unverified pid=$pid" >&2; return 1 ;; esac
	kill -TERM "$pid"; for _ in $(seq 1 50); do [ ! -e "/proc/$pid" ] && return; sleep .1; done; [ ! -e "/proc/$pid" ] || kill -KILL "$pid"
}
cleanup() {
	local status=$? i; trap - EXIT INT TERM; set +e
	for i in "${!roles[@]}"; do stop_vm "${roles[$i]}" "${pidfiles[$i]:-}"; done
	for job in "${jobs[@]:-}"; do [ -z "$job" ] || wait "$job"; done
	if [ "$network_created" -eq 1 ]; then for tap in "${taps[@]:-}"; do sudo -n ip link del "$tap" 2>/dev/null || true; done; sudo -n ip link del "$bridge" 2>/dev/null || true; fi
	exit "$status"
}
trap cleanup EXIT INT TERM
[ -c /dev/kvm ]; [ -r "$KERNEL" ]; sudo -n true; [ ! -e "$OUTDIR" ] || { echo "OUTDIR exists" >&2; exit 1; }
mkdir -p "$ROOTFS"/{bin,usr/bin,proc,sys,dev,run,tmp,modules} "$OUTDIR"
cargo build --release --manifest-path "$ROOT/Cargo.toml" --bin zciops-migration-emu
copy_binary /bin/busybox /bin/busybox
for applet in sh mount cat poweroff sync mkdir insmod; do ln -s busybox "$ROOTFS/bin/$applet"; done
copy_binary /usr/bin/ip /usr/bin/ip; copy_binary "$ROOT/target/release/zciops-migration-emu" /zciops-migration-emu
for module in failover net_failover virtio_net virtio_blk; do copy_module "$module"; done
cp "$ROOT/scripts/zciops-migration-qemu-init.sh" "$ROOTFS/init"; chmod 0755 "$ROOTFS/init" "$ROOTFS/zciops-migration-emu"
(cd "$ROOTFS" && find . -print0 | cpio --null -o --format=newc > "$INITRD" 2> "$OUTDIR/cpio.log")
truncate -s 64M "$OUTDIR/fast.raw"; truncate -s 64M "$OUTDIR/slow.raw"
sudo -n ip link add "$bridge" type bridge; network_created=1; sudo -n ip link set "$bridge" up
for i in "${!roles[@]}"; do tap="zim${tag}${i}"; taps+=("$tap"); pidfiles+=("$OUTDIR/${roles[$i]}.pid"); sudo -n ip tuntap add dev "$tap" mode tap user "$(id -un)"; sudo -n ip link set "$tap" master "$bridge"; sudo -n ip link set "$tap" up; done
cat > "$OUTDIR/topology.log" <<EOF
classification=empirical-qemu representative=false shared_host=true
route=controller-userspace->tcp->userspace-leaf->terminal-virtio-block block_placement=userspace kernel_placement=none
fast_terminal_qemu_iops=20000 slow_terminal_qemu_iops=4000 foreground_provisioned_iops=1500 foreground_burst_iops=3000 snapshot_iops=2000
lane_worker_cpu_map=lane0:controller-vcpu0,fast-vcpu0,slow-vcpu0 aggregate_qd=batch32 raw_transport_rtt=not-separately-measured
hugetlb=not-applicable memlock=not-applicable hctx_affinity=virtio-single-queue batching=32
EOF
launch() {
	local i="$1" role="${roles[$1]}" smp=1; local -a extra=()
	if [ "$role" = fast ]; then smp=2; extra=(-drive "file=$OUTDIR/fast.raw,if=none,id=terminal,format=raw,cache=none,aio=native,throttling.iops-total=20000" -device virtio-blk-pci,drive=terminal); fi
	if [ "$role" = slow ]; then extra=(-drive "file=$OUTDIR/slow.raw,if=none,id=terminal,format=raw,cache=none,aio=native,throttling.iops-total=4000" -device virtio-blk-pci,drive=terminal); fi
	"$QEMU_BIN" -name "guest=zciops-$role,debug-threads=on" -machine q35,accel=kvm -cpu host -m 192M -smp "$smp" -display none -monitor none -serial "file:$OUTDIR/$role-console.log" -no-reboot -nodefaults -pidfile "${pidfiles[$i]}" -kernel "$KERNEL" -initrd "$INITRD" -append "console=ttyS0 panic=-1 oops=panic zciops_role=$role" -netdev "tap,id=link0,ifname=${taps[$i]},script=no,downscript=no" -device "virtio-net-pci,netdev=link0,mac=52:54:00:f1:${tag:0:2}:$(printf '%02x' $((i+1)))" "${extra[@]}" & jobs+=("$!")
}
launch 0; launch 1
for role in fast slow; do for _ in $(seq 1 300); do grep -q IOPS_LEAF_READY "$OUTDIR/$role-console.log" 2>/dev/null && break; sleep .05; done; grep -q IOPS_LEAF_READY "$OUTDIR/$role-console.log"; done
launch 2
for _ in $(seq 1 100); do [ -s "${pidfiles[2]}" ] && break; sleep .02; done
controller_pid="$(cat "${pidfiles[2]}")"; for _ in $(seq 1 2400); do [ ! -e "/proc/$controller_pid" ] && break; sleep .05; done
[ ! -e "/proc/$controller_pid" ]; grep -q IOPS_MIGRATION_SCENARIO_PASS "$OUTDIR/controller-console.log"
grep -E 'classification=|metric |migration_|snapshot_|IOPS_MIGRATION' "$OUTDIR/controller-console.log" > "$OUTDIR/results.log"
awk '
/metric phase=physical_fast / { split($4,a,"="); fast+=a[2]; fastn++ }
/metric phase=physical_slow / { split($4,a,"="); slow+=a[2]; slown++ }
/metric phase=policy_slow_burst_3000 / { split($4,a,"="); burst+=a[2]; burstn++ }
/metric phase=snapshot_slow_floor_1500 / { split($4,a,"="); snap+=a[2]; snapn++; if (a[2] < 1500) below++ }
/metric phase=post_snapshot_slow_recovery / { split($4,a,"="); recovery+=a[2]; recoveryn++ }
END {
	if (!fastn || !slown || !burstn || !snapn || !recoveryn) exit 10
	fast/=fastn; slow/=slown; burst/=burstn; snap/=snapn; recovery/=recoveryn
	printf "physical_fast_mean_iops=%.1f physical_slow_mean_iops=%.1f snapshot_foreground_mean_iops=%.1f burst_mean_iops=%.1f recovery_mean_iops=%.1f snapshot_intervals_below_provision=%d\n", fast, slow, snap, burst, recovery, below
	if (fast <= slow*1.30 || snap >= burst*0.90 || recovery <= snap*1.20 || below != 0) exit 11
}' "$OUTDIR/results.log" | tee "$OUTDIR/validation.log"
printf 'ZCIOPS_MIGRATION_QEMU_PASS artifact=%s\n' "$OUTDIR" | tee "$OUTDIR/result.log"
