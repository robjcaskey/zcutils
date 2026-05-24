#!/usr/bin/env bash

set -Eeuo pipefail

PATH=/usr/sbin:/usr/bin:/sbin:/bin

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
URING_PLAY="${URING_PLAY:-${ROOT}/target/debug/zcutils}"
DEFAULT_NETDEVSIM_KO="/tmp/netdevsim-rdma-resigned.ko"
if [ ! -e "${DEFAULT_NETDEVSIM_KO}" ]; then
	DEFAULT_NETDEVSIM_KO="/home/rob/src/linux-7.0.8/drivers/net/netdevsim/netdevsim.ko"
fi
NETDEVSIM_KO="${NETDEVSIM_KO:-${DEFAULT_NETDEVSIM_KO}}"
SIGN_FILE="${SIGN_FILE:-/home/rob/src/linux-7.0.8/scripts/sign-file}"
SIGN_KEY="${SIGN_KEY:-/home/rob/src/linux-7.0.8/certs/signing_key.pem}"
SIGN_CERT="${SIGN_CERT:-/home/rob/src/linux-7.0.8/certs/signing_key.x509}"

MODE="${1:-${MODE:-raft-bench}}"
NODES="${NODES:-3}"
BASE_ID="${BASE_ID:-$((30000 + $$ % 20000))}"
IP_PREFIX="${IP_PREFIX:-10.240.$((BASE_ID % 200))}"
RAFT_PORT="${RAFT_PORT:-9100}"
RAFT_ENTRIES="${RAFT_ENTRIES:-4096}"
RAFT_PAYLOAD_BYTES="${RAFT_PAYLOAD_BYTES:-4096}"
RAFT_ACK_STRIDE="${RAFT_ACK_STRIDE:-64}"
SMOKE_PORT="${SMOKE_PORT:-8000}"
SMOKE_BYTES="${SMOKE_BYTES:-65536}"
P2P_BASE_PORT="${P2P_BASE_PORT:-8000}"
P2P_PORTS="${P2P_PORTS:-16}"
P2P_CONNECTIONS_PER_PORT="${P2P_CONNECTIONS_PER_PORT:-1}"
P2P_BYTES_PER_CONNECTION="${P2P_BYTES_PER_CONNECTION:-268435456}"
P2P_CHUNK_BYTES="${P2P_CHUNK_BYTES:-1048576}"
P2P_QSTAT_ACTIVE_BYTES="${P2P_QSTAT_ACTIVE_BYTES:-1048576}"
P2P_ENGINE="${P2P_ENGINE:-std}"
P2P_URING_PIPELINE="${P2P_URING_PIPELINE:-4}"
P2P_URING_WORKERS="${P2P_URING_WORKERS:-0}"
P2P_URING_RECV_BYTES="${P2P_URING_RECV_BYTES:-1048576}"
P2P_URING_ENTRIES="${P2P_URING_ENTRIES:-4096}"
P2P_URING_RECV_MODE="${P2P_URING_RECV_MODE:-recv}"
P2P_URING_SEND_MODE="${P2P_URING_SEND_MODE:-send}"
P2P_ZCRX_RXQ="${P2P_ZCRX_RXQ:-0}"
P2P_ZCRX_RXQ_COUNT="${P2P_ZCRX_RXQ_COUNT:-${NETDEVSIM_QUEUES:-8}}"
P2P_ZCRX_RX_PAYLOAD_NOCOPY="${P2P_ZCRX_RX_PAYLOAD_NOCOPY:-1}"
ALLOW_UNSAFE_HOST_SEND_ZC="${ALLOW_UNSAFE_HOST_SEND_ZC:-0}"
RUN_SECONDS="${RUN_SECONDS:-0}"
NETEM="${NETEM:-}"
RUN_DIR="${RUN_DIR:-${ROOT}/qemu-zcrx/raft-lab-${BASE_ID}}"
BRIDGE="zrbr${BASE_ID}"
YNL_CLI="${YNL_CLI:-/home/rob/src/linux-7.0.8/tools/net/ynl/pyynl/cli.py}"
NETDEV_SPEC="${NETDEV_SPEC:-/home/rob/src/linux-7.0.8/Documentation/netlink/specs/netdev.yaml}"
RZC_BIN="${RZC_BIN:-/home/rob/rust-rewrite/raft-zero-copy/target/release/raft-zero-copy}"
RZC_BENCH="${RZC_BENCH:-/home/rob/rust-rewrite/raft-zero-copy/target/release/bench}"
RZC_CLIENT_PORT="${RZC_CLIENT_PORT:-9200}"
RZC_CLIENT_LANES="${RZC_CLIENT_LANES:-1}"
RZC_COUNT="${RZC_COUNT:-4096}"
RZC_SIZE="${RZC_SIZE:-4096}"
RZC_BATCH="${RZC_BATCH:-128}"
RZC_CLIENTS="${RZC_CLIENTS:-1}"
RZC_CLIENT_PIPELINE="${RZC_CLIENT_PIPELINE:-1}"
RZC_IO_URING="${RZC_IO_URING:-1}"
RZC_URING_ENTRIES="${RZC_URING_ENTRIES:-64}"
RZC_SEND_ZC="${RZC_SEND_ZC:-0}"
RZC_SEND_ZC_CHUNK="${RZC_SEND_ZC_CHUNK:-262144}"
RZC_SEND_ZC_DEPTH="${RZC_SEND_ZC_DEPTH:-8}"
RZC_REPLICATION_LANES="${RZC_REPLICATION_LANES:-1}"
RZC_DISCARD_WRITES="${RZC_DISCARD_WRITES:-0}"
RZC_SYNTHETIC_PAYLOAD_BYTE="${RZC_SYNTHETIC_PAYLOAD_BYTE:-0xa5}"
RZC_THREAD_PIN="${RZC_THREAD_PIN:-1}"
SLOTBENCH_BIN="${SLOTBENCH_BIN:-/home/rob/rust-rewrite/raft-zero-copy/target/release/slotbench}"
SLOT_PORT="${SLOT_PORT:-9100}"
SLOT_SLOTS="${SLOT_SLOTS:-2000000}"
SLOT_SLOTS_PER_FRAME="${SLOT_SLOTS_PER_FRAME:-4}"
SLOT_PIPELINE="${SLOT_PIPELINE:-512}"
SLOT_SEND_BURST="${SLOT_SEND_BURST:-32}"
SLOT_ACK_EVERY="${SLOT_ACK_EVERY:-32}"
SLOT_LANES="${SLOT_LANES:-8}"
SLOT_WAIT="${SLOT_WAIT:-quorum}"
SLOT_IO_URING="${SLOT_IO_URING:-0}"
SLOT_URING_ENTRIES="${SLOT_URING_ENTRIES:-512}"
SLOT_SEND_ZC="${SLOT_SEND_ZC:-0}"
SLOT_VALIDATE="${SLOT_VALIDATE:-0}"
SLOT_PIN_THREADS="${SLOT_PIN_THREADS:-1}"
DISABLE_OFFLOADS="${DISABLE_OFFLOADS:-0}"
NETDEVSIM_QUEUES="${NETDEVSIM_QUEUES:-8}"
NETDEVSIM_RING_SIZE="${NETDEVSIM_RING_SIZE:-4096}"
NETDEVSIM_NAPI_DELAY_US="${NETDEVSIM_NAPI_DELAY_US:-0}"
NETDEVSIM_RX_5TUPLE="${NETDEVSIM_RX_5TUPLE:-1}"
NETDEVSIM_RX_DPORT_HASH="${NETDEVSIM_RX_DPORT_HASH:-0}"
NETDEVSIM_RX_DPORT_BASE="${NETDEVSIM_RX_DPORT_BASE:-${RAFT_PORT}}"
NETDEVSIM_TX_5TUPLE="${NETDEVSIM_TX_5TUPLE:-0}"
NETDEVSIM_MTU="${NETDEVSIM_MTU:-9000}"
NETDEVSIM_TOPOLOGY="${NETDEVSIM_TOPOLOGY:-bridge}"
NETDEVSIM_NAPI_CPU_AFFINITY="${NETDEVSIM_NAPI_CPU_AFFINITY:-1}"
NETDEVSIM_NAPI_CPU_STRIDE="${NETDEVSIM_NAPI_CPU_STRIDE:-1}"
NSIM_DEBUGFS_SNAPSHOT="${NSIM_DEBUGFS_SNAPSHOT:-1}"
LAB_PIN_PROCS="${LAB_PIN_PROCS:-1}"
LAB_EXPORT_URING_PIN="${LAB_EXPORT_URING_PIN:-1}"
LAB_NUMA_NODE="${LAB_NUMA_NODE:-0}"
LAB_CPUSET="${LAB_CPUSET:-}"
LAB_CPUS_PER_NODE="${LAB_CPUS_PER_NODE:-0}"
LAB_MEMBIND="${LAB_MEMBIND:-1}"

