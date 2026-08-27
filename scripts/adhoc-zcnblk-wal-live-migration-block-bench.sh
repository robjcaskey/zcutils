#!/usr/bin/env bash
set -euo pipefail

usage() {
	printf 'usage: %s INVENTORY TRANSPORT OUTDIR\n' "$0" >&2
	exit 2
}

[ "$#" -eq 3 ] || usage
inventory=$1
transport=$2
outdir=$3
[ "$transport" = tcp ] || [ "$transport" = ofi ] || usage
[ -r "$inventory" ] || { printf 'missing inventory: %s\n' "$inventory" >&2; exit 2; }
[ "$(jq '.instances | length' "$inventory")" -ge 3 ] || {
	printf 'inventory needs a client, source leaf, and destination leaf\n' >&2
	exit 2
}

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
key=${ADHOC_SSH_KEY:-/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519}
lanes=${LANES:-16}
volume_mib=${VOLUME_MIB:-4096}
ops_per_worker=${OPS_PER_WORKER:-500000}
iodepth=${IODEPTH:-128}
chunk_bytes=${CHUNK_BYTES:-16777216}
system_bps=${SYSTEM_BPS:-34359738368}
latency_sample_rate=${URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE:-0}
repeats=${REPEATS:-3}
migration_start_before_repeat=${MIGRATION_START_BEFORE_REPEAT:-2}
migration_cutover_after_repeat=${MIGRATION_CUTOVER_AFTER_REPEAT:-2}
card_index=${CARD_INDEX:-0}
domain=${OFI_DOMAIN:-efa_${card_index}-rdm}
efa_device=${EFA_DEVICE:-efa_$card_index}
netdev=${EFA_NETDEV:-$([ "$card_index" = 0 ] && printf ens68 || printf ens146)}
source_base=${SOURCE_BASE:-30300}
destination_base=${DESTINATION_BASE:-30400}
gateway_base=${GATEWAY_BASE:-30200}
control_addr=${CONTROL_ADDR:-127.0.0.1:30500}
remote_root=${REMOTE_ROOT:-/home/ubuntu/zcutils}
remote_out="/tmp/zc-live-migration-${transport}-$$"
ssh_opts=(-o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 -i "$key")

for value in "$lanes" "$volume_mib" "$ops_per_worker" "$iodepth" "$chunk_bytes" \
	"$system_bps" "$source_base" "$destination_base" "$gateway_base"; do
	[[ "$value" =~ ^[1-9][0-9]*$ ]] || usage
done
[[ "$latency_sample_rate" =~ ^[0-9]+$ ]] || usage
[ "$card_index" = 0 ] || [ "$card_index" = 1 ] || usage
[ "$lanes" -le 16 ] || {
	printf 'single-card strict topology supports at most 16 lanes (six physical cores per lane on one 96-core NUMA node)\n' >&2
	exit 2
}

public_ip() { jq -r ".instances[$1].public_ip" "$inventory"; }
card_ip() { jq -r ".instances[$1].network_interfaces[] | select(.network_card_index == $card_index) | .private_ip" "$inventory"; }
client_public=$(public_ip 0)
source_public=$(public_ip 1)
destination_public=$(public_ip 2)
client_ip=$(card_ip 0)
source_ip=$(card_ip 1)
destination_ip=$(card_ip 2)
for value in "$client_public" "$source_public" "$destination_public" "$client_ip" "$source_ip" "$destination_ip"; do
	[ -n "$value" ] && [ "$value" != null ] || { printf 'inventory is missing a selected public/private address\n' >&2; exit 2; }
done

