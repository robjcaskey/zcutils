#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GENERIC_PREFLIGHT="${H4D_GENERIC_RDMA_PREFLIGHT:-$ROOT/scripts/zcofi-rdma-topology-preflight.sh}"
STRICT="${URING_PLAY_TOPOLOGY_STRICT:-0}"
FATAL="${URING_PLAY_TOPOLOGY_FATAL:-0}"
PAIR_MANIFEST="${H4D_PAIR_MANIFEST:-}"
RDMA_DEVICE="${H4D_RDMA_DEVICE:-${ZCOFI_RDMA_DEVICE:-}}"
RDMA_NETDEV="${H4D_RDMA_NETDEV:-${ZCOFI_RDMA_NETDEV:-}}"
RDMA_PORT="${H4D_RDMA_PORT:-${ZCOFI_RDMA_PORT:-1}}"
RDMA_PEER_IPV4="${H4D_RDMA_PEER_IPV4:-}"
LANES="${H4D_RDMA_LANES:-${ZCOFI_RDMA_LANES:-}}"
WORKER_CPUS="${H4D_RDMA_WORKER_CPUS:-${URING_PLAY_PIN_CPU_LIST:-}}"
IRQ_CPUS="${H4D_RDMA_IRQ_CPUS:-${ZCOFI_RDMA_IRQ_CPU_LIST:-}}"
REGISTERED_BYTES="${H4D_RDMA_REGISTERED_BYTES:-${ZCOFI_RDMA_REGISTERED_BYTES:-}}"
PROVIDER="${ZCOFI_RMA_MATRIX_PROVIDER:-verbs;ofi_rxm}"
DOMAIN="${URING_PLAY_OFI_DOMAIN:-}"
GID_INDEX="${FI_VERBS_GID_IDX:-}"
IRQ_AFFINITY_CONFIRMED="${ZCOFI_RDMA_IRQ_AFFINITY_CONFIRMED:-0}"
REQUIRE_LIBFABRIC="${H4D_REQUIRE_LIBFABRIC:-1}"
problems=0

die() {
	printf 'gce-h4d-rdma-preflight: %s\n' "$*" >&2
	exit 1
}

issue() {
	printf 'PERF WARNING: %s\n' "$*" >&2
	problems=$((problems + 1))
}

