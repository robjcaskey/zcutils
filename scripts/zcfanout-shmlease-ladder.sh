#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ZCFANOUT_SHMLEASE_BIN:-$REPO_DIR/target/release/zcfanout-shmlease-bench}"

usage() {
	cat <<'EOF'
usage: scripts/zcfanout-shmlease-ladder.sh

Run the same-host shared-WAL descriptor ladder:
  zcnblk-shm-onramp             client -> fan
  zcnblk-shm-pipeline           client -> fan -> leaf
  zcnblk-shm-mirror             client -> fan -> two leaf legs
  zcnblk-shm-mirror-dirty-cache client -> fan dirty cache -> two leaf legs

environment:
  OUTDIR        result directory, default bench-results/local-shmlease-ladder-$UTC
  RUN_ID        run id used when OUTDIR is not set
  LANES_LIST    lane counts to test, default "1 2 4"
  WORKLOADS     workloads to test, default all three zcnblk-shm-* controls
  TOUCH_MODES   touch modes to test, default "none cacheline"
  RECORDS       records per lane, default 500000
  BATCH_RECORDS records per publish batch, default 2048
  WINDOW        mapped slots per lane, default 8192
  PAYLOAD_SLOTS payload slots per lane, default WINDOW
  SYNC_RECORDS  sync/HWM cadence, default 8192
  ACK_RECORDS   write ack/release cadence, default 0
  WORKING_SET_RECORDS
                nonzero random logical page working set for zcnblk-shm-* workloads, default 0
  WAIT_MODE     spin or condvar, default spin
  REPEATS       repeats per case, default 2
  PIN           pass pin_workers, default true
  CPU_BASE      first CPU id for generated maps, default 0
  ALLOW_SMT_OVERLAP
                allow generated CPU range to include sibling threads, default 0
  FAN_TOUCH_PAYLOAD
                URING_PLAY_SHMLEASE_FAN_TOUCH_PAYLOAD, default 1
  LEAF_TOUCH_PAYLOAD
                URING_PLAY_SHMLEASE_LEAF_TOUCH_PAYLOAD, default 1
  ZERO_COPY_FORWARDING_TARGET
                fail/warn if fan or leaf payload inspection is enabled, default 0
  HWM_ONLY_RESULTS
                URING_PLAY_SHMLEASE_HWM_ONLY_RESULTS, default 0
  LEAF_BATCH_DELAY_NS
                benchmark-only busy-spin delay before each leaf HWM publish,
                default 0
  DIRTY_EARLY_RESPONSES
                for zcnblk-shm-mirror-dirty-cache, publish client responses
                after local dirty-cache admission while holding payload slot
                reuse to mirror leaf HWM, default 1
  ARENA_HUGEPAGE
                URING_PLAY_SHMLEASE_ARENA_HUGEPAGE, default 1
  ARENA_PREFAULT
                URING_PLAY_SHMLEASE_ARENA_PREFAULT, default 0
  TOPOLOGY_PREFLIGHT
                record/warn about local host perf state, default 1
  TOPOLOGY_PREFLIGHT_FATAL
                fail a case on preflight warnings, default 0
  TOPOLOGY_BUSY_PCT
                warn when a mapped CPU has a thread above pct, default 1.0
  AGENT_COORD_ENABLED
                request a host advisory lease for each case, default 1
  AGENT_COORD_MODE
                shared, soft-exclusive, or exclusive, default soft-exclusive
  AGENT_COORD_OWNER
                lease owner, default codex:zcutils:$RUN_ID
  AGENT_COORD_PRIORITY
                advisory relative priority, default 50
  AGENT_COORD_TTL
                lease TTL in seconds, default 3600

This script is a same-host descriptor/WAL control only. It never uses block
devices as mirror or stripe primitives.
EOF
}

log() {
	printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"
}

die() {
	printf 'ERROR: %s\n' "$*" >&2
	exit 1
}

COORD_TOKEN=""

coord_release() {
	if [ -n "$COORD_TOKEN" ] && command -v agent-coord >/dev/null 2>&1; then
		agent-coord release "$COORD_TOKEN" >/dev/null 2>&1 || true
	fi
	COORD_TOKEN=""
}

