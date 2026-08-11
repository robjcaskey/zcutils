#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTROOT="${OUTROOT:-$ROOT/bench-results/zcofi-softroce-rc-$(date -u +%Y%m%dT%H%M%SZ)}"
NS="${ZCOFI_RXE_NETNS:-zcrxe-zcutils}"
CLIENT_DEVICE="${ZCOFI_RXE_CLIENT_DEVICE:-rxe_zc_c}"
TARGET_DEVICE="${ZCOFI_RXE_TARGET_DEVICE:-rxe_zc_t}"
TARGET_ADDR="${ZCOFI_RXE_TARGET_ADDR:-198.18.0.2}"
CLIENT_CPU="${ZCOFI_SOFTROCE_CLIENT_CPU:-0}"
TARGET_CPU="${ZCOFI_SOFTROCE_TARGET_CPU:-1}"
PORT="${ZCOFI_SOFTROCE_RC_PORT:-18547}"
GID_INDEX="${ZCOFI_SOFTROCE_GID_INDEX:-1}"
ITERATIONS="${ZCOFI_SOFTROCE_RC_ITERATIONS:-1000}"
MESSAGE_BYTES="${ZCOFI_SOFTROCE_RC_MESSAGE_BYTES:-4096}"
REPEATS="${ZCOFI_SOFTROCE_REPEATS:-3}"
target_pid=
target_job=
pid_file=

die() {
	printf 'zcofi-softroce-rc-smoke: %s\n' "$*" >&2
	exit 1
}

cleanup() {
	local status=$?
	trap - EXIT INT TERM
	if [ -n "$target_pid" ] && [ -d "/proc/$target_pid" ]; then
		local comm cmdline state
		comm="$(sed -n '1p' "/proc/$target_pid/comm" 2>/dev/null || true)"
		cmdline="$(tr '\0' ' ' <"/proc/$target_pid/cmdline" 2>/dev/null || true)"
		state="$(awk '{print $3}' "/proc/$target_pid/stat" 2>/dev/null || true)"
		printf 'softroce_rc_cleanup_inspect: pid=%s comm=%s state=%s cmdline=%s\n' \
			"$target_pid" "${comm:-unavailable}" "${state:-unavailable}" \
			"${cmdline:-unavailable}" >&2
		if [ ! -d "/proc/$target_pid" ] || [ "$state" = Z ]; then
			:
		elif [[ "$comm" == ibv_rc_pingpon* ]] && \
			[[ "$cmdline" == *"ibv_rc_pingpong"*"$TARGET_DEVICE"*"$PORT"* ]]; then
			sudo -n kill -TERM "$target_pid" 2>/dev/null || true
		else
			printf 'zcofi-softroce-rc-smoke: refusing to signal unverified PID %s\n' \
				"$target_pid" >&2
			status=1
		fi
	fi
	if [ -n "$target_job" ]; then
		wait "$target_job" 2>/dev/null || true
	fi
	if [ -n "$pid_file" ]; then
		sudo -n rm -f "$pid_file"
	fi
	exit "$status"
}
trap cleanup EXIT INT TERM

command -v ibv_rc_pingpong >/dev/null 2>&1 || die "ibv_rc_pingpong is required"
[[ "$CLIENT_CPU" =~ ^[0-9]+$ ]] || die "client CPU must be numeric"
[[ "$TARGET_CPU" =~ ^[0-9]+$ ]] || die "target CPU must be numeric"
[[ "$PORT" =~ ^[0-9]+$ ]] && [ "$PORT" -ge 1024 ] && [ "$PORT" -le 65535 ] || \
	die "RC port must be in 1024..=65535"
[[ "$GID_INDEX" =~ ^[0-9]+$ ]] || die "GID index must be numeric"
[[ "$ITERATIONS" =~ ^[1-9][0-9]*$ ]] || die "iterations must be positive"
[[ "$MESSAGE_BYTES" =~ ^[1-9][0-9]*$ ]] || die "message bytes must be positive"
[[ "$REPEATS" =~ ^[0-9]+$ ]] && [ "$REPEATS" -ge 3 ] || \
	die "Soft-RoCE shared-system measurements require at least three repeats"
