#!/usr/bin/env bash
set -euo pipefail

KIND="${ZCOFI_RDMA_PREFLIGHT_KIND:-mlx5}"
RDMA_DEVICE="${ZCOFI_RDMA_DEVICE:-}"
NETDEV="${ZCOFI_RDMA_NETDEV:-}"
PROVIDER="${ZCOFI_RMA_MATRIX_PROVIDER:-}"
DOMAIN="${URING_PLAY_OFI_DOMAIN:-}"
OWNER_CPUS="${URING_PLAY_PIN_CPU_LIST:-}"
LANES="${ZCOFI_RDMA_LANES:-}"
RDMA_PORT="${ZCOFI_RDMA_PORT:-1}"
IRQ_CPUS="${ZCOFI_RDMA_IRQ_CPU_LIST:-}"
REGISTERED_BYTES="${ZCOFI_RDMA_REGISTERED_BYTES:-}"
BLOCK_MODE="${ZCOFI_RDMA_BLOCK_MODE:-0}"
HCTX_CPUS="${ZCOFI_RDMA_HCTX_CPU_LIST:-}"
BLOCK_ENGINE="${ZCOFI_RDMA_BLOCK_ENGINE:-${BLOCK_ENGINE:-}}"
BLOCK_COMPLETION_BATCH="${URING_PLAY_BLOCKBENCH_COMPLETION_BATCH:-}"
BLOCK_WAIT_MIN_COMPLETIONS="${URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS:-}"
BLOCK_CQE_SPIN="${URING_PLAY_BLOCKBENCH_CQE_SPIN:-}"
BLOCK_CQE_ADAPTIVE_SPIN="${URING_PLAY_BLOCKBENCH_CQE_ADAPTIVE_SPIN:-}"
BLOCK_CQE_HOT_POLL="${URING_PLAY_BLOCKBENCH_CQE_HOT_POLL:-}"
STRICT="${URING_PLAY_TOPOLOGY_STRICT:-0}"
FATAL="${URING_PLAY_TOPOLOGY_FATAL:-0}"
CQE_SIZE="${ZCOFI_RDMA_CQE_SIZE:-}"
CQE_COMPRESSION="${ZCOFI_RDMA_CQE_COMPRESSION:-}"
CQ_MODERATION_PERIOD="${ZCOFI_RDMA_CQ_MODERATION_PERIOD:-}"
CQ_MODERATION_COUNT="${ZCOFI_RDMA_CQ_MODERATION_COUNT:-}"
CQ_CONFIGURATION_VERIFIED="${ZCOFI_RDMA_CQ_CONFIGURATION_VERIFIED:-0}"
IRQ_AFFINITY_CONFIRMED="${ZCOFI_RDMA_IRQ_AFFINITY_CONFIRMED:-0}"
IRQ_COLOCATION_CONFIRMED="${ZCOFI_RDMA_IRQ_COLOCATION_CONFIRMED:-0}"
VERBS_DEVICE="${FI_VERBS_DEVICE_NAME:-}"
VERBS_IFACE="${FI_VERBS_IFACE:-}"
VERBS_GID_INDEX="${FI_VERBS_GID_IDX:-}"
RDMA_DEVICE_DECLARED="$([ -n "$RDMA_DEVICE" ] && printf 1 || printf 0)"
NETDEV_DECLARED="$([ -n "$NETDEV" ] && printf 1 || printf 0)"
PROVIDER_DECLARED="$([ -n "$PROVIDER" ] && printf 1 || printf 0)"
problems=0

die() {
	printf 'zcofi-rdma-topology-preflight: %s\n' "$*" >&2
	exit 1
}

issue() {
	printf 'PERF WARNING: %s\n' "$*" >&2
	problems=$((problems + 1))
}

validate_cpu_csv() {
	local list="$1"
	local label="$2"
	local require_allowed="$3"
	local cpu online
	local -a cpus
	local -A seen=()
	[ -n "$list" ] || return 0
	[[ "$list" =~ ^[0-9]+(,[0-9]+)*$ ]] || die "$label must be a comma-separated CPU list"
	IFS=',' read -r -a cpus <<<"$list"
	for cpu in "${cpus[@]}"; do
		if [ -n "${seen[$cpu]:-}" ]; then
			issue "$label repeats CPU $cpu; every owner/vector mapping must be explicit"
			continue
		fi
		seen[$cpu]=1
		if [ ! -d "/sys/devices/system/cpu/cpu$cpu" ]; then
			issue "$label names nonexistent CPU $cpu"
			continue
		fi
		online=1
		if [ -r "/sys/devices/system/cpu/cpu$cpu/online" ]; then
			online="$(<"/sys/devices/system/cpu/cpu$cpu/online")"
		fi
		[ "$online" = 1 ] || issue "$label names offline CPU $cpu"
		if [ "$require_allowed" = 1 ] && ! taskset -c "$cpu" true >/dev/null 2>&1; then
			issue "$label CPU $cpu is outside the current process affinity/cpuset"
		fi
	done
}

