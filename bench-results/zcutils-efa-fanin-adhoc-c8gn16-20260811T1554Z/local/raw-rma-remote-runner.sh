#!/usr/bin/env bash
set -euo pipefail

mode="${1:?mode}"
qd="${2:?qd}"
rep="${3:?repeat}"
point_dir="${4:?point-dir}"

case "$mode" in
read) mode_index=0 ;;
write|write-high-pps) mode_index=1 ;;
*) printf 'unsupported mode %s\n' "$mode" >&2; exit 1 ;;
esac
case "$qd" in
1) qd_index=1 ;; 2) qd_index=2 ;; 4) qd_index=3 ;; 8) qd_index=4 ;;
16) qd_index=5 ;; 32) qd_index=6 ;; 64) qd_index=7 ;; 128) qd_index=8 ;; 256) qd_index=9 ;;
*) printf 'unsupported qd %s\n' "$qd" >&2; exit 1 ;;
esac
[[ "$rep" =~ ^[1-9][0-9]*$ ]] || { printf 'invalid repeat %s\n' "$rep" >&2; exit 1; }

SSH_BASE=(ssh -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30
	-i /home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519)
CLIENT_HOST=ubuntu@52.15.70.216
TARGET_HOST=ubuntu@3.17.27.233
TARGET_IP=172.31.42.23
RUN_ID=zcutils-efa-fanin-adhoc-c8gn16-20260811T1554Z
BASE_SERVICE=$((31000 + mode_index * 200 + qd_index * 10 + rep))
CONTROL_PORT=$((BASE_SERVICE + 1000))
BYTES_PER_LANE=256M
EXTENT_BYTES=4096
REMOTE_ROOT=/home/ubuntu/zcutils
tag="${mode}-qd${qd}-rep${rep}"
target_dir="$REMOTE_ROOT/bench-results/$RUN_ID/raw-rma/target/$tag"
target_log="$target_dir/target.log"
target_pid=

cleanup() {
	local status=$?
	if [ -n "$target_pid" ]; then
		"${SSH_BASE[@]}" "$TARGET_HOST" \
			"pid='$target_pid'; if [ -r \"/proc/\$pid/comm\" ] && [ \"\$(cat /proc/\$pid/comm)\" = zcutils ]; then kill -TERM \"\$pid\"; fi" \
			>/dev/null 2>&1 || true
	fi
	exit "$status"
}
trap cleanup EXIT INT TERM

common_env="URING_PLAY_TOPOLOGY_STRICT=${URING_PLAY_TOPOLOGY_STRICT:-1} URING_PLAY_OFI_DOMAIN=efa_0-rdm FI_EFA_IFACE=efa_0 FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_OFI_TIMEOUT_MS=60000 URING_PLAY_OFI_BUSY_POLL_ITERS=100000 URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_RMA_READ_QD=$qd URING_PLAY_OFI_RMA_WRITE_QD=$qd URING_PLAY_OFI_RMA_ACCESS_PATTERN=${URING_PLAY_OFI_RMA_ACCESS_PATTERN:-sequential} URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=${URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE:-1} URING_PLAY_OFI_RMA_WRITE_MORE=${URING_PLAY_OFI_RMA_WRITE_MORE:-0} URING_PLAY_OFI_RMA_WRITE_MORE_BURST=${URING_PLAY_OFI_RMA_WRITE_MORE_BURST:-64} URING_PLAY_OFI_EFA_WRITE_HIGH_PPS=${URING_PLAY_OFI_EFA_WRITE_HIGH_PPS:-0}"

target_pid="$("${SSH_BASE[@]}" "$TARGET_HOST" \
	"mkdir -p '$target_dir'; nohup env $common_env URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=3 '$REMOTE_ROOT/target/release/zcutils' zcwal-ofi-rma-target efa rdm '$TARGET_IP' '$BASE_SERVICE' 1 '$BYTES_PER_LANE' '$EXTENT_BYTES' 1 >'$target_log' 2>&1 </dev/null & printf '%s\\n' \$!")"
[[ "$target_pid" =~ ^[0-9]+$ ]] || { printf 'invalid target pid %s\n' "$target_pid" >&2; exit 1; }

timeout --foreground 15s "${SSH_BASE[@]}" "$TARGET_HOST" \
	"pid='$target_pid'; for ignored in \$(seq 1 400); do ss -H -ltn | awk -v p=':$CONTROL_PORT' '\$4 ~ p \"$\" {found=1} END{exit !found}' && exit 0; [ -r \"/proc/\$pid/comm\" ] || exit 2; sleep 0.025; done; exit 3"

printf 'remote-pair-start: mode=%s qd=%s repeat=%s base_service=%s bytes_per_lane=%s extent_bytes=%s target_cpu=3 client_cpu=0 lane_to_worker=0:0 lane_to_cpu=0:0 target_lane_to_cpu=0:3\n' \
	"$mode" "$qd" "$rep" "$BASE_SERVICE" "$BYTES_PER_LANE" "$EXTENT_BYTES"
client_command=zcwal-ofi-rma-read
[ "$mode" = read ] || client_command=zcwal-ofi-rma-write
"${SSH_BASE[@]}" "$CLIENT_HOST" \
	"env $common_env URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0 '$REMOTE_ROOT/target/release/zcutils' '$client_command' efa rdm '$TARGET_IP' '$BASE_SERVICE' 1 '$BYTES_PER_LANE' '$EXTENT_BYTES' 1"

timeout --foreground 15s "${SSH_BASE[@]}" "$TARGET_HOST" \
	"pid='$target_pid'; for ignored in \$(seq 1 400); do [ ! -r \"/proc/\$pid/comm\" ] && exit 0; state=\$(awk '{print \$3}' /proc/\$pid/stat 2>/dev/null || true); [ \"\$state\" = Z ] && exit 0; sleep 0.025; done; exit 3"
"${SSH_BASE[@]}" "$TARGET_HOST" "cat '$target_log'"
target_pid=
printf 'remote-pair-complete: mode=%s qd=%s repeat=%s artifact=%s\n' "$mode" "$qd" "$rep" "$point_dir"
trap - EXIT INT TERM