need() {
	command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

private_ipv4() {
	local ip="$1"
	local first second
	IFS=. read -r first second _ _ <<<"$ip"
	[[ "$ip" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]] || return 1
	if [ "$first" = 10 ]; then
		return 0
	fi
	if [ "$first" = 192 ] && [ "$second" = 168 ]; then
		return 0
	fi
	[ "$first" = 172 ] && [ "$second" -ge 16 ] && [ "$second" -le 31 ]
}

driver_for_netdev() {
	local path
	path="$(readlink -f "/sys/class/net/$1/device/driver" 2>/dev/null || true)"
	if [ -n "$path" ]; then
		basename "$path"
	else
		printf 'unreported\n'
	fi
}

cpu_in_csv() {
	[[ ",$2," == *",$1,"* ]]
}

metadata_get() {
	curl --fail --silent --show-error --connect-timeout 2 --max-time 5 --noproxy '*' \
		-H 'Metadata-Flavor: Google' \
		"http://169.254.169.254/computeMetadata/v1/$1" 2>/dev/null || true
}

[[ "$STRICT" =~ ^[01]$ ]] || die "URING_PLAY_TOPOLOGY_STRICT must be zero or one"
[[ "$FATAL" =~ ^[01]$ ]] || die "URING_PLAY_TOPOLOGY_FATAL must be zero or one"
[[ "$RDMA_PORT" =~ ^[1-9][0-9]*$ ]] || die "H4D_RDMA_PORT must be positive"
[[ "$IRQ_AFFINITY_CONFIRMED" =~ ^[01]$ ]] || \
	die "ZCOFI_RDMA_IRQ_AFFINITY_CONFIRMED must be zero or one"
[[ "$REQUIRE_LIBFABRIC" =~ ^[01]$ ]] || die "H4D_REQUIRE_LIBFABRIC must be zero or one"

for command in curl find ip lscpu nproc readlink rdma ibv_devices ibv_devinfo \
	taskset sha256sum python3 modinfo lsmod; do
	need "$command"
done
[ -x "$GENERIC_PREFLIGHT" ] || die "generic preflight is not executable: $GENERIC_PREFLIGHT"

instance_name="$(metadata_get instance/name)"
machine_type="$(basename "$(metadata_get instance/machine-type)")"
zone="$(basename "$(metadata_get instance/zone)")"
project_id="$(metadata_get project/project-id)"
if [ -z "$instance_name" ] || [ -z "$machine_type" ] || [ -z "$zone" ] || [ -z "$project_id" ]; then
	issue "GCE metadata is incomplete; this does not prove an H4D VM identity"
fi
[ "$machine_type" = h4d-standard-192 ] || \
	issue "machine type is ${machine_type:-unreported}, not h4d-standard-192"

visible_cpus="$(nproc)"
threads_per_core="$(lscpu | awk -F: '/^Thread\(s\) per core:/{gsub(/[[:space:]]/, "", $2); print $2}')"
cores_per_socket="$(lscpu | awk -F: '/^Core\(s\) per socket:/{gsub(/[[:space:]]/, "", $2); print $2}')"
sockets="$(lscpu | awk -F: '/^Socket\(s\):/{gsub(/[[:space:]]/, "", $2); print $2}')"
numa_nodes="$(lscpu | awk -F: '/^NUMA node\(s\):/{gsub(/[[:space:]]/, "", $2); print $2}')"
[ "$visible_cpus" = 192 ] || issue "H4D must expose 192 CPUs; nproc reports $visible_cpus"
[ "$threads_per_core" = 1 ] || issue "H4D must expose one thread per physical core; got ${threads_per_core:-unreported}"

manifest_zone=unreported
manifest_policy=unreported
manifest_network=unreported
manifest_subnet=unreported
manifest_local_ip=unreported
manifest_peer_ip=unreported
manifest_peer_name=unreported
manifest_profile=unreported
manifest_mtu=unreported
manifest_cost=unreported
manifest_sha256=unreported
if [ -z "$PAIR_MANIFEST" ]; then
	issue "set H4D_PAIR_MANIFEST to a controller-validated same-zone pair manifest"
elif [ ! -s "$PAIR_MANIFEST" ]; then
	issue "H4D pair manifest is missing or empty: $PAIR_MANIFEST"
else
	manifest_sha256="$(sha256sum "$PAIR_MANIFEST" | awk '{print $1}')"
	mapfile -t manifest_values < <(python3 - "$PAIR_MANIFEST" "$instance_name" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    manifest = json.load(source)
if manifest.get("schema_version") != 1 or manifest.get("provider") != "gce":
    raise SystemExit("invalid H4D pair manifest schema")
nodes = manifest.get("nodes", [])
local = next((node for node in nodes if node.get("name") == sys.argv[2]), None)
peers = [node for node in nodes if node.get("name") != sys.argv[2]]
if local is None or len(nodes) != 2 or len(peers) != 1:
    raise SystemExit("local instance is not one member of the manifest pair")
policy = manifest.get("bulk_traffic_policy", {})
if policy.get("cross_region") is not False or policy.get("internet") is not False:
    raise SystemExit("manifest does not forbid cross-region and internet bulk traffic")
network = manifest.get("rdma_network", {})
values = [
    manifest.get("zone", ""),
    manifest.get("placement", {}).get("policy", ""),
    network.get("name", ""),
    network.get("subnet", ""),
    local.get("rdma_ipv4", ""),
    peers[0].get("rdma_ipv4", ""),
    peers[0].get("name", ""),
    network.get("profile", ""),
    str(network.get("mtu", "")),
    manifest.get("cost", {}).get("estimated_pair_cost_usd", ""),
]
print("\n".join(values))
PY
	) || die "could not validate H4D pair manifest"
	[ "${#manifest_values[@]}" -eq 10 ] || die "H4D pair manifest returned incomplete topology data"
	manifest_zone="${manifest_values[0]}"
	manifest_policy="${manifest_values[1]}"
	manifest_network="${manifest_values[2]}"
	manifest_subnet="${manifest_values[3]}"
	manifest_local_ip="${manifest_values[4]}"
	manifest_peer_ip="${manifest_values[5]}"
	manifest_peer_name="${manifest_values[6]}"
	manifest_profile="${manifest_values[7]}"
	manifest_mtu="${manifest_values[8]}"
	manifest_cost="${manifest_values[9]}"
	[ "$manifest_zone" = "$zone" ] || issue "manifest zone $manifest_zone does not match metadata zone ${zone:-unreported}"
	[ "$manifest_profile" = "$zone-vpc-falcon" ] || issue "manifest Falcon profile $manifest_profile does not match zone $zone"
	[ "$manifest_mtu" = 8896 ] || issue "manifest Falcon MTU is $manifest_mtu, not 8896"
	if [ -n "$RDMA_PEER_IPV4" ] && [ "$RDMA_PEER_IPV4" != "$manifest_peer_ip" ]; then
		issue "declared H4D_RDMA_PEER_IPV4 differs from the validated manifest peer"
	fi
	RDMA_PEER_IPV4="$manifest_peer_ip"
fi

mapfile -t rdma_devices < <(find /sys/class/infiniband -mindepth 1 -maxdepth 1 -printf '%f\n' 2>/dev/null | sort)
if [ -z "$RDMA_DEVICE" ] && [ "${#rdma_devices[@]}" -eq 1 ]; then
	RDMA_DEVICE="${rdma_devices[0]}"
fi
if [ "$STRICT" = 1 ] || [ "$FATAL" = 1 ]; then
	[ -n "${H4D_RDMA_DEVICE:-${ZCOFI_RDMA_DEVICE:-}}" ] || issue "strict H4D runs require explicit H4D_RDMA_DEVICE"
	[ -n "${H4D_RDMA_NETDEV:-${ZCOFI_RDMA_NETDEV:-}}" ] || issue "strict H4D runs require explicit H4D_RDMA_NETDEV"
	[ -n "$RDMA_PEER_IPV4" ] || issue "strict H4D runs require a validated peer IRDMA address"
fi
[ "${#rdma_devices[@]}" -eq 1 ] || issue "H4D must expose exactly one RDMA device; found ${#rdma_devices[@]}"
[ -n "$RDMA_DEVICE" ] && [ -d "/sys/class/infiniband/$RDMA_DEVICE" ] || \
	issue "selected RDMA device is unavailable: ${RDMA_DEVICE:-unreported}"

mapfile -t netdevs < <(find /sys/class/net -mindepth 1 -maxdepth 1 -printf '%f\n' | sort)
declare -a gve_netdevs=()
declare -a idpf_netdevs=()
for netdev in "${netdevs[@]}"; do
	driver="$(driver_for_netdev "$netdev")"
	case "$driver" in
		gve) gve_netdevs+=("$netdev") ;;
		idpf) idpf_netdevs+=("$netdev") ;;
	esac