command -v rdma >/dev/null 2>&1 || die "rdma-core's rdma command is required"
[[ "$KIND" =~ ^(mlx5|rxe)$ ]] || die "ZCOFI_RDMA_PREFLIGHT_KIND must be mlx5 or rxe"
[[ "$STRICT" =~ ^[01]$ ]] || die "URING_PLAY_TOPOLOGY_STRICT must be zero or one"
[[ "$FATAL" =~ ^[01]$ ]] || die "URING_PLAY_TOPOLOGY_FATAL must be zero or one"
[[ "$BLOCK_MODE" =~ ^[01]$ ]] || die "ZCOFI_RDMA_BLOCK_MODE must be zero or one"
[[ "$RDMA_PORT" =~ ^[1-9][0-9]*$ ]] || die "ZCOFI_RDMA_PORT must be a positive port number"
[[ "$CQ_CONFIGURATION_VERIFIED" =~ ^[01]$ ]] || die "ZCOFI_RDMA_CQ_CONFIGURATION_VERIFIED must be zero or one"
[[ "$IRQ_AFFINITY_CONFIRMED" =~ ^[01]$ ]] || die "ZCOFI_RDMA_IRQ_AFFINITY_CONFIRMED must be zero or one"
[[ "$IRQ_COLOCATION_CONFIRMED" =~ ^[01]$ ]] || die "ZCOFI_RDMA_IRQ_COLOCATION_CONFIRMED must be zero or one"

if [ -z "$RDMA_DEVICE" ]; then
	mapfile -t rdma_devices < <(find /sys/class/infiniband -mindepth 1 -maxdepth 1 -printf '%f\n' 2>/dev/null | sort)
	if [ "${#rdma_devices[@]}" -eq 1 ]; then
		RDMA_DEVICE="${rdma_devices[0]}"
	else
		issue "set ZCOFI_RDMA_DEVICE explicitly; discovered ${#rdma_devices[@]} RDMA devices"
	fi
fi

if [ -n "$RDMA_DEVICE" ] && [ ! -d "/sys/class/infiniband/$RDMA_DEVICE" ]; then
	issue "RDMA device $RDMA_DEVICE does not exist"
fi

if [ -z "$NETDEV" ] && [ -n "$RDMA_DEVICE" ] && [ -d "/sys/class/infiniband/$RDMA_DEVICE/device/net" ]; then
	mapfile -t net_devices < <(find "/sys/class/infiniband/$RDMA_DEVICE/device/net" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort)
	if [ "${#net_devices[@]}" -eq 1 ]; then
		NETDEV="${net_devices[0]}"
	else
		issue "set ZCOFI_RDMA_NETDEV explicitly; RDMA device $RDMA_DEVICE maps to ${#net_devices[@]} netdevs"
	fi
fi

if [ "$STRICT" = 1 ] || [ "$FATAL" = 1 ]; then
	[ "$RDMA_DEVICE_DECLARED" = 1 ] || issue "strict/fatal runs require explicit ZCOFI_RDMA_DEVICE"
	[ "$NETDEV_DECLARED" = 1 ] || issue "strict/fatal runs require explicit ZCOFI_RDMA_NETDEV"
	[ "$PROVIDER_DECLARED" = 1 ] || issue "strict/fatal runs require explicit ZCOFI_RMA_MATRIX_PROVIDER"
fi

[ -n "$PROVIDER" ] || issue "set ZCOFI_RMA_MATRIX_PROVIDER explicitly"
[ -n "$DOMAIN" ] || issue "set URING_PLAY_OFI_DOMAIN explicitly"
[ -n "$RDMA_DEVICE" ] || issue "RDMA device is unreported"
[ -n "$NETDEV" ] || issue "RDMA netdev is unreported"

if [ -z "$OWNER_CPUS" ]; then
	issue "set URING_PLAY_PIN_CPU_LIST to the ordered CQ-owner CPUs"
