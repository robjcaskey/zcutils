#!/usr/bin/env bash
set -euo pipefail

MODE="${1:?mode}"
QD="${2:?qd}"
REPEAT="${3:?repeat}"
POINT_DIR="${4:?point directory}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ZCOFI_SOFTROCE_BIN:-$ROOT/target/release/zcutils}"
NS="${ZCOFI_RXE_NETNS:-zcrxe-zcutils}"
CLIENT_DEVICE="${ZCOFI_RXE_CLIENT_DEVICE:-rxe_zc_c}"
TARGET_DEVICE="${ZCOFI_RXE_TARGET_DEVICE:-rxe_zc_t}"
CLIENT_LINK="${ZCOFI_RXE_CLIENT_LINK:-zcrxec0}"
TARGET_LINK="${ZCOFI_RXE_TARGET_LINK:-zcrxet0}"
TARGET_ADDR="${ZCOFI_RXE_TARGET_ADDR:-198.18.0.2}"
PROVIDER="${ZCOFI_SOFTROCE_PROVIDER:-verbs;ofi_rxd}"
ENDPOINT="${ZCOFI_SOFTROCE_ENDPOINT:-rdm}"
if [[ "$PROVIDER" == *ofi_rxd* ]]; then
	default_client_domain="$CLIENT_DEVICE-dgram"
	default_target_domain="$TARGET_DEVICE-dgram"
	provider_path=software-rxe-ud-rxd-emulated-rma
else
	default_client_domain="$CLIENT_DEVICE"
	default_target_domain="$TARGET_DEVICE"
	provider_path=verbs-rc-rxm
fi
CLIENT_DOMAIN="${ZCOFI_SOFTROCE_CLIENT_DOMAIN:-$default_client_domain}"
TARGET_DOMAIN="${ZCOFI_SOFTROCE_TARGET_DOMAIN:-$default_target_domain}"
LANES="${ZCOFI_SOFTROCE_LANES:-1}"
WORKERS="${ZCOFI_SOFTROCE_WORKERS:-$LANES}"
if [ -n "${ZCOFI_SOFTROCE_CLIENT_CPU_LIST:-}" ]; then
	CLIENT_CPU_LIST="$ZCOFI_SOFTROCE_CLIENT_CPU_LIST"
elif [ "$LANES" = 1 ]; then
	CLIENT_CPU_LIST="${ZCOFI_SOFTROCE_CLIENT_CPU:-0}"
else
	CLIENT_CPU_LIST="0,2,4,6,8,10,12,14"
fi
if [ -n "${ZCOFI_SOFTROCE_TARGET_CPU_LIST:-}" ]; then
	TARGET_CPU_LIST="$ZCOFI_SOFTROCE_TARGET_CPU_LIST"
elif [ "$LANES" = 1 ]; then
	TARGET_CPU_LIST="${ZCOFI_SOFTROCE_TARGET_CPU:-1}"
else
	TARGET_CPU_LIST="1,3,5,7,11,15,17,18"
fi
BYTES_PER_LANE="${ZCOFI_SOFTROCE_BYTES_PER_LANE:-64M}"
EXTENT_BYTES="${ZCOFI_SOFTROCE_EXTENT_BYTES:-4096}"
RXM_MSG_TX_SIZE="${ZCOFI_SOFTROCE_RXM_MSG_TX_SIZE:-8}"
RXM_MSG_RX_SIZE="${ZCOFI_SOFTROCE_RXM_MSG_RX_SIZE:-8}"
RXM_TX_SIZE="${ZCOFI_SOFTROCE_RXM_TX_SIZE:-128}"
RXM_RX_SIZE="${ZCOFI_SOFTROCE_RXM_RX_SIZE:-128}"
VERBS_TX_SIZE="${ZCOFI_SOFTROCE_VERBS_TX_SIZE:-128}"
VERBS_RX_SIZE="${ZCOFI_SOFTROCE_VERBS_RX_SIZE:-128}"
VERBS_GID_INDEX="${ZCOFI_SOFTROCE_GID_INDEX:-1}"
TX_QUEUE_DEPTH="${ZCOFI_SOFTROCE_TX_QUEUE_DEPTH:-8}"
RX_QUEUE_DEPTH="${ZCOFI_SOFTROCE_RX_QUEUE_DEPTH:-8}"
TIMEOUT_MS="${ZCOFI_SOFTROCE_TIMEOUT_MS:-60000}"
PID_FILE="/run/zcofi-softroce-target-$$.pid"
target_pid=
target_job=