done
[ "${#gve_netdevs[@]}" -ge 1 ] || issue "no gVNIC/gve control interface is visible"
[ "${#idpf_netdevs[@]}" -eq 1 ] || issue "H4D must expose exactly one IDPF data interface; found ${#idpf_netdevs[@]}"
if [ -z "$RDMA_NETDEV" ] && [ "${#idpf_netdevs[@]}" -eq 1 ]; then
	RDMA_NETDEV="${idpf_netdevs[0]}"
fi
if [ -n "$RDMA_NETDEV" ]; then
	[ "$(driver_for_netdev "$RDMA_NETDEV")" = idpf ] || \
		issue "selected RDMA netdev $RDMA_NETDEV is not driven by IDPF"
	mtu="$(cat "/sys/class/net/$RDMA_NETDEV/mtu" 2>/dev/null || printf unreported)"
	[ "$mtu" = 8896 ] || issue "RDMA netdev $RDMA_NETDEV MTU is $mtu, not 8896"
else
	mtu=unreported
	issue "RDMA netdev is unreported"
fi

rdma_links="$(rdma link show 2>/dev/null || true)"
if [ -n "$RDMA_DEVICE" ] && [ -n "$RDMA_NETDEV" ]; then
	grep -Eq "link ${RDMA_DEVICE}/${RDMA_PORT} .*netdev ${RDMA_NETDEV}([[:space:]]|$)" <<<"$rdma_links" || \
		issue "rdma link does not map $RDMA_DEVICE/$RDMA_PORT to $RDMA_NETDEV"
	port_state="$(cat "/sys/class/infiniband/$RDMA_DEVICE/ports/$RDMA_PORT/state" 2>/dev/null || printf unreported)"
	[[ "$port_state" == *ACTIVE* ]] || issue "RDMA port is not ACTIVE: $port_state"