else
	validate_cpu_csv "$OWNER_CPUS" URING_PLAY_PIN_CPU_LIST 1
fi
validate_cpu_csv "$IRQ_CPUS" ZCOFI_RDMA_IRQ_CPU_LIST 0

IFS=',' read -r -a owner_cpu_array <<<"$OWNER_CPUS"
if [ -z "$OWNER_CPUS" ]; then
	owner_cpu_array=()
fi
if [ -z "$LANES" ]; then
	LANES="${#owner_cpu_array[@]}"
fi
[[ "$LANES" =~ ^[0-9]+$ ]] && [ "$LANES" -ge 1 ] || issue "set ZCOFI_RDMA_LANES to a positive endpoint/QP count"
if [[ "$LANES" =~ ^[0-9]+$ ]] && [ "$LANES" -ge 1 ] && [ "${#owner_cpu_array[@]}" -ne "$LANES" ]; then
	issue "endpoint/QP count $LANES does not equal CQ-owner CPU count ${#owner_cpu_array[@]}"
fi

if [ "${URING_PLAY_PIN_CPUS:-0}" != 1 ]; then
	issue "URING_PLAY_PIN_CPUS=1 is required for a performance run"
fi
if [ "${URING_PLAY_OFI_CQ_SLEEP_NS:-50000}" != 0 ]; then
	issue "URING_PLAY_OFI_CQ_SLEEP_NS=0 is required for the low-latency curve"
fi
if [ -z "$REGISTERED_BYTES" ]; then
	issue "set ZCOFI_RDMA_REGISTERED_BYTES to the total pinned working-set estimate"
elif ! [[ "$REGISTERED_BYTES" =~ ^[0-9]+$ ]] || [ "$REGISTERED_BYTES" -eq 0 ]; then
	die "ZCOFI_RDMA_REGISTERED_BYTES must be a positive byte count"
fi

memlock_kib="$(ulimit -l)"
if [ "$memlock_kib" != unlimited ] && [ -n "$REGISTERED_BYTES" ] && [[ "$REGISTERED_BYTES" =~ ^[0-9]+$ ]]; then
	required_kib=$(((REGISTERED_BYTES + 1023) / 1024))
	if ! [[ "$memlock_kib" =~ ^[0-9]+$ ]] || [ "$memlock_kib" -lt "$required_kib" ]; then
		issue "memlock $memlock_kib KiB is below the registered-memory estimate $required_kib KiB"
	fi
fi

huge_total="$(awk '/^HugePages_Total:/{print $2}' /proc/meminfo)"
huge_free="$(awk '/^HugePages_Free:/{print $2}' /proc/meminfo)"
huge_kib="$(awk '/^Hugepagesize:/{print $2}' /proc/meminfo)"
huge_required=unreported
if [ -n "$REGISTERED_BYTES" ] && [[ "$REGISTERED_BYTES" =~ ^[0-9]+$ ]] && \
	[[ "$huge_kib" =~ ^[1-9][0-9]*$ ]]; then
	hugepage_bytes=$((huge_kib * 1024))
	huge_required=$((REGISTERED_BYTES / hugepage_bytes))
	[ $((REGISTERED_BYTES % hugepage_bytes)) -eq 0 ] || huge_required=$((huge_required + 1))
fi
if [ "$huge_total" -eq 0 ] || [ "$huge_free" -eq 0 ]; then
	issue "no free hugetlb pages are visible"
elif [[ "$huge_required" =~ ^[0-9]+$ ]] && [ "$huge_free" -lt "$huge_required" ]; then
	issue "free hugetlb pages $huge_free are below the registered-memory estimate $huge_required"
fi

