#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNNER="${ZCOFI_RMA_MATRIX_RUNNER:-}"
MODES="${ZCOFI_RMA_MATRIX_MODES:-read,write}"
LOW_QDS="${ZCOFI_RMA_MATRIX_LOW_QDS:-1,2,4,8,16}"
SATURATION_QDS="${ZCOFI_RMA_MATRIX_SATURATION_QDS:-32,64,128,256}"
REPEATS="${ZCOFI_RMA_MATRIX_REPEATS:-3}"
REPRESENTATIVE="${ZCOFI_RMA_MATRIX_REPRESENTATIVE:-1}"
PROVIDER="${ZCOFI_RMA_MATRIX_PROVIDER:-efa}"
NIC_DEVICE="${ZCOFI_RMA_MATRIX_NIC_DEVICE:-${FI_EFA_IFACE:-}}"
WRITE_DELIVERY_COMPLETE="${URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE:-1}"
WRITE_MORE="${URING_PLAY_OFI_RMA_WRITE_MORE:-0}"
WRITE_MORE_BURST="${URING_PLAY_OFI_RMA_WRITE_MORE_BURST:-64}"
TOPOLOGY_ARTIFACT="${ZCOFI_RMA_MATRIX_TOPOLOGY_ARTIFACT:-}"
OUTROOT="${OUTROOT:-$ROOT/bench-results/zcofi-rma-queue-matrix-$(date -u +%Y%m%dT%H%M%SZ)}"

die() {
	printf 'zcofi-rma-queue-matrix: %s\n' "$*" >&2
	exit 1
}

[ -n "$RUNNER" ] || die "set ZCOFI_RMA_MATRIX_RUNNER to an executable that runs one fresh target/client pair"
[ -x "$RUNNER" ] || die "runner is not executable: $RUNNER"
[[ "$REPEATS" =~ ^[0-9]+$ ]] && [ "$REPEATS" -ge 3 ] || \
	die "ZCOFI_RMA_MATRIX_REPEATS must be at least 3"
[[ "$REPRESENTATIVE" =~ ^[01]$ ]] || \
	die "ZCOFI_RMA_MATRIX_REPRESENTATIVE must be zero or one"
[[ "$WRITE_DELIVERY_COMPLETE" =~ ^[01]$ ]] || \
	die "URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE must be zero or one"
[[ "$WRITE_MORE" =~ ^[01]$ ]] || \
	die "URING_PLAY_OFI_RMA_WRITE_MORE must be zero or one"
[[ "$WRITE_MORE_BURST" =~ ^[0-9]+$ ]] && [ "$WRITE_MORE_BURST" -ge 1 ] && \
	[ "$WRITE_MORE_BURST" -le 65536 ] || \
	die "URING_PLAY_OFI_RMA_WRITE_MORE_BURST must be in 1..=65536"

IFS=',' read -r -a modes <<<"$MODES"
IFS=',' read -r -a low_qds <<<"$LOW_QDS"
if [ "$SATURATION_QDS" = none ]; then
	saturation_qds=()
else
	IFS=',' read -r -a saturation_qds <<<"$SATURATION_QDS"
fi
[ "${#modes[@]}" -gt 0 ] || die "mode list is empty"
[ "${#low_qds[@]}" -gt 0 ] || die "low-QD list is empty"

declare -A mode_seen=()
for mode in "${modes[@]}"; do
	case "$mode" in
		read | write | write-high-pps) ;;
		*) die "unsupported mode: $mode" ;;
	esac
	[ -z "${mode_seen[$mode]:-}" ] || die "duplicate mode: $mode"
	mode_seen[$mode]=1
done

declare -A low_seen=()
for qd in "${low_qds[@]}"; do
	[[ "$qd" =~ ^(1|2|4|8|16)$ ]] || die "low-QD entries must be 1,2,4,8,16; got $qd"
	[ -z "${low_seen[$qd]:-}" ] || die "duplicate low QD: $qd"
	low_seen[$qd]=1
done
declare -A saturation_seen=()
have_high_saturation=0
for qd in "${saturation_qds[@]}"; do
	[[ "$qd" =~ ^[0-9]+$ ]] && [ "$qd" -ge 16 ] && [ "$qd" -le 1024 ] || \
		die "saturation QD must be in 16..=1024; got $qd"
	[ -z "${saturation_seen[$qd]:-}" ] || die "duplicate saturation QD: $qd"
	saturation_seen[$qd]=1
	if [ "$qd" -ge 32 ]; then
		have_high_saturation=1
	fi