else
	port_state=unreported
fi

rdma_ipv4=unreported
if [ -n "$RDMA_NETDEV" ] && [ -d "/sys/class/net/$RDMA_NETDEV" ]; then
	mapfile -t rdma_ipv4s < <(ip -o -4 address show dev "$RDMA_NETDEV" scope global | awk '{split($4, value, "/"); print value[1]}')
	if [ "${#rdma_ipv4s[@]}" -eq 1 ]; then
		rdma_ipv4="${rdma_ipv4s[0]}"
		private_ipv4 "$rdma_ipv4" || issue "RDMA address is not RFC1918 private IPv4: $rdma_ipv4"
	else
		issue "RDMA netdev $RDMA_NETDEV must have exactly one IPv4 address; found ${#rdma_ipv4s[@]}"
	fi
fi
[ "$manifest_local_ip" = unreported ] || [ "$rdma_ipv4" = "$manifest_local_ip" ] || \
	issue "local RDMA address $rdma_ipv4 does not match manifest $manifest_local_ip"

peer_route=unreported
peer_route_dev=unreported
peer_route_src=unreported
if [ -n "$RDMA_PEER_IPV4" ]; then
	private_ipv4 "$RDMA_PEER_IPV4" || issue "peer RDMA address is not RFC1918 private IPv4: $RDMA_PEER_IPV4"
	peer_route="$(ip -4 route get "$RDMA_PEER_IPV4" 2>/dev/null | head -n 1 || true)"
	peer_route_dev="$(awk '{for (i=1; i<=NF; i++) if ($i == "dev") print $(i+1)}' <<<"$peer_route")"
	peer_route_src="$(awk '{for (i=1; i<=NF; i++) if ($i == "src") print $(i+1)}' <<<"$peer_route")"
	[ "$peer_route_dev" = "$RDMA_NETDEV" ] || issue "peer route uses ${peer_route_dev:-unreported}, not RDMA netdev $RDMA_NETDEV"
	[ "$peer_route_src" = "$rdma_ipv4" ] || issue "peer route source ${peer_route_src:-unreported} is not local RDMA address $rdma_ipv4"
fi

default_routes="$(ip -4 route show default)"
[ -n "$default_routes" ] || issue "no IPv4 default route is visible on the control plane"
if [ -n "$RDMA_NETDEV" ] && grep -Eq "(^|[[:space:]])dev ${RDMA_NETDEV}([[:space:]]|$)" <<<"$default_routes"; then
	issue "RDMA/Falcon netdev $RDMA_NETDEV carries a default route; internet bulk isolation is not proven"
fi
control_default_proven=0
for control_netdev in "${gve_netdevs[@]}"; do
	if grep -Eq "(^|[[:space:]])dev ${control_netdev}([[:space:]]|$)" <<<"$default_routes"; then
		control_default_proven=1
	fi
done
[ "$control_default_proven" = 1 ] || issue "the default route is not proven to use a gVNIC control interface"

device_numa=unreported
if [ -n "$RDMA_DEVICE" ]; then
	device_numa="$(cat "/sys/class/infiniband/$RDMA_DEVICE/device/numa_node" 2>/dev/null || true)"
fi
if ! [[ "$device_numa" =~ ^[0-9]+$ ]] && [ -n "$RDMA_NETDEV" ]; then
	device_numa="$(cat "/sys/class/net/$RDMA_NETDEV/device/numa_node" 2>/dev/null || printf unreported)"
fi
[[ "$device_numa" =~ ^[0-9]+$ ]] || issue "RDMA device NUMA node is ${device_numa:-unreported}"

if [ -z "$LANES" ]; then
	IFS=',' read -r -a worker_array <<<"$WORKER_CPUS"
	LANES="${#worker_array[@]}"
else
	IFS=',' read -r -a worker_array <<<"$WORKER_CPUS"
fi
if [ -z "$WORKER_CPUS" ]; then
	worker_array=()
	issue "set H4D_RDMA_WORKER_CPUS to the ordered lane/CQ-owner CPU list"
fi
[[ "$LANES" =~ ^[1-9][0-9]*$ ]] || issue "set H4D_RDMA_LANES to a positive lane count"
if [[ "$LANES" =~ ^[1-9][0-9]*$ ]] && [ "${#worker_array[@]}" -ne "$LANES" ]; then
	issue "lane count $LANES differs from worker CPU count ${#worker_array[@]}"
