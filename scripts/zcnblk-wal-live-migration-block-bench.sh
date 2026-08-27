#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${OUTDIR:-$ROOT/bench-results/local-zcnblk-wal-live-migration-block-bench-$(date -u +%Y%m%dT%H%M%SZ)}"
LEAF_BIN="${LEAF_BIN:-$ROOT/target/release/zcnblk-wal-leaf}"
GATEWAY_BIN="${GATEWAY_BIN:-$ROOT/target/release/zcnblk-wal-live-migrate}"
TRANSPORT="${TRANSPORT:-tcp}"
INGRESS_TRANSPORT="${INGRESS_TRANSPORT:-tcp}"
LANES="${LANES:-4}"
VOLUME_MIB="${VOLUME_MIB:-$((LANES * 64))}"
CHUNK_BYTES="${CHUNK_BYTES:-1048576}"
SYSTEM_BPS="${SYSTEM_BPS:-536870912}"
SOURCE_BASE="${SOURCE_BASE:-30300}"
DESTINATION_BASE="${DESTINATION_BASE:-30400}"
GATEWAY_BASE="${GATEWAY_BASE:-30200}"
CONTROL_ADDR="${CONTROL_ADDR:-127.0.0.1:30500}"
GATEWAY_HOST="${GATEWAY_HOST:-127.0.0.1}"
SOURCE_HOST="${SOURCE_HOST:-127.0.0.1}"
DESTINATION_HOST="${DESTINATION_HOST:-127.0.0.1}"
START_LOCAL_LEAVES="${START_LOCAL_LEAVES:-1}"
EXTERNAL_SOURCE_LEAF_CPU_LIST="${EXTERNAL_SOURCE_LEAF_CPU_LIST:-}"
EXTERNAL_DESTINATION_LEAF_CPU_LIST="${EXTERNAL_DESTINATION_LEAF_CPU_LIST:-}"
PROXY_CPU_LIST="${PROXY_CPU_LIST:-}"
COPY_CPU_LIST="${COPY_CPU_LIST:-}"
LANE_TO_NIC="${LANE_TO_NIC:-}"
OFI_DOMAIN="${OFI_DOMAIN:-}"
OFI_PROVIDER="${OFI_PROVIDER:-sockets}"
OFI_ENDPOINT="${OFI_ENDPOINT:-rdm}"
REPRESENTATIVE="${REPRESENTATIVE:-0}"
REPEATS="${REPEATS:-3}"
MIGRATION_START_BEFORE_REPEAT="${MIGRATION_START_BEFORE_REPEAT:-2}"
MIGRATION_CUTOVER_AFTER_REPEAT="${MIGRATION_CUTOVER_AFTER_REPEAT:-2}"
BUILD="${BUILD:-0}"
CONTINUITY_PROOF="${CONTINUITY_PROOF:-1}"
CONTINUITY_PROOF_RESERVE_BYTES="${CONTINUITY_PROOF_RESERVE_BYTES:-4194304}"
CONTINUITY_PROOF_SLOTS="${CONTINUITY_PROOF_SLOTS:-64}"
CONTINUITY_PROOF_INTERVAL_US="${CONTINUITY_PROOF_INTERVAL_US:-500}"
CONTINUITY_PROOF_SYNC_EVERY="${CONTINUITY_PROOF_SYNC_EVERY:-4096}"
jobs=()

die() {
	printf 'zcnblk-wal-live-migration-block-bench: ERROR: %s\n' "$*" >&2
	exit 1
}

expand_cpu_list() {
	local part start end cpu
	tr ',' '\n' <<<"$1" | while IFS= read -r part; do
		if [[ "$part" == *-* ]]; then
			start="${part%-*}"
			end="${part#*-}"
			for ((cpu = start; cpu <= end; cpu++)); do printf '%s\n' "$cpu"; done
		elif [ -n "$part" ]; then
			printf '%s\n' "$part"
		fi
	done
}

join_comma() {
	local IFS=,
	printf '%s' "$*"
}