done

if [ "$REPRESENTATIVE" = 1 ]; then
	[ "$SATURATION_QDS" != none ] || die "representative matrix cannot omit the saturation curve"
	[ -n "${mode_seen[read]:-}" ] || die "representative matrix is missing read mode"
	[ -n "${mode_seen[write]:-}" ] || die "representative matrix is missing write mode"
	for required in 1 2 4 8 16; do
		[ -n "${low_seen[$required]:-}" ] || die "representative matrix is missing QD$required"
	done
	[ "$have_high_saturation" = 1 ] || die "representative matrix requires a saturation QD of at least 32"
	[ "${URING_PLAY_PIN_CPUS:-0}" = 1 ] || die "representative matrix requires URING_PLAY_PIN_CPUS=1"
	[ -n "${URING_PLAY_PIN_CPU_LIST:-}" ] || die "representative matrix requires URING_PLAY_PIN_CPU_LIST"
	[ "${URING_PLAY_OFI_CQ_SLEEP_NS:-50000}" = 0 ] || die "representative matrix requires URING_PLAY_OFI_CQ_SLEEP_NS=0"
	[ -n "${URING_PLAY_OFI_DOMAIN:-}" ] || die "representative matrix requires URING_PLAY_OFI_DOMAIN"
	[ -n "$PROVIDER" ] || die "representative matrix requires ZCOFI_RMA_MATRIX_PROVIDER"
	[ -n "$NIC_DEVICE" ] || die "representative matrix requires ZCOFI_RMA_MATRIX_NIC_DEVICE (FI_EFA_IFACE is accepted for EFA)"
	[ -s "$TOPOLOGY_ARTIFACT" ] || die "representative matrix requires a non-empty ZCOFI_RMA_MATRIX_TOPOLOGY_ARTIFACT"
fi

mkdir -p "$OUTROOT"
if [ -n "$TOPOLOGY_ARTIFACT" ]; then
	cp "$TOPOLOGY_ARTIFACT" "$OUTROOT/topology-artifact"
fi
printf 'modes=%s low_qds=%s saturation_qds=%s repeats=%s representative=%s\n' \
	"$MODES" "$LOW_QDS" "$SATURATION_QDS" "$REPEATS" "$REPRESENTATIVE" \
	| tee "$OUTROOT/matrix-topology.log"
printf 'lane_to_worker_cpu=%s ofi_provider=%s ofi_domain=%s nic_device=%s cq_sleep_ns=%s rma_write_delivery_complete=%s rma_write_more=%s rma_write_more_burst=%s completion_semantics=read:data-visible-local-cq,write:%s remote_admission=separate durability=separate\n' \
	"${URING_PLAY_PIN_CPU_LIST:-unreported}" "$PROVIDER" \
	"${URING_PLAY_OFI_DOMAIN:-unreported}" \
	"${NIC_DEVICE:-unreported}" "${URING_PLAY_OFI_CQ_SLEEP_NS:-50000}" \
	"$WRITE_DELIVERY_COMPLETE" "$WRITE_MORE" "$WRITE_MORE_BURST" \
	"$([ "$WRITE_DELIVERY_COMPLETE" = 1 ] && printf delivery-cq-remote-visible || printf source-reusable-local-cq)" \
	| tee -a "$OUTROOT/matrix-topology.log"
: >"$OUTROOT/matrix-summary.log"