[ -d "/sys/class/infiniband/$CLIENT_DEVICE" ] || die "run Soft-RoCE setup first"

mkdir -p "$OUTROOT"
: >"$OUTROOT/repeats.log"
printf 'softroce-native-rc-start: transport=native-verbs-rc operation=send-recv-pingpong rma=no message_bytes=%s iterations=%s repeats=%s client_device=%s target_device=%s client_cpu=%s target_cpu=%s gid_index=%s shared_system=yes representative=false coordination_token=%s coordination_honored=%s\n' \
	"$MESSAGE_BYTES" "$ITERATIONS" "$REPEATS" "$CLIENT_DEVICE" "$TARGET_DEVICE" \
	"$CLIENT_CPU" "$TARGET_CPU" "$GID_INDEX" \
	"${AGENT_COORD_TOKEN:-unreported}" "${AGENT_COORD_HONORED:-unreported}" \
	| tee "$OUTROOT/topology.log"

for ((repeat = 1; repeat <= REPEATS; repeat++)); do
	pid_file="/run/zcofi-softroce-rc-$$-$repeat.pid"
	target_log="$OUTROOT/target-rep$repeat.log"
	client_log="$OUTROOT/client-rep$repeat.log"
	sudo -n ip netns exec "$NS" sh -c '
		pid_file=$1; shift
		printf "%s\n" "$$" >"$pid_file"
		exec "$@"
	' sh "$pid_file" taskset -c "$TARGET_CPU" ibv_rc_pingpong \
		-d "$TARGET_DEVICE" -g "$GID_INDEX" -p "$PORT" -n "$ITERATIONS" \
		-s "$MESSAGE_BYTES" >"$target_log" 2>&1 &
	target_job=$!
	for ignored in $(seq 1 400); do
		if sudo -n test -s "$pid_file"; then
			target_pid="$(sudo -n cat "$pid_file")"
			[[ "$target_pid" =~ ^[0-9]+$ ]] && break
		fi
		sleep 0.025
	done
	[[ "$target_pid" =~ ^[0-9]+$ ]] || die "target PID did not appear"
	timeout 10 sh -c '
		ns=$1; port=$2; pid=$3
		for ignored in $(seq 1 400); do
			sudo -n ip netns exec "$ns" ss -H -ltn | awk -v p=":$port" '\''$4 ~ p "$" {found=1} END{exit !found}'\'' && exit 0
			[ -r "/proc/$pid/comm" ] || exit 2
			sleep 0.025
		done
		exit 3
	' sh "$NS" "$PORT" "$target_pid"
	timeout 30 taskset -c "$CLIENT_CPU" ibv_rc_pingpong \
		-d "$CLIENT_DEVICE" -g "$GID_INDEX" -p "$PORT" -n "$ITERATIONS" \
		-s "$MESSAGE_BYTES" "$TARGET_ADDR" | tee "$client_log"
	wait "$target_job"
	target_job=
	target_pid=
	sudo -n rm -f "$pid_file"
	pid_file=
	usec="$(awk '/usec\/iter$/{value=$(NF-1)} END{if (value == "") exit 1; print value}' "$client_log")"
	printf 'repeat=%s usec_per_pingpong_iteration=%s\n' "$repeat" "$usec" \
		| tee -a "$OUTROOT/repeats.log"
done

awk '
	{
		split($2, pair, "=")
		value = pair[2] + 0
		if (count == 0 || value < min) min = value
		if (count == 0 || value > max) max = value
		sum += value
		count++
	}
	END {
		if (count < 3) exit 1
		mean = sum / count
		printf "softroce-native-rc-summary: operation=send-recv-pingpong rma=no repeats=%d min_usec=%.3f mean_usec=%.3f max_usec=%.3f spread_pct=%.2f shared_system=yes representative=false\n", count, min, mean, max, (max-min)/mean*100
	}
' "$OUTROOT/repeats.log" | tee "$OUTROOT/summary.log"
printf 'artifact=%s\n' "$OUTROOT"
trap - EXIT INT TERM