stop_exact_pid() {
	local pid="$1" comm
	[ -n "$pid" ] && [ -r "/proc/$pid/comm" ] || return 0
	comm="$(cat "/proc/$pid/comm")"
	case "$comm" in
		zcnblk-wal-leaf|zcnblk-wal-live) ;;
		*) printf 'refusing to stop unexpected pid=%s comm=%s\n' "$pid" "$comm" >&2; return 1 ;;
	esac
	kill -TERM "$pid" 2>/dev/null || true
	for _ in $(seq 1 100); do
		[ ! -e "/proc/$pid" ] && return 0
		sleep 0.02
	done
	kill -KILL "$pid" 2>/dev/null || true
}

cleanup() {
	local status=$? pid
	set +e
	for pid in "${jobs[@]}"; do stop_exact_pid "$pid"; done
	exit "$status"
}
trap cleanup EXIT INT TERM

[ "$TRANSPORT" = tcp ] || [ "$TRANSPORT" = ofi ] || die "TRANSPORT must be tcp or ofi"
[ "$INGRESS_TRANSPORT" = tcp ] || [ "$INGRESS_TRANSPORT" = ofi ] || \
	die "INGRESS_TRANSPORT must be tcp or ofi"
[ "$TRANSPORT" = ofi ] || [ "$INGRESS_TRANSPORT" = tcp ] || \
	die "OFI ingress requires OFI terminal-leaf transport"
[[ "$LANES" =~ ^[0-9]+$ ]] && [ "$LANES" -gt 0 ] || die "LANES must be positive"
[ "$VOLUME_MIB" -gt 0 ] || die "VOLUME_MIB must be positive"
[ "$BUILD" = 0 ] || [ "$BUILD" = 1 ] || die "BUILD must be zero or one"
[ "$CONTINUITY_PROOF" = 0 ] || [ "$CONTINUITY_PROOF" = 1 ] || \
	die "CONTINUITY_PROOF must be zero or one"
[ "$START_LOCAL_LEAVES" = 0 ] || [ "$START_LOCAL_LEAVES" = 1 ] || \
	die "START_LOCAL_LEAVES must be zero or one"
[ "$REPRESENTATIVE" = 0 ] || [ "$REPRESENTATIVE" = 1 ] || \
	die "REPRESENTATIVE must be zero or one"
[[ "$REPEATS" =~ ^[0-9]+$ ]] && [ "$REPEATS" -ge 3 ] || \
	die "REPEATS must be at least three"
[[ "$MIGRATION_START_BEFORE_REPEAT" =~ ^[0-9]+$ ]] && \
	[ "$MIGRATION_START_BEFORE_REPEAT" -ge 2 ] && \
	[ "$MIGRATION_START_BEFORE_REPEAT" -le "$MIGRATION_CUTOVER_AFTER_REPEAT" ] || \
	die "MIGRATION_START_BEFORE_REPEAT must be in 2..=MIGRATION_CUTOVER_AFTER_REPEAT"
[[ "$MIGRATION_CUTOVER_AFTER_REPEAT" =~ ^[0-9]+$ ]] && \
	[ "$MIGRATION_CUTOVER_AFTER_REPEAT" -lt "$REPEATS" ] || \
	die "MIGRATION_CUTOVER_AFTER_REPEAT must leave at least one destination repeat"
mkdir -p "$OUTDIR"

if [ "$BUILD" = 1 ]; then
	(cd "$ROOT" && cargo build --release --bin zcnblk-wal-leaf \
		--bin zcnblk-wal-live-migrate --bin zcnblk-shm-target --bin zcblockbench \
		--bin zcnblk-edge-sync --bin zcnblk-edge-continuity)
fi
[ -x "$LEAF_BIN" ] || die "missing leaf binary: $LEAF_BIN"
[ -x "$GATEWAY_BIN" ] || die "missing migration gateway binary: $GATEWAY_BIN"

allowed="$(taskset -pc $$ | sed 's/^.*: //')"
mapfile -t cpus < <(expand_cpu_list "$allowed")
[ "${#cpus[@]}" -ge "$((LANES * 2))" ] || \
	die "need at least $((LANES * 2)) allowed CPUs for disjoint proxy/copy roles; allowed=$allowed"