if [ "$BLOCK_MODE" = 1 ]; then
	[ -n "$HCTX_CPUS" ] || issue "block mode requires ZCOFI_RDMA_HCTX_CPU_LIST"
	validate_cpu_csv "$HCTX_CPUS" ZCOFI_RDMA_HCTX_CPU_LIST 1
	declare -a hctx_cpu_array=()
	if [ -n "$HCTX_CPUS" ]; then
		IFS=',' read -r -a hctx_cpu_array <<<"$HCTX_CPUS"
	fi
	if [[ "$LANES" =~ ^[1-9][0-9]*$ ]] && [ "${#hctx_cpu_array[@]}" -ne "$LANES" ]; then
		issue "block hctx CPU count ${#hctx_cpu_array[@]} does not equal lane count $LANES"
	fi
	[ "$BLOCK_ENGINE" = uring-fixed ] || issue "block mode requires ZCOFI_RDMA_BLOCK_ENGINE=uring-fixed (or BLOCK_ENGINE=uring-fixed from the block harness)"
	[[ "$BLOCK_COMPLETION_BATCH" =~ ^[1-9][0-9]*$ ]] || \
		issue "block mode requires explicit positive URING_PLAY_BLOCKBENCH_COMPLETION_BATCH"
	[[ "$BLOCK_WAIT_MIN_COMPLETIONS" =~ ^[1-9][0-9]*$ ]] || \
		issue "block mode requires explicit positive URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS"
	if { ! [[ "$BLOCK_CQE_SPIN" =~ ^[1-9][0-9]*$ ]]; } && \
		[ "$BLOCK_CQE_ADAPTIVE_SPIN" != 1 ] && [ "$BLOCK_CQE_HOT_POLL" != 1 ]; then
		issue "block mode requires an explicit io_uring fast path: positive URING_PLAY_BLOCKBENCH_CQE_SPIN, adaptive spin, or CQE hot poll"
	fi
fi

pci_bdf=unreported
driver=unreported
numa_node=unreported
if [ -n "$RDMA_DEVICE" ] && [ -e "/sys/class/infiniband/$RDMA_DEVICE/device" ]; then
	device_path="$(readlink -f "/sys/class/infiniband/$RDMA_DEVICE/device")"
	pci_bdf="$(basename "$device_path")"
	if [ -e "$device_path/driver" ]; then
		driver="$(basename "$(readlink -f "$device_path/driver")")"
	fi
	if [ -r "$device_path/numa_node" ]; then
		numa_node="$(<"$device_path/numa_node")"
	fi
fi
if [ "$KIND" = mlx5 ] && [ "$driver" != mlx5_core ]; then
	issue "ConnectX run requires mlx5_core; selected driver is $driver"
fi
if [ "$KIND" = rxe ] && [[ "$RDMA_DEVICE" != rxe* ]]; then
	issue "Soft-RoCE rehearsal expected an rxe* RDMA device, got ${RDMA_DEVICE:-unreported}"
fi

port_state=unreported
port_phys_state=unreported
port_link_layer=unreported
port_rate=unreported
selected_gid=unreported
selected_gid_type=unreported
selected_gid_netdev=unreported
port_path="/sys/class/infiniband/$RDMA_DEVICE/ports/$RDMA_PORT"
if [ -n "$RDMA_DEVICE" ] && [ -d "$port_path" ]; then
	[ ! -r "$port_path/state" ] || port_state="$(<"$port_path/state")"
	[ ! -r "$port_path/phys_state" ] || port_phys_state="$(<"$port_path/phys_state")"
	[ ! -r "$port_path/link_layer" ] || port_link_layer="$(<"$port_path/link_layer")"
	[ ! -r "$port_path/rate" ] || port_rate="$(<"$port_path/rate")"
	if [[ "$VERBS_GID_INDEX" =~ ^[0-9]+$ ]]; then
		gid_path="$port_path/gids/$VERBS_GID_INDEX"
		gid_type_path="$port_path/gid_attrs/types/$VERBS_GID_INDEX"
		gid_netdev_path="$port_path/gid_attrs/ndevs/$VERBS_GID_INDEX"
		[ ! -r "$gid_path" ] || selected_gid="$(<"$gid_path")"
		[ ! -r "$gid_type_path" ] || selected_gid_type="$(<"$gid_type_path")"
		[ ! -r "$gid_netdev_path" ] || selected_gid_netdev="$(<"$gid_netdev_path")"
	fi
fi

