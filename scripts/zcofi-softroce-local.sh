#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTROOT="${OUTROOT:-$ROOT/bench-results/zcofi-softroce-local-$(date -u +%Y%m%dT%H%M%SZ)}"
NS="${ZCOFI_RXE_NETNS:-zcrxe-zcutils}"
CLIENT_DEVICE="${ZCOFI_RXE_CLIENT_DEVICE:-rxe_zc_c}"
TARGET_DEVICE="${ZCOFI_RXE_TARGET_DEVICE:-rxe_zc_t}"
CLIENT_LINK="${ZCOFI_RXE_CLIENT_LINK:-zcrxec0}"
PROVIDER="${ZCOFI_SOFTROCE_PROVIDER:-verbs;ofi_rxd}"
REPEATS="${ZCOFI_SOFTROCE_REPEATS:-3}"
CLIENT_CPU_POOL="${ZCOFI_SOFTROCE_CLIENT_CPU_POOL:-0,2,4,6,8,10,12,14}"
TARGET_CPU_POOL="${ZCOFI_SOFTROCE_TARGET_CPU_POOL:-1,3,5,7,11,15,17,18}"
MULTI_QP_COUNTS="${ZCOFI_SOFTROCE_MULTI_QP_COUNTS:-2}"
PROBE_QP_COUNTS="${ZCOFI_SOFTROCE_PROBE_QP_COUNTS:-4,8}"
BASE_BYTES_PER_LANE="${ZCOFI_SOFTROCE_BASE_BYTES_PER_LANE:-16M}"
MULTI_BYTES_PER_LANE="${ZCOFI_SOFTROCE_MULTI_BYTES_PER_LANE:-2M}"
cleanup_needed=0

die() {
	printf 'zcofi-softroce-local: %s\n' "$*" >&2
	exit 1
}

cpu_prefix() {
	local list="$1"
	local count="$2"
	local -a cpus
	local result=
	[[ "$list" =~ ^[0-9]+(,[0-9]+)*$ ]] || die "invalid CPU pool: $list"
	IFS=',' read -r -a cpus <<<"$list"
	[ "${#cpus[@]}" -ge "$count" ] || \
		die "CPU pool $list has ${#cpus[@]} entries but $count are required"
	for ((index = 0; index < count; index++)); do
		[ -z "$result" ] || result+=','
		result+="${cpus[$index]}"
	done
	printf '%s' "$result"
}

capture_process_noise() {
	local output="$1"
	{
		printf 'captured_at=%s\n' "$(date -u +%FT%TZ)"
		printf 'loadavg=%s\n' "$(</proc/loadavg)"
		free -h
		ps -eo psr,pcpu,pmem,pid,comm,args --sort=-pcpu | sed -n '1,40p'
	} >"$output"
}

provider_probe() {
	local provider="$1"
	local domain="$2"
	local label="$3"
	printf 'probe=%s provider=%s domain=%s endpoint=FI_EP_RDM capability_only=yes\n' \
		"$label" "$provider" "$domain"
	if fi_info -p "$provider" -t FI_EP_RDM -d "$domain"; then
		printf 'probe=%s status=fi_getinfo-success runtime_enable_not_proven\n' "$label"
	else
		local rc=$?
		printf 'probe=%s status=fi_getinfo-failed rc=%s\n' "$label" "$rc"
	fi
}

cleanup() {
	local status=$?
	trap - EXIT INT TERM
	if [ "$cleanup_needed" = 1 ]; then
		"$ROOT/scripts/zcofi-softroce-netns.sh" cleanup | tee -a "$OUTROOT/cleanup.log" || status=1
	fi
	exit "$status"
}
trap cleanup EXIT INT TERM

[[ "$REPEATS" =~ ^[0-9]+$ ]] && [ "$REPEATS" -ge 3 ] || \
	die "shared-system Soft-RoCE runs require at least three repeats"
if [[ "$PROVIDER" == *ofi_rxd* ]]; then
	CLIENT_DOMAIN="${ZCOFI_SOFTROCE_CLIENT_DOMAIN:-$CLIENT_DEVICE-dgram}"
	TARGET_DOMAIN="${ZCOFI_SOFTROCE_TARGET_DOMAIN:-$TARGET_DEVICE-dgram}"
	PROVIDER_PATH=software-rxe-ud-rxd-emulated-rma