fi
[ -n "$IRQ_CPUS" ] || issue "set H4D_RDMA_IRQ_CPUS to the verified IDPF/iRDMA completion IRQ CPUs"
[ "$IRQ_AFFINITY_CONFIRMED" = 1 ] || \
	issue "set and verify IDPF/iRDMA IRQ masks, then set ZCOFI_RDMA_IRQ_AFFINITY_CONFIRMED=1"
[ "${URING_PLAY_PIN_CPUS:-0}" = 1 ] || issue "URING_PLAY_PIN_CPUS=1 is required"

declare -A worker_seen=()
for cpu in "${worker_array[@]}"; do
	[[ "$cpu" =~ ^[0-9]+$ ]] || {
		issue "worker CPU must be numeric: $cpu"
		continue
	}
	if [ -n "${worker_seen[$cpu]:-}" ]; then
		issue "worker CPU $cpu is repeated"
	fi
	worker_seen[$cpu]=1
	[ -d "/sys/devices/system/cpu/cpu$cpu" ] || {
		issue "worker CPU $cpu does not exist"
		continue
	}
	taskset -c "$cpu" true >/dev/null 2>&1 || issue "worker CPU $cpu is outside the current cpuset"
	cpu_node_path="$(find "/sys/devices/system/cpu/cpu$cpu" -mindepth 1 -maxdepth 1 -name 'node*' -print -quit 2>/dev/null || true)"
	if [[ "$device_numa" =~ ^[0-9]+$ ]] && [ -n "$cpu_node_path" ]; then
		cpu_node="${cpu_node_path##*node}"
		[ "$cpu_node" = "$device_numa" ] || issue "worker CPU $cpu is on NUMA $cpu_node, not RDMA NUMA $device_numa"
	fi
done
if [ -n "$IRQ_CPUS" ]; then
	[[ "$IRQ_CPUS" =~ ^[0-9]+(,[0-9]+)*$ ]] || issue "H4D_RDMA_IRQ_CPUS must be a numeric CSV"
	IFS=',' read -r -a irq_cpu_array <<<"$IRQ_CPUS"
	for cpu in "${irq_cpu_array[@]}"; do
		if [ -n "${worker_seen[$cpu]:-}" ]; then
			issue "IRQ CPU $cpu overlaps a lane/CQ-owner CPU"
		fi
	done
fi

if ! [[ "$REGISTERED_BYTES" =~ ^[1-9][0-9]*$ ]]; then
	issue "set H4D_RDMA_REGISTERED_BYTES to the pinned working-set byte count"
	REGISTERED_BYTES=0
fi
memlock_kib="$(ulimit -l)"
required_memlock_kib=$(((REGISTERED_BYTES + 1023) / 1024))
if [ "$memlock_kib" != unlimited ] && \
	{ ! [[ "$memlock_kib" =~ ^[0-9]+$ ]] || [ "$memlock_kib" -lt "$required_memlock_kib" ]; }; then
	issue "memlock $memlock_kib KiB is below required $required_memlock_kib KiB"
fi
huge_total="$(awk '/^HugePages_Total:/{print $2}' /proc/meminfo)"
huge_free="$(awk '/^HugePages_Free:/{print $2}' /proc/meminfo)"
huge_kib="$(awk '/^Hugepagesize:/{print $2}' /proc/meminfo)"
huge_required=0
if [[ "$huge_kib" =~ ^[1-9][0-9]*$ ]]; then
	huge_bytes=$((huge_kib * 1024))
	huge_required=$(((REGISTERED_BYTES + huge_bytes - 1) / huge_bytes))
fi
[ "$huge_total" -gt 0 ] || issue "no hugetlb pages are configured"
[ "$huge_free" -ge "$huge_required" ] || \
	issue "free hugetlb pages $huge_free are below required $huge_required"

for module in gve idpf irdma; do
	modinfo "$module" >/dev/null 2>&1 || issue "kernel module $module is unavailable for $(uname -r)"
done
lsmod | grep -q '^gve\b' || issue "gve is not loaded"
lsmod | grep -q '^idpf\b' || issue "idpf is not loaded"
lsmod | grep -q '^irdma\b' || issue "irdma is not loaded"