coord_request() {
	local case_dir="$1"
	local cpu_list="$2"
	local output log_file token honored
	log_file="$case_dir/coordination.log"
	coord_release
	if [ "$AGENT_COORD_ENABLED" != "1" ]; then
		printf 'coordination_enabled=0\ncoordination_token=disabled\ncoordination_honored=unknown\n' >"$log_file"
		return
	fi
	if ! command -v agent-coord >/dev/null 2>&1; then
		printf 'coordination_enabled=1\ncoordination_token=unavailable\ncoordination_honored=unknown\n' >"$log_file"
		log "WARNING: agent-coord is unavailable; shared-host exclusivity is unverified"
		return
	fi
	if ! output="$(agent-coord request \
		--owner "$AGENT_COORD_OWNER" \
		--mode "$AGENT_COORD_MODE" \
		--sensitivity critical \
		--priority "$AGENT_COORD_PRIORITY" \
		--ttl "$AGENT_COORD_TTL" \
		--resource "cpu=$cpu_list;memory-bandwidth=*" \
		--note "$RUN_ID shared-WAL case")"; then
		printf '%s\n' "$output" >"$log_file"
		die "agent-coord refused resource claim for CPUs $cpu_list"
	fi
	token="$(field_from_line token "$output")"
	honored="$(field_from_line honored "$output")"
	[ -n "$token" ] || die "agent-coord returned no lease token"
	COORD_TOKEN="$token"
	{
		printf 'coordination_enabled=1\n'
		printf 'coordination_token=%s\n' "$token"
		printf 'coordination_honored=%s\n' "${honored:-unknown}"
		printf 'coordination_mode=%s\n' "$AGENT_COORD_MODE"
		printf 'coordination_owner=%s\n' "$AGENT_COORD_OWNER"
		printf 'coordination_priority=%s\n' "$AGENT_COORD_PRIORITY"
		printf 'coordination_resource=cpu=%s;memory-bandwidth=*\n' "$cpu_list"
		printf 'coordination_result=%s\n' "$output"
	} >"$log_file"
	if [ "$honored" != "true" ]; then
		log "WARNING: soft exclusivity was not honored for CPUs $cpu_list: $output"
	fi
}

range_list() {
	local start="$1"
	local count="$2"
	local end
	[ "$count" -gt 0 ] || die "range_list count must be positive"
	end=$((start + count - 1))
	if [ "$start" -eq "$end" ]; then
		printf '%s\n' "$start"
	else
		printf '%s-%s\n' "$start" "$end"
	fi
}

cpu_in_range() {
	local cpu="$1"
	local start="$2"
	local end="$3"
	[ "$cpu" -ge "$start" ] && [ "$cpu" -le "$end" ]
}

range_has_smt_overlap() {
	local start="$1"
	local end="$2"
	local cpu token sib lo hi
	for ((cpu = start; cpu <= end; cpu++)); do
		[ -r "/sys/devices/system/cpu/cpu${cpu}/topology/thread_siblings_list" ] || continue
		for token in $(tr ',' ' ' <"/sys/devices/system/cpu/cpu${cpu}/topology/thread_siblings_list"); do
			if [[ "$token" == *-* ]]; then
				lo="${token%-*}"
				hi="${token#*-}"
				for ((sib = lo; sib <= hi; sib++)); do
					if [ "$sib" -ne "$cpu" ] && cpu_in_range "$sib" "$start" "$end"; then
						return 0
					fi
				done
			else
				sib="$token"
				if [ "$sib" -ne "$cpu" ] && cpu_in_range "$sib" "$start" "$end"; then
					return 0
				fi
			fi
		done
	done
	return 1
}

