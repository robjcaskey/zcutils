#!/usr/bin/env bash
set -euo pipefail

usage() {
	printf 'usage: %s INVENTORY OUTDIR\n' "$0" >&2
	exit 2
}

[ "$#" -eq 2 ] || usage
inventory=$1
outdir=$2
[ -r "$inventory" ] || usage
[ "$(jq '.instances | length' "$inventory")" -ge 3 ] || {
	printf 'inventory needs client, source-leaf, and destination-leaf nodes\n' >&2
	exit 2
}

key=${ADHOC_SSH_KEY:-/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519}
remote_root=${REMOTE_ROOT:-/home/ubuntu/zcutils}
lanes=${LANES:-16}
volume_mib=${VOLUME_MIB:-4096}
ops_per_worker=${OPS_PER_WORKER:-500000}
iodepth=${IODEPTH:-128}
repeats=${REPEATS:-3}
chunk_bytes=${CHUNK_BYTES:-16777216}
card_index=${CARD_INDEX:-1}
source_base=${SOURCE_BASE:-30600}
destination_base=${DESTINATION_BASE:-30700}
ssh_opts=(-o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 -i "$key")

for value in "$lanes" "$volume_mib" "$ops_per_worker" "$iodepth" "$repeats" \
	"$chunk_bytes" "$source_base" "$destination_base"; do
	[[ "$value" =~ ^[1-9][0-9]*$ ]] || usage
done
[ "$card_index" = 0 ] || [ "$card_index" = 1 ] || usage
[ "$lanes" -le 16 ] || {
	printf 'strict single-NUMA direct migration currently supports at most 16 lanes\n' >&2
	exit 2
}
[ "$repeats" -ge 3 ] || {
	printf 'representative shared-system measurements require at least three repeats\n' >&2
	exit 2
}

public_ip() { jq -r ".instances[$1].public_ip" "$inventory"; }
card_ip() {
	jq -r ".instances[$1].network_interfaces[] | select(.network_card_index == $card_index) | .private_ip" "$inventory"
}
client_public=$(public_ip 0)
source_public=$(public_ip 1)
destination_public=$(public_ip 2)
client_ip=$(card_ip 0)
source_ip=$(card_ip 1)
destination_ip=$(card_ip 2)
for value in "$client_public" "$source_public" "$destination_public" \
	"$client_ip" "$source_ip" "$destination_ip"; do
	if [ -z "$value" ] || [ "$value" = null ]; then usage; fi
done

numa_base=$((card_index * 96))
# c8gn.48xlarge exposes six same-NUMA physical CPUs to each of the 16 blk-mq
# hardware contexts. Keep all foreground and migration roles local to card N,
# while assigning distinct physical cores within each hctx's actual affinity.
# The final two CPUs in hctx0 are reserved for control and continuity.
client_cpu_list=
target_cpu_list=
kernel_cpu_list=
copy_cpu_list=
for ((lane = 0; lane < lanes; lane++)); do
	hctx_base=$((numa_base + 6 * lane))
	client_cpu_list+="${client_cpu_list:+,}$hctx_base"
	target_cpu_list+="${target_cpu_list:+,}$((hctx_base + 1))"
	kernel_cpu_list+="${kernel_cpu_list:+,}$((hctx_base + 2))"
	copy_cpu_list+="${copy_cpu_list:+,}$((hctx_base + 3))"
done
control_cpu=$((numa_base + 4))
continuity_cpu=$((numa_base + 5))
hot_cpu_list="$numa_base-$((numa_base + 96 - 1))"
source_leaf_cpu_list="$numa_base-$((numa_base + 3 * lanes - 1))"
destination_leaf_cpu_list="$numa_base-$((numa_base + 2 * lanes - 1))"
netdev=$([ "$card_index" = 0 ] && printf ens68 || printf ens146)
volume_bytes=$((volume_mib * 1024 * 1024))
proof_slots=64
proof_bytes=$((proof_slots * 4096))
proof_offset=$((volume_bytes - proof_bytes))
region_bytes_per_worker=$((proof_offset / lanes / 4096 * 4096))
remote_out="/home/ubuntu/zcutils/bench-results/adhoc-direct-migration-$(date -u +%Y%m%dT%H%M%SZ)-$$"
control_socket="/tmp/zcnblk-dm-$$.sock"
source_log="$remote_out-source-leaf.log"
destination_log="$remote_out-destination-leaf.log"
mkdir -p "$outdir"

