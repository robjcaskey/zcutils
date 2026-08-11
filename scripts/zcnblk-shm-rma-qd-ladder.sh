#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BLOCK_RUNNER="${RMA_QD_BLOCK_RUNNER:-$ROOT/scripts/zcnblk-shm-block-bench.sh}"
RAW_RUNNER="${RMA_QD_RAW_RUNNER:-}"
BEFORE_RAW_HOOK="${RMA_QD_BEFORE_RAW_HOOK:-}"
BEFORE_BLOCK_HOOK="${RMA_QD_BEFORE_BLOCK_HOOK:-}"
AFTER_QD_HOOK="${RMA_QD_AFTER_QD_HOOK:-}"
EXTERNAL_LEAF_SUPERVISED="${RMA_QD_EXTERNAL_LEAF_SUPERVISED:-0}"
QD_LIST="${RMA_QD_LIST:-1,2,4,8,16}"
RAW_REPEATS="${RMA_QD_RAW_REPEATS:-3}"
BLOCK_REPEATS="${REPEATS:-3}"
REPRESENTATIVE_RUN="${REPRESENTATIVE:-1}"
LANE_COUNT="${LANES:-1}"
OUTROOT="${OUTROOT:-$ROOT/bench-results/zcnblk-shm-rma-qd-ladder-$(date -u +%Y%m%dT%H%M%SZ)}"

die() {
	printf 'zcnblk-shm-rma-qd-ladder: %s\n' "$*" >&2
	exit 1
}

run_hook() {
	local hook="$1"
	shift
	[ -z "$hook" ] || "$hook" "$@"
}

[ -x "$BLOCK_RUNNER" ] || die "block runner is not executable: $BLOCK_RUNNER"
[[ "$RAW_REPEATS" =~ ^[0-9]+$ ]] && [ "$RAW_REPEATS" -ge 3 ] || \
	die "RMA_QD_RAW_REPEATS must be at least 3"
[[ "$BLOCK_REPEATS" =~ ^[0-9]+$ ]] && [ "$BLOCK_REPEATS" -ge 3 ] || \
	die "REPEATS must be at least 3"
[[ "$LANE_COUNT" =~ ^[0-9]+$ ]] && [ "$LANE_COUNT" -gt 0 ] || \
	die "LANES must be a positive integer"
[[ "$REPRESENTATIVE_RUN" =~ ^[01]$ ]] || die "REPRESENTATIVE must be zero or one"
[ -z "$RAW_RUNNER" ] || [ -x "$RAW_RUNNER" ] || \
	die "raw runner is not executable: $RAW_RUNNER"
[ -z "$BEFORE_RAW_HOOK" ] || [ -x "$BEFORE_RAW_HOOK" ] || \
	die "raw setup hook is not executable: $BEFORE_RAW_HOOK"
[ -z "$BEFORE_BLOCK_HOOK" ] || [ -x "$BEFORE_BLOCK_HOOK" ] || \
	die "block setup hook is not executable: $BEFORE_BLOCK_HOOK"
[ -z "$AFTER_QD_HOOK" ] || [ -x "$AFTER_QD_HOOK" ] || \
	die "after-QD hook is not executable: $AFTER_QD_HOOK"
if [ "$REPRESENTATIVE_RUN" = 1 ]; then
	[ -n "$RAW_RUNNER" ] || \
		die "representative ladders require RMA_QD_RAW_RUNNER for matched raw RTT measurements"
	if [ -z "$BEFORE_BLOCK_HOOK" ] && [ "$EXTERNAL_LEAF_SUPERVISED" != 1 ]; then
		die "representative external OFI ladders require RMA_QD_BEFORE_BLOCK_HOOK or RMA_QD_EXTERNAL_LEAF_SUPERVISED=1"
	fi
fi

IFS=',' read -r -a qds <<<"$QD_LIST"
[ "${#qds[@]}" -gt 0 ] || die "RMA_QD_LIST is empty"
declare -A seen=()
for qd in "${qds[@]}"; do
	[[ "$qd" =~ ^(1|2|4|8|16)$ ]] || \
		die "RMA_QD_LIST entries must be one of 1,2,4,8,16; got $qd"
	[ -z "${seen[$qd]:-}" ] || die "duplicate QD in RMA_QD_LIST: $qd"
	seen[$qd]=1
done
if [ "$REPRESENTATIVE_RUN" = 1 ]; then
	for required in 1 2 4 8 16; do
		[ -n "${seen[$required]:-}" ] || \
			die "representative ladder is missing QD$required"
	done
fi

mkdir -p "$OUTROOT"
printf 'qd_list=%s lanes=%s raw_repeats=%s block_repeats=%s representative=%s\n' \
	"$QD_LIST" "$LANE_COUNT" "$RAW_REPEATS" "$BLOCK_REPEATS" "$REPRESENTATIVE_RUN" \
	| tee "$OUTROOT/ladder-topology.log"
printf 'completion_semantics=remote-read-initiator-local-cq-data-visible sync_fua=separate\n' \
	| tee -a "$OUTROOT/ladder-topology.log"
: >"$OUTROOT/ladder-summary.log"

