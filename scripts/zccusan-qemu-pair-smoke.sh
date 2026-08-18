#!/usr/bin/env bash

set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KREL="${KREL:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-${KREL}}"
MODULE_ROOT="${MODULE_ROOT:-/lib/modules/${KREL}}"
PHASES="${PHASES:-softroce zcnet}"
SOCKETSRMA_STRESS_OPS="${SOCKETSRMA_STRESS_OPS:-10000}"
SOCKETSRMA_STRESS_TIMEOUT="${SOCKETSRMA_STRESS_TIMEOUT:-90}"
QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
OUTDIR="${OUTDIR:-${ROOT}/bench-results/zccusan-qemu-pair-$(date -u +%Y%m%dT%H%M%SZ)}"
ROOTFS="${OUTDIR}/rootfs"
INITRD="${OUTDIR}/zccusan-pair-initramfs.cpio"

ACTIVE_PHASE=""
BRIDGE=""
TARGET_TAP=""
CLIENT_TAP=""
TARGET_PIDFILE=""
CLIENT_PIDFILE=""
TARGET_JOB_PID=""
CLIENT_JOB_PID=""
NETWORK_CREATED=0

case "$SOCKETSRMA_STRESS_OPS:$SOCKETSRMA_STRESS_TIMEOUT" in
	*[!0-9:]*|:*|*:|0:*|*:0)
		printf 'SOCKETSRMA_STRESS_OPS and SOCKETSRMA_STRESS_TIMEOUT must be positive integers\n' >&2
		exit 2
		;;
esac

log()
{
	printf '[zccusan-qemu] %s\n' "$*"
}

copy_runtime_file()
{
	source_path="$1"
	dest_path="$2"
	dest="${ROOTFS}${dest_path}"
	mkdir -p "$(dirname "$dest")"
	cp -L "$source_path" "$dest"
}

copy_ldd_dependencies()
{
	source_path="$1"
	while read -r library; do
		[ -n "$library" ] || continue
		copy_runtime_file "$library" "$library"
	done < <(
		ldd "$source_path" |
			awk '
				/=> not found/ { print "MISSING:" $1; next }
				/=> \// { print $3; next }
				/^[[:space:]]*\/lib/ { print $1; next }
			'
	)
	if find "$ROOTFS" -name 'MISSING:*' -print -quit | grep -q .; then
		printf 'missing runtime dependency for %s\n' "$source_path" >&2
		return 1
	fi
}

copy_binary()
{
	source_path="$1"
	dest_path="$2"
	[ -x "$source_path" ] || {
		printf 'missing executable: %s\n' "$source_path" >&2
		return 1
	}
	copy_runtime_file "$source_path" "$dest_path"
	copy_ldd_dependencies "$source_path"
}

copy_module()
{
	name="$1"
	source_path="$(/usr/sbin/modinfo -k "$KREL" -n "$name")"
	vermagic="$(/usr/sbin/modinfo -F vermagic "$source_path" | awk '{print $1}')"
	[ "$vermagic" = "$KREL" ] || {
		printf 'module vermagic mismatch: name=%s expected=%s actual=%s path=%s\n' \
			"$name" "$KREL" "$vermagic" "$source_path" >&2
		return 1
	}
	case "$source_path" in
		*.xz)
			xz -dc "$source_path" > "${ROOTFS}/modules/${name}.ko"
			;;
		*.zst)
			zstd -dc "$source_path" > "${ROOTFS}/modules/${name}.ko"
			;;
		*.ko)
			cp "$source_path" "${ROOTFS}/modules/${name}.ko"
			;;
		*)
			printf 'unsupported module compression: %s\n' "$source_path" >&2
			return 1
			;;
	esac
	printf 'guest-module: name=%s source=%s vermagic=%s\n' \
		"$name" "$source_path" "$vermagic" >> "${OUTDIR}/module-manifest.log"
}