expand_cpu_list() {
	local list="$1"
	local part start end cpu
	list="${list// /}"
	[ -n "$list" ] || return 0
	IFS=',' read -r -a parts <<<"$list"
	for part in "${parts[@]}"; do
		[ -n "$part" ] || continue
		if [[ "$part" =~ ^([0-9]+)-([0-9]+)$ ]]; then
			start="${BASH_REMATCH[1]}"
			end="${BASH_REMATCH[2]}"
			if [ "$start" -gt "$end" ]; then
				die "invalid descending CPU range: $part"
			fi
			for ((cpu = start; cpu <= end; cpu++)); do
				printf '%s\n' "$cpu"
			done
		elif [[ "$part" =~ ^[0-9]+$ ]]; then
			printf '%s\n' "$part"
		else
			die "invalid CPU list element: $part"
		fi
	done
}

case_preflight_warn() {
	local log_file="$1"
	shift
	PREFLIGHT_WARNINGS=$((PREFLIGHT_WARNINGS + 1))
	printf 'PERF WARNING: %s\n' "$*" >>"$log_file"
	log "WARNING: $*"
}

run_case_preflight() {
	local case_dir="$1"
	local cpu_list="$2"
	local log_file="$case_dir/topology-preflight.log"
	local cpu governor non_perf busy_rows memlock hugepages

	PREFLIGHT_WARNINGS=0
	: >"$log_file"
	printf 'topology_preflight=1\n' >>"$log_file"
	printf 'cpu_list=%s\n' "$cpu_list" >>"$log_file"
	printf 'topology_busy_pct=%s\n' "$TOPOLOGY_BUSY_PCT" >>"$log_file"
	printf 'strict_local_shared_host=yes\n' >>"$log_file"
	printf 'zero_copy_forwarding_target=%s\n' "$ZERO_COPY_FORWARDING_TARGET" >>"$log_file"

	non_perf=""
	while read -r cpu; do
		[ -n "$cpu" ] || continue
		if [ -r "/sys/devices/system/cpu/cpu${cpu}/cpufreq/scaling_governor" ]; then
			governor="$(cat "/sys/devices/system/cpu/cpu${cpu}/cpufreq/scaling_governor")"
			printf 'cpu%s_governor=%s\n' "$cpu" "$governor" >>"$log_file"
			if [ "$governor" != "performance" ]; then
				if [ -n "$non_perf" ]; then
					non_perf="$non_perf,"
				fi
				non_perf="${non_perf}cpu${cpu}=${governor}"
			fi
		fi
	done < <(expand_cpu_list "$cpu_list")
	if [ -n "$non_perf" ]; then
		case_preflight_warn "$log_file" "mapped CPUs are not all using performance governor: $non_perf"
	fi

	busy_rows="$(
		ps -eLo psr,pcpu,pid,tid,comm --no-headers 2>/dev/null |
		awk -v cpus="$cpu_list" -v pct="$TOPOLOGY_BUSY_PCT" '
			function add_cpu(c) { wanted[c] = 1 }
			BEGIN {
				n = split(cpus, parts, ",")
				for (i = 1; i <= n; i++) {
					if (parts[i] ~ /^[0-9]+-[0-9]+$/) {
						split(parts[i], r, "-")
						for (c = r[1]; c <= r[2]; c++) add_cpu(c)
					} else if (parts[i] ~ /^[0-9]+$/) {
						add_cpu(parts[i])
					}
				}
			}
			($1 in wanted) && ($2 + 0 > pct) {
				printf "busy_cpu=%s pcpu=%s pid=%s tid=%s comm=%s\n", $1, $2, $3, $4, $5
			}
		'
	)"
	if [ -n "$busy_rows" ]; then
		printf '%s\n' "$busy_rows" >>"$log_file"
		case_preflight_warn "$log_file" "mapped CPUs already have threads above ${TOPOLOGY_BUSY_PCT}% CPU; benchmark is noisy unless those cores are isolated"
	fi

	memlock="$(ulimit -l 2>/dev/null || true)"
	printf 'memlock_limit=%s\n' "$memlock" >>"$log_file"
	if [ "$memlock" != "unlimited" ] && [[ "$memlock" =~ ^[0-9]+$ ]] && [ "$memlock" -lt 1048576 ]; then
		case_preflight_warn "$log_file" "memlock is $memlock; zero-copy/RDMA/hugetlb comparisons are not representative without enough locked-memory headroom"
	fi

	hugepages="$(cat /proc/sys/vm/nr_hugepages 2>/dev/null || printf 'unknown')"
	printf 'nr_hugepages=%s\n' "$hugepages" >>"$log_file"
	if [ "$hugepages" = "0" ]; then
		case_preflight_warn "$log_file" "vm.nr_hugepages is 0; this is a smoke run for hugetlb-backed zero-copy/RDMA comparisons"
	fi
	if [ "$ZERO_COPY_FORWARDING_TARGET" = "1" ] && { [ "$FAN_TOUCH_PAYLOAD" != "0" ] || [ "$LEAF_TOUCH_PAYLOAD" != "0" ]; }; then
		case_preflight_warn "$log_file" "ZERO_COPY_FORWARDING_TARGET=1 requires FAN_TOUCH_PAYLOAD=0 and LEAF_TOUCH_PAYLOAD=0; payload inspection is not a representative forwarding path"
	fi

	printf 'topology_preflight_warnings=%s\n' "$PREFLIGHT_WARNINGS" >>"$log_file"
	if [ "$PREFLIGHT_WARNINGS" -gt 0 ]; then
		printf 'topology_preflight_representative=0\n' >>"$log_file"
		if [ "$TOPOLOGY_PREFLIGHT_FATAL" = "1" ]; then
			die "topology preflight found $PREFLIGHT_WARNINGS issue(s); see $log_file"
		fi
	else
		printf 'topology_preflight_representative=1\n' >>"$log_file"
	fi
}