for qd in "${qds[@]}"; do
	qd_dir="$OUTROOT/qd$qd"
	mkdir -p "$qd_dir"
	aggregate_depth=$((LANE_COUNT * qd))
	printf 'qd=%s per_worker_qd=%s workers=%s lanes=%s aggregate_outstanding_depth=%s\n' \
		"$qd" "$qd" "$LANE_COUNT" "$LANE_COUNT" "$aggregate_depth" \
		| tee "$qd_dir/qd-topology.log"

	if [ -n "$RAW_RUNNER" ]; then
		: >"$qd_dir/raw-results.log"
		for ((rep = 1; rep <= RAW_REPEATS; rep++)); do
			run_hook "$BEFORE_RAW_HOOK" "$qd" "$rep" "$qd_dir"
			raw_log="$qd_dir/raw-rep$rep.log"
			env URING_PLAY_OFI_RMA_READ_QD="$qd" \
				"$RAW_RUNNER" "$qd" "$rep" "$qd_dir" | tee "$raw_log"
			raw_summary="$(grep 'zcofi-rma-read-summary:' "$raw_log" | tail -n 1)"
			[ -n "$raw_summary" ] || die "raw QD$qd repeat $rep emitted no RMA summary"
			printf 'repeat=%s %s\n' "$rep" "$raw_summary" >>"$qd_dir/raw-results.log"
		done
	fi

	run_hook "$BEFORE_BLOCK_HOOK" "$qd" "$qd_dir"
	env BACKEND=wal-tcp START_LOCAL_LEAF=0 MODE=read READ_PERCENT=100 \
		LANES="$LANE_COUNT" REPEATS="$BLOCK_REPEATS" REPRESENTATIVE="$REPRESENTATIVE_RUN" \
		IODEPTH="$qd" KERNEL_QUEUE_DEPTH="$qd" KERNEL_PIPELINE_DEPTH="$qd" \
		URING_PLAY_ZCNBLK_SHM_READ_BATCH=1 \
		URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_US=0 \
		URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW="$qd" \
		URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS=0 \
		URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=ofi \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS=1 \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD="$qd" \
		URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE="${URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE:-1}" \
		OUTDIR="$qd_dir/block" "$BLOCK_RUNNER"

	if [ -s "$qd_dir/raw-results.log" ]; then
		awk -v qd="$qd" -v lanes="$LANE_COUNT" '
			function field(name,    i, pair) {
				for (i = 1; i <= NF; i++) {
					split($i, pair, "=")
					if (pair[1] == name) return pair[2] + 0
				}
				return 0
			}
			/zcofi-rma-read-summary:/ {
				iops = field("operation_iops")
				rtt = field("measured_raw_transport_rtt_us")
				ceiling = field("matching_theoretical_iops")
				eff = field("actual_theoretical_efficiency_pct")
				if (count == 0 || iops < min_iops) min_iops = iops
				if (count == 0 || iops > max_iops) max_iops = iops
				sum_iops += iops; sum_rtt += rtt; sum_ceiling += ceiling; sum_eff += eff; count++
			}
			END {
				if (!count) exit 1
				mean_iops = sum_iops / count
				printf "qd=%d raw_runs=%d raw_min_iops=%.0f raw_mean_iops=%.0f raw_max_iops=%.0f raw_spread_pct=%.2f measured_raw_transport_rtt_us=%.3f matching_raw_theoretical_iops=%.0f raw_actual_theoretical_efficiency_pct=%.2f per_worker_qd=%d workers=%d lanes=%d aggregate_outstanding_depth=%d\n", qd, count, min_iops, mean_iops, max_iops, (max_iops-min_iops)/mean_iops*100, sum_rtt/count, sum_ceiling/count, sum_eff/count, qd, lanes, lanes, qd*lanes
			}
		' "$qd_dir/raw-results.log" | tee "$qd_dir/raw-summary.log"
	fi

	block_summary="$(head -n 1 "$qd_dir/block/summary.log")"
	printf 'qd=%s %s\n' "$qd" "$block_summary" | tee "$qd_dir/block-summary.log"
	if [ -s "$qd_dir/raw-summary.log" ]; then
		awk -v qd="$qd" -v lanes="$LANE_COUNT" -v block_summary="$block_summary" '
			function named(text, name,    count, fields, i, pair) {
				count = split(text, fields, " ")
				for (i = 1; i <= count; i++) {
					split(fields[i], pair, "=")
					if (pair[1] == name) return pair[2] + 0
				}
				return 0
			}
			{
				rtt = named($0, "measured_raw_transport_rtt_us")
				raw_iops = named($0, "raw_mean_iops")
				block_iops = named(block_summary, "mean_iops")
				ceiling = rtt > 0 ? lanes * qd * 1000000 / rtt : 0
				eff = ceiling > 0 ? block_iops * 100 / ceiling : 0
				printf "qd=%d per_worker_qd=%d workers=%d lanes=%d aggregate_outstanding_depth=%d measured_raw_transport_rtt_us=%.3f matching_theoretical_iops=%.0f raw_mean_iops=%.0f block_mean_iops=%.0f block_actual_theoretical_efficiency_pct=%.2f completion=remote-read-initiator-local-cq-data-visible\n", qd, qd, lanes, lanes, lanes*qd, rtt, ceiling, raw_iops, block_iops, eff
			}
		' "$qd_dir/raw-summary.log" | tee "$qd_dir/latency-efficiency.log" \
			| tee -a "$OUTROOT/ladder-summary.log"
	else
		printf 'qd=%s raw_rtt=missing block_summary=%s representative_eligible=false\n' \
			"$qd" "$block_summary" | tee -a "$OUTROOT/ladder-summary.log"
	fi
	run_hook "$AFTER_QD_HOOK" "$qd" "$qd_dir"
done

printf 'artifact=%s\n' "$OUTROOT" | tee -a "$OUTROOT/ladder-summary.log"
