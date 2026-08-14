#!/usr/bin/env bash
set -euo pipefail

phase="${1:?usage: zcutils-block-matrix-node.sh PHASE [ARGS...]}"
shift

ROOT="${ZCUTILS_ROOT:-/home/ubuntu/zcutils}"
RUN_ID="${URING_RUN_ID:?URING_RUN_ID is required}"
NODE_INDEX="${URING_NODE_INDEX:?URING_NODE_INDEX is required}"
PRIVATE_IPS="${URING_PRIVATE_IPS:?URING_PRIVATE_IPS is required}"
RUN_ROOT="$ROOT/bench-results/$RUN_ID"
BOOTSTRAP_MANIFEST="${ZCUTILS_BOOTSTRAP_MANIFEST:-$HOME/.local/state/zcutils/adhoc-bootstrap.env}"
BASE_PORT=29000
OFI_CONTROL_PORT_OFFSET=1000
NVMET_PORT=4420
NVMET_NQN="nqn.2026-08.io.zcutils:${RUN_ID}"
NVMET_CONFIG_PORT=1

IFS=, read -r CLIENT_IP LEAF_IP extra_ip <<<"$PRIVATE_IPS"
[ -n "$CLIENT_IP" ] && [ -n "$LEAF_IP" ] && [ -z "${extra_ip:-}" ] || {
	printf 'expected exactly two private IPs, got %s\n' "$PRIVATE_IPS" >&2
	exit 1
}

die() {
	printf 'zcutils-block-matrix-node: %s\n' "$*" >&2
	exit 1
}

tree_root() {
	case "$1" in
	current) printf '%s\n' "$ROOT" ;;
	base) printf '%s\n' "${ZCUTILS_BASE_ROOT:-/home/ubuntu/zcutils-base}" ;;
	*) die "unknown source tree $1" ;;
	esac
}

csv_map() {
	local label="$1" values="$2" index=0 value output=""
	IFS=, read -r -a parts <<<"$values"
	for value in "${parts[@]}"; do
		[ -z "$output" ] || output+=,
		output+="$index:$label$value"
		index=$((index + 1))
	done
	printf '%s\n' "$output"
}

worker_cpus() {
	case "$1" in
	1) printf '3\n' ;;
	2) printf '3,35\n' ;;
	4) printf '3,19,35,51\n' ;;
	8) printf '3,11,19,27,35,43,51,59\n' ;;
	*) die "campaign supports 1, 2, 4, or 8 workers, got $1" ;;
	esac
}

client_cpus() {
	case "$1" in
	1) printf '0\n' ;;
	2) printf '0,32\n' ;;
	4) printf '0,16,32,48\n' ;;
	8) printf '0,8,16,24,32,40,48,56\n' ;;
	*) die "campaign supports 1, 2, 4, or 8 workers, got $1" ;;
	esac
}

target_cpus() {
	case "$1" in
	1) printf '1\n' ;;
	2) printf '1,33\n' ;;
	4) printf '1,17,33,49\n' ;;
	8) printf '1,9,17,25,33,41,49,57\n' ;;
	*) die "campaign supports 1, 2, 4, or 8 workers, got $1" ;;
	esac
}

kernel_cpus() {
	case "$1" in
	1) printf '2\n' ;;
	2) printf '2,34\n' ;;
	4) printf '2,18,34,50\n' ;;
	8) printf '2,10,18,26,34,42,50,58\n' ;;
	*) die "campaign supports 1, 2, 4, or 8 workers, got $1" ;;
	esac
}

owner_cpus() {
	case "$1" in
	1) printf '18\n' ;;
	2) printf '18,50\n' ;;
	4) printf '4,20,36,52\n' ;;
	8) printf '4,12,20,28,36,44,52,59\n' ;;
	*) die "campaign supports 1, 2, 4, or 8 workers, got $1" ;;
	esac
}

route_interface() {
	local peer="$1"
	ip -o route get "$peer" | awk '{ for (i=1; i<=NF; i++) if ($i == "dev") { print $(i+1); exit } }'
}

route_source() {
	local peer="$1"
	ip -o route get "$peer" | awk '{ for (i=1; i<=NF; i++) if ($i == "src") { print $(i+1); exit } }'
}