validate_cpu_list() {
	local list="$1"
	local label="$2"
	local expected="$3"
	local -a cpus
	local -A seen=()
	[[ "$list" =~ ^[0-9]+(,[0-9]+)*$ ]] || {
		printf 'zcofi-softroce-point: %s CPU list must be comma-separated integers: %s\n' \
			"$label" "$list" >&2
		exit 2
	}
	IFS=',' read -r -a cpus <<<"$list"
	[ "${#cpus[@]}" -eq "$expected" ] || {
		printf 'zcofi-softroce-point: %s CPU list has %s entries; one-owner topology needs exactly %s\n' \
			"$label" "${#cpus[@]}" "$expected" >&2
		exit 2
	}
	for cpu in "${cpus[@]}"; do
		[ -z "${seen[$cpu]:-}" ] || {
			printf 'zcofi-softroce-point: %s CPU list repeats CPU %s\n' "$label" "$cpu" >&2
			exit 2
		}
		seen[$cpu]=1
		[ -d "/sys/devices/system/cpu/cpu$cpu" ] || {
			printf 'zcofi-softroce-point: %s CPU %s does not exist\n' "$label" "$cpu" >&2
			exit 2
		}
	done
	taskset -c "$list" true >/dev/null 2>&1 || {
		printf 'zcofi-softroce-point: %s CPU list is outside the current allowed affinity: %s\n' \
			"$label" "$list" >&2
		exit 2
	}
}

case "$MODE" in
	read) mode_index=0; command=zcwal-ofi-rma-read ;;
	write) mode_index=1; command=zcwal-ofi-rma-write ;;
	write-high-pps) mode_index=2; command=zcwal-ofi-rma-write ;;
	*) printf 'zcofi-softroce-point: unsupported mode %s\n' "$MODE" >&2; exit 2 ;;
esac
[[ "$QD" =~ ^[0-9]+$ ]] && [ "$QD" -ge 1 ] && [ "$QD" -le 1024 ] || exit 2
[[ "$REPEAT" =~ ^[1-9][0-9]*$ ]] || exit 2
[[ "$LANES" =~ ^[1-9][0-9]*$ ]] && [ "$LANES" -le 8 ] || {
	printf 'zcofi-softroce-point: lanes must be in 1..=8\n' >&2
	exit 2
}
[[ "$WORKERS" =~ ^[1-9][0-9]*$ ]] || exit 2
[ "$WORKERS" -eq "$LANES" ] || {
	printf 'zcofi-softroce-point: ConnectX rehearsal requires lanes=workers for one owner per endpoint/QP/CQ (lanes=%s workers=%s)\n' \
		"$LANES" "$WORKERS" >&2
	exit 2
}
validate_cpu_list "$CLIENT_CPU_LIST" client "$WORKERS"
validate_cpu_list "$TARGET_CPU_LIST" target "$WORKERS"
[[ "$RXM_MSG_TX_SIZE" =~ ^[1-9][0-9]*$ ]] || exit 2
[[ "$RXM_MSG_RX_SIZE" =~ ^[1-9][0-9]*$ ]] || exit 2
[[ "$RXM_TX_SIZE" =~ ^[1-9][0-9]*$ ]] || exit 2
[[ "$RXM_RX_SIZE" =~ ^[1-9][0-9]*$ ]] || exit 2
[[ "$VERBS_TX_SIZE" =~ ^[1-9][0-9]*$ ]] || exit 2
[[ "$VERBS_RX_SIZE" =~ ^[1-9][0-9]*$ ]] || exit 2
[[ "$VERBS_GID_INDEX" =~ ^[0-9]+$ ]] || exit 2
[[ "$TX_QUEUE_DEPTH" =~ ^[1-9][0-9]*$ ]] || exit 2
[[ "$RX_QUEUE_DEPTH" =~ ^[1-9][0-9]*$ ]] || exit 2
[[ "$TIMEOUT_MS" =~ ^[1-9][0-9]*$ ]] || exit 2
[ -x "$BIN" ] || { printf 'zcofi-softroce-point: missing binary %s\n' "$BIN" >&2; exit 1; }
[ -d "/sys/class/infiniband/$CLIENT_DEVICE" ] || { printf 'zcofi-softroce-point: run softroce setup first\n' >&2; exit 1; }