if [ "$START_LOCAL_LEAVES" = 0 ] && [ "${#cpus[@]}" -ge "$((LANES * 5))" ]; then
	block_cpus=("${cpus[@]:0:$((LANES * 3))}")
	proxy_cpus=("${cpus[@]:$((LANES * 3)):LANES}")
	copy_cpus=("${cpus[@]:$((LANES * 4)):LANES}")
	[ -n "$EXTERNAL_SOURCE_LEAF_CPU_LIST" ] || \
		die "external leaves require EXTERNAL_SOURCE_LEAF_CPU_LIST"
	[ -n "$EXTERNAL_DESTINATION_LEAF_CPU_LIST" ] || \
		die "external leaves require EXTERNAL_DESTINATION_LEAF_CPU_LIST"
	mapfile -t source_leaf_cpus < <(expand_cpu_list "$EXTERNAL_SOURCE_LEAF_CPU_LIST")
	mapfile -t destination_leaf_cpus < <(expand_cpu_list "$EXTERNAL_DESTINATION_LEAF_CPU_LIST")
	[ "${#source_leaf_cpus[@]}" -eq "$((LANES * 2))" ] || \
		die "external source leaf CPU list must contain exactly $((LANES * 2)) CPUs (foreground and base-copy bands)"
	[ "${#destination_leaf_cpus[@]}" -eq "$((LANES * 3))" ] || \
		die "external destination leaf CPU list must contain exactly $((LANES * 3)) CPUs (foreground, base-copy, and replay bands)"
	block_cpu_list="${BLOCK_TOPOLOGY_CPU_LIST:-$(join_comma "${block_cpus[@]}")}"
	role_isolation=client-block-gateway-and-remote-leaf-process-cpus-disjoint
elif [ "${#cpus[@]}" -ge "$((LANES * 10))" ]; then
	block_cpus=("${cpus[@]:0:$((LANES * 3))}")
	proxy_cpus=("${cpus[@]:$((LANES * 3)):LANES}")
	copy_cpus=("${cpus[@]:$((LANES * 4)):LANES}")
	source_leaf_cpus=("${cpus[@]:$((LANES * 5)):$((LANES * 2))}")
	destination_leaf_cpus=("${cpus[@]:$((LANES * 7)):$((LANES * 3))}")
	# blk-mq hctx membership, not numerical CPU order, constrains the client,
	# target, and completion triple. Let the child planner select those triples
	# from the full machine unless the caller supplies a verified subset.
	block_cpu_list="${BLOCK_TOPOLOGY_CPU_LIST:-}"
	role_isolation=userspace-process-cpus-disjoint-block-roles-child-planned
else
	block_cpu_list=""
	proxy_cpus=("${cpus[@]:0:LANES}")
	copy_cpus=("${cpus[@]:LANES:LANES}")
	source_leaf_cpus=("${cpus[@]:0:LANES}")
	destination_leaf_cpus=("${cpus[@]:0:LANES}")
	role_isolation=proxy-copy-only
	printf 'PERF WARNING: allowed CPUs cannot isolate block, gateway, and both leaf roles; results are not representative\n' >&2
fi
if [ -n "$PROXY_CPU_LIST$COPY_CPU_LIST" ]; then
	[ -n "$PROXY_CPU_LIST" ] && [ -n "$COPY_CPU_LIST" ] || \
		die "explicit gateway topology requires both PROXY_CPU_LIST and COPY_CPU_LIST"
	mapfile -t proxy_cpus < <(expand_cpu_list "$PROXY_CPU_LIST")
	mapfile -t copy_cpus < <(expand_cpu_list "$COPY_CPU_LIST")
	[ "${#proxy_cpus[@]}" -eq "$LANES" ] || \
		die "PROXY_CPU_LIST must provide exactly one CPU per lane"
	[ "${#copy_cpus[@]}" -eq "$LANES" ] || \
		die "COPY_CPU_LIST must provide exactly one CPU per lane"