safe_stop_pidfile() {
	local pidfile="$1" expected_comm="$2" pid comm
	[ -s "$pidfile" ] || return 0
	pid="$(<"$pidfile")"
	[[ "$pid" =~ ^[0-9]+$ ]] || die "invalid PID in $pidfile"
	[ -r "/proc/$pid/comm" ] || return 0
	comm="$(<"/proc/$pid/comm")"
	[ "$comm" = "$expected_comm" ] || die "refusing to stop pid=$pid comm=$comm; expected $expected_comm"
	kill -TERM "$pid"
	for _ in $(seq 1 200); do
		[ -r "/proc/$pid/comm" ] || break
		sleep 0.05
	done
	[ ! -r "/proc/$pid/comm" ] || die "pid=$pid did not stop"
}

prepare_node() {
	local out="$RUN_ROOT/node$NODE_INDEX" peer iface irq cpu_index=0
	mkdir -p "$out"
	[ -r "$BOOTSTRAP_MANIFEST" ] || die "missing bootstrap manifest $BOOTSTRAP_MANIFEST"
	grep -qx 'coordination_scope=dedicated-adhoc-instance' "$BOOTSTRAP_MANIFEST" || \
		die "bootstrap manifest does not prove dedicated ad-hoc ownership"
	peer="$([ "$NODE_INDEX" = 1 ] && printf '%s' "$LEAF_IP" || printf '%s' "$CLIENT_IP")"
	"$ROOT/scripts/adhoc-nic-low-latency.sh" apply "$out/nic"
	sudo -n systemctl stop irqbalance.service 2>/dev/null || true
	mapfile -t ifaces < <(for netdev in /sys/class/net/*; do
		iface="${netdev##*/}"
		[ "$iface" != lo ] || continue
		[ "$(ethtool -i "$iface" 2>/dev/null | awk '$1 == "driver:" { print $2; exit }')" = ena ] || continue
		printf '%s\n' "$iface"
	done)
	[ "${#ifaces[@]}" -gt 0 ] || die "no ENA interfaces found"
	: >"$out/irq-affinity.log"
	for iface in "${ifaces[@]}"; do
		while read -r irq; do
			[ -n "$irq" ] || continue
			cpu=$((60 + cpu_index % 4))
			sudo -n sh -c 'printf "%s" "$1" > "$2"' sh "$cpu" "/proc/irq/$irq/smp_affinity_list"
			printf 'interface=%s irq=%s cpu=%s effective=%s\n' \
				"$iface" "$irq" "$cpu" "$(<"/proc/irq/$irq/effective_affinity_list")" \
				>>"$out/irq-affinity.log"
			cpu_index=$((cpu_index + 1))
		done < <(awk -F: -v iface="$iface" '$0 ~ iface { gsub(/[[:space:]]/, "", $1); if ($1 ~ /^[0-9]+$/) print $1 }' /proc/interrupts)
	done
	{
		printf 'run_id=%s node_index=%s private_ip=%s peer_ip=%s route_interface=%s route_source=%s\n' \
			"$RUN_ID" "$NODE_INDEX" "$([ "$NODE_INDEX" = 1 ] && printf '%s' "$CLIENT_IP" || printf '%s' "$LEAF_IP")" \
			"$peer" "$(route_interface "$peer")" "$(route_source "$peer")"
		printf 'kernel=%s\n' "$(uname -r)"
		printf 'memlock_kib=%s\n' "$(ulimit -l)"
		awk '/HugePages_Total:|HugePages_Free:|Hugepagesize:/{gsub(":", "", $1); printf "%s=%s\n", tolower($1), $2}' /proc/meminfo
		lscpu -e=CPU,NODE,SOCKET,CORE,ONLINE
	} >"$out/topology.log"
	cp "$BOOTSTRAP_MANIFEST" "$out/bootstrap.env"
	fi_info -p efa -e rdm >"$out/fi-info-efa-rdm.log" 2>&1
	printf 'prepared_node=%s route_interface=%s\n' "$NODE_INDEX" "$(route_interface "$peer")"
}