roles_for_workload() {
	case "$1" in
		zcnblk-shm-onramp) printf '2\n' ;;
		zcnblk-shm-pipeline) printf '3\n' ;;
		zcnblk-shm-mirror) printf '4\n' ;;
		zcnblk-shm-mirror-dirty-cache) printf '4\n' ;;
		*) die "unsupported workload: $1" ;;
	esac
}

summary_prefix_for_workload() {
	case "$1" in
		zcnblk-shm-onramp) printf 'zcfanout-shmlease-summary:' ;;
		zcnblk-shm-pipeline) printf 'zcfanout-shmlease-pipeline-summary:' ;;
		zcnblk-shm-mirror) printf 'zcfanout-shmlease-mirror-summary:' ;;
		zcnblk-shm-mirror-dirty-cache) printf 'zcfanout-shmlease-mirror-dirty-cache-summary:' ;;
		*) die "unsupported workload: $1" ;;
	esac
}

wait_prefix_for_workload() {
	case "$1" in
		zcnblk-shm-onramp) printf 'zcfanout-shmlease-wait-latency-summary:' ;;
		zcnblk-shm-pipeline) printf 'zcfanout-shmlease-pipeline-wait-latency-summary:' ;;
		zcnblk-shm-mirror) printf 'zcfanout-shmlease-mirror-wait-latency-summary:' ;;
		zcnblk-shm-mirror-dirty-cache) printf 'zcfanout-shmlease-mirror-dirty-cache-wait-latency-summary:' ;;
		*) die "unsupported workload: $1" ;;
	esac
}

field_from_line() {
	local key="$1"
	local line="$2"
	printf '%s\n' "$line" | tr ' ' '\n' | awk -F= -v key="$key" '$1 == key { print substr($0, length(key) + 2); found=1; exit } END { if (!found) print "" }'
}

field_from_file() {
	local key="$1"
	local file="$2"
	[ -r "$file" ] || {
		printf '\n'
		return
	}
	awk -F= -v key="$key" '$1 == key { print substr($0, length(key) + 2); found=1; exit } END { if (!found) print "" }' "$file"
}