service=$((37000 + mode_index * 1000 + (QD % 200) * 10 + REPEAT))
control_port=$((service + 1000))
last_control_port=$((control_port + LANES - 1))
[ "$last_control_port" -le 65535 ] || {
	printf 'zcofi-softroce-point: service range overflows with %s lanes\n' "$LANES" >&2
	exit 2
}
aggregate_outstanding_depth=$((LANES * QD))
endpoint_to_owner_map=
for ((lane = 0; lane < LANES; lane++)); do
	[ -z "$endpoint_to_owner_map" ] || endpoint_to_owner_map+=';'
	endpoint_to_owner_map+="w$lane:[$lane]"
done
mkdir -p "$POINT_DIR"
target_log="$POINT_DIR/target-${MODE}-qd${QD}-rep${REPEAT}.log"

common_env=(
	FI_OFI_RXM_MSG_TX_SIZE="$RXM_MSG_TX_SIZE"
	FI_OFI_RXM_MSG_RX_SIZE="$RXM_MSG_RX_SIZE"
	FI_OFI_RXM_TX_SIZE="$RXM_TX_SIZE"
	FI_OFI_RXM_RX_SIZE="$RXM_RX_SIZE"
	FI_VERBS_TX_SIZE="$VERBS_TX_SIZE"
	FI_VERBS_RX_SIZE="$VERBS_RX_SIZE"
	FI_VERBS_GID_IDX="$VERBS_GID_INDEX"
	URING_PLAY_TOPOLOGY_STRICT=0
	URING_PLAY_OFI_TIMEOUT_MS="$TIMEOUT_MS"
	URING_PLAY_OFI_BUSY_POLL_ITERS=100000
	URING_PLAY_OFI_CQ_SLEEP_NS=0
	URING_PLAY_OFI_TX_QUEUE_DEPTH="$TX_QUEUE_DEPTH"
	URING_PLAY_OFI_RX_QUEUE_DEPTH="$RX_QUEUE_DEPTH"
	URING_PLAY_OFI_RMA_READ_QD="$([ "$MODE" = read ] && printf '%s' "$QD" || printf 1)"
	URING_PLAY_OFI_RMA_WRITE_QD="$([ "$MODE" = read ] && printf 1 || printf '%s' "$QD")"
	URING_PLAY_OFI_RMA_ACCESS_PATTERN="${URING_PLAY_OFI_RMA_ACCESS_PATTERN:-sequential}"
	URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE="${URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE:-1}"
	URING_PLAY_OFI_RMA_WRITE_MORE="${URING_PLAY_OFI_RMA_WRITE_MORE:-0}"
	URING_PLAY_OFI_RMA_WRITE_MORE_BURST="${URING_PLAY_OFI_RMA_WRITE_MORE_BURST:-64}"
)

cleanup() {
	local status=$?
	if [ -n "$target_pid" ] && [ -d "/proc/$target_pid" ]; then
		comm="$(sed -n '1p' "/proc/$target_pid/comm" 2>/dev/null || true)"
		cmdline="$(tr '\0' ' ' <"/proc/$target_pid/cmdline" 2>/dev/null || true)"
		state="$(awk '{print $3}' "/proc/$target_pid/stat" 2>/dev/null || true)"
		printf 'softroce_target_cleanup_inspect: pid=%s comm=%s state=%s cmdline=%s\n' \
			"$target_pid" "${comm:-unavailable}" "${state:-unavailable}" \
			"${cmdline:-unavailable}" >&2
		if [ ! -d "/proc/$target_pid" ] || [ "$state" = Z ]; then
			: # Exited between the PID check and inspection, or waiting to be reaped.
		elif [ "$comm" = zcutils ] && [[ "$cmdline" == *"zcwal-ofi-rma-target"*"$service"* ]]; then
			# The target can exit between verification and the signal.
			sudo -n kill -TERM "$target_pid" 2>/dev/null || true
		else
			printf 'zcofi-softroce-point: refusing to signal unverified target pid %s\n' "$target_pid" >&2
			status=1
		fi
	fi
	if [ -n "$target_job" ]; then
		wait "$target_job" 2>/dev/null || true
	fi
	sudo -n rm -f "$PID_FILE"
	exit "$status"
}
trap cleanup EXIT INT TERM