write_leaf_topology() {
	local tag="$1" transport="$2" lanes="$3" cpus iface nic_map lane_map out peer
	cpus="$(worker_cpus "$lanes")"
	peer="$([ "$NODE_INDEX" = 1 ] && printf '%s' "$LEAF_IP" || printf '%s' "$CLIENT_IP")"
	iface="$(route_interface "$peer")"
	lane_map="$(csv_map '' "$cpus")"
	if [ "$transport" = efa ]; then
		nic_map="$(for ((lane=0; lane<lanes; lane++)); do [ "$lane" -eq 0 ] || printf ','; printf '%s:efa_0/efa_0-rdm' "$lane"; done)"
	else
		nic_map="$(for ((lane=0; lane<lanes; lane++)); do [ "$lane" -eq 0 ] || printf ','; printf '%s:%s' "$lane" "$iface"; done)"
	fi
	out="$RUN_ROOT/topologies/$tag.leaf-topology.log"
	mkdir -p "${out%/*}"
	{
		printf 'lane_to_worker_cpu=%s\n' "$lane_map"
		printf 'lane_to_nic=%s\n' "$nic_map"
		printf 'worker_count=%s\nworker_cpus=%s\n' "$lanes" "$cpus"
		printf 'transport=%s efa_domain=efa_0-rdm efa_device=efa_0\n' "$transport"
		printf 'tcp_interface=%s tcp_route_src=%s\n' "$iface" "$(route_source "$peer")"
		printf 'hugetlb_total_pages=%s\nhugetlb_free_pages=%s\nhugetlb_page_kib=%s\n' \
			"$(awk '/HugePages_Total:/{print $2}' /proc/meminfo)" \
			"$(awk '/HugePages_Free:/{print $2}' /proc/meminfo)" \
			"$(awk '/Hugepagesize:/{print $2}' /proc/meminfo)"
		printf 'memlock_kib=%s\nirq_cpu_set=60-63\n' "$(ulimit -l)"
		printf 'coordination_scope=dedicated-adhoc-instance\n'
	} >"$out"
}

leaf_start() {
	local tag="${1:?tag}" transport="${2:?transport}" mode="${3:?mode}" lanes="${4:?lanes}" qd="${5:?qd}" tree="${6:-current}" rma_write_qd="${7:-$qd}"
	local tree_dir cpus rma_reads=0 rma_writes=0 control_base out pid listeners
	tree_dir="$(tree_root "$tree")"
	write_leaf_topology "$tag" "$transport" "$lanes"
	[ "$NODE_INDEX" = 2 ] || return 0
	[ -x "$tree_dir/target/release/zcnblk-wal-leaf" ] || die "missing WAL leaf in $tree_dir"
	[ "$transport" = tcp ] || [ "$transport" = efa ] || die "transport must be tcp or efa"
	case "$mode" in
	read|rw) [ "$transport" != efa ] || rma_reads=1 ;;
	write) [ "$transport" != efa ] || rma_writes=1 ;;
	*) die "unknown mode $mode" ;;
	esac
	cpus="$(worker_cpus "$lanes")"
	out="$RUN_ROOT/leaf/$tag"
	mkdir -p "$out"
	safe_stop_pidfile "$out/leaf.pid" zcnblk-wal-leaf
	control_base="$BASE_PORT"
	[ "$transport" = tcp ] || control_base=$((BASE_PORT + OFI_CONTROL_PORT_OFFSET))
	listeners="$(ss -H -ltn | awk -v base="$control_base" -v lanes="$lanes" '{p=$4; sub(/^.*:/,"",p); if (p+0>=base && p+0<base+lanes) seen[p]=1} END{for(p in seen)n++; print n+0}')"
	[ "$listeners" -eq 0 ] || die "leaf listener ports are already occupied"
	nohup env \
		URING_PLAY_PIN_CPUS=1 \
		URING_PLAY_PIN_CPU_LIST="$cpus" \
		URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT="$([ "$transport" = efa ] && printf ofi || printf tcp)" \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER=efa \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT=rdm \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS="$rma_reads" \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_WRITES="$rma_writes" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MIN=256 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MAX=65536 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_WAIT_NS=50000 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_HYSTERESIS_NS=10000000 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
		URING_PLAY_OFI_DOMAIN=efa_0-rdm \
		URING_PLAY_OFI_CONTROL_PORT_OFFSET="$OFI_CONTROL_PORT_OFFSET" \
		URING_PLAY_OFI_CQ_SLEEP_NS=0 \
		URING_PLAY_OFI_RMA_WRITE_QD="$rma_write_qd" \
		URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 \
		URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=65536 \
		URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 \
		FI_EFA_USE_DEVICE_RDMA=1 \
		"$tree_dir/target/release/zcnblk-wal-leaf" \
		zcmem:4096M "$LEAF_IP" "$BASE_PORT" "$lanes" 1 4096 "$lanes" true blocking \
		>"$out/leaf.log" 2>&1 </dev/null &
	pid=$!
	printf '%s\n' "$pid" >"$out/leaf.pid"
	for _ in $(seq 1 600); do
		listeners="$(ss -H -ltn | awk -v base="$control_base" -v lanes="$lanes" '{p=$4; sub(/^.*:/,"",p); if (p+0>=base && p+0<base+lanes) seen[p]=1} END{for(p in seen)n++; print n+0}')"
		[ "$listeners" -eq "$lanes" ] && break
		[ -r "/proc/$pid/comm" ] || {
			tail -n 120 "$out/leaf.log" >&2
			die "WAL leaf exited before readiness"
		}
		sleep 0.05
	done
	[ "$listeners" -eq "$lanes" ] || die "WAL leaf did not open $lanes control/listener ports"
	printf 'leaf_ready=true tag=%s transport=%s mode=%s lanes=%s block_qd=%s rma_write_qd=%s pid=%s\n' \
		"$tag" "$transport" "$mode" "$lanes" "$qd" "$rma_write_qd" "$pid"
}