config="/boot/config-$(uname -r)"
if [ -r "$config" ]; then
	for symbol in CONFIG_GVE CONFIG_IDPF CONFIG_INFINIBAND CONFIG_INFINIBAND_IRDMA \
		CONFIG_BLK_DEV_NVME CONFIG_IO_URING; do
		grep -Eq "^${symbol}=(y|m)$" "$config" || issue "$symbol is not enabled in $config"
	done
else
	issue "running kernel config is unreadable: $config"
fi

for command in ib_send_bw ib_read_bw ib_write_bw ib_send_lat ib_read_lat ib_write_lat fi_info; do
	command -v "$command" >/dev/null 2>&1 || issue "$command is unavailable"
done
ibv_devices_output="$(ibv_devices 2>&1 || true)"
grep -q "$RDMA_DEVICE" <<<"$ibv_devices_output" || issue "ibv_devices does not report $RDMA_DEVICE"
ibv_devinfo_output="$(ibv_devinfo -d "$RDMA_DEVICE" -i "$RDMA_PORT" 2>&1 || true)"
grep -Eq 'state:[[:space:]]+PORT_ACTIVE|phys_state:[[:space:]]+LINK_UP' <<<"$ibv_devinfo_output" || \
	issue "ibv_devinfo does not report an active/up IRDMA port"

irq_pci_path=unreported
irq_count=0
if [ -n "$RDMA_NETDEV" ]; then
	device_path="$(readlink -f "/sys/class/net/$RDMA_NETDEV/device" 2>/dev/null || true)"
	while [[ "$device_path" == /sys/* ]] && [ "$device_path" != /sys ]; do
		if [ -d "$device_path/msi_irqs" ]; then
			irq_pci_path="$device_path"
			break
		fi
		device_path="$(dirname "$device_path")"
	done
fi
if [ "$irq_pci_path" != unreported ]; then
	for irq_path in "$irq_pci_path"/msi_irqs/*; do
		[ -e "$irq_path" ] || continue
		irq="$(basename "$irq_path")"
		effective="$(cat "/proc/irq/$irq/effective_affinity_list" 2>/dev/null || printf unreported)"
		configured="$(cat "/proc/irq/$irq/smp_affinity_list" 2>/dev/null || printf unreported)"
		name="$(awk -v irq="$irq" '$1 == irq ":" {sub(/^[^:]*:[[:space:]]*/, ""); print}' /proc/interrupts)"
		printf 'irdma_irq=%s effective_affinity=%s configured_affinity=%s name=%s\n' \
			"$irq" "$effective" "$configured" "$name"
		irq_count=$((irq_count + 1))
		[[ "$effective" =~ ^[0-9]+$ ]] || issue "IRQ $irq effective affinity is not one explicit CPU: $effective"
		if [[ "$effective" =~ ^[0-9]+$ ]] && [ -n "$IRQ_CPUS" ]; then
			cpu_in_csv "$effective" "$IRQ_CPUS" || issue "IRQ $irq runs on CPU $effective outside H4D_RDMA_IRQ_CPUS"
		fi
		[ "$configured" = "$effective" ] || issue "IRQ $irq configured affinity $configured differs from effective $effective"
	done
fi
[ "$irq_count" -gt 0 ] || issue "no IDPF/iRDMA MSI-X vectors were found for $RDMA_NETDEV"

printf 'h4d_preflight_identity project=%s zone=%s instance=%s machine_type=%s kernel=%s\n' \
	"${project_id:-unreported}" "${zone:-unreported}" "${instance_name:-unreported}" \
	"${machine_type:-unreported}" "$(uname -r)"
printf 'h4d_cpu_topology visible_cpus=%s threads_per_core=%s cores_per_socket=%s sockets=%s numa_nodes=%s\n' \
	"$visible_cpus" "${threads_per_core:-unreported}" "${cores_per_socket:-unreported}" \
	"${sockets:-unreported}" "${numa_nodes:-unreported}"
printf 'h4d_pair_manifest path=%s sha256=%s zone=%s policy=%s network=%s subnet=%s profile=%s mtu=%s estimated_pair_cost_usd=%s\n' \
	"${PAIR_MANIFEST:-unreported}" "$manifest_sha256" "$manifest_zone" "$manifest_policy" \
	"$manifest_network" "$manifest_subnet" "$manifest_profile" "$manifest_mtu" "$manifest_cost"