sudo -n ip netns exec "$NS" sh -c '
	pid_file=$1; shift
	printf "%s\n" "$$" >"$pid_file"
	exec "$@"
' sh "$PID_FILE" env "${common_env[@]}" URING_PLAY_OFI_DOMAIN="$TARGET_DOMAIN" \
	FI_VERBS_DEVICE_NAME="$TARGET_DEVICE" FI_VERBS_IFACE="$TARGET_LINK" \
	URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST="$TARGET_CPU_LIST" \
	taskset -c "$TARGET_CPU_LIST" "$BIN" zcwal-ofi-rma-target "$PROVIDER" "$ENDPOINT" \
	"$TARGET_ADDR" "$service" "$LANES" "$BYTES_PER_LANE" "$EXTENT_BYTES" "$WORKERS" \
	>"$target_log" 2>&1 &
target_job=$!
for ignored in $(seq 1 400); do
	if sudo -n test -s "$PID_FILE"; then
		target_pid="$(sudo -n cat "$PID_FILE")"
		[[ "$target_pid" =~ ^[0-9]+$ ]] && break
	fi
	sleep 0.025
done
[[ "$target_pid" =~ ^[0-9]+$ ]] || { printf 'zcofi-softroce-point: target PID did not appear\n' >&2; exit 1; }

timeout 15 sh -c '
	ns=$1; port=$2; pid=$3
	for ignored in $(seq 1 400); do
		sudo -n ip netns exec "$ns" ss -H -ltn | awk -v p=":$port" '\''$4 ~ p "$" {found=1} END{exit !found}'\'' && exit 0
		[ -r "/proc/$pid/comm" ] || exit 2
		sleep 0.025
	done
	exit 3
' sh "$NS" "$control_port" "$target_pid"

printf 'softroce-pair-start: mode=%s qd=%s repeat=%s provider=%s provider_path=%s endpoint=%s client_domain=%s target_domain=%s lanes=%s workers=%s per_worker_qd=%s aggregate_outstanding_depth=%s endpoint_to_owner_map=%s client_cpu_list=%s target_cpu_list=%s rxm_msg_tx_size=%s rxm_msg_rx_size=%s shared_system=yes representative=false coordination_token=%s coordination_honored=%s\n' \
	"$MODE" "$QD" "$REPEAT" "$PROVIDER" "$provider_path" "$ENDPOINT" "$CLIENT_DOMAIN" \
	"$TARGET_DOMAIN" "$LANES" "$WORKERS" "$QD" "$aggregate_outstanding_depth" \
	"$endpoint_to_owner_map" "$CLIENT_CPU_LIST" "$TARGET_CPU_LIST" \
	"$RXM_MSG_TX_SIZE" "$RXM_MSG_RX_SIZE" \
	"${AGENT_COORD_TOKEN:-unreported}" "${AGENT_COORD_HONORED:-unreported}"
env "${common_env[@]}" URING_PLAY_OFI_DOMAIN="$CLIENT_DOMAIN" \
	FI_VERBS_DEVICE_NAME="$CLIENT_DEVICE" FI_VERBS_IFACE="$CLIENT_LINK" \
	URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST="$CLIENT_CPU_LIST" \
	taskset -c "$CLIENT_CPU_LIST" "$BIN" "$command" "$PROVIDER" "$ENDPOINT" \
	"$TARGET_ADDR" "$service" "$LANES" "$BYTES_PER_LANE" "$EXTENT_BYTES" "$WORKERS"

wait "$target_job"
target_job=
cat "$target_log"
target_pid=
sudo -n rm -f "$PID_FILE"
printf 'softroce-pair-complete: mode=%s qd=%s repeat=%s artifact=%s\n' \
	"$MODE" "$QD" "$REPEAT" "$POINT_DIR"
trap - EXIT INT TERM