if [ "$KIND" = mlx5 ]; then
	[ -d "$port_path" ] || issue "selected mlx5 port does not exist: $RDMA_DEVICE/$RDMA_PORT"
	[[ "$port_state" == *ACTIVE* ]] || issue "selected mlx5 port is not ACTIVE: $port_state"
	[[ "$numa_node" =~ ^[0-9]+$ ]] || issue "selected mlx5 device has no usable NUMA node: $numa_node"
	[[ "$CQE_SIZE" =~ ^(64|128)$ ]] || issue "set ZCOFI_RDMA_CQE_SIZE to the verified mlx5 CQE size (64 or 128)"
	[[ "$CQE_COMPRESSION" =~ ^(enabled|disabled|unsupported)$ ]] || \
		issue "set ZCOFI_RDMA_CQE_COMPRESSION to enabled, disabled, or unsupported"
	[[ "$CQ_MODERATION_PERIOD" =~ ^[0-9]+$ ]] || \
		issue "set ZCOFI_RDMA_CQ_MODERATION_PERIOD to the verified CQ period"
	[[ "$CQ_MODERATION_COUNT" =~ ^[0-9]+$ ]] || \
		issue "set ZCOFI_RDMA_CQ_MODERATION_COUNT to the verified CQ count"
	[ "$CQ_CONFIGURATION_VERIFIED" = 1 ] || \
		issue "verify CQE size/compression/moderation, then set ZCOFI_RDMA_CQ_CONFIGURATION_VERIFIED=1"
	if [[ "$PROVIDER" == *verbs* ]]; then
		[ -n "$VERBS_DEVICE" ] || issue "verbs hardware run requires FI_VERBS_DEVICE_NAME"
		[ -n "$VERBS_IFACE" ] || issue "verbs hardware run requires FI_VERBS_IFACE"
		[[ "$VERBS_GID_INDEX" =~ ^[0-9]+$ ]] || issue "verbs hardware run requires numeric FI_VERBS_GID_IDX"
		[ -z "$VERBS_DEVICE" ] || [ "$VERBS_DEVICE" = "$RDMA_DEVICE" ] || \
			issue "FI_VERBS_DEVICE_NAME=$VERBS_DEVICE does not match ZCOFI_RDMA_DEVICE=$RDMA_DEVICE"
		[ -z "$VERBS_IFACE" ] || [ "$VERBS_IFACE" = "$NETDEV" ] || \
			issue "FI_VERBS_IFACE=$VERBS_IFACE does not match ZCOFI_RDMA_NETDEV=$NETDEV"
		if [[ "$VERBS_GID_INDEX" =~ ^[0-9]+$ ]]; then
			[ "$selected_gid" != unreported ] || issue "selected GID index $VERBS_GID_INDEX is unreadable on $RDMA_DEVICE/$RDMA_PORT"
			if [[ "$selected_gid" =~ ^(0+:)+0+$ ]]; then
				issue "selected GID index $VERBS_GID_INDEX is all zero"
			fi
			[ "$selected_gid_netdev" = unreported ] || [ "$selected_gid_netdev" = "$NETDEV" ] || \
				issue "selected GID index $VERBS_GID_INDEX maps to netdev $selected_gid_netdev, not $NETDEV"
		fi
	fi
fi

declare -A owner_cpu_set=()
declare -A owner_core_set=()
declare -A owner_cpu_core=()
for cpu in "${owner_cpu_array[@]}"; do
	owner_cpu_set[$cpu]=1
	package=unreported
	core=unreported
	[ ! -r "/sys/devices/system/cpu/cpu$cpu/topology/physical_package_id" ] || \
		package="$(<"/sys/devices/system/cpu/cpu$cpu/topology/physical_package_id")"
	[ ! -r "/sys/devices/system/cpu/cpu$cpu/topology/core_id" ] || \
		core="$(<"/sys/devices/system/cpu/cpu$cpu/topology/core_id")"
	owner_cpu_core[$cpu]="$package:$core"
	if [[ "$package" =~ ^[0-9]+$ ]] && [[ "$core" =~ ^[0-9]+$ ]]; then
		core_key="$package:$core"
		if [ -n "${owner_core_set[$core_key]:-}" ]; then
			issue "CQ owner CPUs ${owner_core_set[$core_key]} and $cpu share physical core $core_key"
		else
			owner_core_set[$core_key]="$cpu"
		fi
	fi
	if [ "$KIND" = mlx5 ] && [[ "$numa_node" =~ ^[0-9]+$ ]]; then
		cpu_node_path="$(find "/sys/devices/system/cpu/cpu$cpu" -mindepth 1 -maxdepth 1 -name 'node*' -print -quit 2>/dev/null || true)"
		if [ -n "$cpu_node_path" ]; then
			cpu_node="${cpu_node_path##*node}"
			[ "$cpu_node" = "$numa_node" ] || \
				issue "CQ owner CPU $cpu is on NUMA node $cpu_node, not HCA node $numa_node"
		fi
	fi