zcnblk_run() {
	local tag="${1:?tag}" transport="${2:?transport}" mode="${3:?mode}" lanes="${4:?lanes}" qd="${5:?qd}" fua="${6:-0}" tree="${7:-current}" rma_write_qd="${8:-$qd}"
	local ops_override="${9:-}" tree_dir read_percent owner_ingress=0 rma_reads=0 rma_writes=0 representative=1 pipeline_batches=16
	local ring_entries=64 shm_entries=128 ops=300000 latency_rate=1 client_cpu target_cpu kernel_cpu owner_cpu out
	[ "$NODE_INDEX" = 1 ] || return 0
	tree_dir="$(tree_root "$tree")"
	case "$mode" in
	read) read_percent=100 ;;
	write) read_percent=0; owner_ingress=1 ;;
	rw) read_percent=50 ;;
	*) die "unknown mode $mode" ;;
	esac
	if [ "$transport" = efa ]; then
		case "$mode" in
		read|rw) rma_reads=1 ;;
		write) rma_writes=1; representative=0; pipeline_batches=1 ;;
		esac
	elif [ "$transport" != tcp ]; then
		die "transport must be tcp or efa"
	fi
	[ "$fua" = 0 ] || [ "$fua" = 1 ] || die "fua must be zero or one"
	[ "$fua" != 1 ] || [ "$mode" != read ] || die "FUA requires writes"
	if [ "$qd" -ge 32 ]; then
		ops=1000000
		latency_rate=64
	fi
	if [ -n "$ops_override" ]; then
		[[ "$ops_override" =~ ^[1-9][0-9]*$ ]] || die "ops override must be a positive integer"
		ops="$ops_override"
	fi
	while [ "$ring_entries" -lt $((qd * 2)) ]; do ring_entries=$((ring_entries * 2)); done
	while [ "$shm_entries" -lt "$qd" ]; do shm_entries=$((shm_entries * 2)); done
	client_cpu="$(client_cpus "$lanes")"
	target_cpu="$(target_cpus "$lanes")"
	kernel_cpu="$(kernel_cpus "$lanes")"
	owner_cpu="$(owner_cpus "$lanes")"
	out="$RUN_ROOT/client/$tag"
	[ -r "$RUN_ROOT/topologies/$tag.leaf-topology.log" ] || die "missing leaf topology for $tag"
	env \
		COORDINATION_SCOPE=dedicated-adhoc \
		ZCUTILS_BOOTSTRAP_MANIFEST="$BOOTSTRAP_MANIFEST" \
		BUILD=0 REPRESENTATIVE="$representative" \
		MODULE="$tree_dir/kmods/zcnblk_client_mod.ko" \
		TARGET_BIN="$tree_dir/target/release/zcnblk-shm-target" \
		BENCH_BIN="$tree_dir/target/release/zcblockbench" \
		ORDER_BIN="$tree_dir/target/release/zcnblk-order-smoke" \
		CONTRACT_BIN="$tree_dir/target/release/zcnblk-contract-smoke" \
		BACKEND=wal-tcp START_LOCAL_LEAF=0 \
		LEAF_ADDR="$LEAF_IP" LEAF_PORT="$BASE_PORT" \
		EXTERNAL_LEAF_TOPOLOGY_ARTIFACT="$RUN_ROOT/topologies/$tag.leaf-topology.log" \
		LANES="$lanes" REPEATS=4 OPS_PER_WORKER="$ops" IODEPTH="$qd" \
		RING_ENTRIES="$ring_entries" SHM_RING_ENTRIES="$shm_entries" SHM_PAYLOAD_ENTRIES=4096 \
		KERNEL_QUEUES="$lanes" KERNEL_QUEUE_DEPTH="$qd" KERNEL_PIPELINE_DEPTH="$qd" \
		MODE="$mode" READ_PERCENT="$read_percent" BUFFER_MODE=hugetlb \
		CLIENT_CPU_LIST="$client_cpu" TARGET_CPU_LIST="$target_cpu" KERNEL_CPU_LIST="$kernel_cpu" \
		URING_PLAY_ZCNBLK_SHM_OWNER_CPU_LIST="$owner_cpu" \
		URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS="$owner_ingress" \
		URING_PLAY_ZCNBLK_SHM_OWNER_COUNT="$lanes" \
		URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_BATCHES="$pipeline_batches" \
		URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW="$qd" \
		URING_PLAY_ZCNBLK_SHM_READ_BATCH=1 \
		URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_US=0 \
		URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT="$([ "$transport" = efa ] && printf ofi || printf tcp)" \
		URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER=efa \
		URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT=rdm \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS="$rma_reads" \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD="$qd" \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES="$rma_writes" \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES_REQUIRED="$rma_writes" \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_QD="$rma_write_qd" \
		URING_PLAY_ZCNBLK_SHM_RMA_SOURCE_HUGETLB_CONFIRMED=0 \
		URING_PLAY_OFI_DOMAIN=efa_0-rdm \
		URING_PLAY_OFI_CQ_SLEEP_NS=0 \
		URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 \
		URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=65536 \
		URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 \
		FI_EFA_USE_DEVICE_RDMA=1 \
		URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED=1 \
		URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE="$latency_rate" \
		URING_PLAY_BLOCKBENCH_RING_STATS=1 \
		URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS="$([ "$qd" -ge 128 ] && printf 16 || printf 1)" \
		URING_PLAY_BLOCKBENCH_CQE_HOT_POLL=1 \
		URING_PLAY_BLOCKBENCH_CQE_HOT_POLL_PROGRESS_SPINS="$([ "$qd" -le 4 ] && printf 256 || printf 4096)" \
		URING_PLAY_BLOCKBENCH_FUA_WRITES="$fua" \
		PERF_STAT=0 KERNEL_STATE_INTERVAL_MS=0 POLL_US=1000 KERNEL_POLL_US=1000 \
		OUTDIR="$out" \
		"$tree_dir/scripts/zcnblk-shm-block-bench.sh"
}