declare -a NODE_IDS=()
declare -a SWITCH_IDS=()
declare -a NODE_NS=()
declare -a NODE_FDS=()
declare -a NODE_IFIDX=()
declare -a NODE_IPS=()
declare -a SWITCH_IFS=()
declare -a NODE_CPUSETS=()
declare -a LAB_CPUS=()
declare -a CHILD_PIDS=()

ROOT_FD=""
NETDEVSIM_LOADED_BY_US=0
PSAMPLE_LOADED_BY_US=0
DEBUGFS_MOUNTED_BY_US=0
LAB_USE_NUMACTL=0

log()
{
	printf '[raft-lab] %s\n' "$*"
}

nsim_debugfs_snapshot()
{
	local label="$1"
	local out="${RUN_DIR}/netdevsim-debugfs-${label}.txt"
	local id sub dir path name value
	local -a ids=()

	if [ "${NSIM_DEBUGFS_SNAPSHOT}" != "1" ]; then
		return 0
	fi

	ids+=("${NODE_IDS[@]:-}")
	ids+=("${SWITCH_IDS[@]:-}")
	: >"${out}"
	for id in "${ids[@]}"; do
		[ -n "${id}" ] || continue
		for sub in fastpath zcrx rdma; do
			dir="/sys/kernel/debug/netdevsim/netdevsim${id}/ports/0/${sub}"
			if [ ! -d "${dir}" ]; then
				printf 'netdevsim%s/%s=<missing>\n' "${id}" "${sub}" >>"${out}"
				continue
			fi
			for path in "${dir}"/*; do
				[ -f "${path}" ] && [ -r "${path}" ] || continue
				name="$(basename "${path}")"
				value="$(cat "${path}" 2>/dev/null | tr '\n' ' ' | sed 's/[[:space:]]*$//')"
				printf 'netdevsim%s/%s/%s=%s\n' "${id}" "${sub}" "${name}" "${value}" >>"${out}"
			done
		done
	done
	log "netdevsim debugfs snapshot ${label}: ${out}"
}

nsim_debugfs_delta()
{
	local before="${RUN_DIR}/netdevsim-debugfs-${1}.txt"
	local after="${RUN_DIR}/netdevsim-debugfs-${2}.txt"
	local out="${RUN_DIR}/netdevsim-debugfs-${3}-delta.txt"

	if [ "${NSIM_DEBUGFS_SNAPSHOT}" != "1" ]; then
		return 0
	fi
	if [ ! -e "${before}" ] || [ ! -e "${after}" ]; then
		log "skipping netdevsim debugfs delta; missing ${before} or ${after}"
		return 0
	fi

	awk -F= '
		FNR == NR {
			if ($2 ~ /^[0-9]+$/)
				before[$1] = $2
			next
		}
		$2 ~ /^[0-9]+$/ && ($1 in before) {
			delta = $2 - before[$1]
			if (delta != 0)
				printf "%s_delta=%s\n", $1, delta
		}
	' "${before}" "${after}" >"${out}"
	if [ ! -s "${out}" ]; then
		printf 'no numeric counter deltas\n' >"${out}"
	fi
	log "netdevsim debugfs delta ${3}: ${out}"
}

expand_cpulist()
{
	local list="$1"
	local part start end cpu
	local old_ifs="${IFS}"

	IFS=,
	for part in ${list}; do
		IFS="${old_ifs}"
		if [[ "${part}" == *-* ]]; then
			start="${part%-*}"
			end="${part#*-}"
			for ((cpu = start; cpu <= end; cpu++)); do
				printf '%s\n' "${cpu}"
			done
		elif [ -n "${part}" ]; then
			printf '%s\n' "${part}"
		fi
		IFS=,
	done
	IFS="${old_ifs}"
}

cpu_slice()
{
	local start="$1"
	local count="$2"
	local total="$3"
	local -a out=()
	local i idx

	for ((i = 0; i < count; i++)); do
		idx=$(((start + i) % total))
		out+=("${LAB_CPUS[$idx]}")
	done

	IFS=,
	printf '%s\n' "${out[*]}"
}

init_cpu_policy()
{
	local cpulist="${LAB_CPUSET}"
	local cpus_per_node total idx start

	NODE_CPUSETS=()
	LAB_CPUS=()
	LAB_USE_NUMACTL=0

	if [ "${LAB_PIN_PROCS}" != "1" ]; then
		log "process pinning disabled"
		return
	fi

	if [ -z "${cpulist}" ] && [ -n "${LAB_NUMA_NODE}" ] &&
		[ -r "/sys/devices/system/node/node${LAB_NUMA_NODE}/cpulist" ]; then
		cpulist="$(cat "/sys/devices/system/node/node${LAB_NUMA_NODE}/cpulist")"
	fi
	if [ -z "${cpulist}" ]; then
		log "process pinning requested but no LAB_CPUSET or NUMA cpulist is available"
		return
	fi

	mapfile -t LAB_CPUS < <(expand_cpulist "${cpulist}")
	total="${#LAB_CPUS[@]}"
	if [ "${total}" -eq 0 ]; then
		log "process pinning requested but cpuset '${cpulist}' expanded to no CPUs"
		return
	fi

	cpus_per_node="${LAB_CPUS_PER_NODE}"
	if ! [[ "${cpus_per_node}" =~ ^[0-9]+$ ]] || [ "${cpus_per_node}" -le 0 ]; then
		cpus_per_node=$((total / NODES))
		if [ "${cpus_per_node}" -lt 1 ]; then
			cpus_per_node=1
		fi
	fi

	for ((idx = 0; idx < NODES; idx++)); do
		start=$(((idx * cpus_per_node) % total))
		NODE_CPUSETS[$idx]="$(cpu_slice "${start}" "${cpus_per_node}" "${total}")"
	done

	if [ "${LAB_MEMBIND}" = "1" ] && [ -n "${LAB_NUMA_NODE}" ]; then
		if command -v numactl >/dev/null 2>&1; then
			LAB_USE_NUMACTL=1
		else
			log "LAB_MEMBIND=1 requested but numactl is not installed; using CPU affinity only"
		fi
	fi

	log "cpu policy numa_node=${LAB_NUMA_NODE:-none} cpuset=${cpulist} cpus_per_node=${cpus_per_node} membind=${LAB_USE_NUMACTL}"
}

run_node()
{
	local idx="$1"
	shift
	local -a cmd=(ip netns exec "${NODE_NS[$idx]}")
	local first_cpu count

	if [ "${LAB_USE_NUMACTL}" = "1" ]; then
		cmd+=(numactl "--cpunodebind=${LAB_NUMA_NODE}" "--membind=${LAB_NUMA_NODE}")
	fi
	if [ "${LAB_PIN_PROCS}" = "1" ] && [ -n "${NODE_CPUSETS[$idx]:-}" ] &&
		command -v taskset >/dev/null 2>&1; then
		cmd+=(taskset -c "${NODE_CPUSETS[$idx]}")
	fi
	if [ "${LAB_EXPORT_URING_PIN}" = "1" ] && [ -n "${NODE_CPUSETS[$idx]:-}" ]; then
		first_cpu="${NODE_CPUSETS[$idx]%%,*}"
		count="$(comma_count "${NODE_CPUSETS[$idx]}")"
		if [ -n "${first_cpu}" ] && [ "${count}" -gt 0 ]; then
			cmd+=(
				env
				URING_PLAY_PIN_CPUS=1
				URING_PLAY_PIN_BASE_CPU="${first_cpu}"
				URING_PLAY_PIN_CPU_COUNT="${count}"
				URING_PLAY_PIN_STRIDE=1
			)
		fi
	fi

	"${cmd[@]}" "$@"
}

module_loaded()
{
	lsmod | awk '{print $1}' | grep -qx "$1"
}

running_under_qemu()
{
	local path value

	for path in \
		/sys/class/dmi/id/sys_vendor \
		/sys/class/dmi/id/product_name \
		/sys/class/dmi/id/board_vendor; do
		[ -r "${path}" ] || continue
		value="$(tr '[:upper:]' '[:lower:]' < "${path}")"
		case "${value}" in
			*qemu*|*kvm*|*bochs*)
				return 0
				;;
		esac
	done
	return 1
}

guard_host_send_zc()
{
	if [ "${MODE}" != "p2p-mux" ] || [ "${P2P_ENGINE}" != "uring" ] ||
		[ "${P2P_URING_SEND_MODE}" = "send" ]; then
		return
	fi
	if running_under_qemu; then
		return
	fi
	if [ "${ALLOW_UNSAFE_HOST_SEND_ZC}" = "1" ]; then
		export URING_PLAY_ALLOW_UNSAFE_SEND_ZC=1
		return
	fi

	log "refusing host P2P_URING_SEND_MODE=${P2P_URING_SEND_MODE}; Linux 7.0.8 host runs hit bad-page/slub crashes. Use QEMU or set ALLOW_UNSAFE_HOST_SEND_ZC=1."
	exit 1
}

require_root()
{
	if [ "$(id -u)" -ne 0 ]; then
		printf 'run this script as root\n' >&2
		exit 1
	fi
}

require_file()
{
	if [ ! -e "$1" ]; then
		printf 'missing required file: %s\n' "$1" >&2
		exit 1
	fi
}

delete_device()
{
	local id="$1"

	if [ -e "/sys/bus/netdevsim/devices/netdevsim${id}" ]; then
		echo "${id}" > /sys/bus/netdevsim/del_device 2>/dev/null || true
	fi
}

cleanup()
{
	local status=$?

	set +e
	for pid in "${CHILD_PIDS[@]:-}"; do
		kill "${pid}" 2>/dev/null || true
	done
	for pid in "${CHILD_PIDS[@]:-}"; do
		wait "${pid}" 2>/dev/null || true
	done
	for i in $(seq 1 "${#NODE_IDS[@]}"); do
		local idx=$((i - 1))
		if [ -n "${NODE_FDS[$idx]:-}" ] && [ -n "${NODE_IFIDX[$idx]:-}" ]; then
			echo "${NODE_FDS[$idx]}:${NODE_IFIDX[$idx]}" \
				> /sys/bus/netdevsim/unlink_device 2>/dev/null || true
		fi
	done
	for fd in "${NODE_FDS[@]:-}"; do
		eval "exec ${fd}<&-" 2>/dev/null || true
	done
	if [ -n "${ROOT_FD}" ]; then
		eval "exec ${ROOT_FD}<&-" 2>/dev/null || true
	fi
	for ns in "${NODE_NS[@]:-}"; do
		ip netns del "${ns}" 2>/dev/null || true
	done
	ip link del "${BRIDGE}" 2>/dev/null || true
	for id in "${NODE_IDS[@]:-}" "${SWITCH_IDS[@]:-}"; do
		[ -n "${id}" ] && delete_device "${id}"
	done
	if [ "${NETDEVSIM_LOADED_BY_US}" -eq 1 ]; then
		rmmod netdevsim 2>/dev/null || true
	fi
	if [ "${PSAMPLE_LOADED_BY_US}" -eq 1 ]; then
		rmmod psample 2>/dev/null || true
	fi
	if [ "${DEBUGFS_MOUNTED_BY_US}" -eq 1 ]; then
		umount /sys/kernel/debug 2>/dev/null || true
	fi
	log "cleanup complete status=${status}"
	exit "${status}"
}

trap cleanup EXIT INT TERM

new_device()
{
	local id="$1"

	echo "${id} 1 ${NETDEVSIM_QUEUES}" > /sys/bus/netdevsim/new_device
	for _ in $(seq 1 50); do
		if [ -d "/sys/bus/netdevsim/devices/netdevsim${id}/net" ] &&
			[ -n "$(ls "/sys/bus/netdevsim/devices/netdevsim${id}/net")" ]; then
			return 0
		fi
		sleep 0.1
	done
	printf 'timed out waiting for netdevsim%s\n' "${id}" >&2
	return 1
}

configure_device_fastpath()
{
	local id="$1"
	local idx="${2:-}"
	local dir="/sys/kernel/debug/netdevsim/netdevsim${id}/ports/0/fastpath"

	if [ ! -d "${dir}" ]; then
		printf 'missing netdevsim fastpath debugfs dir: %s\n' "${dir}" >&2
		return 1
	fi
	echo "${NETDEVSIM_RING_SIZE}" > "${dir}/rx_ring_size"
	echo "${NETDEVSIM_NAPI_DELAY_US}" > "${dir}/napi_delay_us"
	echo "${NETDEVSIM_RX_5TUPLE}" > "${dir}/rx_5tuple_hash"
	if [ -e "${dir}/rx_dport_hash" ]; then
		echo "${NETDEVSIM_RX_DPORT_HASH}" > "${dir}/rx_dport_hash"
	fi
	if [ -e "${dir}/rx_dport_base" ]; then
		echo "${NETDEVSIM_RX_DPORT_BASE}" > "${dir}/rx_dport_base"
	fi
	if [ -e "${dir}/napi_cpu_affinity" ]; then
		echo "${NETDEVSIM_NAPI_CPU_AFFINITY}" > "${dir}/napi_cpu_affinity"
	fi
	if [ "${NETDEVSIM_NAPI_CPU_AFFINITY}" = "1" ] &&
		[ -n "${idx}" ] && [ -n "${NODE_CPUSETS[$idx]:-}" ]; then
		local -a node_cpus=()

		IFS=, read -r -a node_cpus <<< "${NODE_CPUSETS[$idx]}"
		if [ "${#node_cpus[@]}" -gt 0 ] && [ -e "${dir}/napi_cpu_base" ]; then
			echo "${node_cpus[0]}" > "${dir}/napi_cpu_base"
		fi
		if [ -e "${dir}/napi_cpu_stride" ]; then
			echo "${NETDEVSIM_NAPI_CPU_STRIDE}" > "${dir}/napi_cpu_stride"
		fi
	fi
	if [ -e "${dir}/tx_5tuple_hash" ]; then
		echo "${NETDEVSIM_TX_5TUPLE}" > "${dir}/tx_5tuple_hash"
	fi
}

dev_iface()
{
	local id="$1"
	local path name ifindex root_name

	for _ in $(seq 1 50); do
		for path in "/sys/bus/netdevsim/devices/netdevsim${id}/net"/*; do
			[ -e "${path}" ] || continue
			name="$(basename "${path}")"
			if [ -r "${path}/ifindex" ]; then
				ifindex="$(cat "${path}/ifindex" 2>/dev/null || true)"
				[ -n "${ifindex}" ] || continue
				root_name="$(ip -o link show | awk -F': ' -v ifindex="${ifindex}" '
					$1 == ifindex {
						split($2, parts, "@")
						print parts[1]
						exit
					}
				')"
				if [ -n "${root_name}" ]; then
					printf '%s\n' "${root_name}"
					return 0
				fi
			fi
			if ip link show dev "${name}" >/dev/null 2>&1; then
				printf '%s\n' "${name}"
				return 0
			fi
		done
		sleep 0.1
	done

	printf 'timed out resolving root netdev name for netdevsim%s; sysfs names: ' "${id}" >&2
	ls "/sys/bus/netdevsim/devices/netdevsim${id}/net" >&2
	return 1
}

load_modules()
{
	if ! mountpoint -q /sys/kernel/debug; then
		mount -t debugfs debugfs /sys/kernel/debug
		DEBUGFS_MOUNTED_BY_US=1
	fi

	if ! module_loaded psample; then
		modprobe psample
		PSAMPLE_LOADED_BY_US=1
	fi

	if module_loaded netdevsim; then
		log "refusing to run because netdevsim is already loaded"
		exit 1
	fi

	log "loading patched netdevsim ${NETDEVSIM_KO}"
	if ! insmod "${NETDEVSIM_KO}" 2>"${RUN_DIR}/insmod.err"; then
		if grep -q "Key was rejected" "${RUN_DIR}/insmod.err" &&
			[ -x "${SIGN_FILE}" ] && [ -e "${SIGN_KEY}" ] && [ -e "${SIGN_CERT}" ]; then
			log "module signature rejected; signing with local kernel build key and retrying"
			"${SIGN_FILE}" sha256 "${SIGN_KEY}" "${SIGN_CERT}" "${NETDEVSIM_KO}"
			insmod "${NETDEVSIM_KO}"
		else
			cat "${RUN_DIR}/insmod.err" >&2
			exit 1
		fi
	fi
	NETDEVSIM_LOADED_BY_US=1
}

peer_list()
{
	local first=1
	local peers=""

	for i in $(seq 1 "${NODES}"); do
		if [ "${first}" -eq 1 ]; then
			first=0
		else
			peers+=","
		fi
		peers+="${i}=${NODE_IPS[$((i - 1))]}:${RAFT_PORT}"
	done
	printf '%s\n' "${peers}"
}

peer_list_except()
{
	local self="$1"
	local first=1
	local peers=""

	for i in $(seq 1 "${NODES}"); do
		[ "${i}" -eq "${self}" ] && continue
		if [ "${first}" -eq 1 ]; then
			first=0
		else
			peers+=","
		fi
		peers+="${i}=${NODE_IPS[$((i - 1))]}:${RAFT_PORT}"
	done
	printf '%s\n' "${peers}"
}

leader_peer_addrs()
{
	local first=1
	local peers=""

	for i in $(seq 2 "${NODES}"); do
		if [ "${first}" -eq 1 ]; then
			first=0
		else
			peers+=","
		fi
		peers+="${NODE_IPS[$((i - 1))]}:${RAFT_PORT}"
	done
	printf '%s\n' "${peers}"
}

slotbench_peer_addrs()
{
	local first=1
	local peers=""

	for i in $(seq 2 "${NODES}"); do
		if [ "${first}" -eq 1 ]; then
			first=0
		else
			peers+=","
		fi
		peers+="${NODE_IPS[$((i - 1))]}:${SLOT_PORT}"
	done
	printf '%s\n' "${peers}"
}

comma_count()
{
	local value="$1"
	local count=1

	if [ -z "${value}" ]; then
		printf '0\n'
		return
	fi
	while [[ "${value}" == *,* ]]; do
		value="${value#*,}"
		count=$((count + 1))
	done
	printf '%s\n' "${count}"
}

slotbench_pin_args()
{
	local idx="$1"
	local cpuset="${NODE_CPUSETS[$idx]:-}"
	local first_cpu count

	if [ "${SLOT_PIN_THREADS}" != "1" ] || [ -z "${cpuset}" ]; then
		return
	fi

	first_cpu="${cpuset%%,*}"
	count="$(comma_count "${cpuset}")"
	if [ -n "${first_cpu}" ] && [ "${count}" -gt 0 ]; then
		printf '%s\n' "--pin-base-cpu"
		printf '%s\n' "${first_cpu}"
		printf '%s\n' "--pin-cpus"
		printf '%s\n' "${count}"
	fi
}

setup_lab()
{
	mkdir -p "${RUN_DIR}"
	init_cpu_policy
	log "setting up ${NODES}-node netdevsim ${NETDEVSIM_TOPOLOGY} lab base_id=${BASE_ID} ip_prefix=${IP_PREFIX} queues=${NETDEVSIM_QUEUES} ring=${NETDEVSIM_RING_SIZE} napi_delay_us=${NETDEVSIM_NAPI_DELAY_US} rx_5tuple=${NETDEVSIM_RX_5TUPLE} rx_dport_hash=${NETDEVSIM_RX_DPORT_HASH} tx_5tuple=${NETDEVSIM_TX_5TUPLE} napi_cpu_affinity=${NETDEVSIM_NAPI_CPU_AFFINITY} mtu=${NETDEVSIM_MTU}"

	case "${NETDEVSIM_TOPOLOGY}" in
		bridge)
			ip link add name "${BRIDGE}" type bridge
			ip link set "${BRIDGE}" mtu "${NETDEVSIM_MTU}"
			ip link set "${BRIDGE}" up
			;;
		direct)
			if [ "${NODES}" -ne 2 ]; then
				log "NETDEVSIM_TOPOLOGY=direct requires NODES=2"
				exit 1
			fi
			;;
		*)
			log "unknown NETDEVSIM_TOPOLOGY=${NETDEVSIM_TOPOLOGY} (use bridge or direct)"
			exit 1
			;;
	esac
	exec {ROOT_FD}< /proc/self/ns/net

	for i in $(seq 1 "${NODES}"); do
		local idx=$((i - 1))
		local node_id=$((BASE_ID + i * 2))
		local switch_id=$((BASE_ID + i * 2 + 1))
		local ns="zraft_${BASE_ID}_${i}"
		local ip_addr="${IP_PREFIX}.${i}"
		local node_if node_fd node_idx

		NODE_IDS[$idx]="${node_id}"
		SWITCH_IDS[$idx]=""
		NODE_NS[$idx]="${ns}"
		NODE_IPS[$idx]="${ip_addr}"

		ip netns add "${ns}"
		new_device "${node_id}"
		configure_device_fastpath "${node_id}" "${idx}"
		node_if="$(dev_iface "${node_id}")"

		if [ "${NETDEVSIM_TOPOLOGY}" = "bridge" ]; then
			local switch_if sw_name

			SWITCH_IDS[$idx]="${switch_id}"
			new_device "${switch_id}"
			configure_device_fastpath "${switch_id}" "${idx}"
			switch_if="$(dev_iface "${switch_id}")"
			sw_name="zrsw${i}_${BASE_ID}"
			sw_name="${sw_name:0:15}"
			SWITCH_IFS[$idx]="${sw_name}"

			ip link set "${switch_if}" name "${sw_name}"
			ip link set "${sw_name}" mtu "${NETDEVSIM_MTU}"
			ip link set "${sw_name}" master "${BRIDGE}"
			ip link set "${sw_name}" up
		fi

		ip link set "${node_if}" netns "${ns}"
		ip netns exec "${ns}" ip link set "${node_if}" name raft0
		ip netns exec "${ns}" ip link set dev raft0 mtu "${NETDEVSIM_MTU}"
		ip netns exec "${ns}" ip addr add "${ip_addr}/24" dev raft0
		ip netns exec "${ns}" ip link set dev lo up
		ip netns exec "${ns}" ip link set dev raft0 up
		ip netns exec "${ns}" ethtool -G raft0 tcp-data-split on || true
		if [ "${DISABLE_OFFLOADS}" = "1" ]; then
			ip netns exec "${ns}" ethtool -K raft0 \
				tcp-segmentation-offload off \
				generic-segmentation-offload off \
				generic-receive-offload off || true
		fi
		if [ -n "${NETEM}" ] && command -v tc >/dev/null 2>&1; then
			ip netns exec "${ns}" tc qdisc replace dev raft0 root netem ${NETEM}
		fi

		exec {node_fd}<"/run/netns/${ns}"
		NODE_FDS[$idx]="${node_fd}"
		node_idx="$(ip netns exec "${ns}" cat /sys/class/net/raft0/ifindex)"
		NODE_IFIDX[$idx]="${node_idx}"

		if [ "${NETDEVSIM_TOPOLOGY}" = "bridge" ]; then
			local sw_name="${SWITCH_IFS[$idx]}"
			local sw_idx

			sw_idx="$(cat "/sys/class/net/${sw_name}/ifindex")"
			echo "${node_fd}:${node_idx} ${ROOT_FD}:${sw_idx}" > /sys/bus/netdevsim/link_device
			log "node ${i}: ns=${ns} ip=${ip_addr} dev=netdevsim${node_id} switch=netdevsim${switch_id}/${sw_name} cpus=${NODE_CPUSETS[$idx]:-unbound}"
		else
			log "node ${i}: ns=${ns} ip=${ip_addr} dev=netdevsim${node_id} direct cpus=${NODE_CPUSETS[$idx]:-unbound}"
		fi
	done

	if [ "${NETDEVSIM_TOPOLOGY}" = "direct" ]; then
		echo "${NODE_FDS[0]}:${NODE_IFIDX[0]} ${NODE_FDS[1]}:${NODE_IFIDX[1]}" \
			> /sys/bus/netdevsim/link_device
		log "direct link: ${NODE_NS[0]}/raft0 <-> ${NODE_NS[1]}/raft0"
	fi
}

run_tcp_smoke()
{
	log "running all-to-all TCP smoke bytes_per_pair=${SMOKE_BYTES}"
	for i in $(seq 1 "${NODES}"); do
		local idx=$((i - 1))
		run_node "${idx}" "${URING_PLAY}" \
			tcp-sink-server 0.0.0.0 "${SMOKE_PORT}" "$((NODES - 1))" "${SMOKE_BYTES}" \
			>"${RUN_DIR}/tcp-node${i}.log" 2>&1 &
		CHILD_PIDS+=("$!")
	done
	sleep 1

	for src in $(seq 1 "${NODES}"); do
		for dst in $(seq 1 "${NODES}"); do
			[ "${src}" -eq "${dst}" ] && continue
			local src_idx=$((src - 1))
			local dst_idx=$((dst - 1))
			run_node "${src_idx}" "${URING_PLAY}" \
				tcp-send "${NODE_IPS[$dst_idx]}" "${SMOKE_PORT}" "${SMOKE_BYTES}" \
				>>"${RUN_DIR}/tcp-client.log" 2>&1
		done
	done

	wait_children
	log "all-to-all TCP smoke: ok"
}

dump_node_qstats()
{
	local idx="$1"
	local out="$2"
	local ifindex

	if [ ! -x "${YNL_CLI}" ] || [ ! -e "${NETDEV_SPEC}" ]; then
		log "skipping qstats dump; missing YNL_CLI=${YNL_CLI} or NETDEV_SPEC=${NETDEV_SPEC}"
		return 0
	fi

	ifindex="$(ip netns exec "${NODE_NS[$idx]}" cat /sys/class/net/raft0/ifindex)"
	ip netns exec "${NODE_NS[$idx]}" "${YNL_CLI}" \
		--spec "${NETDEV_SPEC}" \
		--dump qstats-get \
		--json "{\"ifindex\":${ifindex},\"scope\":[\"queue\"]}" \
		--output-json >"${out}"
}

summarize_rx_qstats()
{
	local label="$1"
	local json="$2"

	if ! command -v jq >/dev/null 2>&1; then
		log "${label}: qstats saved to ${json}; install jq for summaries"
		return 0
	fi

	jq -r --arg label "${label}" --argjson active_bytes "${P2P_QSTAT_ACTIVE_BYTES}" '
		[.[] | select(."queue-type" == "rx")] as $rx |
		[$rx[] | select(."rx-bytes" > $active_bytes)] as $active_rx |
		[.[] | select(."queue-type" == "tx")] as $tx |
		[$tx[] | select(."tx-bytes" > $active_bytes)] as $active_tx |
		"\($label): rx_queues=\($rx | length) active_rx_payload_queues=\($active_rx | length) total_rx_bytes=\(($rx | map(."rx-bytes") | add) // 0) max_rx_bytes=\(($rx | map(."rx-bytes") | max) // 0) min_active_rx_bytes=\(($active_rx | map(."rx-bytes") | min) // 0) tx_queues=\($tx | length) active_tx_payload_queues=\($active_tx | length) total_tx_bytes=\(($tx | map(."tx-bytes") | add) // 0) max_tx_bytes=\(($tx | map(."tx-bytes") | max) // 0) min_active_tx_bytes=\(($active_tx | map(."tx-bytes") | min) // 0)"
	' "${json}" | tee "${json%.json}.summary"
}

enable_p2p_zcrx_receive()
{
	local dir="/sys/kernel/debug/netdevsim/netdevsim${NODE_IDS[0]}/ports/0/zcrx"

	if [ "${P2P_ENGINE}" != "uring" ] || [ "${P2P_URING_RECV_MODE}" != "zcrx" ]; then
		return 0
	fi
	if [ ! -d "${dir}" ]; then
		log "missing netdevsim zcrx debugfs dir: ${dir}"
		exit 1
	fi
	echo 1 > "${dir}/rx_netmem"
	if [ -e "${dir}/rx_payload_nocopy" ]; then
		echo "${P2P_ZCRX_RX_PAYLOAD_NOCOPY}" > "${dir}/rx_payload_nocopy"
	fi
	log "enabled p2p ZCRX receive on netdevsim${NODE_IDS[0]} rxq=${P2P_ZCRX_RXQ} rxq_count=${P2P_ZCRX_RXQ_COUNT} payload_nocopy=${P2P_ZCRX_RX_PAYLOAD_NOCOPY}"
}

run_p2p_mux_bench()
{
	local total_connections=$((P2P_PORTS * P2P_CONNECTIONS_PER_PORT))
	local total_bytes=$((total_connections * P2P_BYTES_PER_CONNECTION))
	local server_cmd client_cmd

	case "${P2P_ENGINE}" in
		std)
			server_cmd=tcp-bench-mux-server
			client_cmd=tcp-bench-mux-send
			;;
		uring)
			server_cmd=tcp-bench-uring-mux-server
			client_cmd=tcp-bench-uring-mux-send
			;;
		*)
			log "unknown P2P_ENGINE=${P2P_ENGINE} (use std or uring)"
			exit 1
			;;
	esac

	enable_p2p_zcrx_receive
	log "running p2p TCP mux node2 -> node1 engine=${P2P_ENGINE} uring_recv_mode=${P2P_URING_RECV_MODE} uring_send_mode=${P2P_URING_SEND_MODE} ports=${P2P_PORTS} connections_per_port=${P2P_CONNECTIONS_PER_PORT} total_connections=${total_connections} bytes=${total_bytes}"
	if [ "${P2P_ENGINE}" = "uring" ]; then
		local -a server_args=(
			"${URING_PLAY}"
			"${server_cmd}" 0.0.0.0 "${P2P_BASE_PORT}" "${P2P_PORTS}" \
			"${P2P_CONNECTIONS_PER_PORT}" "${P2P_BYTES_PER_CONNECTION}" \
			"${P2P_URING_WORKERS}" "${P2P_URING_RECV_BYTES}" "${P2P_URING_ENTRIES}"
		)
		if [ "${P2P_URING_RECV_MODE}" = "zcrx" ]; then
			server_args+=("${P2P_URING_RECV_MODE}" raft0 "${P2P_ZCRX_RXQ}" "${P2P_ZCRX_RXQ_COUNT}")
		else
			server_args+=("${P2P_URING_RECV_MODE}")
		fi
		run_node 0 "${server_args[@]}" >"${RUN_DIR}/p2p-server-node1.log" 2>&1 &
	else
		run_node 0 "${URING_PLAY}" \
			"${server_cmd}" 0.0.0.0 "${P2P_BASE_PORT}" "${P2P_PORTS}" \
			"${P2P_CONNECTIONS_PER_PORT}" "${P2P_BYTES_PER_CONNECTION}" \
			>"${RUN_DIR}/p2p-server-node1.log" 2>&1 &
	fi
	CHILD_PIDS+=("$!")
	sleep 1

	if [ "${P2P_ENGINE}" = "uring" ]; then
		run_node 1 "${URING_PLAY}" \
			"${client_cmd}" "${NODE_IPS[0]}" "${P2P_BASE_PORT}" "${P2P_PORTS}" \
			"${P2P_CONNECTIONS_PER_PORT}" "${P2P_BYTES_PER_CONNECTION}" \
			"${P2P_CHUNK_BYTES}" "${P2P_URING_PIPELINE}" \
			"${P2P_URING_WORKERS}" "${P2P_URING_ENTRIES}" \
			"${P2P_URING_SEND_MODE}" \
			| tee "${RUN_DIR}/p2p-client-node2.log"
	else
		run_node 1 "${URING_PLAY}" \
			"${client_cmd}" "${NODE_IPS[0]}" "${P2P_BASE_PORT}" "${P2P_PORTS}" \
			"${P2P_CONNECTIONS_PER_PORT}" "${P2P_BYTES_PER_CONNECTION}" "${P2P_CHUNK_BYTES}" \
			| tee "${RUN_DIR}/p2p-client-node2.log"
	fi

	wait_children
	dump_node_qstats 0 "${RUN_DIR}/p2p-server-node1-qstats.json"
	dump_node_qstats 1 "${RUN_DIR}/p2p-client-node2-qstats.json"
	summarize_rx_qstats "p2p-server-node1" "${RUN_DIR}/p2p-server-node1-qstats.json"
	summarize_rx_qstats "p2p-client-node2" "${RUN_DIR}/p2p-client-node2-qstats.json"
	log "p2p TCP mux bench: ok"
}

run_raft_bench()
{
	local peers

	peers="$(leader_peer_addrs)"
	log "running raft transport bench leader=node1 followers=$((NODES - 1)) entries=${RAFT_ENTRIES} payload=${RAFT_PAYLOAD_BYTES}"
	for i in $(seq 2 "${NODES}"); do
		local idx=$((i - 1))
		run_node "${idx}" "${URING_PLAY}" \
			raft-follower 0.0.0.0 "${RAFT_PORT}" "${RAFT_ENTRIES}" \
			"${RAFT_PAYLOAD_BYTES}" "${RAFT_ACK_STRIDE}" \
			>"${RUN_DIR}/raft-follower-node${i}.log" 2>&1 &
		CHILD_PIDS+=("$!")
	done
	sleep 1

	run_node 0 "${URING_PLAY}" \
		raft-leader "${peers}" "${RAFT_ENTRIES}" "${RAFT_PAYLOAD_BYTES}" "${RAFT_ACK_STRIDE}" \
		| tee "${RUN_DIR}/raft-leader-node1.log"

	wait_children
	log "raft transport bench: ok"
}

run_zcrx_smoke()
{
	if [ "${NODES}" -lt 2 ]; then
		log "zcrx-smoke requires at least two nodes"
		exit 1
	fi

	local zcrx_dfs="/sys/kernel/debug/netdevsim/netdevsim${NODE_IDS[0]}/ports/0/zcrx"

	log "running fixed-payload ZCRX smoke node2 -> node1"
	echo 1 > "${zcrx_dfs}/rx_netmem"
	run_node 0 "${URING_PLAY}" \
		recv-zc-server raft0 0 "${SMOKE_PORT}" "${SMOKE_BYTES}" 0x5a \
		>"${RUN_DIR}/zcrx-server-node1.log" 2>&1 &
	CHILD_PIDS+=("$!")
	sleep 1
	run_node 1 "${URING_PLAY}" \
		tcp-send "${NODE_IPS[0]}" "${SMOKE_PORT}" "${SMOKE_BYTES}" 0x5a \
		>"${RUN_DIR}/zcrx-client-node2.log" 2>&1
	wait_children
	echo 0 > "${zcrx_dfs}/rx_netmem"

	local packets bytes
	packets="$(cat "${zcrx_dfs}/rx_netmem_packets")"
	bytes="$(cat "${zcrx_dfs}/rx_netmem_bytes")"
	log "ZCRX fixed smoke packets=${packets} bytes=${bytes}"
	if [ "${packets}" -le 0 ] || [ "${bytes}" -le 0 ]; then
		log "ZCRX fixed smoke failed"
		exit 1
	fi
	log "ZCRX fixed smoke: ok"
}

run_custom_raft_cmd()
{
	local peers_all

	peers_all="$(peer_list)"
	if [ -z "${RAFT_CMD:-}" ]; then
		log "MODE=custom requires RAFT_CMD"
		exit 1
	fi

	log "running custom RAFT_CMD for ${NODES} nodes"
	for i in $(seq 1 "${NODES}"); do
		local idx=$((i - 1))
		local data_dir="${RUN_DIR}/node${i}"
		local peers

		peers="$(peer_list_except "${i}")"
		mkdir -p "${data_dir}"
		run_node "${idx}" env \
			RAFT_NODE_ID="${i}" \
			RAFT_BIND="${NODE_IPS[$idx]}:${RAFT_PORT}" \
			RAFT_PEERS="${peers}" \
			RAFT_PEERS_ALL="${peers_all}" \
			RAFT_DATA_DIR="${data_dir}" \
			RAFT_IFACE=raft0 \
			bash -lc "${RAFT_CMD}" \
			>"${RUN_DIR}/custom-node${i}.log" 2>&1 &
		CHILD_PIDS+=("$!")
	done

	if [ "${RUN_SECONDS}" -gt 0 ]; then
		sleep "${RUN_SECONDS}"
		log "custom run duration elapsed; stopping children"
		for pid in "${CHILD_PIDS[@]}"; do
			kill "${pid}" 2>/dev/null || true
		done
		for pid in "${CHILD_PIDS[@]}"; do
			wait "${pid}" 2>/dev/null || true
		done
		CHILD_PIDS=()
		log "custom RAFT_CMD run: ok"
		return
	fi
	wait_children
	log "custom RAFT_CMD run: ok"
}

run_raft_zero_copy()
{
	require_file "${RZC_BIN}"
	require_file "${RZC_BENCH}"

	local io_args=()
	if [ "${RZC_IO_URING}" = "1" ]; then
		io_args+=(--io-uring --uring-entries "${RZC_URING_ENTRIES}")
		if [ "${RZC_SEND_ZC}" = "1" ]; then
			io_args+=(--send-zc --send-zc-chunk "${RZC_SEND_ZC_CHUNK}" --send-zc-depth "${RZC_SEND_ZC_DEPTH}")
		fi
	fi
	if [ "${RZC_DISCARD_WRITES}" = "1" ]; then
		io_args+=(--discard-writes --synthetic-payload-byte "${RZC_SYNTHETIC_PAYLOAD_BYTE}")
	fi
	io_args+=(--replication-lanes "${RZC_REPLICATION_LANES}")

	log "running raft-zero-copy cluster nodes=${NODES} count=${RZC_COUNT} size=${RZC_SIZE} batch=${RZC_BATCH} clients=${RZC_CLIENTS} client_pipeline=${RZC_CLIENT_PIPELINE} client_lanes=${RZC_CLIENT_LANES} io_uring=${RZC_IO_URING} send_zc=${RZC_SEND_ZC} lanes=${RZC_REPLICATION_LANES} thread_pin=${RZC_THREAD_PIN} discard_writes=${RZC_DISCARD_WRITES}"
	for i in $(seq 1 "${NODES}"); do
		local idx=$((i - 1))
		local data_dir="${RUN_DIR}/rzc-node${i}"
		local peers
		local -a pin_args=()
		local -a node_cpus=()

		peers="$(peer_list_except "${i}")"
		if [ "${RZC_THREAD_PIN}" = "1" ] && [ -n "${NODE_CPUSETS[$idx]:-}" ]; then
			IFS=, read -r -a node_cpus <<< "${NODE_CPUSETS[$idx]}"
			if [ "${#node_cpus[@]}" -gt 0 ]; then
				pin_args=(--pin-base-cpu "${node_cpus[0]}" --pin-cpus "${#node_cpus[@]}" --pin-stride 1)
			fi
		fi
		mkdir -p "${data_dir}"
		if [ "${i}" -eq 1 ]; then
			run_node "${idx}" "${RZC_BIN}" \
				--id "${i}" \
				--rpc-addr "${NODE_IPS[$idx]}:${RAFT_PORT}" \
				--client-addr "${NODE_IPS[$idx]}:${RZC_CLIENT_PORT}" \
				--client-lanes "${RZC_CLIENT_LANES}" \
				--data-dir "${data_dir}" \
				--peers "${peers}" \
				--force-leader \
				--disable-election \
				--parallel-replication \
				"${pin_args[@]}" \
				"${io_args[@]}" \
				>"${RUN_DIR}/rzc-node${i}.log" 2>&1 &
		else
			run_node "${idx}" "${RZC_BIN}" \
				--id "${i}" \
				--rpc-addr "${NODE_IPS[$idx]}:${RAFT_PORT}" \
				--data-dir "${data_dir}" \
				--peers "${peers}" \
				--disable-election \
				"${pin_args[@]}" \
				"${io_args[@]}" \
				>"${RUN_DIR}/rzc-node${i}.log" 2>&1 &
		fi
		CHILD_PIDS+=("$!")
	done

	sleep 1
	run_node 0 "${RZC_BENCH}" \
		--addr "${NODE_IPS[0]}:${RZC_CLIENT_PORT}" \
		--count "${RZC_COUNT}" \
		--size "${RZC_SIZE}" \
		--batch "${RZC_BATCH}" \
		--clients "${RZC_CLIENTS}" \
		--ports "${RZC_CLIENT_LANES}" \
		--pipeline "${RZC_CLIENT_PIPELINE}" \
		| tee "${RUN_DIR}/rzc-bench.log"

	for pid in "${CHILD_PIDS[@]}"; do
		kill "${pid}" 2>/dev/null || true
	done
	for pid in "${CHILD_PIDS[@]}"; do
		wait "${pid}" 2>/dev/null || true
	done
	CHILD_PIDS=()
	pkill -TERM -f "${RUN_DIR}/rzc-node" 2>/dev/null || true
	sleep 0.2
	pkill -KILL -f "${RUN_DIR}/rzc-node" 2>/dev/null || true
	for i in $(seq 1 "${NODES}"); do
		local idx=$((i - 1))
		dump_node_qstats "${idx}" "${RUN_DIR}/rzc-node${i}-qstats.json"
		summarize_rx_qstats "rzc-node${i}" "${RUN_DIR}/rzc-node${i}-qstats.json"
	done
	log "raft-zero-copy bench: ok"
}

run_slotbench()
{
	require_file "${SLOTBENCH_BIN}"
	if [ "${NODES}" -lt 3 ]; then
		log "slotbench requires at least 3 nodes"
		exit 1
	fi
	if [ "${SLOT_SEND_ZC}" = "1" ] && ! running_under_qemu &&
		[ "${ALLOW_UNSAFE_HOST_SEND_ZC}" != "1" ]; then
		log "refusing host slotbench SEND_ZC after prior host zerocopy crashes; set ALLOW_UNSAFE_HOST_SEND_ZC=1 to override"
		exit 1
	fi

	local common_args=(
		--slots "${SLOT_SLOTS}"
		--slots-per-frame "${SLOT_SLOTS_PER_FRAME}"
		--pipeline "${SLOT_PIPELINE}"
		--send-burst "${SLOT_SEND_BURST}"
		--ack-every "${SLOT_ACK_EVERY}"
		--lanes "${SLOT_LANES}"
		--wait "${SLOT_WAIT}"
		--uring-entries "${SLOT_URING_ENTRIES}"
	)
	if [ "${SLOT_IO_URING}" = "1" ]; then
		common_args+=(--io-uring)
	fi
	if [ "${SLOT_SEND_ZC}" = "1" ]; then
		common_args+=(--send-zc)
	fi
	if [ "${SLOT_VALIDATE}" = "1" ]; then
		common_args+=(--validate-records)
	fi

	log "running slotbench nodes=${NODES} slots=${SLOT_SLOTS} slots_per_frame=${SLOT_SLOTS_PER_FRAME} lanes=${SLOT_LANES} pipeline=${SLOT_PIPELINE} burst=${SLOT_SEND_BURST} ack_every=${SLOT_ACK_EVERY} wait=${SLOT_WAIT} io_uring=${SLOT_IO_URING} send_zc=${SLOT_SEND_ZC} pin_threads=${SLOT_PIN_THREADS}"
	for i in $(seq 2 "${NODES}"); do
		local idx=$((i - 1))
		local pin_args=()
		mapfile -t pin_args < <(slotbench_pin_args "${idx}")
		run_node "${idx}" "${SLOTBENCH_BIN}" follower \
			--bind "${NODE_IPS[$idx]}:${SLOT_PORT}" \
			"${common_args[@]}" \
			"${pin_args[@]}" \
			>"${RUN_DIR}/slotbench-follower-node${i}.log" 2>&1 &
		CHILD_PIDS+=("$!")
	done

	sleep 1
	local leader_pin_args=()
	mapfile -t leader_pin_args < <(slotbench_pin_args 0)
	run_node 0 "${SLOTBENCH_BIN}" leader \
		--peers "$(slotbench_peer_addrs)" \
		"${common_args[@]}" \
		"${leader_pin_args[@]}" \
		| tee "${RUN_DIR}/slotbench-leader-node1.log"

	wait_children
	for i in $(seq 1 "${NODES}"); do
		local idx=$((i - 1))
		dump_node_qstats "${idx}" "${RUN_DIR}/slotbench-node${i}-qstats.json"
		summarize_rx_qstats "slotbench-node${i}" "${RUN_DIR}/slotbench-node${i}-qstats.json"
	done
	log "slotbench: ok"
}

wait_children()
{
	local failed=0

	for pid in "${CHILD_PIDS[@]:-}"; do
		if ! wait "${pid}"; then
			failed=1
		fi
	done
	CHILD_PIDS=()
	if [ "${failed}" -ne 0 ]; then
		log "one or more child processes failed; logs are in ${RUN_DIR}"
		exit 1
	fi
}

main()
{
	require_root
	require_file "${URING_PLAY}"
	require_file "${NETDEVSIM_KO}"
	if [ "${NODES}" -lt 2 ]; then
		printf 'NODES must be at least 2\n' >&2
		exit 1
	fi

	mkdir -p "${RUN_DIR}"
	guard_host_send_zc
	load_modules
	setup_lab
	nsim_debugfs_snapshot "${MODE}-before"

	case "${MODE}" in
		tcp-smoke)
			run_tcp_smoke
			;;
		p2p-mux)
			run_p2p_mux_bench
			;;
		raft-bench)
			run_raft_bench
			;;
		zcrx-smoke)
			run_zcrx_smoke
			;;
		raft-zero-copy)
			run_raft_zero_copy
			;;
		slotbench)
			run_slotbench
			;;
		all)
			run_tcp_smoke
			run_raft_bench
			run_zcrx_smoke
			;;
		custom)
			run_custom_raft_cmd
			;;
		*)
			printf 'unknown mode %s (use tcp-smoke, p2p-mux, raft-bench, zcrx-smoke, raft-zero-copy, slotbench, all, custom)\n' "${MODE}" >&2
			exit 1
			;;
	esac
	nsim_debugfs_snapshot "${MODE}-after"
	nsim_debugfs_delta "${MODE}-before" "${MODE}-after" "${MODE}"

	log "logs: ${RUN_DIR}"
}

main "$@"