else
	CLIENT_DOMAIN="${ZCOFI_SOFTROCE_CLIENT_DOMAIN:-$CLIENT_DEVICE}"
	TARGET_DOMAIN="${ZCOFI_SOFTROCE_TARGET_DOMAIN:-$TARGET_DEVICE}"
	PROVIDER_PATH=verbs-rc-rxm
fi
IFS=',' read -r -a multi_qp_counts <<<"$MULTI_QP_COUNTS"
for lanes in "${multi_qp_counts[@]}"; do
	[[ "$lanes" =~ ^(2|4|8)$ ]] || die "multi-QP counts must be selected from 2,4,8"
done
IFS=',' read -r -a probe_qp_counts <<<"$PROBE_QP_COUNTS"
for lanes in "${probe_qp_counts[@]}"; do
	[[ "$lanes" =~ ^(4|8)$ ]] || die "bounded probe QP counts must be selected from 4,8"
done

mkdir -p "$OUTROOT"
capture_process_noise "$OUTROOT/process-noise.before.log"
printf 'classification=soft-roce-semantic-rehearsal representative=false shared_system=yes provider=%s provider_path=%s client_domain=%s target_domain=%s repeats=%s client_cpu_pool=%s target_cpu_pool=%s coordination_token=%s coordination_honored=%s\n' \
	"$PROVIDER" "$PROVIDER_PATH" "$CLIENT_DOMAIN" "$TARGET_DOMAIN" "$REPEATS" \
	"$CLIENT_CPU_POOL" "$TARGET_CPU_POOL" \
	"${AGENT_COORD_TOKEN:-unreported}" "${AGENT_COORD_HONORED:-unreported}" \
	| tee "$OUTROOT/run-manifest.log"
cargo build --release --manifest-path "$ROOT/Cargo.toml" --bin zcutils \
	2>&1 | tee "$OUTROOT/build.log"
"$ROOT/scripts/zcofi-softroce-netns.sh" setup | tee "$OUTROOT/setup.log"
cleanup_needed=1

{
	provider_probe 'verbs;ofi_rxm' "$CLIENT_DEVICE" client-rxm
	provider_probe 'verbs;ofi_rxd' "$CLIENT_DEVICE-dgram" client-rxd
	printf 'target_namespace=%s\n' "$NS"
	if sudo -n ip netns exec "$NS" fi_info -p 'verbs;ofi_rxm' -t FI_EP_RDM -d "$TARGET_DEVICE"; then
		printf 'probe=target-rxm status=fi_getinfo-success runtime_enable_not_proven\n'
	else
		printf 'probe=target-rxm status=fi_getinfo-failed rc=%s\n' "$?"
	fi
	if sudo -n ip netns exec "$NS" fi_info -p 'verbs;ofi_rxd' -t FI_EP_RDM -d "$TARGET_DEVICE-dgram"; then
		printf 'probe=target-rxd status=fi_getinfo-success runtime_enable_not_proven\n'
	else
		printf 'probe=target-rxd status=fi_getinfo-failed rc=%s\n' "$?"
	fi
} >"$OUTROOT/provider-capabilities.log" 2>&1

one_client_cpu="$(cpu_prefix "$CLIENT_CPU_POOL" 1)"
one_target_cpu="$(cpu_prefix "$TARGET_CPU_POOL" 1)"
env \
	OUTROOT="$OUTROOT/native-rc" \
	ZCOFI_SOFTROCE_REPEATS="$REPEATS" \
	ZCOFI_SOFTROCE_CLIENT_CPU="$one_client_cpu" \
	ZCOFI_SOFTROCE_TARGET_CPU="$one_target_cpu" \
	AGENT_COORD_TOKEN="${AGENT_COORD_TOKEN:-unreported}" \
	AGENT_COORD_HONORED="${AGENT_COORD_HONORED:-unreported}" \
	"$ROOT/scripts/zcofi-softroce-rc-smoke.sh" \
	2>&1 | tee "$OUTROOT/native-rc.console.log"