leaf_stop() {
	local tag="${1:?tag}" out pid
	[ "$NODE_INDEX" = 2 ] || return 0
	out="$RUN_ROOT/leaf/$tag"
	[ -s "$out/leaf.pid" ] || return 0
	pid="$(<"$out/leaf.pid")"
	for _ in $(seq 1 200); do
		[ -r "/proc/$pid/comm" ] || break
		sleep 0.05
	done
	if [ -r "/proc/$pid/comm" ]; then
		safe_stop_pidfile "$out/leaf.pid" zcnblk-wal-leaf
	fi
	printf 'leaf_stopped=true tag=%s\n' "$tag" | tee "$out/leaf-stop.log"
}

nvmet_target_setup() {
	local cfg="/sys/kernel/config/nvmet" subsys="$cfg/subsystems/$NVMET_NQN" port="$cfg/ports/$NVMET_CONFIG_PORT"
	local mountpoint="/mnt/zcutils-nvmet-$RUN_ID" backing="$mountpoint/namespace.img" wq mask
	[ "$NODE_INDEX" = 2 ] || return 0
	sudo -n modprobe nvmet
	sudo -n modprobe nvmet-tcp
	mountpoint -q /sys/kernel/config || sudo -n mount -t configfs none /sys/kernel/config
	if [ -L "$port/subsystems/$NVMET_NQN" ]; then sudo -n rm "$port/subsystems/$NVMET_NQN"; fi
	if [ -d "$subsys/namespaces/1" ]; then
		[ ! -e "$subsys/namespaces/1/enable" ] || printf '0' | sudo -n tee "$subsys/namespaces/1/enable" >/dev/null
		sudo -n rmdir "$subsys/namespaces/1"
	fi
	[ ! -d "$subsys" ] || sudo -n rmdir "$subsys"
	[ ! -d "$port" ] || sudo -n rmdir "$port"
	sudo -n mkdir -p "$mountpoint"
	mountpoint -q "$mountpoint" || sudo -n mount -t tmpfs -o size=12G,mode=0755 zcutils-nvmet "$mountpoint"
	sudo -n truncate -s 8G "$backing"
	sudo -n mkdir -p "$subsys/namespaces/1" "$port/subsystems"
	printf '1' | sudo -n tee "$subsys/attr_allow_any_host" >/dev/null
	[ ! -e "$subsys/attr_serial" ] || printf 'zcutils%s' "${RUN_ID: -12}" | sudo -n tee "$subsys/attr_serial" >/dev/null
	[ ! -e "$subsys/attr_model" ] || printf 'zcutils-nvmet-tmpfs' | sudo -n tee "$subsys/attr_model" >/dev/null
	printf '%s' "$backing" | sudo -n tee "$subsys/namespaces/1/device_path" >/dev/null
	printf '1' | sudo -n tee "$subsys/namespaces/1/buffered_io" >/dev/null
	printf '1' | sudo -n tee "$subsys/namespaces/1/enable" >/dev/null
	printf 'ipv4' | sudo -n tee "$port/addr_adrfam" >/dev/null
	printf 'tcp' | sudo -n tee "$port/addr_trtype" >/dev/null
	printf '%s' "$LEAF_IP" | sudo -n tee "$port/addr_traddr" >/dev/null
	printf '%s' "$NVMET_PORT" | sudo -n tee "$port/addr_trsvcid" >/dev/null
	sudo -n ln -s "$subsys" "$port/subsystems/$NVMET_NQN"
	mask="$(printf '%x' $(((1 << 3) | (1 << 35))))"
	mkdir -p "$RUN_ROOT/nvmet/target"
	: >"$RUN_ROOT/nvmet/target/workqueue-topology.log"
	for wq in /sys/bus/workqueue/devices/nvmet*/cpumask; do
		[ -e "$wq" ] || continue
		if [ -w "$wq" ] || sudo -n test -w "$wq"; then
			sudo -n sh -c 'printf "%s" "$1" > "$2"' sh "$mask" "$wq"
		fi
		printf 'path=%s cpumask=%s requested_cpus=3,35\n' "$wq" "$(<"$wq")" >>"$RUN_ROOT/nvmet/target/workqueue-topology.log"
	done
	{
		printf 'nqn=%s transport=tcp bind=%s port=%s namespace=regular-file buffered_io=1\n' "$NVMET_NQN" "$LEAF_IP" "$NVMET_PORT"
		printf 'backing=%s backing_filesystem=tmpfs durability=volatile completion=remote-target-page-cache-or-fua-sync\n' "$backing"
		printf 'target_worker_cpus=3,35 irq_cpus=60-63 route_interface=%s\n' "$(route_interface "$CLIENT_IP")"
		find "$subsys" "$port" -maxdepth 2 -type f -printf '%p=' -exec sh -c 'head -c 256 "$1" 2>/dev/null || true' sh {} \; -printf '\n'
	} >"$RUN_ROOT/nvmet/target/topology.log"
	ss -H -ltn | awk -v port=":$NVMET_PORT" '$4 ~ port "$" {found=1} END{exit !found}' || die "nvmet TCP listener did not start"
	printf 'nvmet_target_ready=true nqn=%s\n' "$NVMET_NQN"
}