done
if [ -n "$IRQ_CPUS" ]; then
	IFS=',' read -r -a irq_cpu_array <<<"$IRQ_CPUS"
	for cpu in "${irq_cpu_array[@]}"; do
		if [ -n "${owner_cpu_set[$cpu]:-}" ] && [ "$IRQ_COLOCATION_CONFIRMED" != 1 ]; then
			issue "IRQ CPU $cpu overlaps a CQ-owner CPU; set ZCOFI_RDMA_IRQ_COLOCATION_CONFIRMED=1 only for an intentional measured control"
		elif [ "$IRQ_COLOCATION_CONFIRMED" != 1 ]; then
			irq_package=unreported
			irq_core=unreported
			[ ! -r "/sys/devices/system/cpu/cpu$cpu/topology/physical_package_id" ] || \
				irq_package="$(<"/sys/devices/system/cpu/cpu$cpu/topology/physical_package_id")"
			[ ! -r "/sys/devices/system/cpu/cpu$cpu/topology/core_id" ] || \
				irq_core="$(<"/sys/devices/system/cpu/cpu$cpu/topology/core_id")"
			irq_core_key="$irq_package:$irq_core"
			if [ -n "${owner_core_set[$irq_core_key]:-}" ]; then
				issue "IRQ CPU $cpu shares physical core $irq_core_key with CQ owner CPU ${owner_core_set[$irq_core_key]}"
			fi
		fi
	done
fi

governors="$(find /sys/devices/system/cpu/cpufreq -name scaling_governor -type f -exec cat {} + 2>/dev/null | sort -u | paste -sd, -)"
[ -n "$governors" ] || governors=unreported
if [ "$KIND" = mlx5 ] && [ "$governors" != performance ]; then
	issue "CPU governors are $governors, not uniformly performance"
fi

irqbalance_state=unavailable
if command -v systemctl >/dev/null 2>&1; then
	irqbalance_state="$(systemctl is-active irqbalance 2>/dev/null || true)"
	[ -n "$irqbalance_state" ] || irqbalance_state=inactive
fi
if [ "$KIND" = mlx5 ] && [ "$irqbalance_state" = active ] && \
	[ "$IRQ_AFFINITY_CONFIRMED" != 1 ]; then
	issue "irqbalance is active; set and verify IRQ affinity, then export ZCOFI_RDMA_IRQ_AFFINITY_CONFIRMED=1"
fi
if [ "$KIND" = mlx5 ] && [ -z "$IRQ_CPUS" ]; then
	issue "set ZCOFI_RDMA_IRQ_CPU_LIST and keep completion vectors off CQ-owner CPUs unless the measured topology intentionally co-locates them"
fi

send_depth="${URING_PLAY_OFI_TX_QUEUE_DEPTH:-64}"
recv_depth="${URING_PLAY_OFI_RX_QUEUE_DEPTH:-64}"
read_depth="${URING_PLAY_OFI_RMA_READ_QD:-1}"
write_depth="${URING_PLAY_OFI_RMA_WRITE_QD:-1}"
cq_headroom="${URING_PLAY_OFI_CQ_HEADROOM:-64}"
for value in "$send_depth" "$recv_depth" "$read_depth" "$write_depth" "$cq_headroom"; do
	[[ "$value" =~ ^[0-9]+$ ]] || die "OFI queue depths and CQ headroom must be numeric"
done
provider_tx_requested=$((send_depth + read_depth + write_depth))
provider_rx_requested="$recv_depth"
tx_cq_requested=$((provider_tx_requested + cq_headroom))
rx_cq_requested=$((provider_rx_requested + cq_headroom))

printf 'rdma_preflight_kind=%s strict=%s fatal=%s pre_runtime_problems=%s\n' "$KIND" "$STRICT" "$FATAL" "$problems"
printf 'rdma_device=%s netdev=%s provider=%s ofi_domain=%s pci_bdf=%s driver=%s numa_node=%s\n' \
	"${RDMA_DEVICE:-unreported}" "${NETDEV:-unreported}" "${PROVIDER:-unreported}" \
	"${DOMAIN:-unreported}" "$pci_bdf" "$driver" "$numa_node"
printf 'endpoint_count=%s cq_owner_count=%s owner_cpus=%s irq_cpus=%s hctx_cpus=%s block_mode=%s\n' \
	"${LANES:-unreported}" "${#owner_cpu_array[@]}" "${OWNER_CPUS:-unreported}" \
	"${IRQ_CPUS:-unreported}" "${HCTX_CPUS:-not-applicable}" "$BLOCK_MODE"