build_guest_artifacts()
{
	log "building current zcutils binaries"
	cargo build --release --manifest-path "${ROOT}/Cargo.toml" \
		--bin zcutils --bin zcnblk-shm-target --bin zcnblk-order-smoke --bin zcblockbench

	mkdir -p \
		"${ROOTFS}/bin" \
		"${ROOTFS}/usr/bin" \
		"${ROOTFS}/usr/lib/x86_64-linux-gnu/libibverbs" \
		"${ROOTFS}/etc/libibverbs.d" \
		"${ROOTFS}/modules" \
		"${ROOTFS}/proc" \
		"${ROOTFS}/sys/kernel/debug" \
		"${ROOTFS}/dev" \
		"${ROOTFS}/run" \
		"${ROOTFS}/tmp" \
		"${ROOTFS}/var"

	copy_binary /bin/busybox /bin/busybox
	for applet in sh mount umount insmod rmmod sleep poweroff sync cat grep awk sed \
		ping dd cmp tr kill head tail tee uname mkdir ln date seq timeout nc find \
		basename dirname cut sort wc env true false; do
		ln -s busybox "${ROOTFS}/bin/${applet}"
	done

	copy_binary /usr/bin/ip /usr/bin/ip
	copy_binary /usr/bin/rdma /usr/bin/rdma
	copy_binary /usr/bin/ibv_rc_pingpong /usr/bin/ibv_rc_pingpong
	copy_binary /usr/bin/ibv_devinfo /usr/bin/ibv_devinfo
	copy_binary /usr/bin/fi_info /usr/bin/fi_info
	copy_binary /usr/sbin/ethtool /usr/bin/ethtool
	copy_binary "${ROOT}/target/release/zcutils" /uring-play
	copy_binary "${ROOT}/target/release/zcnblk-shm-target" /zcnblk-shm-target
	copy_binary "${ROOT}/target/release/zcnblk-order-smoke" /zcnblk-order-smoke
	copy_binary "${ROOT}/target/release/zcblockbench" /zcblockbench

	rxe_provider=/usr/lib/x86_64-linux-gnu/libibverbs/librxe-rdmav34.so
	[ -r "$rxe_provider" ]
	copy_runtime_file "$rxe_provider" /usr/lib/x86_64-linux-gnu/libibverbs/librxe-rdmav34.so
	copy_ldd_dependencies "$rxe_provider"
	copy_runtime_file /etc/libibverbs.d/rxe.driver /etc/libibverbs.d/rxe.driver

	: > "${OUTDIR}/module-manifest.log"
	for module in \
		failover net_failover virtio_net \
		aead \
		configfs ib_core ib_uverbs_support ib_uverbs ib_cm iw_cm rdma_cm rdma_ucm \
		udp_tunnel ip6_udp_tunnel rdma_rxe \
		psample llc stp bridge netdevsim; do
		copy_module "$module"
	done

	zcnblk_module="${ROOT}/kmods/zcnblk_client_mod.ko"
	zcnblk_vermagic="$(/usr/sbin/modinfo -F vermagic "$zcnblk_module" | awk '{print $1}')"
	[ "$zcnblk_vermagic" = "$KREL" ] || {
		printf 'zcnblk client vermagic mismatch: expected=%s actual=%s path=%s\n' \
			"$KREL" "$zcnblk_vermagic" "$zcnblk_module" >&2
		return 1
	}
	cp "$zcnblk_module" "${ROOTFS}/modules/zcnblk_client_mod.ko"
	printf 'guest-module: name=zcnblk_client_mod source=%s vermagic=%s\n' \
		"$zcnblk_module" "$zcnblk_vermagic" >> "${OUTDIR}/module-manifest.log"

	cp "${ROOT}/scripts/zccusan-qemu-pair-init.sh" "${ROOTFS}/init"
	chmod 0755 "${ROOTFS}/init" "${ROOTFS}/uring-play" \
		"${ROOTFS}/zcnblk-shm-target" "${ROOTFS}/zcnblk-order-smoke" \
		"${ROOTFS}/zcblockbench"
	ln -s /run "${ROOTFS}/var/run"

	(
		cd "$ROOTFS"
		find . -print0 | cpio --null -o --format=newc > "$INITRD" 2> "${OUTDIR}/cpio.log"
	)
	stat -c 'initramfs_bytes=%s' "$INITRD" | tee "${OUTDIR}/artifact-size.log"
	sha256sum "$KERNEL" "$INITRD" \
		"${ROOTFS}/modules/zcnblk_client_mod.ko" \
		"${ROOTFS}/modules/rdma_rxe.ko" \
		"${ROOTFS}/modules/netdevsim.ko" > "${OUTDIR}/artifact-sha256.txt"
}