append_summary_row() {
	local workload="$1"
	local lanes="$2"
	local touch="$3"
	local repeat="$4"
	local log_file="$5"
	local summary_prefix wait_prefix summary_line wait_line
	summary_prefix="$(summary_prefix_for_workload "$workload")"
	wait_prefix="$(wait_prefix_for_workload "$workload")"
	summary_line="$(grep -F "$summary_prefix" "$log_file" | tail -1 || true)"
	wait_line="$(grep -F "$wait_prefix" "$log_file" | tail -1 || true)"
	[ -n "$summary_line" ] || die "$log_file missing $summary_prefix"
	[ -n "$wait_line" ] || die "$log_file missing $wait_prefix"

	local branch_gbit case_dir preflight_log preflight_representative preflight_warnings
	branch_gbit="$(field_from_line branch_Gibitps "$summary_line")"
	[ -n "$branch_gbit" ] || branch_gbit="$(field_from_line leaf_reference_Gibitps "$summary_line")"
	[ -n "$branch_gbit" ] || branch_gbit="$(field_from_line mirror_reference_Gibitps "$summary_line")"
	case_dir="$(dirname "$log_file")"
	preflight_log="$case_dir/topology-preflight.log"
	preflight_representative="$(field_from_file topology_preflight_representative "$preflight_log")"
	preflight_warnings="$(field_from_file topology_preflight_warnings "$preflight_log")"
	local coordination_log
	coordination_log="$case_dir/coordination.log"

	local -a row
	row=( \
		"$workload" \
		"$lanes" \
		"$touch" \
		"$repeat" \
		"$preflight_representative" \
		"$preflight_warnings" \
		"$(field_from_line logical_4k_iops "$summary_line")" \
		"$(field_from_line first_hop_Gibitps "$summary_line")" \
		"$branch_gbit" \
		"$(field_from_line payload_copy_bytes "$summary_line")" \
		"$(field_from_line payload_reference_bytes "$summary_line")" \
		"$(field_from_line control_descriptor_bytes "$summary_line")" \
		"$(field_from_line observed_payload_touch_bytes "$summary_line")" \
		"$(field_from_line local_memory_traffic_bytes_lower_bound "$summary_line")" \
		"$(field_from_line payload_slots_per_lane "$summary_line")" \
		"$(field_from_line working_set_records "$summary_line")" \
		"$(field_from_line client_visible_read_records "$summary_line")" \
		"$(field_from_line internal_dirty_cache_read_records "$summary_line")" \
		"$(field_from_line dirty_table_bytes "$summary_line")" \
		"$(field_from_line syncs "$summary_line")" \
		"$(field_from_line sync_wait_seconds "$summary_line")" \
		"$(field_from_line dirty_window_drains "$summary_line")" \
		"$(field_from_line ack_records "$summary_line")" \
		"$(field_from_line acks "$summary_line")" \
		"$(field_from_line ack_wait_seconds "$summary_line")" \
		"$(field_from_line voluntary_ctxt_switches "$summary_line")" \
		"$(field_from_line involuntary_ctxt_switches "$summary_line")" \
		"$(field_from_line migrations "$summary_line")" \
		"$(field_from_line sync_wait_p50_ns "$wait_line")" \
		"$(field_from_line ack_wait_p50_ns "$wait_line")" \
		"$(field_from_line dirty_window_wait_p50_ns "$wait_line")" \
		"$(field_from_line fan_leaf_sync_wait_p50_ns "$wait_line")" \
		"$(field_from_line fan_leaf_ack_wait_p50_ns "$wait_line")" \
			"$(field_from_line hwm_only_results "$summary_line")" \
			"$(field_from_line leaf_batch_delay_ns "$summary_line")" \
			"$(field_from_line early_client_responses "$summary_line")" \
			"$(field_from_line client_response_release "$summary_line")" \
			"$(field_from_line payload_slot_reuse "$summary_line")" \
			"$(field_from_line fan_touch_payload "$summary_line")" \
			"$(field_from_line leaf_touch_payload "$summary_line")" \
		"$ZERO_COPY_FORWARDING_TARGET" \
		"$(field_from_line lane_cpu_map "$summary_line")" \
		"$(field_from_file coordination_token "$coordination_log")" \
		"$(field_from_file coordination_honored "$coordination_log")" \
		"$(field_from_file coordination_mode "$coordination_log")" \
		"$(field_from_file coordination_resource "$coordination_log")" \
	)
	(IFS=$'\t'; printf '%s\n' "${row[*]}") >>"$SUMMARY_TSV"
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
	usage
	exit 0
fi

RUN_ID="${RUN_ID:-local-shmlease-ladder-$(date -u +%Y%m%dT%H%M%SZ)}"
OUTDIR="${OUTDIR:-$REPO_DIR/bench-results/$RUN_ID}"
LANES_LIST="${LANES_LIST:-1 2 4}"
WORKLOADS="${WORKLOADS:-zcnblk-shm-onramp zcnblk-shm-pipeline zcnblk-shm-mirror}"
TOUCH_MODES="${TOUCH_MODES:-none cacheline}"
RECORDS="${RECORDS:-500000}"
BATCH_RECORDS="${BATCH_RECORDS:-2048}"
WINDOW="${WINDOW:-8192}"
PAYLOAD_SLOTS="${PAYLOAD_SLOTS:-}"
SYNC_RECORDS="${SYNC_RECORDS:-8192}"
ACK_RECORDS="${ACK_RECORDS:-0}"
WORKING_SET_RECORDS="${WORKING_SET_RECORDS:-0}"
WAIT_MODE="${WAIT_MODE:-spin}"
REPEATS="${REPEATS:-2}"
PIN="${PIN:-true}"
CPU_BASE="${CPU_BASE:-0}"
ALLOW_SMT_OVERLAP="${ALLOW_SMT_OVERLAP:-0}"
FAN_TOUCH_PAYLOAD="${FAN_TOUCH_PAYLOAD:-1}"
LEAF_TOUCH_PAYLOAD="${LEAF_TOUCH_PAYLOAD:-1}"
ZERO_COPY_FORWARDING_TARGET="${ZERO_COPY_FORWARDING_TARGET:-0}"
HWM_ONLY_RESULTS="${HWM_ONLY_RESULTS:-0}"
LEAF_BATCH_DELAY_NS="${LEAF_BATCH_DELAY_NS:-${URING_PLAY_SHMLEASE_LEAF_BATCH_DELAY_NS:-0}}"
DIRTY_EARLY_RESPONSES="${DIRTY_EARLY_RESPONSES:-${URING_PLAY_SHMLEASE_DIRTY_EARLY_RESPONSES:-1}}"
ARENA_HUGEPAGE="${ARENA_HUGEPAGE:-1}"
ARENA_PREFAULT="${ARENA_PREFAULT:-0}"
TOPOLOGY_PREFLIGHT="${TOPOLOGY_PREFLIGHT:-1}"
TOPOLOGY_PREFLIGHT_FATAL="${TOPOLOGY_PREFLIGHT_FATAL:-0}"
TOPOLOGY_BUSY_PCT="${TOPOLOGY_BUSY_PCT:-1.0}"
AGENT_COORD_ENABLED="${AGENT_COORD_ENABLED:-1}"
AGENT_COORD_MODE="${AGENT_COORD_MODE:-soft-exclusive}"
AGENT_COORD_OWNER="${AGENT_COORD_OWNER:-codex:zcutils:$RUN_ID}"
AGENT_COORD_PRIORITY="${AGENT_COORD_PRIORITY:-50}"
AGENT_COORD_TTL="${AGENT_COORD_TTL:-3600}"

trap coord_release EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

case "$PIN" in
	true|false) ;;
	*) die "PIN must be true or false" ;;