numa_base=$((card_index * 96))
numa_cpus=${NUMA_CPUS:-96}
# Leaves map streams with affinity_index=connection*lanes+lane. Source needs
# foreground+base bands; destination additionally needs replay.
source_leaf_cpu_list="$numa_base-$((numa_base + 2 * lanes - 1))"
destination_leaf_cpu_list="$numa_base-$((numa_base + 3 * lanes - 1))"
block_cpu_list=
proxy_cpu_list=
copy_cpu_list=
lane_to_nic=
for ((lane = 0; lane < lanes; lane++)); do
	start=$((numa_base + lane * numa_cpus / lanes))
	[ "$lane" -eq 0 ] || {
		block_cpu_list+=,
		proxy_cpu_list+=,
		copy_cpu_list+=,
		lane_to_nic+=,
	}
	block_cpu_list+="$start,$((start + 1)),$((start + 2)),$((start + 3))"
	proxy_cpu_list+="$((start + 4))"
	copy_cpu_list+="$((start + 5))"
	lane_to_nic+="$lane:efa_$card_index"
done
volume_bytes=$((volume_mib * 1024 * 1024))
mkdir -p "$outdir"

source_pid=
destination_pid=
stop_remote_leaf() {
	local host=$1 pid=$2
	[[ "$pid" =~ ^[0-9]+$ ]] || return 0
	ssh "${ssh_opts[@]}" "ubuntu@$host" \
		"if [ -r /proc/$pid/comm ] && [ \"\$(cat /proc/$pid/comm)\" = zcnblk-wal-leaf ]; then kill -TERM $pid; fi" \
		>/dev/null 2>&1 || true
}
cleanup() {
	stop_remote_leaf "$source_public" "$source_pid"
	stop_remote_leaf "$destination_public" "$destination_pid"
}
trap cleanup EXIT INT TERM

leaf_transport_env="URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT=tcp"
if [ "$transport" = ofi ]; then
	leaf_transport_env="URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT=ofi URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER=efa URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT=rdm URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1 URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_WRITES=1 URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 URING_PLAY_OFI_DOMAIN=$domain FI_EFA_IFACE=$efa_device FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_OFI_CQ_SLEEP_NS=0"
fi
leaf_common="LD_LIBRARY_PATH=/opt/amazon/efa/lib:/usr/local/lib:/usr/lib/aarch64-linux-gnu URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB=1 $leaf_transport_env"

start_leaf() {
	local host=$1 bind_ip=$2 base=$3 branch=$4 cpu_list=$5 log=$6
	ssh "${ssh_opts[@]}" "ubuntu@$host" \
		"nohup setsid env URING_PLAY_PIN_CPU_LIST='$cpu_list' $leaf_common '$remote_root/target/release/zcnblk-wal-leaf' 'zcmem:$volume_bytes' '$bind_ip' '$base' '$lanes' '$branch' 4096 '$lanes' true blocking > '$log' 2>&1 </dev/null & pid=\$!; disown; printf '%s\\n' \"\$pid\""
}
source_log="$remote_out-source-leaf.log"
destination_log="$remote_out-destination-leaf.log"
source_pid=$(start_leaf "$source_public" "$source_ip" "$source_base" 2 "$source_leaf_cpu_list" "$source_log")
destination_pid=$(start_leaf "$destination_public" "$destination_ip" "$destination_base" 3 "$destination_leaf_cpu_list" "$destination_log")

for _ in $(seq 1 400); do
	source_live=$(ssh "${ssh_opts[@]}" "ubuntu@$source_public" "[ -r /proc/$source_pid/comm ] && echo 1 || echo 0" 2>/dev/null || printf 0)
	destination_live=$(ssh "${ssh_opts[@]}" "ubuntu@$destination_public" "[ -r /proc/$destination_pid/comm ] && echo 1 || echo 0" 2>/dev/null || printf 0)
	[ "$source_live" = 1 ] && [ "$destination_live" = 1 ] && break
	sleep 0.025
done
[ "${source_live:-0}" = 1 ] && [ "${destination_live:-0}" = 1 ] || {
	printf 'one or both terminal leaves exited before the client run\n' >&2
	exit 1
}
sleep 0.5

transport_env="OFI_PROVIDER=sockets"
if [ "$transport" = ofi ]; then
	transport_env="OFI_PROVIDER=efa OFI_DOMAIN=$domain FI_EFA_IFACE=$efa_device FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1"