verified_stop_qemu()
{
	label="$1"
	pidfile="$2"
	[ -s "$pidfile" ] || return 0
	pid="$(cat "$pidfile")"
	case "$pid" in
		''|*[!0-9]*)
			printf 'cleanup: refusing invalid %s pid=%s\n' "$label" "$pid"
			return 1
			;;
	esac
	[ -r "/proc/$pid/comm" ] || return 0
	comm="$(cat "/proc/$pid/comm")"
	cmdline="$(tr '\0' ' ' < "/proc/$pid/cmdline")"
	printf 'cleanup-inspect: label=%s pid=%s comm=%s cmdline=%s\n' \
		"$label" "$pid" "$comm" "$cmdline"
	case "$comm:$cmdline" in
		qemu-system-x86*:*$INITRD*"zccusan_phase=$ACTIVE_PHASE"*) ;;
		*)
			printf 'cleanup: refusing to signal unverified %s pid=%s\n' "$label" "$pid"
			return 1
			;;
	esac
	kill -TERM "$pid"
	for _ in $(seq 1 100); do
		[ ! -e "/proc/$pid" ] && return 0
		sleep 0.1
	done
	if [ -e "/proc/$pid" ]; then
		printf 'cleanup: escalating verified %s pid=%s to KILL\n' "$label" "$pid"
		kill -KILL "$pid"
	fi
}

cleanup_phase()
{
	set +e
	verified_stop_qemu client "$CLIENT_PIDFILE"
	verified_stop_qemu target "$TARGET_PIDFILE"
	[ -z "$CLIENT_JOB_PID" ] || wait "$CLIENT_JOB_PID"
	[ -z "$TARGET_JOB_PID" ] || wait "$TARGET_JOB_PID"
	CLIENT_JOB_PID=""
	TARGET_JOB_PID=""
	if [ "$NETWORK_CREATED" -eq 1 ]; then
		sudo -n ip link del "$CLIENT_TAP" 2>/dev/null || true
		sudo -n ip link del "$TARGET_TAP" 2>/dev/null || true
		sudo -n ip link del "$BRIDGE" 2>/dev/null || true
	fi
	NETWORK_CREATED=0
	BRIDGE=""
	TARGET_TAP=""
	CLIENT_TAP=""
	TARGET_PIDFILE=""
	CLIENT_PIDFILE=""
	ACTIVE_PHASE=""
	set -e
}

cleanup_all()
{
	status=$?
	trap - EXIT INT TERM
	cleanup_phase
	exit "$status"
}

wait_for_pidfile()
{
	pidfile="$1"
	for _ in $(seq 1 100); do
		[ -s "$pidfile" ] && return 0
		sleep 0.1
	done
	return 1
}

pin_vm()
{
	label="$1"
	pid="$2"
	emulator_cpu="$3"
	first_vcpu_cpu="$4"
	map_log="$5"
	taskset -apc "$emulator_cpu" "$pid" >> "$map_log" 2>&1
	found=0
	for _ in $(seq 1 100); do
		found=0
		for task in /proc/"$pid"/task/[0-9]*; do
			[ -r "$task/comm" ] || continue
			tid="${task##*/}"
			comm="$(cat "$task/comm")"
			case "$comm" in
				CPU\ [01]/KVM)
					vcpu="${comm#CPU }"
					vcpu="${vcpu%/KVM}"
					host_cpu=$((first_vcpu_cpu + vcpu))
					taskset -pc "$host_cpu" "$tid" >> "$map_log" 2>&1
					printf 'vcpu-map: vm=%s guest_vcpu=%s host_cpu=%s tid=%s\n' \
						"$label" "$vcpu" "$host_cpu" "$tid" >> "$map_log"
					found=$((found + 1))
					;;
			esac
		done
		[ "$found" -eq 2 ] && break
		sleep 0.1
	done
	[ "$found" -eq 2 ]
	printf 'emulator-map: vm=%s host_cpu=%s pid=%s\n' \
		"$label" "$emulator_cpu" "$pid" >> "$map_log"
}

wait_for_vm_exit()
{
	pid="$1"
	for _ in $(seq 1 2400); do
		[ ! -e "/proc/$pid" ] && return 0
		sleep 0.1
	done
	return 1
}