source_pid=
destination_pid=
stop_remote_leaf() {
	local host=$1 pid=$2
	[[ "$pid" =~ ^[0-9]+$ ]] || return 0
	# shellcheck disable=SC2029
	ssh "${ssh_opts[@]}" "ubuntu@$host" \
		"if [ -r /proc/$pid/comm ]; then comm=\$(cat /proc/$pid/comm); if [ \"\$comm\" = zcnblk-wal-lea ] || [ \"\$comm\" = zcnblk-wal-leaf ]; then kill -TERM $pid; fi; fi" \
		>/dev/null 2>&1 || true
}
collect_artifacts() {
	set +e
	rsync -az -e "ssh ${ssh_opts[*]}" "ubuntu@$client_public:$remote_out/" "$outdir/client/"
	scp "${ssh_opts[@]}" "ubuntu@$source_public:$source_log" "$outdir/source-leaf.log" >/dev/null 2>&1
	scp "${ssh_opts[@]}" "ubuntu@$destination_public:$destination_log" "$outdir/destination-leaf.log" >/dev/null 2>&1
}
cleanup() {
	local status=$?
	set +e
	collect_artifacts
	stop_remote_leaf "$source_public" "$source_pid"
	stop_remote_leaf "$destination_public" "$destination_pid"
	exit "$status"
}
trap cleanup EXIT INT TERM

leaf_common="URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 URING_PLAY_ZCNBLK_WAL_LEAF_DYNAMIC_ACCEPT=1 URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB=1 URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1"
start_leaf() {
	local host=$1 bind_ip=$2 base=$3 connections=$4 workers=$5 cpus=$6 log=$7
	# shellcheck disable=SC2029
	ssh "${ssh_opts[@]}" "ubuntu@$host" \
		"mkdir -p '$remote_root/bench-results'; nohup setsid env URING_PLAY_PIN_CPU_LIST='$cpus' $leaf_common '$remote_root/target/release/zcnblk-wal-leaf' 'zcmem:$volume_bytes' '$bind_ip' '$base' '$lanes' '$connections' 4096 '$workers' true blocking >'$log' 2>&1 </dev/null & pid=\$!; disown; printf '%s\\n' \"\$pid\""
}

# Source accepts an ordinary-control foreground, the migration-run foreground,
# and the off-path copier. Destination accepts copier plus prepared foreground.
source_pid=$(start_leaf "$source_public" "$source_ip" "$source_base" 3 "$((3 * lanes))" "$source_leaf_cpu_list" "$source_log")
destination_pid=$(start_leaf "$destination_public" "$destination_ip" "$destination_base" 2 "$((2 * lanes))" "$destination_leaf_cpu_list" "$destination_log")
for _ in $(seq 1 400); do
	# shellcheck disable=SC2029
	source_listeners=$(ssh "${ssh_opts[@]}" "ubuntu@$source_public" \
		"ss -ltnH | awk '\$4 ~ /:$source_base|:$((source_base + lanes - 1))/ {n++} END {print n+0}'" 2>/dev/null || printf 0)
	# shellcheck disable=SC2029
	destination_listeners=$(ssh "${ssh_opts[@]}" "ubuntu@$destination_public" \
		"ss -ltnH | awk '\$4 ~ /:$destination_base|:$((destination_base + lanes - 1))/ {n++} END {print n+0}'" 2>/dev/null || printf 0)
	[ "$source_listeners" -ge 2 ] && [ "$destination_listeners" -ge 2 ] && break
	sleep 0.025
done
if [ "${source_listeners:-0}" -lt 2 ] || [ "${destination_listeners:-0}" -lt 2 ]; then
	printf 'terminal leaves did not publish their lane listeners\n' >&2
	exit 1
fi

