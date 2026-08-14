#!/usr/bin/env bash
set -euo pipefail

phase="${1:?usage: fanin-node.sh PHASE [ARGS...]}"
shift

ROOT="${ZCUTILS_ROOT:-/home/ubuntu/zcutils}"
RUN_ID="${URING_RUN_ID:?URING_RUN_ID is required}"
NODE_INDEX="${URING_NODE_INDEX:?URING_NODE_INDEX is required}"
PRIVATE_IPS="${URING_PRIVATE_IPS:?URING_PRIVATE_IPS is required}"
RUN_ROOT="$ROOT/bench-results/$RUN_ID"
BOOTSTRAP_MANIFEST="${ZCUTILS_BOOTSTRAP_MANIFEST:-$HOME/.local/state/zcutils/adhoc-bootstrap.env}"
SOURCE_COMMIT=117ad8ad
BASE_PORT=29000
OFI_CONTROL_PORT_OFFSET=1000

IFS=, read -r CLIENT_IP LEAF_IP extra_ip <<<"$PRIVATE_IPS"
[ -n "$CLIENT_IP" ] && [ -n "$LEAF_IP" ] && [ -z "${extra_ip:-}" ] || {
	printf 'expected exactly two private IPs, got %s\n' "$PRIVATE_IPS" >&2
	exit 1
}

die() {
	printf 'fanin-node: %s\n' "$*" >&2
	exit 1
}

csv_map() {
	local values="$1" index=0 value output=""
	IFS=, read -r -a parts <<<"$values"
	for value in "${parts[@]}"; do
		[ -z "$output" ] || output+=,
		output+="$index:$value"
		index=$((index + 1))
	done
	printf '%s\n' "$output"
}

prefix_cpus() {
	local values="$1" count="$2" output="" index
	IFS=, read -r -a parts <<<"$values"
	[ "$count" -le "${#parts[@]}" ] || die "CPU prefix $count exceeds ${#parts[@]} entries"
	for ((index = 0; index < count; index++)); do
		[ -z "$output" ] || output+=,
		output+="${parts[$index]}"
	done
	printf '%s\n' "$output"
}

client_cpus() {
	case "$1" in
	1) printf '0\n' ;;
	2) printf '0,32\n' ;;
	4) printf '0,16,32,48\n' ;;
	8) printf '0,8,16,24,32,40,48,56\n' ;;
	*) die "supported block lane counts are 1, 2, 4, and 8" ;;
	esac
}

target_cpus() {
	case "$1" in
	1) printf '1\n' ;;
	2) printf '1,33\n' ;;
	4) printf '1,17,33,49\n' ;;
	8) printf '1,9,17,25,33,41,49,57\n' ;;
	*) die "supported block lane counts are 1, 2, 4, and 8" ;;
	esac
}

kernel_cpus() {
	case "$1" in
	1) printf '2\n' ;;
	2) printf '2,34\n' ;;
	4) printf '2,18,34,50\n' ;;
	8) printf '2,10,18,26,34,42,50,58\n' ;;
	*) die "supported block lane counts are 1, 2, 4, and 8" ;;
	esac
}

owner_cpu_pool() {
	case "$1" in
	1) printf '18\n' ;;
	2) printf '18,50\n' ;;
	4) printf '4,20,36,52\n' ;;
	8) printf '4,12,20,28,36,44,52,59\n' ;;
	*) die "supported block lane counts are 1, 2, 4, and 8" ;;
	esac
}

leaf_cpu_pool() {
	case "$1" in
	1) printf '3\n' ;;
	2) printf '3,35\n' ;;
	4) printf '3,19,35,51\n' ;;
	8) printf '3,11,19,27,35,43,51,59\n' ;;
	*) die "supported endpoint counts are 1, 2, 4, and 8" ;;
	esac
}

route_interface() {
	ip -o route get "$1" | awk '{for (i=1; i<=NF; i++) if ($i=="dev") {print $(i+1); exit}}'
}