run_phase()
{
	phase="$1"
	case "$phase" in softroce|socketsrma|zcnet) ;; *) printf 'invalid phase: %s\n' "$phase" >&2; return 1 ;; esac
	ACTIVE_PHASE="$phase"
	phase_dir="${OUTDIR}/${phase}"
	mkdir "$phase_dir"
	tag="$(printf '%04x' $(( $$ % 65536 )))"
	case "$phase" in
		softroce) suffix=s; mac_phase=82 ;;
		socketsrma) suffix=r; mac_phase=84 ;;
		zcnet) suffix=z; mac_phase=83 ;;
	esac
	BRIDGE="zq${tag}${suffix}b"
	TARGET_TAP="zq${tag}${suffix}t"
	CLIENT_TAP="zq${tag}${suffix}c"
	TARGET_PIDFILE="${phase_dir}/target-qemu.pid"
	CLIENT_PIDFILE="${phase_dir}/client-qemu.pid"

	for link in "$BRIDGE" "$TARGET_TAP" "$CLIENT_TAP"; do
		if ip link show dev "$link" >/dev/null 2>&1; then
			printf 'refusing to reuse existing host link: %s\n' "$link" >&2
			return 1
		fi
	done
	sudo -n ip link add "$BRIDGE" type bridge
	NETWORK_CREATED=1
	sudo -n ip link set "$BRIDGE" type bridge stp_state 0
	sudo -n ip tuntap add dev "$TARGET_TAP" mode tap user "$(id -un)"
	sudo -n ip tuntap add dev "$CLIENT_TAP" mode tap user "$(id -un)"
	sudo -n ip link set "$TARGET_TAP" master "$BRIDGE"
	sudo -n ip link set "$CLIENT_TAP" master "$BRIDGE"
	for link in "$BRIDGE" "$TARGET_TAP" "$CLIENT_TAP"; do
		sudo -n ip link set "$link" mtu 1500 up
	done
	{
		printf 'classification=correctness-only representative=false phase=%s kernel=%s lanes=1\n' "$phase" "$KREL"
		printf 'target-map=guest-vcpu0:host-cpu3,guest-vcpu1:host-cpu4,emulator:host-cpu2\n'
		printf 'client-map=guest-vcpu0:host-cpu6,guest-vcpu1:host-cpu7,emulator:host-cpu5\n'
		case "$phase" in
			softroce) transport_map=eth0-rxe0-ofi-verbs ;;
			socketsrma) transport_map=eth0-ofi-sockets-rma ;;
			*) transport_map=zcnode/zcnet0-netdevsim-zcsw0-bridge-eth0-tcp ;;
		esac
		printf 'lane0-map=client:/dev/zcnblk0:kernel-kthread-unpinned->userspace-onramp:guest-cpu1->transport:%s->target-leaf:guest-cpu1\n' \
			"$transport_map"
		printf 'virtio-irq-affinity=guest-default hctx-affinity=single-queue-default memlock=guest-init hugetlb=not-configured benchmark_numbers=non-representative\n'
		if [ "$phase" = socketsrma ]; then
			printf 'latency-shape=per-worker-qd8 workers=1 lanes=1 aggregate-outstanding-depth=8 raw-transport-rtt=not-measured theoretical-iops-ceiling=not-computed actual-theoretical-efficiency=not-reported reason=correctness-only-qemu\n'
		fi
		for link in "$BRIDGE" "$TARGET_TAP" "$CLIENT_TAP"; do
			ip -details link show dev "$link"
		done
	} > "${phase_dir}/topology.log"

	log "launching phase=${phase} target VM"
	/usr/bin/time -v -o "${phase_dir}/target-qemu-time.log" \
		taskset -c 2-4 "$QEMU_BIN" \
			-name "guest=zccusan-${phase}-target,debug-threads=on" \
			-machine q35,accel=kvm -cpu host \
			-m 2048M -smp 2,sockets=1,cores=2,threads=1 \
			-display none -monitor none -serial "file:${phase_dir}/target-console.log" \
			-no-reboot -nodefaults -pidfile "$TARGET_PIDFILE" \
			-kernel "$KERNEL" -initrd "$INITRD" \
			-append "console=ttyS0 panic=-1 oops=panic zccusan_phase=${phase} zccusan_role=target zccusan_stress_ops=${SOCKETSRMA_STRESS_OPS} zccusan_stress_timeout=${SOCKETSRMA_STRESS_TIMEOUT}" \
			-netdev "tap,id=link0,ifname=${TARGET_TAP},script=no,downscript=no" \
			-device "virtio-net-pci,netdev=link0,mac=52:54:00:90:${mac_phase}:02" &
	TARGET_JOB_PID=$!
	wait_for_pidfile "$TARGET_PIDFILE"
	target_pid="$(cat "$TARGET_PIDFILE")"

	log "launching phase=${phase} client VM"
	/usr/bin/time -v -o "${phase_dir}/client-qemu-time.log" \
		taskset -c 5-7 "$QEMU_BIN" \
			-name "guest=zccusan-${phase}-client,debug-threads=on" \
			-machine q35,accel=kvm -cpu host \
			-m 2048M -smp 2,sockets=1,cores=2,threads=1 \
			-display none -monitor none -serial "file:${phase_dir}/client-console.log" \
			-no-reboot -nodefaults -pidfile "$CLIENT_PIDFILE" \
			-kernel "$KERNEL" -initrd "$INITRD" \
			-append "console=ttyS0 panic=-1 oops=panic zccusan_phase=${phase} zccusan_role=client zccusan_stress_ops=${SOCKETSRMA_STRESS_OPS} zccusan_stress_timeout=${SOCKETSRMA_STRESS_TIMEOUT}" \
			-netdev "tap,id=link0,ifname=${CLIENT_TAP},script=no,downscript=no" \
			-device "virtio-net-pci,netdev=link0,mac=52:54:00:90:${mac_phase}:01" &
	CLIENT_JOB_PID=$!
	wait_for_pidfile "$CLIENT_PIDFILE"
	client_pid="$(cat "$CLIENT_PIDFILE")"

	: > "${phase_dir}/host-thread-map.log"
	pin_vm target "$target_pid" 2 3 "${phase_dir}/host-thread-map.log"
	pin_vm client "$client_pid" 5 6 "${phase_dir}/host-thread-map.log"

	wait_for_vm_exit "$client_pid"
	wait_for_vm_exit "$target_pid"
	set +e
	wait "$CLIENT_JOB_PID"
	client_status=$?
	CLIENT_JOB_PID=""
	wait "$TARGET_JOB_PID"
	target_status=$?
	TARGET_JOB_PID=""
	set -e
	printf 'qemu-wait-status: phase=%s client=%s target=%s\n' \
		"$phase" "$client_status" "$target_status" | tee "${phase_dir}/qemu-wait-status.log"
	[ "$client_status" -eq 0 ]
	[ "$target_status" -eq 0 ]
	grep -q "ZCCUSAN_PHASE_PASS phase=${phase} role=client kernel=${KREL}" \
		"${phase_dir}/client-console.log"
	grep -q "ZCCUSAN_PHASE_PASS phase=${phase} role=target kernel=${KREL}" \
		"${phase_dir}/target-console.log"
	grep -q "ZCCUSAN_BLOCK_PATH_PASS phase=${phase}" "${phase_dir}/client-console.log"
	grep -q "ZCCUSAN_LEAF_PASS phase=${phase}" "${phase_dir}/target-console.log"
	if [ "$phase" = softroce ]; then
		grep -q 'SOFTROCE_VERBS_RC_PASS role=client' "${phase_dir}/client-console.log"
		grep -q 'SOFTROCE_VERBS_RC_PASS role=target' "${phase_dir}/target-console.log"
	elif [ "$phase" = zcnet ]; then
		grep -q 'ZCRX_STANDALONE_SEND_PASS' "${phase_dir}/client-console.log"
		grep -q 'ZCRX_STANDALONE_PASS' "${phase_dir}/target-console.log"
	fi
	grep -E 'SOFTROCE_|ZCRX_|ZCNET_|ZCCUSAN_|zcnblk-(shm-target|wal-leaf).*summary:' \
		"${phase_dir}/target-console.log" "${phase_dir}/client-console.log" \
		> "${phase_dir}/validation-summary.log"
	log "phase=${phase} PASS"
	cleanup_phase
}