topology_local="$outdir/external-leaf-topology.log"
{
	printf 'classification=dedicated-adhoc transport=tcp-unicast placement=userspace terminal_media=remote-zcmem-volatile-test-only\n'
	printf 'client=%s source=%s destination=%s placement_group=same card=%s netdev=%s nic_numa=%s\n' \
		"$client_ip" "$source_ip" "$destination_ip" "$card_index" "$netdev" "$card_index"
	printf 'lanes=%s per_worker_qd=%s aggregate_outstanding_depth=%s\n' \
		"$lanes" "$iodepth" "$((lanes * iodepth))"
	printf 'foreground_hot_cpu_pool=%s copy_cpu_list=%s control_cpu=%s continuity_cpu=%s\n' \
		"$hot_cpu_list" "$copy_cpu_list" "$control_cpu" "$continuity_cpu"
	printf 'client_lane_to_cpu=%s target_lane_to_cpu=%s kernel_hctx_to_cpu=%s\n' \
		"$client_cpu_list" "$target_cpu_list" "$kernel_cpu_list"
	printf 'source_stream_to_cpu=connection*lanes+lane:%s destination_stream_to_cpu=connection*lanes+lane:%s\n' \
		"$source_leaf_cpu_list" "$destination_leaf_cpu_list"
	printf 'lane_to_worker_cpu='
	for ((lane = 0; lane < lanes; lane++)); do
		[ "$lane" -eq 0 ] || printf ','
		printf '%s:%s' "$lane" "$((numa_base + lane))"
	done
	printf '\n'
	printf 'foreground_topology=/dev/zcnblk0->userspace-owner->active-terminal-leaf foreground_hops=1 migration_gateway=false block_client_placement=false\n'
} | tee "$topology_local"

scp "${ssh_opts[@]}" "$topology_local" "ubuntu@$client_public:/tmp/direct-migration-external-topology.log" >/dev/null

common="COORDINATION_SCOPE=dedicated-adhoc REPRESENTATIVE=1 BUILD=0 BACKEND=wal-tcp START_LOCAL_LEAF=0 LEAF_ADDR=$source_ip LEAF_PORT=$source_base LEAF_SOURCE_ADDR=$client_ip LANES=$lanes SIZE_MIB=$volume_mib TOPOLOGY_CPU_LIST=$hot_cpu_list CLIENT_CPU_LIST=$client_cpu_list TARGET_CPU_LIST=$target_cpu_list KERNEL_CPU_LIST=$kernel_cpu_list EXTERNAL_LEAF_TOPOLOGY_ARTIFACT=/tmp/direct-migration-external-topology.log REPEATS=$repeats MODE=write OPS_PER_WORKER=$ops_per_worker IODEPTH=$iodepth REGION_BYTES_PER_WORKER=$region_bytes_per_worker RING_ENTRIES=256 SHM_PAYLOAD_ENTRIES=16384 HCTX_NUMA_NODE=$card_index BUFFER_MODE=hugetlb URING_PLAY_ZCNBLK_SHM_ARENA_BACKING=hugetlb URING_PLAY_ZCNBLK_SHM_APP_ARENA_BUFFERS=0 URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS=1 URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=tcp URING_PLAY_ZCNBLK_SHM_REMOTE_RESULT_RANGES=1 URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_ZC_REQUIRED=0 URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED=1 URING_PLAY_ROUTE_PROBE=1 URING_PLAY_EXPECT_ROUTE_DEV=$netdev URING_PLAY_EXPECT_ROUTE_SRC=$client_ip PERF_STAT=0 ZCCUSAN_PLACEMENT_SCOPE=same-placement-group ZCCUSAN_TOPOLOGY_CLASS=client-terminal-leaf ZCCUSAN_TOPOLOGY_PATH_COUNT=1 ZCCUSAN_TOPOLOGY_TRANSPORT=tcp ZCCUSAN_TOPOLOGY_NUMA_NODE_COUNT=2 ZCCUSAN_TOPOLOGY_NUMA_LOCAL=1"