# On this RXE device, verbs/RxM advertises RDM but the utility provider cannot
# construct a usable CQ/metadata path at normal queue sizes.  Preserve a
# bounded runtime probe so a later rdma-core/libfabric update cannot silently
# change that classification.
mkdir -p "$OUTROOT/rxm-runtime-probe"
if env \
	ZCOFI_SOFTROCE_PROVIDER='verbs;ofi_rxm' \
	ZCOFI_SOFTROCE_CLIENT_DOMAIN="$CLIENT_DEVICE" \
	ZCOFI_SOFTROCE_TARGET_DOMAIN="$TARGET_DEVICE" \
	ZCOFI_SOFTROCE_BYTES_PER_LANE=1M \
	ZCOFI_SOFTROCE_TIMEOUT_MS=3000 \
	ZCOFI_SOFTROCE_RXM_MSG_TX_SIZE=4 \
	ZCOFI_SOFTROCE_RXM_MSG_RX_SIZE=4 \
	ZCOFI_SOFTROCE_RXM_TX_SIZE=8 \
	ZCOFI_SOFTROCE_RXM_RX_SIZE=8 \
	ZCOFI_SOFTROCE_VERBS_TX_SIZE=8 \
	ZCOFI_SOFTROCE_VERBS_RX_SIZE=8 \
	ZCOFI_SOFTROCE_TX_QUEUE_DEPTH=4 \
	ZCOFI_SOFTROCE_RX_QUEUE_DEPTH=4 \
	ZCOFI_SOFTROCE_CLIENT_CPU_LIST="$one_client_cpu" \
	ZCOFI_SOFTROCE_TARGET_CPU_LIST="$one_target_cpu" \
	AGENT_COORD_TOKEN="${AGENT_COORD_TOKEN:-unreported}" \
	AGENT_COORD_HONORED="${AGENT_COORD_HONORED:-unreported}" \
	"$ROOT/scripts/zcofi-softroce-point.sh" read 1 1 "$OUTROOT/rxm-runtime-probe" \
	>"$OUTROOT/rxm-runtime-probe/console.log" 2>&1; then
	printf 'rxm_runtime_probe=pass qualification=unexpected-runtime-success\n' \
		| tee "$OUTROOT/rxm-runtime-probe/result.log"
else
	rxm_rc=$?
	printf 'rxm_runtime_probe=blocked rc=%s qualification=local-rxe-utility-provider-limitation\n' \
		"$rxm_rc" | tee "$OUTROOT/rxm-runtime-probe/result.log"
fi