trap cleanup_all EXIT INT TERM

[ "$KREL" = "$(uname -r)" ]
[ -r "$KERNEL" ]
[ -d "$MODULE_ROOT" ]
[ -c /dev/kvm ]
command -v "$QEMU_BIN" >/dev/null
command -v cargo >/dev/null
command -v cpio >/dev/null
command -v sudo >/dev/null
sudo -n true
[ ! -e "$OUTDIR" ] || {
	printf 'refusing to overwrite existing output directory: %s\n' "$OUTDIR" >&2
	exit 1
}
mkdir -p "$(dirname "$OUTDIR")"
mkdir "$OUTDIR"

{
	printf 'host_kernel=%s\n' "$(uname -r)"
	printf 'guest_kernel=%s\n' "$KREL"
	printf 'guest_kernel_image=%s\n' "$KERNEL"
	printf 'phases=%s\n' "$PHASES"
	printf 'classification=correctness-only\nrepresentative=false\n'
	git -C "$ROOT" status --short
} > "${OUTDIR}/run-manifest.log"

build_guest_artifacts
for phase in $PHASES; do
	run_phase "$phase"
done

cat "${OUTDIR}"/*/validation-summary.log > "${OUTDIR}/validation-summary.log"
printf 'ZCCUSAN_QEMU_PAIR_PASS kernel=%s phases=%s pair_count=2-per-phase softroce=verbs-rxe-rxd-message socketsrma=ofi-sockets-rma-physical-block-q8-stress zcnet=netdevsim-zcrx-plus-storage-tcp representative=false artifact=%s\n' \
	"$KREL" "$(printf '%s' "$PHASES" | tr ' ' ',')" "$OUTDIR" | tee "${OUTDIR}/result.log"