run_point() {
	local mode="$1"
	local class="$2"
	local qd="$3"
	local access="$4"
	local point_dir="$OUTROOT/$mode/$class-qd$qd"
	local summary_prefix
	local expected_completion
	local expected_rtt_semantics
	local high_pps=0
	case "$mode" in
		read)
			summary_prefix='zcofi-rma-read-summary:'
			expected_completion='initiator-local-cq-data-visible'
			expected_rtt_semantics='rma-read-post-to-initiator-local-cq-data-visible'
			;;
		write)
			summary_prefix='zcofi-rma-write-summary:'
			if [ "$WRITE_DELIVERY_COMPLETE" = 1 ]; then
				expected_completion='initiator-delivery-cq-remote-visible'
				expected_rtt_semantics='rma-write-post-to-initiator-delivery-cq-remote-visible'
			else
				expected_completion='initiator-local-cq-source-reusable'
				expected_rtt_semantics='rma-write-post-to-initiator-local-cq-source-reusable'
			fi
			;;
		write-high-pps)
			summary_prefix='zcofi-rma-write-summary:'
			if [ "$WRITE_DELIVERY_COMPLETE" = 1 ]; then
				expected_completion='initiator-delivery-cq-remote-visible'
				expected_rtt_semantics='rma-write-post-to-initiator-delivery-cq-remote-visible'
			else
				expected_completion='initiator-local-cq-source-reusable'
				expected_rtt_semantics='rma-write-post-to-initiator-local-cq-source-reusable'
			fi
			high_pps=1
			;;
	esac
	mkdir -p "$point_dir"
	: >"$point_dir/repeats.log"
	for ((rep = 1; rep <= REPEATS; rep++)); do
		local log="$point_dir/rep$rep.log"
			env \
			URING_PLAY_TOPOLOGY_STRICT="$REPRESENTATIVE" \
			URING_PLAY_OFI_RMA_ACCESS_PATTERN="$access" \
			URING_PLAY_OFI_RMA_READ_QD="$qd" \
			URING_PLAY_OFI_RMA_WRITE_QD="$qd" \
			URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE="$WRITE_DELIVERY_COMPLETE" \
			URING_PLAY_OFI_RMA_WRITE_MORE="$WRITE_MORE" \
			URING_PLAY_OFI_RMA_WRITE_MORE_BURST="$WRITE_MORE_BURST" \
			URING_PLAY_OFI_EFA_WRITE_HIGH_PPS="$high_pps" \
			"$RUNNER" "$mode" "$qd" "$rep" "$point_dir" 2>&1 | tee "$log"
		local summary
		summary="$(grep "$summary_prefix" "$log" | tail -n 1)"
		[ -n "$summary" ] || die "$mode $class QD$qd repeat $rep emitted no summary"
		if [ "$REPRESENTATIVE" = 1 ]; then
			grep -Eq 'zcofi-rma-(read|write)-lane-map: lane=[0-9]+ worker=[0-9]+ cpu=[0-9]+ domain=[^ ]+ .*aggregate_mapping_declared=yes' "$log" || \
				die "$mode $class QD$qd repeat $rep emitted no explicit lane/worker/CPU/domain map"
			grep -Eq 'zcofi-rma-(read|write)-memory-preflight: .*hugepages_total=[1-9][0-9]* .*hugepages_free=[1-9][0-9]* .*needed_hugepages=[1-9][0-9]* .*memlock_bytes=' "$log" || \
				die "$mode $class QD$qd repeat $rep emitted no usable hugetlb/memlock preflight"
			grep -Eq 'zcofi-endpoint-profile: .*strict_topology=1' "$log" || \
				die "$mode $class QD$qd repeat $rep did not prove strict endpoint setup"
			grep -Eq "(^| )per_lane_qd=$qd( |$)" <<<"$summary" || \
				die "$mode $class QD$qd repeat $rep summary has the wrong per-lane QD"
			grep -Eq "(^| )access_pattern=$access( |$)" <<<"$summary" || \
				die "$mode $class QD$qd repeat $rep summary has the wrong access pattern"
			grep -Eq "(^| )completion=$expected_completion( |$)" <<<"$summary" || \
				die "$mode $class QD$qd repeat $rep summary has the wrong completion semantic"
			grep -Eq "(^| )raw_rtt_semantics=$expected_rtt_semantics( |$)" <<<"$summary" || \
				die "$mode $class QD$qd repeat $rep summary has the wrong theoretical-ceiling denominator"
			for field in per_worker_qd_min per_worker_qd_max workers lanes endpoint_count cq_owner_count endpoint_to_owner_map aggregate_outstanding_depth peak_outstanding measured_raw_transport_rtt_us raw_rtt_semantics matching_theoretical_iops actual_theoretical_efficiency_pct completion cq_poll_calls cq_batches cq_completions post_eagain local_cq_completion_count; do
				grep -q "${field}=" <<<"$summary" || \
					die "$mode $class QD$qd repeat $rep summary is missing $field"
			done
			for field in send_depth send_peak recv_depth recv_peak read_depth read_peak write_depth write_peak tx_cq_polls tx_cq_entries rx_cq_polls rx_cq_entries send_eagain recv_eagain read_eagain write_eagain send_mr_hot recv_mr_hot read_mr_hot write_mr_hot target_mr_hot rma_write_more rma_write_more_posts rma_write_flush_posts rma_write_forced_flush_posts rma_write_more_followup_eagain rma_write_force_flush fatal_rc; do
				grep -Eq "^zcofi-endpoint-stats: .*${field}=" "$log" || \
					die "$mode $class QD$qd repeat $rep endpoint statistics are missing $field"
			done
			if grep -Eq '(send_errors|recv_errors|read_errors|write_errors|tx_cq_errors|rx_cq_errors|send_mr_hot|recv_mr_hot|read_mr_hot|write_mr_hot|target_mr_hot|fatal_rc)=(-?[1-9][0-9]*)' "$log"; then
				die "$mode $class QD$qd repeat $rep reported a CQ/provider/MR fatal error"
			fi
			if grep -Eq '^zcofi-endpoint-stats: .*rma_write_force_flush=[1-9][0-9]*' "$log"; then
				die "$mode $class QD$qd repeat $rep ended with an unflushed FI_MORE boundary"
			fi
			grep -Eq 'zcofi-endpoint-profile: .*provider_tx_queue_requested=[1-9][0-9]* provider_tx_queue_size=[1-9][0-9]* provider_rx_queue_requested=[1-9][0-9]* provider_rx_queue_size=[1-9][0-9]*' "$log" || \
				die "$mode $class QD$qd repeat $rep did not report provider queue capacities"
			grep -Eq 'zcofi-rma-setup-admission: .*order=lane-ascending parallelism=1 .*status=complete' "$log" || \
				die "$mode $class QD$qd repeat $rep did not prove ordered endpoint/CM/MR setup"
			grep -Eq 'zcofi-rma-start-barrier: .*failure_aware=yes status=ready' "$log" || \
				die "$mode $class QD$qd repeat $rep did not prove the failure-aware all-owner start gate"
		fi
		if [ "$mode" = write-high-pps ]; then
			grep -Eq 'zcofi-endpoint-stats: .*efa_write_high_pps_verified=1' "$log" || \
				die "$mode $class QD$qd repeat $rep did not positively verify FI_EFA_WR_HIGH_PPS"
		fi
		printf 'repeat=%s %s\n' "$rep" "$summary" >>"$point_dir/repeats.log"
	done

	awk -v mode="$mode" -v class="$class" -v qd="$qd" -v access="$access" '
		function number(name,    i, pair) {
			for (i = 1; i <= NF; i++) {
				split($i, pair, "=")
				if (pair[1] == name) return pair[2] + 0
			}
			return 0
		}
		function text(name,    i, pair) {
			for (i = 1; i <= NF; i++) {
				split($i, pair, "=")
				if (pair[1] == name) return pair[2]
			}
			return "missing"
		}
		/zcofi-rma-(read|write)-summary:/ {
			iops = number("operation_iops")
			rtt = number("measured_raw_transport_rtt_us")
			ceiling = number("matching_theoretical_iops")
			efficiency = number("actual_theoretical_efficiency_pct")
			if (count == 0 || iops < min_iops) min_iops = iops
			if (count == 0 || iops > max_iops) max_iops = iops
			sum_iops += iops
			sum_rtt += rtt
			sum_ceiling += ceiling
			sum_efficiency += efficiency
			workers = number("workers")
			lanes = number("lanes")
			per_worker_min = number("per_worker_qd_min")
			per_worker_max = number("per_worker_qd_max")
			aggregate = number("aggregate_outstanding_depth")
			completion = text("completion")
			observed_access = text("access_pattern")
			count++
		}
		END {
			if (!count) exit 1
			mean = sum_iops / count
			if (observed_access != access) exit 2
			printf "mode=%s class=%s qd=%d access_pattern=%s repeats=%d min_iops=%.0f mean_iops=%.0f max_iops=%.0f spread_pct=%.2f measured_raw_transport_rtt_us=%.3f matching_theoretical_iops=%.0f actual_theoretical_efficiency_pct=%.2f per_worker_qd_min=%d per_worker_qd_max=%d workers=%d lanes=%d aggregate_outstanding_depth=%d completion=%s\n", mode, class, qd, access, count, min_iops, mean, max_iops, (max_iops-min_iops)/mean*100, sum_rtt/count, sum_ceiling/count, sum_efficiency/count, per_worker_min, per_worker_max, workers, lanes, aggregate, completion
		}
	' "$point_dir/repeats.log" | tee "$point_dir/summary.log" | tee -a "$OUTROOT/matrix-summary.log"
}

for mode in "${modes[@]}"; do
	for qd in "${low_qds[@]}"; do
		run_point "$mode" low "$qd" sequential
	done
	for qd in "${saturation_qds[@]}"; do
		run_point "$mode" saturation "$qd" random-permutation
	done
done

printf 'artifact=%s\n' "$OUTROOT" | tee -a "$OUTROOT/matrix-summary.log"