fi
proxy_cpu_list="$(join_comma "${proxy_cpus[@]}")"
copy_cpu_list="$(join_comma "${copy_cpus[@]}")"
source_leaf_cpu_list="$(join_comma "${source_leaf_cpus[@]}")"
destination_leaf_cpu_list="$(join_comma "${destination_leaf_cpus[@]}")"
volume_bytes=$((VOLUME_MIB * 1024 * 1024))
if [ "$CONTINUITY_PROOF" = 1 ]; then
	for value in "$CONTINUITY_PROOF_RESERVE_BYTES" "$CONTINUITY_PROOF_SLOTS" \
		"$CONTINUITY_PROOF_INTERVAL_US" "$CONTINUITY_PROOF_SYNC_EVERY"; do
		[[ "$value" =~ ^[0-9]+$ ]] || die "continuity proof values must be unsigned integers"
	done
	proof_bytes=$((CONTINUITY_PROOF_SLOTS * 4096))
	[ "$CONTINUITY_PROOF_RESERVE_BYTES" -ge "$proof_bytes" ] || \
		die "continuity reserve is smaller than its proof slots"
	[ "$volume_bytes" -gt "$CONTINUITY_PROOF_RESERVE_BYTES" ] || \
		die "volume is too small for the continuity reserve"
	default_region_bytes=$(( (volume_bytes - CONTINUITY_PROOF_RESERVE_BYTES) / LANES / 4096 * 4096 ))
	region_bytes_per_worker="${REGION_BYTES_PER_WORKER:-$default_region_bytes}"
	continuity_proof_offset=$((region_bytes_per_worker * LANES))
	[ "$((continuity_proof_offset + proof_bytes))" -le "$volume_bytes" ] || \
		die "random-I/O regions leave no non-overlapping continuity proof range"
else
	region_bytes_per_worker="${REGION_BYTES_PER_WORKER:-$((volume_bytes / LANES))}"
	continuity_proof_offset=0
fi

if [ "$TRANSPORT" = ofi ]; then
	[ -n "$OFI_DOMAIN" ] || [ "$REPRESENTATIVE" = 0 ] || \
		die "representative OFI requires OFI_DOMAIN"
	[ -n "$LANE_TO_NIC" ] || [ "$REPRESENTATIVE" = 0 ] || \
		die "representative OFI requires an explicit LANE_TO_NIC map"
fi

{
	printf 'classification=%s representative=%s\n' \
		"$([ "$REPRESENTATIVE" = 1 ] && printf representative-dedicated-adhoc || printf exploratory-shared-system)" \
		"$([ "$REPRESENTATIVE" = 1 ] && printf true || printf false)"
	printf 'topology=linux-block-edge->userspace-wal-target->userspace-live-migration->userspace-terminal-leaf\n'
	printf 'ingress_transport=%s leaf_transport=%s volume_bytes=%s lanes=%s gateway_host=%s source_host=%s destination_host=%s\n' \
		"$INGRESS_TRANSPORT" "$TRANSPORT" "$volume_bytes" "$LANES" "$GATEWAY_HOST" "$SOURCE_HOST" "$DESTINATION_HOST"
	# Keep the legacy key consumed by the block harness; it describes the
	# foreground band. The following role maps describe every system session.
	printf 'lane_to_worker_cpu='
	for ((lane = 0; lane < LANES; lane++)); do
		[ "$lane" -eq 0 ] || printf ','
		printf '%s:%s' "$lane" "${source_leaf_cpus[$lane]}"
	done
	printf '\nsource_leaf_stream_to_cpu=connection*lanes+lane:%s\ndestination_leaf_stream_to_cpu=connection*lanes+lane:%s\n' \
		"$source_leaf_cpu_list" "$destination_leaf_cpu_list"
	printf 'lane_to_nic=%s\n' "${LANE_TO_NIC:-not-applicable}"
	printf 'proxy_lane_to_cpu=%s\ncopy_lane_to_cpu=%s\nblock_topology_cpu_list=%s\nrole_isolation=%s\n' \
		"$proxy_cpu_list" "$copy_cpu_list" "${block_cpu_list:-unassigned}" "$role_isolation"
	printf 'per_worker_qd=from-child-harness aggregate_outstanding=from-child-harness write_completion=early-local-retained-wal-admission sync_completion=remote-global-hwm-drain\n'
	printf 'continuity_proof=%s proof_offset=%s proof_slots=%s random_region_bytes_per_worker=%s\n' \
		"$CONTINUITY_PROOF" "$continuity_proof_offset" "$CONTINUITY_PROOF_SLOTS" \
		"$region_bytes_per_worker"
} | tee "$OUTDIR/topology.log"
cp "$OUTDIR/topology.log" "$OUTDIR/external-leaf-topology.log"

leaf_env=(URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1
	URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1
	URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1)