printf 'h4d_network_isolation control_netdevs=%s rdma_netdev=%s rdma_ipv4=%s peer_name=%s peer_ipv4=%s peer_route_dev=%s peer_route_src=%s default_route_on_rdma=no-required cross_region=no internet_bulk=no\n' \
	"$(IFS=,; printf '%s' "${gve_netdevs[*]:-unreported}")" "${RDMA_NETDEV:-unreported}" \
	"$rdma_ipv4" "$manifest_peer_name" "${RDMA_PEER_IPV4:-unreported}" \
	"$peer_route_dev" "$peer_route_src"
printf 'h4d_rdma_path rdma_device=%s rdma_port=%s rdma_netdev=%s netdev_driver=%s port_state=%s mtu=%s numa_node=%s irq_pci_path=%s irq_count=%s\n' \
	"${RDMA_DEVICE:-unreported}" "$RDMA_PORT" "${RDMA_NETDEV:-unreported}" \
	"$(driver_for_netdev "${RDMA_NETDEV:-missing}")" "$port_state" "$mtu" "$device_numa" \
	"$irq_pci_path" "$irq_count"
printf 'h4d_lane_topology lanes=%s worker_cpus=%s irq_cpus=%s registered_bytes=%s provider=%s domain=%s gid_index=%s\n' \
	"${LANES:-unreported}" "${WORKER_CPUS:-unreported}" "${IRQ_CPUS:-unreported}" \
	"${REGISTERED_BYTES:-unreported}" "$PROVIDER" "${DOMAIN:-unreported}" "${GID_INDEX:-unreported}"
printf 'h4d_memory_topology memlock_kib=%s hugepages_total=%s hugepages_free=%s hugepage_size_kib=%s hugepages_required=%s\n' \
	"$memlock_kib" "$huge_total" "$huge_free" "$huge_kib" "$huge_required"
for ((lane = 0; lane < ${#worker_array[@]}; lane++)); do
	printf 'h4d_lane_map lane=%s worker=%s cq=%s qp=%s worker_cpu=%s rdma_device=%s rdma_port=%s rdma_netdev=%s numa_node=%s peer=%s\n' \
		"$lane" "$lane" "$lane" "$lane" "${worker_array[$lane]}" \
		"${RDMA_DEVICE:-unreported}" "$RDMA_PORT" "${RDMA_NETDEV:-unreported}" \
		"$device_numa" "${RDMA_PEER_IPV4:-unreported}"
done
printf 'h4d_completion_semantics perftest_send=remote-receive-completion perftest_read=initiator-local-data-visible perftest_write=initiator-local-or-delivery-per-tool-options zcutils_reads=initiator-local-data-visible zcutils_writes=reported-per-run sync_fua=separate durability=terminal-media-only\n'
printf 'h4d_preflight_problems=%s representative_ready=%s\n' \
	"$problems" "$([ "$problems" -eq 0 ] && printf yes || printf no)"

if [ "$problems" -ne 0 ] && { [ "$STRICT" = 1 ] || [ "$FATAL" = 1 ]; }; then
	die "$problems H4D topology/performance preflight problem(s); refusing representative benchmark"
fi

if [ "$REQUIRE_LIBFABRIC" = 1 ]; then
	ZCOFI_RDMA_PREFLIGHT_KIND=irdma \
	ZCOFI_RDMA_DEVICE="$RDMA_DEVICE" \
	ZCOFI_RDMA_NETDEV="$RDMA_NETDEV" \
	ZCOFI_RDMA_PORT="$RDMA_PORT" \
	ZCOFI_RDMA_LANES="$LANES" \
	ZCOFI_RDMA_IRQ_CPU_LIST="$IRQ_CPUS" \
	ZCOFI_RDMA_REGISTERED_BYTES="$REGISTERED_BYTES" \
	ZCOFI_RMA_MATRIX_PROVIDER="$PROVIDER" \
	URING_PLAY_OFI_DOMAIN="$DOMAIN" \
	URING_PLAY_PIN_CPU_LIST="$WORKER_CPUS" \
	FI_VERBS_DEVICE_NAME="${FI_VERBS_DEVICE_NAME:-$RDMA_DEVICE}" \
	FI_VERBS_IFACE="${FI_VERBS_IFACE:-$RDMA_NETDEV}" \
		"$GENERIC_PREFLIGHT"
else
	printf 'generic_libfabric_preflight=skipped reason=verbs-perftest-qualification-only\n'
fi