nvmet_client_connect() {
	local queues="${1:?queues}" controller="" dev="" part="" uuid="" sys
	[ "$NODE_INDEX" = 1 ] || return 0
	sudo -n modprobe nvme-tcp
	sudo -n nvme disconnect -n "$NVMET_NQN" >/dev/null 2>&1 || true
	sudo -n nvme connect -t tcp -n "$NVMET_NQN" -a "$LEAF_IP" -s "$NVMET_PORT" \
		--nr-io-queues="$queues" --queue-size=1024
	for _ in $(seq 1 200); do
		for sys in /sys/class/nvme/nvme*; do
			[ -r "$sys/subsysnqn" ] || continue
			[ "$(<"$sys/subsysnqn")" = "$NVMET_NQN" ] || continue
			controller="${sys##*/}"
			dev="/dev/${controller}n1"
			[ -b "$dev" ] && break 2
		done
		sleep 0.05
	done
	[ -b "$dev" ] || die "connected NVMe namespace not found"
	part="${dev}p1"
	if ! sudo -n blkid -s PARTUUID -o value "$part" >/dev/null 2>&1; then
		sudo -n parted -s "$dev" mklabel gpt
		sudo -n parted -s "$dev" mkpart primary 4MiB 100%
		sudo -n partprobe "$dev"
		sudo -n udevadm settle
	fi
	uuid="$(sudo -n blkid -s PARTUUID -o value "$part")"
	[ -n "$uuid" ] || die "NVMe partition has no PARTUUID"
	mkdir -p "$RUN_ROOT/nvmet/client"
	printf '%s\n' "$uuid" >"$RUN_ROOT/nvmet/client/raw-partitions.allow"
	{
		printf 'nqn=%q\n' "$NVMET_NQN"
		printf 'controller=%q\nnamespace_device=%q\npartition_device=%q\npartuuid=%q\nio_queues=%q\n' \
			"$controller" "$dev" "$part" "$uuid" "$queues"
	} >"$RUN_ROOT/nvmet/client/client.env"
	{
		printf 'nqn=%s controller=%s namespace=%s partition=%s partuuid=%s io_queues=%s\n' \
			"$NVMET_NQN" "$controller" "$dev" "$part" "$uuid" "$queues"
		printf 'client_worker_cpus=0,32 irq_cpus=60-63 route_interface=%s route_source=%s\n' \
			"$(route_interface "$LEAF_IP")" "$(route_source "$LEAF_IP")"
		nvme list-subsys
		for mq in "/sys/block/${dev##*/}/mq"/*/cpu_list; do printf '%s=%s\n' "$mq" "$(<"$mq")"; done
	} >"$RUN_ROOT/nvmet/client/topology-q${queues}.log"
	printf 'nvmet_client_ready=true device=%s partuuid=%s io_queues=%s\n' "$dev" "$uuid" "$queues"
}

nvme_run() {
	local tag="${1:?tag}" mode="${2:?mode}" workers="${3:?workers}" qd="${4:?qd}" fua="${5:-0}"
	local read_percent cpus ring_entries=64 ops=300000 latency_rate=1 out rep huge_free needed cpu found
	[ "$NODE_INDEX" = 1 ] || return 0
	[ -r "$RUN_ROOT/nvmet/client/client.env" ] || die "NVMe client state is missing"
	# shellcheck disable=SC1090
	source "$RUN_ROOT/nvmet/client/client.env"
	case "$mode" in
	read) read_percent=100 ;;
	write) read_percent=0 ;;
	rw) read_percent=50 ;;
	*) die "unknown mode $mode" ;;
	esac
	[ "$fua" = 0 ] || [ "$fua" = 1 ] || die "fua must be zero or one"
	[ "$fua" != 1 ] || [ "$mode" != read ] || die "FUA requires writes"
	[ "$workers" -le "$io_queues" ] || die "workers=$workers exceeds NVMe IO queues=$io_queues"
	cpus="$(client_cpus "$workers")"
	if [ "$qd" -ge 32 ]; then ops=1000000; latency_rate=64; fi
	while [ "$ring_entries" -lt $((qd * 2)) ]; do ring_entries=$((ring_entries * 2)); done
	huge_free="$(awk '/HugePages_Free:/{print $2}' /proc/meminfo)"
	needed=$((workers * qd))
	[ "$huge_free" -ge "$needed" ] || die "HugeTLB preflight needs $needed pages, found $huge_free"
	[ "$(ulimit -l)" = unlimited ] || die "representative NVMe run requires unlimited memlock"
	IFS=, read -r -a cpu_array <<<"$cpus"
	for cpu in "${cpu_array[@]}"; do
		found=0
		for mq in "/sys/block/${namespace_device##*/}/mq"/*/cpu_list; do
			if awk -v wanted="$cpu" '
				{n=split($0,r,","); for(i=1;i<=n;i++){m=split(r[i],p,"-"); if((m==1&&wanted==p[1])||(m==2&&wanted>=p[1]&&wanted<=p[2])) found=1}}
				END{exit !found}' "$mq"; then found=1; break; fi
		done
		[ "$found" -eq 1 ] || die "client CPU $cpu is absent from NVMe hctx mappings"
	done
	out="$RUN_ROOT/nvmet/runs/$tag"
	mkdir -p "$out"
	{
		printf 'classification=dedicated-adhoc-topology-explicit\ntransport=nvme-tcp\n'
		printf 'per_worker_qd=%s workers=%s lanes=%s aggregate_outstanding_depth=%s\n' "$qd" "$workers" "$io_queues" "$((workers * qd))"
		printf 'lane_to_worker_cpu=%s\n' "$(csv_map '' "$cpus")"
		printf 'client_cpu_list=%s target_worker_cpus=3,35 irq_cpus=60-63\n' "$cpus"
		printf 'hugetlb_free=%s hugetlb_needed=%s memlock_kib=%s\n' "$huge_free" "$needed" "$(ulimit -l)"
		printf 'namespace=%s partuuid=%s io_queues=%s target_backing=tmpfs-regular-file target_buffered_io=1 durability=volatile\n' \
			"$namespace_device" "$partuuid" "$io_queues"
		for mq in "/sys/block/${namespace_device##*/}/mq"/*/cpu_list; do printf '%s=%s\n' "$mq" "$(<"$mq")"; done
		printf 'fua_writes=%s write_completion=%s\n' "$fua" "$([ "$fua" = 1 ] && printf remote-nvme-fua-target-sync || printf remote-nvme-command-target-page-cache-admission)"
	} >"$out/topology.log"
	: >"$out/results.log"
	for rep in 1 2 3 4; do
		cmd=("$ROOT/target/release/zcblockbench" "PARTUUID=$partuuid"
			--engine uring-fixed --mode "$mode" --workers "$workers"
			--ops-per-worker "$ops" --bs 4096 --iodepth "$qd"
			--region-bytes-per-worker 67108864 --read-percent "$read_percent"
			--ring-entries "$ring_entries" --ring-mode normal
			--buffer-mode hugetlb --pin true --latency-sample-rate "$latency_rate")
		[ "$fua" != 1 ] || cmd+=(--fua)
		sudo -n env \
			URING_PLAY_PIN_CPU_LIST="$cpus" \
			URING_PLAY_TOPOLOGY_STRICT=1 \
			URING_PLAY_BLOCKBENCH_RING_STATS=1 \
			URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS="$([ "$qd" -ge 128 ] && printf 16 || printf 1)" \
			URING_PLAY_CQE_HOT_POLL=1 \
			URING_PLAY_CQE_HOT_POLL_PROGRESS_SPINS="$([ "$qd" -le 4 ] && printf 256 || printf 4096)" \
			URING_PLAY_ALLOW_RAW_BLOCK_WRITE=1 \
			URING_PLAY_RAW_TARGET_PARTUUID="$partuuid" \
			URING_PLAY_RAW_PARTITION_ALLOWLIST="$RUN_ROOT/nvmet/client/raw-partitions.allow" \
			"${cmd[@]}" >"$out/rep$rep.log" 2>&1
		grep 'zcblockbench-result:' "$out/rep$rep.log" | tail -n 1 | sed "s/^/repeat=$rep /" | tee -a "$out/results.log"
		grep 'zcblockbench-latency:' "$out/rep$rep.log" | tail -n 1 | sed "s/^/repeat=$rep /" | tee -a "$out/results.log"
	done
}

nvmet_client_disconnect() {
	[ "$NODE_INDEX" = 1 ] || return 0
	sudo -n nvme disconnect -n "$NVMET_NQN" >/dev/null 2>&1 || true
	printf 'nvmet_client_disconnected=true\n'
}

nvmet_target_cleanup() {
	local cfg="/sys/kernel/config/nvmet" subsys="$cfg/subsystems/$NVMET_NQN" port="$cfg/ports/$NVMET_CONFIG_PORT"
	local mountpoint="/mnt/zcutils-nvmet-$RUN_ID"
	[ "$NODE_INDEX" = 2 ] || return 0
	[ ! -L "$port/subsystems/$NVMET_NQN" ] || sudo -n rm "$port/subsystems/$NVMET_NQN"
	if [ -d "$subsys/namespaces/1" ]; then
		printf '0' | sudo -n tee "$subsys/namespaces/1/enable" >/dev/null
		sudo -n rmdir "$subsys/namespaces/1"
	fi
	[ ! -d "$subsys" ] || sudo -n rmdir "$subsys"
	[ ! -d "$port" ] || sudo -n rmdir "$port"
	mountpoint -q "$mountpoint" && sudo -n umount "$mountpoint" || true
	sudo -n rmdir "$mountpoint" 2>/dev/null || true
	printf 'nvmet_target_cleaned=true\n'
}

case "$phase" in
prepare) prepare_node "$@" ;;
leaf-start) leaf_start "$@" ;;
zcnblk-run) zcnblk_run "$@" ;;
leaf-stop) leaf_stop "$@" ;;
nvmet-target-setup) nvmet_target_setup "$@" ;;
nvmet-client-connect) nvmet_client_connect "$@" ;;
nvme-run) nvme_run "$@" ;;
nvmet-client-disconnect) nvmet_client_disconnect "$@" ;;
nvmet-target-cleanup) nvmet_target_cleanup "$@" ;;
*) die "unknown phase $phase" ;;
esac