fi
client_cmd="cd '$remote_root' && env COORDINATION_SCOPE=dedicated-adhoc REPRESENTATIVE=1 START_LOCAL_LEAVES=0 TRANSPORT=$transport INGRESS_TRANSPORT=tcp GATEWAY_HOST=$client_ip SOURCE_HOST=$source_ip DESTINATION_HOST=$destination_ip CONTROL_ADDR=$control_addr SOURCE_BASE=$source_base DESTINATION_BASE=$destination_base GATEWAY_BASE=$gateway_base LANES=$lanes VOLUME_MIB=$volume_mib OPS_PER_WORKER=$ops_per_worker IODEPTH=$iodepth CHUNK_BYTES=$chunk_bytes SYSTEM_BPS=$system_bps REPEATS=$repeats MIGRATION_START_BEFORE_REPEAT=$migration_start_before_repeat MIGRATION_CUTOVER_AFTER_REPEAT=$migration_cutover_after_repeat EXTERNAL_SOURCE_LEAF_CPU_LIST=$source_leaf_cpu_list EXTERNAL_DESTINATION_LEAF_CPU_LIST=$destination_leaf_cpu_list BLOCK_TOPOLOGY_CPU_LIST=$block_cpu_list PROXY_CPU_LIST=$proxy_cpu_list COPY_CPU_LIST=$copy_cpu_list HCTX_NUMA_NODE=$card_index LANE_TO_NIC=$lane_to_nic BUFFER_MODE=hugetlb URING_PLAY_ZCNBLK_SHM_ARENA_BACKING=hugetlb URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED=1 URING_PLAY_ZCNBLK_SHM_APP_ARENA_BUFFERS=0 URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE=$latency_sample_rate SHM_PAYLOAD_ENTRIES=16384 RING_ENTRIES=256 MODE=write PERF_STAT=1 BUILD=0 OUTDIR=$remote_out-client $transport_env scripts/zcnblk-wal-live-migration-block-bench.sh"
set +e
ssh "${ssh_opts[@]}" "ubuntu@$client_public" "$client_cmd" | tee "$outdir/client-console.log"
status=${PIPESTATUS[0]}
set -e

rsync -az -e "ssh ${ssh_opts[*]}" "ubuntu@$client_public:$remote_out-client/" "$outdir/client/" || true
scp "${ssh_opts[@]}" "ubuntu@$source_public:$source_log" "$outdir/source-leaf.log" >/dev/null 2>&1 || true
scp "${ssh_opts[@]}" "ubuntu@$destination_public:$destination_log" "$outdir/destination-leaf.log" >/dev/null 2>&1 || true
{
	printf 'ingress_transport=tcp leaf_transport=%s write_completion=early-local-retained-wal-admission sync_completion=remote-global-hwm-drain\n' "$transport"
	printf 'client=%s source=%s destination=%s card=%s efa_device=%s netdev=%s domain=%s\n' \
		"$client_ip" "$source_ip" "$destination_ip" "$card_index" "$efa_device" "$netdev" "$domain"
	printf 'lanes=%s per_worker_qd=%s aggregate_outstanding=%s block_cpu_list=%s gateway_proxy_cpus=%s gateway_copy_cpus=%s leaf_cpu_list=%s leaf_stream_affinity=connection*lanes+lane lane_to_nic=%s\n' \
		"$lanes" "$iodepth" "$((lanes * iodepth))" "$block_cpu_list" \
		"$proxy_cpu_list" "$copy_cpu_list" "source:$source_leaf_cpu_list;destination:$destination_leaf_cpu_list" "$lane_to_nic"
} >"$outdir/adhoc-topology.log"

[ "$status" -eq 0 ] || exit "$status"
grep -q 'ZCNBLK_WAL_LIVE_MIGRATION_BLOCK_BENCH_PASS' "$outdir/client-console.log" || {
	printf 'client run did not publish its migration PASS marker\n' >&2
	exit 1
}
printf 'ADHOC_ZCNBLK_WAL_LIVE_MIGRATION_BLOCK_BENCH_PASS transport=%s artifact=%s\n' \
	"$transport" "$outdir"