esac

HOST_CPUS="$(nproc)"
[ "$REPEATS" -gt 0 ] || die "REPEATS must be positive"
[ "$RECORDS" -gt 0 ] || die "RECORDS must be positive"
[ "$BATCH_RECORDS" -gt 0 ] || die "BATCH_RECORDS must be positive"
[ "$WINDOW" -gt 0 ] || die "WINDOW must be positive"

if [ ! -x "$BIN" ]; then
	log "building zcfanout-shmlease-bench release binary"
	( cd "$REPO_DIR" && cargo build --release --bin zcfanout-shmlease-bench )
fi

mkdir -p "$OUTDIR"
SUMMARY_TSV="$OUTDIR/summary.tsv"
{
	printf 'run_id=%s\n' "$RUN_ID"
	printf 'host_cpus=%s\n' "$HOST_CPUS"
	printf 'lanes_list=%s\n' "$LANES_LIST"
	printf 'workloads=%s\n' "$WORKLOADS"
	printf 'touch_modes=%s\n' "$TOUCH_MODES"
	printf 'records_per_lane=%s\n' "$RECORDS"
	printf 'batch_records=%s\n' "$BATCH_RECORDS"
	printf 'window=%s\n' "$WINDOW"
	printf 'payload_slots=%s\n' "${PAYLOAD_SLOTS:-$WINDOW}"
	printf 'sync_records=%s\n' "$SYNC_RECORDS"
	printf 'ack_records=%s\n' "$ACK_RECORDS"
	printf 'working_set_records=%s\n' "$WORKING_SET_RECORDS"
	printf 'wait_mode=%s\n' "$WAIT_MODE"
	printf 'repeats=%s\n' "$REPEATS"
	printf 'pin=%s\n' "$PIN"
	printf 'cpu_base=%s\n' "$CPU_BASE"
	printf 'allow_smt_overlap=%s\n' "$ALLOW_SMT_OVERLAP"
	printf 'fan_touch_payload=%s\n' "$FAN_TOUCH_PAYLOAD"
	printf 'leaf_touch_payload=%s\n' "$LEAF_TOUCH_PAYLOAD"
	printf 'zero_copy_forwarding_target=%s\n' "$ZERO_COPY_FORWARDING_TARGET"
		printf 'hwm_only_results=%s\n' "$HWM_ONLY_RESULTS"
		printf 'leaf_batch_delay_ns=%s\n' "$LEAF_BATCH_DELAY_NS"
		printf 'dirty_early_responses=%s\n' "$DIRTY_EARLY_RESPONSES"
		printf 'arena_hugepage=%s\n' "$ARENA_HUGEPAGE"
	printf 'arena_prefault=%s\n' "$ARENA_PREFAULT"
	printf 'topology_preflight=%s\n' "$TOPOLOGY_PREFLIGHT"
	printf 'topology_preflight_fatal=%s\n' "$TOPOLOGY_PREFLIGHT_FATAL"
	printf 'topology_busy_pct=%s\n' "$TOPOLOGY_BUSY_PCT"
	printf 'agent_coord_enabled=%s\n' "$AGENT_COORD_ENABLED"
	printf 'agent_coord_mode=%s\n' "$AGENT_COORD_MODE"
	printf 'agent_coord_owner=%s\n' "$AGENT_COORD_OWNER"
	printf 'agent_coord_priority=%s\n' "$AGENT_COORD_PRIORITY"
	printf 'block_devices=no\n'
	printf 'copy_fallback=fatal\n'
} >"$OUTDIR/run.env"