route_source() {
	ip -o route get "$1" | awk '{for (i=1; i<=NF; i++) if ($i=="src") {print $(i+1); exit}}'
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
	local out="$RUN_ROOT/node$NODE_INDEX" peer iface irq cpu_index=0 cpu
	mkdir -p "$out"
	[ -r "$BOOTSTRAP_MANIFEST" ] || die "missing bootstrap manifest"
	grep -qx 'coordination_scope=dedicated-adhoc-instance' "$BOOTSTRAP_MANIFEST" || die "invalid bootstrap scope"
	peer="$([ "$NODE_INDEX" = 1 ] && printf '%s' "$LEAF_IP" || printf '%s' "$CLIENT_IP")"
	"$ROOT/scripts/adhoc-nic-low-latency.sh" apply "$out/nic"
	sudo -n systemctl stop irqbalance.service 2>/dev/null || true
	: >"$out/irq-affinity.log"
	for netdev in /sys/class/net/*; do
		iface="${netdev##*/}"
		[ "$iface" != lo ] || continue
		[ "$(ethtool -i "$iface" 2>/dev/null | awk '$1=="driver:" {print $2; exit}')" = ena ] || continue
		while read -r irq; do
			[ -n "$irq" ] || continue
			cpu=$((60 + cpu_index % 4))
			sudo -n sh -c 'printf "%s" "$1" > "$2"' sh "$cpu" "/proc/irq/$irq/smp_affinity_list"
			printf 'interface=%s irq=%s cpu=%s effective=%s\n' "$iface" "$irq" "$cpu" \
				"$(<"/proc/irq/$irq/effective_affinity_list")" >>"$out/irq-affinity.log"
			cpu_index=$((cpu_index + 1))
		done < <(awk -F: -v iface="$iface" '$0~iface {gsub(/[[:space:]]/,"",$1); if($1~/^[0-9]+$/) print $1}' /proc/interrupts)
	done
	{
		printf 'source_commit=%s run_id=%s node_index=%s\n' "$SOURCE_COMMIT" "$RUN_ID" "$NODE_INDEX"
		printf 'client_ip=%s leaf_ip=%s peer_ip=%s route_interface=%s route_source=%s\n' \
			"$CLIENT_IP" "$LEAF_IP" "$peer" "$(route_interface "$peer")" "$(route_source "$peer")"
		printf 'kernel=%s memlock_kib=%s\n' "$(uname -r)" "$(ulimit -l)"
		awk '/HugePages_Total:|HugePages_Free:|Hugepagesize:/{gsub(":","",$1); printf "%s=%s\n",tolower($1),$2}' /proc/meminfo
		fi_info -p efa -e rdm | sed -n '1,80p'
		lscpu -e=CPU,NODE,SOCKET,CORE,ONLINE
	} >"$out/topology.log"
	cp "$BOOTSTRAP_MANIFEST" "$out/bootstrap.env"
	printf 'prepared_node=%s route_interface=%s\n' "$NODE_INDEX" "$(route_interface "$peer")"
}