run_matrix() {
	local name="$1"
	local lanes="$2"
	local modes="$3"
	local low_qds="$4"
	local saturation_qds="$5"
	local bytes_per_lane="$6"
	local client_cpus target_cpus topology
	client_cpus="$(cpu_prefix "$CLIENT_CPU_POOL" "$lanes")"
	target_cpus="$(cpu_prefix "$TARGET_CPU_POOL" "$lanes")"
	topology="$OUTROOT/topology-$name.log"
	env \
		ZCOFI_RDMA_PREFLIGHT_KIND=rxe \
		ZCOFI_RDMA_DEVICE="$CLIENT_DEVICE" \
		ZCOFI_RDMA_NETDEV="$CLIENT_LINK" \
		ZCOFI_RDMA_LANES="$lanes" \
		ZCOFI_RDMA_REGISTERED_BYTES=$((lanes * 8 * 1024 * 1024 + 1024 * 1024)) \
		ZCOFI_RMA_MATRIX_PROVIDER="$PROVIDER" \
		ZCOFI_RMA_MATRIX_NIC_DEVICE="$CLIENT_DEVICE" \
		URING_PLAY_OFI_DOMAIN="$CLIENT_DOMAIN" \
		URING_PLAY_PIN_CPUS=1 \
		URING_PLAY_PIN_CPU_LIST="$client_cpus" \
		URING_PLAY_OFI_CQ_SLEEP_NS=0 \
		URING_PLAY_OFI_TX_QUEUE_DEPTH=8 \
		URING_PLAY_OFI_RX_QUEUE_DEPTH=8 \
		URING_PLAY_OFI_RMA_READ_QD=64 \
		URING_PLAY_OFI_RMA_WRITE_QD=64 \
		URING_PLAY_TOPOLOGY_STRICT=0 \
		FI_VERBS_DEVICE_NAME="$CLIENT_DEVICE" \
		FI_VERBS_IFACE="$CLIENT_LINK" \
		FI_VERBS_GID_IDX="${ZCOFI_SOFTROCE_GID_INDEX:-1}" \
		AGENT_COORD_TOKEN="${AGENT_COORD_TOKEN:-unreported}" \
		AGENT_COORD_HONORED="${AGENT_COORD_HONORED:-unreported}" \
		"$ROOT/scripts/zcofi-rdma-topology-preflight.sh" \
		2>&1 | tee "$topology"

	env \
		OUTROOT="$OUTROOT/$name" \
		ZCOFI_RMA_MATRIX_RUNNER="$ROOT/scripts/zcofi-softroce-point.sh" \
		ZCOFI_RMA_MATRIX_MODES="$modes" \
		ZCOFI_RMA_MATRIX_LOW_QDS="$low_qds" \
		ZCOFI_RMA_MATRIX_SATURATION_QDS="$saturation_qds" \
		ZCOFI_RMA_MATRIX_REPEATS="$REPEATS" \
		ZCOFI_RMA_MATRIX_REPRESENTATIVE=0 \
		ZCOFI_RMA_MATRIX_PROVIDER="$PROVIDER" \
		ZCOFI_RMA_MATRIX_NIC_DEVICE="$CLIENT_DEVICE" \
		ZCOFI_RMA_MATRIX_TOPOLOGY_ARTIFACT="$topology" \
		ZCOFI_SOFTROCE_PROVIDER="$PROVIDER" \
		ZCOFI_SOFTROCE_CLIENT_DOMAIN="$CLIENT_DOMAIN" \
		ZCOFI_SOFTROCE_TARGET_DOMAIN="$TARGET_DOMAIN" \
		ZCOFI_SOFTROCE_LANES="$lanes" \
		ZCOFI_SOFTROCE_WORKERS="$lanes" \
		ZCOFI_SOFTROCE_CLIENT_CPU_LIST="$client_cpus" \
		ZCOFI_SOFTROCE_TARGET_CPU_LIST="$target_cpus" \
		ZCOFI_SOFTROCE_BYTES_PER_LANE="$bytes_per_lane" \
		ZCOFI_SOFTROCE_TX_QUEUE_DEPTH=8 \
		ZCOFI_SOFTROCE_RX_QUEUE_DEPTH=8 \
		URING_PLAY_OFI_DOMAIN="$CLIENT_DOMAIN" \
		URING_PLAY_OFI_CQ_SLEEP_NS=0 \
		URING_PLAY_PIN_CPUS=1 \
		URING_PLAY_PIN_CPU_LIST="$client_cpus" \
		URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 \
		URING_PLAY_OFI_RMA_WRITE_MORE=0 \
		AGENT_COORD_TOKEN="${AGENT_COORD_TOKEN:-unreported}" \
		AGENT_COORD_HONORED="${AGENT_COORD_HONORED:-unreported}" \
		"$ROOT/scripts/zcofi-rma-queue-matrix.sh" \
		2>&1 | tee "$OUTROOT/$name.console.log"
}

# RXD supplies emulated RMA over RXE UD, so this is a semantic rehearsal only.
# Reads have proved stable through the full low-QD and saturation curves.  RXD
# delivery-complete writes have proved stable through QD4, while repeated QD8
# stress can stall or fault in the local RXE/libfabric stack (direct perftest
# QD8 can stall too).  Keep the supported and blocked qualifications explicit
# instead of printing a partial curve as if it were a hardware result.
run_matrix qp1-read 1 read "${ZCOFI_SOFTROCE_LOW_QDS:-1,2,4,8,16}" \
	"${ZCOFI_SOFTROCE_SATURATION_QDS:-32,64}" "$BASE_BYTES_PER_LANE"
run_matrix qp1-write-qualified 1 write "${ZCOFI_SOFTROCE_WRITE_QDS:-1,2,4}" \
	none "$BASE_BYTES_PER_LANE"
printf 'provider_path=%s operation=write qualified_per_worker_qd_max=4 blocked_qds=8-plus reason=local-rxe-libfabric-rxd-progress-instability hardware_forecast=no\n' \
	"$PROVIDER_PATH" | tee "$OUTROOT/rxd-write-qualification.log"