printf 'workload\tlanes\ttouch\trepeat\ttopology_preflight_representative\ttopology_preflight_warnings\tlogical_4k_iops\tfirst_hop_Gibitps\tbranch_or_ref_Gibitps\tpayload_copy_bytes\tpayload_reference_bytes\tcontrol_descriptor_bytes\tobserved_payload_touch_bytes\tlocal_memory_traffic_bytes_lower_bound\tpayload_slots_per_lane\tworking_set_records\tclient_visible_read_records\tinternal_dirty_cache_read_records\tdirty_table_bytes\tsyncs\tsync_wait_seconds\tdirty_window_drains\tack_records\tacks\tack_wait_seconds\tvoluntary_ctxt_switches\tinvoluntary_ctxt_switches\tmigrations\tsync_wait_p50_ns\tack_wait_p50_ns\tdirty_window_wait_p50_ns\tfan_leaf_sync_wait_p50_ns\tfan_leaf_ack_wait_p50_ns\thwm_only_results\tleaf_batch_delay_ns\tearly_client_responses\tclient_response_release\tpayload_slot_reuse\tfan_touch_payload\tleaf_touch_payload\tzero_copy_forwarding_target\tlane_cpu_map\tcoordination_token\tcoordination_honored\tcoordination_mode\tcoordination_resource\n' >"$SUMMARY_TSV"