for ((lane = 0; lane < ${#owner_cpu_array[@]}; lane++)); do
	owner_cpu="${owner_cpu_array[$lane]}"
	printf 'lane=%s endpoint=%s qp=%s cq=%s owner=%s owner_cpu=%s owner_physical_core=%s ownership=single-post-and-poll-thread\n' \
		"$lane" "$lane" "$lane" "$lane" "$lane" "$owner_cpu" "${owner_cpu_core[$owner_cpu]:-unreported}"
done
printf 'provider_tx_queue_requested=%s provider_rx_queue_requested=%s tx_cq_requested=%s rx_cq_requested=%s cq_headroom=%s runtime_returned_capacity_check=required\n' \
	"$provider_tx_requested" "$provider_rx_requested" "$tx_cq_requested" "$rx_cq_requested" "$cq_headroom"
printf 'hugepages_total=%s hugepages_free=%s hugepages_required=%s hugepage_size_kib=%s memlock_kib=%s registered_bytes_estimate=%s\n' \
	"$huge_total" "$huge_free" "$huge_required" "$huge_kib" "$memlock_kib" "${REGISTERED_BYTES:-unreported}"
printf 'rdma_port=%s port_state=%s port_phys_state=%s port_link_layer=%s port_rate=%s selected_gid_index=%s selected_gid=%s selected_gid_type=%s selected_gid_netdev=%s\n' \
	"$RDMA_PORT" "$port_state" "$port_phys_state" "$port_link_layer" "$port_rate" \
	"${VERBS_GID_INDEX:-unreported}" "$selected_gid" "$selected_gid_type" "$selected_gid_netdev"
printf 'block_engine=%s block_completion_batch=%s block_wait_min_completions=%s block_cqe_spin=%s block_cqe_adaptive_spin=%s block_cqe_hot_poll=%s\n' \
	"${BLOCK_ENGINE:-not-applicable}" "${BLOCK_COMPLETION_BATCH:-not-applicable}" \
	"${BLOCK_WAIT_MIN_COMPLETIONS:-not-applicable}" "${BLOCK_CQE_SPIN:-not-applicable}" \
	"${BLOCK_CQE_ADAPTIVE_SPIN:-not-applicable}" "${BLOCK_CQE_HOT_POLL:-not-applicable}"
printf 'cpu_governors=%s irqbalance=%s cq_sleep_ns=%s fi_more=%s fi_more_burst=%s\n' \
	"$governors" "$irqbalance_state" "${URING_PLAY_OFI_CQ_SLEEP_NS:-50000}" \
	"${URING_PLAY_OFI_RMA_WRITE_MORE:-0}" "${URING_PLAY_OFI_RMA_WRITE_MORE_BURST:-64}"
printf 'cqe_size=%s cqe_compression=%s cq_moderation_period=%s cq_moderation_count=%s cq_configuration_verified=%s\n' \
	"${CQE_SIZE:-unreported}" "${CQE_COMPRESSION:-unreported}" \
	"${CQ_MODERATION_PERIOD:-unreported}" "${CQ_MODERATION_COUNT:-unreported}" \
	"$CQ_CONFIGURATION_VERIFIED"
printf 'verbs_device=%s verbs_iface=%s verbs_gid_index=%s irq_affinity_confirmed=%s irq_colocation_confirmed=%s\n' \
	"${VERBS_DEVICE:-unreported}" "${VERBS_IFACE:-unreported}" \
	"${VERBS_GID_INDEX:-unreported}" "$IRQ_AFFINITY_CONFIRMED" \
	"$IRQ_COLOCATION_CONFIRMED"
if [ "$KIND" = mlx5 ]; then
	printf 'queue_model=mlx5-sq-64B-WQEBB-and-power-of-two-buffer;rq-power-of-two-WQE-and-WR-count;cq-64B-or-128B-CQE;one-owner-per-QP-CQ\n'
else
	printf 'queue_model=rxe-software-semantic-rehearsal;hardware-WQEBB-UAR-CQE-IRQ-behavior=not-exercised;one-owner-per-endpoint-CQ\n'
fi
printf 'completion_semantics=read:data-visible-local-cq;write:configured-local-or-delivery-cq;remote-ack:separate;sync-fua:separate;durability:terminal-media-only\n'
printf 'coordination_token=%s coordination_honored=%s shared_system=%s\n' \
	"${AGENT_COORD_TOKEN:-unreported}" "${AGENT_COORD_HONORED:-unreported}" \
	"$([ "$KIND" = rxe ] && printf yes || printf no)"

if [ -n "$RDMA_DEVICE" ]; then
	rdma link show 2>/dev/null | awk -v dev="$RDMA_DEVICE" '$0 ~ dev {print "rdma_link=" $0}' || true
fi
if [ -n "$NETDEV" ] && command -v ethtool >/dev/null 2>&1; then
	ethtool -i "$NETDEV" 2>/dev/null | sed 's/^/ethtool_driver_/' || true
fi
if command -v fi_info >/dev/null 2>&1 && [ -n "$PROVIDER" ]; then
	fi_info_args=(-p "$PROVIDER" -t FI_EP_RDM)
	[ -z "$DOMAIN" ] || fi_info_args+=(-d "$DOMAIN")
	fi_info_output=
	if fi_info_output="$(fi_info "${fi_info_args[@]}" 2>&1)" && [ -n "$fi_info_output" ]; then
		printf '%s\n' "$fi_info_output" | awk '
			/^[[:space:]]*provider:|^[[:space:]]*fabric:|^[[:space:]]*domain:|^[[:space:]]*type:|^[[:space:]]*protocol:/{gsub(/^[[:space:]]+/, ""); print "fi_info_" $0}
		' | head -40 || true
	else
		fi_info_detail="$(printf '%s\n' "$fi_info_output" | tail -n 1)"
		issue "fi_info could not resolve RDM provider=$PROVIDER domain=${DOMAIN:-unreported}: ${fi_info_detail:-no provider result}"
	fi
elif ! command -v fi_info >/dev/null 2>&1; then
	issue "fi_info is unavailable"
fi

mlx5_vectors=0
declare -A mlx5_irq_cpu_seen=()
if [ "$KIND" = mlx5 ] && [ "$pci_bdf" != unreported ] && \
	[ -d "/sys/bus/pci/devices/$pci_bdf/msi_irqs" ]; then
	for irq_path in /sys/bus/pci/devices/"$pci_bdf"/msi_irqs/*; do
		[ -e "$irq_path" ] || continue
		irq="$(basename "$irq_path")"
		name="$(awk -v irq="$irq" '$1 == irq ":" {sub(/^[^:]*:[[:space:]]*/, ""); print}' /proc/interrupts)"
		effective=unreported
		configured=unreported
		[ ! -r "/proc/irq/$irq/effective_affinity_list" ] || \
			effective="$(<"/proc/irq/$irq/effective_affinity_list")"
		[ ! -r "/proc/irq/$irq/smp_affinity_list" ] || \
			configured="$(<"/proc/irq/$irq/smp_affinity_list")"
		printf 'irq=%s effective_affinity=%s configured_affinity=%s name=%s\n' \
			"$irq" "$effective" "$configured" "$name"
		if [[ "$name" == *mlx5_comp* ]]; then
			mlx5_vectors=$((mlx5_vectors + 1))
			if ! [[ "$effective" =~ ^[0-9]+$ ]]; then
				issue "mlx5 completion IRQ $irq effective affinity is $effective, not one explicit CPU"
			elif [ -n "$IRQ_CPUS" ]; then
				mlx5_irq_cpu_seen[$effective]=1
				if [[ ",$IRQ_CPUS," != *",$effective,"* ]]; then
					issue "mlx5 completion IRQ $irq runs on CPU $effective outside declared IRQ CPUs $IRQ_CPUS"
				fi
			fi
			if [ "$configured" != "$effective" ]; then
				issue "mlx5 completion IRQ $irq configured affinity $configured differs from effective affinity $effective"
			fi
		fi
	done
	[ "$mlx5_vectors" -gt 0 ] || issue "no mlx5_comp completion vectors were found for $pci_bdf"
	if [ -n "$IRQ_CPUS" ]; then
		IFS=',' read -r -a irq_cpu_array <<<"$IRQ_CPUS"
		for cpu in "${irq_cpu_array[@]}"; do
			[ -n "${mlx5_irq_cpu_seen[$cpu]:-}" ] || \
				issue "declared IRQ CPU $cpu owns no observed mlx5 completion vector"
		done
	fi
fi
printf 'mlx5_completion_vectors=%s\n' "$mlx5_vectors"

printf 'rdma_preflight_final_problems=%s representative_ready=%s\n' \
	"$problems" "$([ "$problems" -eq 0 ] && printf yes || printf no)"
if [ "$problems" -ne 0 ] && { [ "$STRICT" = 1 ] || [ "$FATAL" = 1 ]; }; then
	die "$problems topology/performance preflight problem(s); refusing representative benchmark"
fi