gateway_env=(env ZCNBLK_WAL_MIGRATION_TRANSPORT="$TRANSPORT"
	ZCNBLK_WAL_MIGRATION_INGRESS_TRANSPORT="$INGRESS_TRANSPORT"
	ZCNBLK_WAL_MIGRATION_PROXY_CPU_LIST="$proxy_cpu_list"
	ZCNBLK_WAL_MIGRATION_COPY_CPU_LIST="$copy_cpu_list"
	URING_PLAY_TOPOLOGY_STRICT="$REPRESENTATIVE"
	URING_PLAY_TOPOLOGY_FATAL="$REPRESENTATIVE")
child_transport_env=()
if [ "$TRANSPORT" = ofi ]; then
	leaf_env+=(URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT=ofi
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER="$OFI_PROVIDER"
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT="$OFI_ENDPOINT"
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_WRITES=1
		URING_PLAY_OFI_CQ_SLEEP_NS=0)
	gateway_env+=(ZCNBLK_WAL_MIGRATION_OFI_PROVIDER="$OFI_PROVIDER"
		ZCNBLK_WAL_MIGRATION_OFI_ENDPOINT="$OFI_ENDPOINT"
		URING_PLAY_OFI_CQ_SLEEP_NS=0)
	if [ "$INGRESS_TRANSPORT" = ofi ]; then
		gateway_env+=(ZCNBLK_WAL_MIGRATION_INGRESS_OFI_RMA_CAPABLE=1)
		child_transport_env+=(URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=ofi
			URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER="$OFI_PROVIDER"
			URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT="$OFI_ENDPOINT"
			URING_PLAY_OFI_CQ_SLEEP_NS=0)
	else
		child_transport_env+=(URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=tcp)
	fi
	if [ -n "$OFI_DOMAIN" ]; then
		gateway_env+=(ZCNBLK_WAL_MIGRATION_SOURCE_OFI_DOMAIN="$OFI_DOMAIN"
			ZCNBLK_WAL_MIGRATION_DESTINATION_OFI_DOMAIN="$OFI_DOMAIN")
		if [ "$INGRESS_TRANSPORT" = ofi ]; then
			gateway_env+=(ZCNBLK_WAL_MIGRATION_INGRESS_OFI_DOMAIN="$OFI_DOMAIN")
		fi
		child_transport_env+=(URING_PLAY_OFI_DOMAIN="$OFI_DOMAIN")
	fi
else
	child_transport_env+=(URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=tcp)
fi

if [ "$START_LOCAL_LEAVES" = 1 ]; then
	env URING_PLAY_PIN_CPU_LIST="$source_leaf_cpu_list" "${leaf_env[@]}" \
		"$LEAF_BIN" "zcmem:$volume_bytes" "$SOURCE_HOST" "$SOURCE_BASE" \
		"$LANES" 2 4096 "$LANES" true blocking >"$OUTDIR/source-leaf.log" 2>&1 &
	jobs+=("$!")
	env URING_PLAY_PIN_CPU_LIST="$destination_leaf_cpu_list" "${leaf_env[@]}" \
		"$LEAF_BIN" "zcmem:$volume_bytes" "$DESTINATION_HOST" "$DESTINATION_BASE" \
		"$LANES" 3 4096 "$LANES" true blocking >"$OUTDIR/destination-leaf.log" 2>&1 &
	jobs+=("$!")
fi
"${gateway_env[@]}" "$GATEWAY_BIN" "$GATEWAY_HOST:$GATEWAY_BASE" \
	"$SOURCE_HOST:$SOURCE_BASE" "$DESTINATION_HOST:$DESTINATION_BASE" "$CONTROL_ADDR" \
	"$volume_bytes" "$LANES" "$CHUNK_BYTES" "$SYSTEM_BPS" >"$OUTDIR/gateway.log" 2>&1 &
jobs+=("$!")

for _ in $(seq 1 200); do
	if (exec 9<>"/dev/tcp/${CONTROL_ADDR%:*}/${CONTROL_ADDR##*:}") 2>/dev/null; then
		exec 9>&-
		break
	fi
	sleep 0.05
done
gateway_pid="${jobs[$((${#jobs[@]} - 1))]}"
[ -r "/proc/$gateway_pid/comm" ] || die "migration gateway exited before the block client connected"