for lanes in "${multi_qp_counts[@]}"; do
	fixed_aggregate_qd=$((32 / lanes))
	run_matrix "qp${lanes}-read" "$lanes" read "1,$fixed_aggregate_qd" 32 \
		"$MULTI_BYTES_PER_LANE"
	run_matrix "qp${lanes}-write-qualified" "$lanes" write 1,4 none \
		"$MULTI_BYTES_PER_LANE"
done

run_bounded_multi_probe() {
	local lanes="$1"
	local client_cpus target_cpus probe_root
	local point qualification rc
	local passes=0
	local failures=0
	client_cpus="$(cpu_prefix "$CLIENT_CPU_POOL" "$lanes")"
	target_cpus="$(cpu_prefix "$TARGET_CPU_POOL" "$lanes")"
	probe_root="$OUTROOT/qp${lanes}-bounded-provider-probe"
	mkdir -p "$probe_root"
	: >"$probe_root/result.log"
	for ((repeat = 1; repeat <= REPEATS; repeat++)); do
		point="$probe_root/rep$repeat"
		mkdir -p "$point"
		if env \
			ZCOFI_SOFTROCE_PROVIDER="$PROVIDER" \
			ZCOFI_SOFTROCE_CLIENT_DOMAIN="$CLIENT_DOMAIN" \
			ZCOFI_SOFTROCE_TARGET_DOMAIN="$TARGET_DOMAIN" \
			ZCOFI_SOFTROCE_LANES="$lanes" \
			ZCOFI_SOFTROCE_WORKERS="$lanes" \
			ZCOFI_SOFTROCE_CLIENT_CPU_LIST="$client_cpus" \
			ZCOFI_SOFTROCE_TARGET_CPU_LIST="$target_cpus" \
			ZCOFI_SOFTROCE_BYTES_PER_LANE=1M \
			ZCOFI_SOFTROCE_TIMEOUT_MS=5000 \
			ZCOFI_SOFTROCE_TX_QUEUE_DEPTH=8 \
			ZCOFI_SOFTROCE_RX_QUEUE_DEPTH=8 \
			AGENT_COORD_TOKEN="${AGENT_COORD_TOKEN:-unreported}" \
			AGENT_COORD_HONORED="${AGENT_COORD_HONORED:-unreported}" \
			"$ROOT/scripts/zcofi-softroce-point.sh" read 1 "$repeat" "$point" \
			>"$point/console.log" 2>&1; then
			passes=$((passes + 1))
			printf 'lanes=%s repeat=%s per_worker_qd=1 aggregate_outstanding_depth=%s result=pass\n' \
				"$lanes" "$repeat" "$lanes" | tee -a "$probe_root/result.log"
		else
			rc=$?
			failures=$((failures + 1))
			printf 'lanes=%s repeat=%s per_worker_qd=1 aggregate_outstanding_depth=%s result=blocked rc=%s\n' \
				"$lanes" "$repeat" "$lanes" "$rc" | tee -a "$probe_root/result.log"
		fi
	done
	if [ "$failures" -eq 0 ]; then
		qualification=semantic-probe-pass-not-hardware-forecast
	else
		qualification=local-rxe-provider-endpoint-capacity-blocked
	fi
	printf 'lanes=%s repeats=%s passes=%s blocked=%s qualification=%s setup_admission=lane-ascending-serial start_barrier=failure-aware hardware_forecast=no\n' \
		"$lanes" "$REPEATS" "$passes" "$failures" "$qualification" \
		| tee -a "$probe_root/result.log"
}

for lanes in "${probe_qp_counts[@]}"; do
	run_bounded_multi_probe "$lanes"
done

capture_process_noise "$OUTROOT/process-noise.after.log"
printf 'softroce-local-result=pass representative=false provider_path=%s native_rc=pass rxd_read_semantics=pass rxd_write_qualified_qd_max=4 rxd_write_qd8_plus=blocked stable_multi_qp_counts=%s bounded_multi_qp_probes=%s rxm_runtime=blocked-local-rxe-utility-provider hardware_forecast=no artifact=%s\n' \
	"$PROVIDER_PATH" "$MULTI_QP_COUNTS" "$PROBE_QP_COUNTS" "$OUTROOT" \
	| tee "$OUTROOT/result.log"