client_cmd="set -eu; export PATH=\$HOME/.cargo/bin:\$PATH; cd '$remote_root'; mkdir -p '$remote_out'; rm -f '$control_socket'; env $common OUTDIR='$remote_out/baseline' scripts/zcnblk-shm-block-bench.sh; env $common OUTDIR='$remote_out/migration' URING_PLAY_ZCNBLK_SHM_MIGRATION_CONTROL_SOCKET='$control_socket' URING_PLAY_ZCNBLK_SHM_MIGRATION_SOURCE_ADDR='$source_ip:$source_base' URING_PLAY_ZCNBLK_SHM_MIGRATION_DEST_ADDR='$destination_ip:$destination_base' URING_PLAY_ZCNBLK_SHM_MIGRATION_TCP_COPY_METHOD=splice URING_PLAY_ZCNBLK_SHM_MIGRATION_CATCHUP_PASSES=2 ZCNBLK_WAL_MIGRATION_COPY_CPU_LIST='$copy_cpu_list' ZCNBLK_WAL_MIGRATION_CONTROL_CPU='$control_cpu' ZCNBLK_WAL_DIRECT_MIGRATION_AFTER_REPEAT=1 ZCNBLK_WAL_DIRECT_MIGRATION_EPOCH=2 ZCNBLK_WAL_DIRECT_MIGRATION_VOLUME_BYTES='$volume_bytes' ZCNBLK_WAL_DIRECT_MIGRATION_CHUNK_BYTES='$chunk_bytes' ZCNBLK_WAL_DIRECT_MIGRATION_GRANULE_BYTES=4096 ZCNBLK_WAL_LIVE_MIGRATION_READY_TIMEOUT_SECONDS=180 ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_PROOF=1 ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_OFFSET='$proof_offset' ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_SLOTS='$proof_slots' ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_INTERVAL_US=500 ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_SYNC_EVERY=4096 ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_CPU='$continuity_cpu' scripts/zcnblk-shm-block-bench.sh; rm -f '$control_socket'"
set +e
# shellcheck disable=SC2029
ssh "${ssh_opts[@]}" "ubuntu@$client_public" "$client_cmd" | tee "$outdir/client-console.log"
status=${PIPESTATUS[0]}
set -e
collect_artifacts
[ "$status" -eq 0 ] || exit "$status"

grep -q '^OK active_destination=true ' "$outdir/client/migration/direct-migration-control.log"
grep -q 'foreground_hops=1 foreground_payload_rebuffer_copies=0' \
	"$outdir/client/migration/direct-migration-control.log"
grep -q 'copy_payload_userspace_buffers=0 copy_method=Splice' \
	"$outdir/client/migration/direct-migration-control.log"
grep -q '^ZCNBLK_EDGE_CONTINUITY_PASS .*identity_stable=true open_descriptor_replaced=false .*mismatches=0 ' \
	"$outdir/client/migration/continuity.log"

baseline_mean=$(awk -F'[ =]' '/^runs=/ {for (i=1;i<=NF;i++) if ($i=="mean_iops") print $(i+1)}' \
	"$outdir/client/baseline/summary.log" | tail -1)
migration_source_iops=$(awk '/^repeat=1 zcblockbench-result:/ {for(i=1;i<=NF;i++) if($i ~ /^ops_per_sec=/){split($i,a,"="); print a[2]}}' \
	"$outdir/client/migration/results.log")
migration_destination_iops=$(awk '/^repeat=[23] zcblockbench-result:/ {for(i=1;i<=NF;i++) if($i ~ /^ops_per_sec=/){split($i,a,"="); total+=a[2]; n++}} END {if(n) printf "%.0f", total/n}' \
	"$outdir/client/migration/results.log")
printf 'ADHOC_ZCNBLK_WAL_DIRECT_MIGRATION_BLOCK_PASS transport=tcp lanes=%s baseline_mean_iops=%s migration_source_iops=%s migration_destination_mean_iops=%s reconnects=0 foreground_hops=1 payload_rebuffer_copies=0 artifact=%s\n' \
	"$lanes" "${baseline_mean:-unknown}" "${migration_source_iops:-unknown}" \
	"${migration_destination_iops:-unknown}" "$outdir"