write_leaf_topology() {
	local tag="$1" transport="$2" endpoints="$3" cpus iface lane_map nic_map out lane
	cpus="$(leaf_cpu_pool "$endpoints")"
	iface="$(route_interface "$CLIENT_IP")"
	lane_map="$(csv_map "$cpus")"
	if [ "$transport" = efa ]; then
		nic_map="$(for ((lane=0; lane<endpoints; lane++)); do [ "$lane" -eq 0 ] || printf ','; printf '%s:efa_0/efa_0-rdm' "$lane"; done)"
	else
		nic_map="$(for ((lane=0; lane<endpoints; lane++)); do [ "$lane" -eq 0 ] || printf ','; printf '%s:%s' "$lane" "$iface"; done)"
	fi
	out="$RUN_ROOT/topologies/$tag.leaf-topology.log"
	mkdir -p "${out%/*}"
	{
		printf 'source_commit=%s\n' "$SOURCE_COMMIT"
		printf 'lane_to_worker_cpu=%s\nlane_to_nic=%s\n' "$lane_map" "$nic_map"
		printf 'worker_count=%s worker_cpus=%s remote_endpoint_count=%s\n' "$endpoints" "$cpus" "$endpoints"
		printf 'transport=%s efa_domain=efa_0-rdm efa_device=efa_0\n' "$transport"
		printf 'tcp_interface=%s tcp_route_src=%s irq_cpu_set=60-63\n' "$iface" "$(route_source "$CLIENT_IP")"
		printf 'hugetlb_total_pages=%s hugetlb_free_pages=%s memlock_kib=%s\n' \
			"$(awk '/HugePages_Total:/{print $2}' /proc/meminfo)" \
			"$(awk '/HugePages_Free:/{print $2}' /proc/meminfo)" "$(ulimit -l)"
		printf 'coordination_scope=dedicated-adhoc-instance\n'
	} >"$out"
}

leaf_start() {
	local tag="${1:?tag}" transport="${2:?transport}" mode="${3:?mode}" endpoints="${4:?endpoints}" rma_qd="${5:-64}"
	local cpus rma_reads=0 rma_writes=0 control_base out pid listeners
	local leaf_target="${ZCUTILS_LEAF_TARGET:-zcmem:4096M}"
	write_leaf_topology "$tag" "$transport" "$endpoints"
	[ "$NODE_INDEX" = 2 ] || return 0
	case "$transport:$mode" in
		efa:read|efa:rw) rma_reads=1 ;;
		efa:write) rma_writes=1 ;;
		tcp:read|tcp:write|tcp:rw) ;;
		*) die "unsupported transport/mode $transport/$mode" ;;
	esac
	cpus="$(leaf_cpu_pool "$endpoints")"
	out="$RUN_ROOT/leaf/$tag"
	mkdir -p "$out"
	safe_stop_pidfile "$out/leaf.pid" zcnblk-wal-leaf
	control_base="$BASE_PORT"
	[ "$transport" = tcp ] || control_base=$((BASE_PORT + OFI_CONTROL_PORT_OFFSET))
	listeners="$(ss -H -ltn | awk -v base="$control_base" -v n="$endpoints" '{p=$4;sub(/^.*:/,"",p);if(p+0>=base&&p+0<base+n)seen[p]=1}END{for(p in seen)c++;print c+0}')"
	[ "$listeners" -eq 0 ] || die "leaf listener ports already occupied"
	nohup env URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST="$cpus" \
		URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT="$([ "$transport" = efa ] && printf ofi || printf tcp)" \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER=efa URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT=rdm \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS="$rma_reads" \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_WRITES="$rma_writes" \
		URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MIN=256 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MAX=65536 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_WAIT_NS=50000 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_HYSTERESIS_NS=10000000 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
		URING_PLAY_OFI_DOMAIN=efa_0-rdm URING_PLAY_OFI_CONTROL_PORT_OFFSET="$OFI_CONTROL_PORT_OFFSET" \
		URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_RMA_WRITE_QD="$rma_qd" \
		URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=1048576 \
		URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 FI_EFA_USE_DEVICE_RDMA=1 \
		"$ROOT/target/release/zcnblk-wal-leaf" "$leaf_target" "$LEAF_IP" "$BASE_PORT" \
		"$endpoints" 1 4096 "$endpoints" true blocking >"$out/leaf.log" 2>&1 </dev/null &
	pid=$!
	printf '%s\n' "$pid" >"$out/leaf.pid"
	for _ in $(seq 1 600); do
		listeners="$(ss -H -ltn | awk -v base="$control_base" -v n="$endpoints" '{p=$4;sub(/^.*:/,"",p);if(p+0>=base&&p+0<base+n)seen[p]=1}END{for(p in seen)c++;print c+0}')"
		[ "$listeners" -eq "$endpoints" ] && break
		[ -r "/proc/$pid/comm" ] || { tail -n 120 "$out/leaf.log" >&2; die "leaf exited before readiness"; }
		sleep 0.05
	done
	[ "$listeners" -eq "$endpoints" ] || die "leaf readiness timed out"
	printf 'leaf_ready=true tag=%s transport=%s mode=%s endpoints=%s rma_qd=%s target=%s pid=%s\n' \
		"$tag" "$transport" "$mode" "$endpoints" "$rma_qd" "$leaf_target" "$pid"
}