env "${child_transport_env[@]}" \
	OUTDIR="$OUTDIR/block" BACKEND=wal-tcp START_LOCAL_LEAF=0 \
	LEAF_ADDR="$GATEWAY_HOST" LEAF_PORT="$GATEWAY_BASE" LANES="$LANES" SIZE_MIB="$VOLUME_MIB" \
	TOPOLOGY_CPU_LIST="$block_cpu_list" \
	EXTERNAL_LEAF_TOPOLOGY_ARTIFACT="$OUTDIR/external-leaf-topology.log" \
	ZCNBLK_WAL_LIVE_MIGRATION_CONTROL_ADDR="$CONTROL_ADDR" \
	ZCNBLK_WAL_LIVE_MIGRATION_START_BEFORE_REPEAT="$MIGRATION_START_BEFORE_REPEAT" \
	ZCNBLK_WAL_LIVE_MIGRATION_CUTOVER_AFTER_REPEAT="$MIGRATION_CUTOVER_AFTER_REPEAT" \
	REPEATS="$REPEATS" MODE="${MODE:-rw}" READ_PERCENT="${READ_PERCENT:-50}" \
	OPS_PER_WORKER="${OPS_PER_WORKER:-200000}" IODEPTH="${IODEPTH:-128}" \
	ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_PROOF="$CONTINUITY_PROOF" \
	ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_OFFSET="$continuity_proof_offset" \
	ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_SLOTS="$CONTINUITY_PROOF_SLOTS" \
	ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_INTERVAL_US="$CONTINUITY_PROOF_INTERVAL_US" \
	ZCNBLK_WAL_LIVE_MIGRATION_CONTINUITY_SYNC_EVERY="$CONTINUITY_PROOF_SYNC_EVERY" \
	URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS="${URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS:-1}" \
	RING_ENTRIES="${RING_ENTRIES:-256}" REGION_BYTES_PER_WORKER="$region_bytes_per_worker" \
	PERF_STAT="${PERF_STAT:-0}" REPRESENTATIVE="$REPRESENTATIVE" \
	BUILD=0 "$ROOT/scripts/zcnblk-shm-block-bench.sh"

grep -q 'phase=active_secondary' "$OUTDIR/block/live-migration-control.log" || \
	die "block harness did not prove an active secondary"
grep -q 'client_target_session_reconnect=false' "$OUTDIR/block/live-migration-control.log" || \
	die "block harness did not prove stable target sessions"
[ "$CONTINUITY_PROOF" != 1 ] || \
	grep -q 'ZCNBLK_EDGE_CONTINUITY_PASS .* identity_stable=true .* mismatches=0 ' \
		"$OUTDIR/block/continuity.log" || \
	die "block harness did not prove one-open-descriptor data continuity"
grep -Eq '^zcnblk-shm-target-route-fence: .*placement_epoch=2 .*result_boundary=true$' \
	"$OUTDIR/block/target.log" || die "block target did not observe the route epoch/HWM fence"
grep -q 'base_payload=socket-pipe-socket-splice-zero-userspace-buffer' "$OUTDIR/gateway.log" || \
	[ "$TRANSPORT" = ofi ] || die "TCP migration did not use splice for its bulk payload"
grep -q 'base_payload=source-rma-read->one-registered-arena->destination-rma-write-zero-cpu-copy' \
	"$OUTDIR/gateway.log" || [ "$TRANSPORT" = tcp ] || \
	die "OFI migration did not use its one-arena RMA bulk path"

mapfile -t measured_iops < <(awk '/zcblockbench-result:/ { for (i=1; i<=NF; i++) if ($i ~ /^ops_per_sec=/) { split($i,a,"="); print a[2] } }' "$OUTDIR/block/results.log")
[ "${#measured_iops[@]}" -eq "$REPEATS" ] || \
	die "expected exactly $REPEATS measured IOPS results"
source_index=$((MIGRATION_START_BEFORE_REPEAT - 2))
migration_index=$((MIGRATION_START_BEFORE_REPEAT - 1))
destination_index=$MIGRATION_CUTOVER_AFTER_REPEAT
printf 'ZCNBLK_WAL_LIVE_MIGRATION_BLOCK_BENCH_PASS transport=%s lanes=%s reconnects=0 route_epoch_fence=true source_iops=%s migration_iops=%s destination_iops=%s artifact=%s\n' \
	"$TRANSPORT" "$LANES" "${measured_iops[$source_index]}" \
	"${measured_iops[$migration_index]}" "${measured_iops[$destination_index]}" "$OUTDIR"