for workload in $WORKLOADS; do
	roles="$(roles_for_workload "$workload")"
	for lanes in $LANES_LIST; do
		needed_cpus=$((roles * lanes))
		last_cpu=$((CPU_BASE + needed_cpus - 1))
		if [ "$needed_cpus" -gt "$HOST_CPUS" ] || [ "$last_cpu" -ge "$HOST_CPUS" ]; then
			log "skip workload=$workload lanes=$lanes: needs CPUs $CPU_BASE-$last_cpu on host with $HOST_CPUS CPUs"
			continue
		fi
		if range_has_smt_overlap "$CPU_BASE" "$last_cpu" && [ "$ALLOW_SMT_OVERLAP" != "1" ]; then
			log "skip workload=$workload lanes=$lanes: CPUs $CPU_BASE-$last_cpu include SMT siblings; choose CPU_BASE/LANES_LIST that keeps one role per physical core"
			continue
		elif range_has_smt_overlap "$CPU_BASE" "$last_cpu"; then
			log "warning workload=$workload lanes=$lanes: CPUs $CPU_BASE-$last_cpu include SMT siblings because ALLOW_SMT_OVERLAP=1; treat results as SMT-topology-specific"
		fi
		cpu_list="$(range_list "$CPU_BASE" "$needed_cpus")"
		for touch in $TOUCH_MODES; do
			for ((repeat = 1; repeat <= REPEATS; repeat++)); do
				case_dir="$OUTDIR/${workload}-lanes${lanes}-${touch}-rep${repeat}"
				mkdir -p "$case_dir"
				log "run workload=$workload lanes=$lanes touch=$touch repeat=$repeat cpus=$cpu_list"
				coord_request "$case_dir" "$cpu_list"
				if [ "$TOPOLOGY_PREFLIGHT" = "1" ]; then
					run_case_preflight "$case_dir" "$cpu_list"
				else
					printf 'topology_preflight=0\n' >"$case_dir/topology-preflight.log"
				fi
				URING_PLAY_PIN_CPUS=1 \
				URING_PLAY_PIN_CPU_LIST="$cpu_list" \
				URING_PLAY_SHMLEASE_FAN_TOUCH_PAYLOAD="$FAN_TOUCH_PAYLOAD" \
				URING_PLAY_SHMLEASE_LEAF_TOUCH_PAYLOAD="$LEAF_TOUCH_PAYLOAD" \
				URING_PLAY_SHMLEASE_ZERO_COPY_FORWARDING_TARGET="$ZERO_COPY_FORWARDING_TARGET" \
				URING_PLAY_SHMLEASE_ACK_RECORDS="$ACK_RECORDS" \
					URING_PLAY_SHMLEASE_PAYLOAD_SLOTS="$PAYLOAD_SLOTS" \
					URING_PLAY_SHMLEASE_HWM_ONLY_RESULTS="$HWM_ONLY_RESULTS" \
					URING_PLAY_SHMLEASE_LEAF_BATCH_DELAY_NS="$LEAF_BATCH_DELAY_NS" \
					URING_PLAY_SHMLEASE_DIRTY_EARLY_RESPONSES="$DIRTY_EARLY_RESPONSES" \
					URING_PLAY_SHMLEASE_ARENA_HUGEPAGE="$ARENA_HUGEPAGE" \
				URING_PLAY_SHMLEASE_ARENA_PREFAULT="$ARENA_PREFAULT" \
					"$BIN" "$RECORDS" 4K "$BATCH_RECORDS" "$WINDOW" "$touch" "$WAIT_MODE" "$PIN" "$lanes" "$workload" "$SYNC_RECORDS" "$WORKING_SET_RECORDS" \
					| tee "$case_dir/run.log"
				append_summary_row "$workload" "$lanes" "$touch" "$repeat" "$case_dir/run.log"
				coord_release
			done
		done
	done
done

log "summary: $SUMMARY_TSV"