zcnblk_run() {
	local tag="${1:?tag}" transport="${2:?transport}" mode="${3:?mode}" lanes="${4:?lanes}" qd="${5:?qd}"
	local owners="${6:?owners}" owner_mode="${7:-placement}" rma_qd="${8:-64}" ops="${9:-500000}"
	local pipeline="${10:-1}" wait_min="${11:-1}" fua="${12:-0}"
	local progress_spins="${13:-}"
	local read_percent owner_ingress=0 rma_reads=0 rma_writes=0 representative=1 multi_confirm=0
	local ring_entries=64 shm_entries=128 latency_rate=1 client_cpu target_cpu kernel_cpu owner_cpu out
	[ "$NODE_INDEX" = 1 ] || return 0
	case "$mode" in
		read) read_percent=100 ;;
		write) read_percent=0; owner_ingress=1 ;;
		rw) read_percent=50 ;;
		*) die "unknown mode $mode" ;;
	esac
	if [ "$transport" = efa ]; then
		case "$mode" in
			read|rw) rma_reads=1 ;;
			write) rma_writes=1; representative=0 ;;
		esac
	elif [ "$transport" != tcp ]; then
		die "transport must be tcp or efa"
	fi
	[ "$owners" -le "$lanes" ] || die "owners exceeds block lanes"
	if [ -z "$progress_spins" ]; then
		progress_spins="$([ "$qd" -le 16 ] && printf 256 || printf 4096)"
	fi
	[[ "$progress_spins" =~ ^[1-9][0-9]*$ ]] || die "progress spins must be a positive integer"
	[ "$mode" = write ] || [ "$owners" -eq "$lanes" ] || die "read/mixed streams must equal block lanes"
	[ "$transport:$owner_mode" != tcp:single-domain-fan-in ] || die "TCP uses placement label with an explicit reduced owner count"
	if [ "$transport" = efa ] && [ "$mode" = write ] && [ "$owner_mode" = placement ] && [ "$owners" -gt 1 ]; then
		multi_confirm=1
	fi
	while [ "$ring_entries" -lt $((qd * 2)) ]; do ring_entries=$((ring_entries * 2)); done
	while [ "$shm_entries" -lt "$qd" ]; do shm_entries=$((shm_entries * 2)); done
	client_cpu="$(client_cpus "$lanes")"
	target_cpu="$(target_cpus "$lanes")"
	kernel_cpu="$(kernel_cpus "$lanes")"
	owner_cpu="$(prefix_cpus "$(owner_cpu_pool "$lanes")" "$owners")"
	out="$RUN_ROOT/client/$tag"
	[ -r "$RUN_ROOT/topologies/$tag.leaf-topology.log" ] || die "missing leaf topology for $tag"
	env COORDINATION_SCOPE=dedicated-adhoc ZCUTILS_BOOTSTRAP_MANIFEST="$BOOTSTRAP_MANIFEST" \
		BUILD=0 REPRESENTATIVE="$representative" \
		MODULE="$ROOT/kmods/zcnblk_client_mod.ko" TARGET_BIN="$ROOT/target/release/zcnblk-shm-target" \
		BENCH_BIN="$ROOT/target/release/zcblockbench" ORDER_BIN="$ROOT/target/release/zcnblk-order-smoke" \
		BACKEND=wal-tcp START_LOCAL_LEAF=0 LEAF_ADDR="$LEAF_IP" LEAF_PORT="$BASE_PORT" \
		EXTERNAL_LEAF_TOPOLOGY_ARTIFACT="$RUN_ROOT/topologies/$tag.leaf-topology.log" \
		LANES="$lanes" REPEATS=4 OPS_PER_WORKER="$ops" IODEPTH="$qd" \
		RING_ENTRIES="$ring_entries" SHM_RING_ENTRIES="$shm_entries" SHM_PAYLOAD_ENTRIES=4096 \
		KERNEL_QUEUES="$lanes" KERNEL_QUEUE_DEPTH="$qd" KERNEL_PIPELINE_DEPTH="$qd" \
		MODE="$mode" READ_PERCENT="$read_percent" BUFFER_MODE=hugetlb \
		CLIENT_CPU_LIST="$client_cpu" TARGET_CPU_LIST="$target_cpu" KERNEL_CPU_LIST="$kernel_cpu" \
		URING_PLAY_ZCNBLK_SHM_OWNER_CPU_LIST="$owner_cpu" \
		URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS="$owner_ingress" \
		URING_PLAY_ZCNBLK_SHM_OWNER_COUNT="$owners" \
		URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_BATCHES="$pipeline" \
		URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW="$qd" \
		URING_PLAY_ZCNBLK_SHM_READ_BATCH=1 URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_US=0 \
		URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT="$([ "$transport" = efa ] && printf ofi || printf tcp)" \
		URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER=efa URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT=rdm \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS="$rma_reads" URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD="$qd" \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES="$rma_writes" \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES_REQUIRED="$rma_writes" \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_QD="$rma_qd" \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_OWNER_MODE="$owner_mode" \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED="$multi_confirm" \
		URING_PLAY_ZCNBLK_SHM_RMA_SOURCE_HUGETLB_CONFIRMED=0 \
		URING_PLAY_OFI_DOMAIN=efa_0-rdm URING_PLAY_OFI_CQ_SLEEP_NS=0 \
		URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES=1048576 \
		URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 FI_EFA_USE_DEVICE_RDMA=1 \
		URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED=1 \
		URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE="$([ "$qd" -ge 32 ] && printf 64 || printf 1)" \
		URING_PLAY_BLOCKBENCH_RING_STATS=1 URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS="$wait_min" \
		URING_PLAY_BLOCKBENCH_CQE_HOT_POLL=1 \
		URING_PLAY_BLOCKBENCH_CQE_HOT_POLL_PROGRESS_SPINS="$progress_spins" \
		URING_PLAY_BLOCKBENCH_FUA_WRITES="$fua" PERF_STAT=0 KERNEL_STATE_INTERVAL_MS=0 \
		POLL_US=1000 KERNEL_POLL_US=1000 OUTDIR="$out" \
		"$ROOT/scripts/zcnblk-shm-block-bench.sh"
}

leaf_stop() {
	local tag="${1:?tag}" out pid
	[ "$NODE_INDEX" = 2 ] || return 0
	out="$RUN_ROOT/leaf/$tag"
	[ -s "$out/leaf.pid" ] || return 0
	pid="$(<"$out/leaf.pid")"
	for _ in $(seq 1 100); do
		[ -r "/proc/$pid/comm" ] || break
		sleep 0.05
	done
	[ ! -r "/proc/$pid/comm" ] || safe_stop_pidfile "$out/leaf.pid" zcnblk-wal-leaf
	printf 'leaf_stopped=true tag=%s\n' "$tag" | tee "$out/leaf-stop.log"
}

case "$phase" in
	prepare) prepare_node "$@" ;;
	leaf-start) leaf_start "$@" ;;
	zcnblk-run) zcnblk_run "$@" ;;
	leaf-stop) leaf_stop "$@" ;;
	*) die "unknown phase $phase" ;;
esac
