#!/usr/bin/env bash
set -euo pipefail

card="${1:?card (0 or 1)}"
mode="${2:?mode (read or write)}"
qd="${3:?per-lane QD}"
repeat="${4:?repeat}"
lanes="${5:?lane/worker count}"
bytes_per_lane="${6:?bytes per lane}"
base_service="${7:?base service}"
point_dir="${8:?point directory}"

case "$card" in
	0)
		domain=efa_0-rdm
		iface=efa_0
		target_ip=172.31.39.134
		cpu_base=0
		;;
	1)
		domain=efa_1-rdm
		iface=efa_1
		target_ip=172.31.47.13
		cpu_base=96
		;;
	*) printf 'card must be 0 or 1: %s\n' "$card" >&2; exit 2 ;;
esac
case "$mode" in
	read) command=zcwal-ofi-rma-read ;;
	write) command=zcwal-ofi-rma-write ;;
	*) printf 'mode must be read or write: %s\n' "$mode" >&2; exit 2 ;;
esac
[[ "$qd" =~ ^[1-9][0-9]*$ ]] || exit 2
[[ "$repeat" =~ ^[1-9][0-9]*$ ]] || exit 2
[[ "$lanes" =~ ^[1-9][0-9]*$ ]] && [ "$lanes" -le 96 ] || exit 2
[[ "$base_service" =~ ^[1-9][0-9]*$ ]] || exit 2
[ $((base_service + 1000 + lanes - 1)) -le 65535 ] || exit 2

client_host=ubuntu@3.151.54.65
target_host=ubuntu@18.225.65.76
ssh_key=/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519
remote_root=/home/ubuntu/zcutils
run_id=zcutils-rmadirect-dualcard-adhoc-c8gn48-20260813T0059Z
ssh_base=(ssh -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 -i "$ssh_key")

cpu_list=
lane_map=
for ((lane = 0; lane < lanes; lane++)); do
	cpu=$((cpu_base + lane))
	[ -z "$cpu_list" ] || cpu_list+=,
	[ -z "$lane_map" ] || lane_map+=';'
	cpu_list+="$cpu"
	lane_map+="lane${lane}:worker${lane}:cpu${cpu}:${domain}"
done

mkdir -p "$point_dir"
tag="card${card}-${mode}-lanes${lanes}-qd${qd}-rep${repeat}"
client_log="$point_dir/$tag-client.log"
target_copy="$point_dir/$tag-target.log"
target_dir="$remote_root/bench-results/$run_id/raw-rma/target/$tag"
target_log="$target_dir/target.log"
target_pid=

cleanup() {
	status=$?
	if [ -n "$target_pid" ]; then
		"${ssh_base[@]}" "$target_host" bash -s -- "$target_pid" "$base_service" <<'REMOTE' >/dev/null 2>&1 || status=1
set -u
pid="$1"
service="$2"
if [ -r "/proc/$pid/comm" ]; then
	comm="$(cat "/proc/$pid/comm" 2>/dev/null || true)"
	cmdline="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)"
	state="$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null || true)"
	if [ "$state" = Z ]; then
		exit 0
	elif [ "$comm" = zcutils ] && [[ "$cmdline" == *zcwal-ofi-rma-target*" $service "* ]]; then
		kill -TERM "$pid" 2>/dev/null || true
	else
		printf 'refusing to signal pid=%s comm=%s state=%s command=%s\n' \
			"$pid" "$comm" "$state" "$cmdline" >&2
		exit 1
	fi
fi
REMOTE
	fi
	exit "$status"
}
trap cleanup EXIT INT TERM

target_pid="$("${ssh_base[@]}" "$target_host" bash -s -- \
	"$remote_root" "$target_dir" "$target_log" "$domain" "$iface" "$target_ip" \
	"$cpu_list" "$qd" "$base_service" "$lanes" "$bytes_per_lane" <<'REMOTE'
set -euo pipefail
root="$1"; outdir="$2"; log="$3"; domain="$4"; iface="$5"; bind_ip="$6"
cpus="$7"; qd="$8"; service="$9"; lanes="${10}"; bytes="${11}"
mkdir -p "$outdir"
cd "$root"
nohup env \
	URING_PLAY_TOPOLOGY_STRICT=1 \
	URING_PLAY_OFI_DOMAIN="$domain" \
	URING_PLAY_OFI_EFA_FABRIC=efa-direct \
	FI_EFA_IFACE="$iface" \
	FI_EFA_USE_DEVICE_RDMA=1 \
	FI_EFA_USE_HUGE_PAGE=1 \
	URING_PLAY_OFI_TIMEOUT_MS=60000 \
	URING_PLAY_OFI_BUSY_POLL_ITERS=1000000 \
	URING_PLAY_OFI_CQ_SLEEP_NS=0 \
	URING_PLAY_OFI_RMA_READ_QD="$qd" \
	URING_PLAY_OFI_RMA_WRITE_QD="$qd" \
	URING_PLAY_OFI_RMA_ACCESS_PATTERN=random-permutation \
	URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 \
	URING_PLAY_OFI_RMA_WRITE_MORE=0 \
	URING_PLAY_OFI_EFA_WRITE_HIGH_PPS=0 \
	URING_PLAY_PIN_CPUS=1 \
	URING_PLAY_PIN_CPU_LIST="$cpus" \
	taskset -c "$cpus" target/release/zcutils zcwal-ofi-rma-target \
		efa rdm "$bind_ip" "$service" "$lanes" "$bytes" 4K "$lanes" \
	>"$log" 2>&1 </dev/null &
printf '%s\n' "$!"
REMOTE
)"
[[ "$target_pid" =~ ^[0-9]+$ ]] || {
	printf 'invalid target pid: %s\n' "$target_pid" >&2
	exit 1
}

control_port=$((base_service + 1000))
timeout --foreground 30s "${ssh_base[@]}" "$target_host" bash -s -- \
	"$target_pid" "$control_port" <<'REMOTE'
set -euo pipefail
pid="$1"; port="$2"
for ignored in $(seq 1 1000); do
	ss -H -ltn | awk -v p=":$port" '$4 ~ p "$" {found=1} END{exit !found}' && exit 0
	[ -r "/proc/$pid/comm" ] || exit 2
	state="$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null || true)"
	[ "$state" != Z ] || exit 2
	sleep 0.025
done
exit 3
REMOTE

{
	printf 'remote-pair-start: utc=%s card=%s mode=%s repeat=%s lanes=%s workers=%s per_worker_qd=%s aggregate_outstanding_depth=%s bytes_per_lane=%s access_pattern=random-permutation completion=%s domain=%s fabric=efa-direct iface=%s target_ip=%s lane_to_worker_cpu_domain=%s shared_system=yes\n' \
		"$(date -u +%FT%TZ)" "$card" "$mode" "$repeat" "$lanes" "$lanes" "$qd" \
		"$((lanes * qd))" "$bytes_per_lane" \
		"$([ "$mode" = read ] && printf initiator-local-cq-data-visible || printf initiator-delivery-cq-remote-visible)" \
		"$domain" "$iface" "$target_ip" "$lane_map"
	"${ssh_base[@]}" "$client_host" bash -s -- \
		"$remote_root" "$command" "$domain" "$iface" "$target_ip" "$cpu_list" \
		"$qd" "$base_service" "$lanes" "$bytes_per_lane" <<'REMOTE'
set -euo pipefail
root="$1"; command="$2"; domain="$3"; iface="$4"; target_ip="$5"; cpus="$6"
qd="$7"; service="$8"; lanes="$9"; bytes="${10}"
cd "$root"
exec env \
	URING_PLAY_TOPOLOGY_STRICT=1 \
	URING_PLAY_OFI_DOMAIN="$domain" \
	URING_PLAY_OFI_EFA_FABRIC=efa-direct \
	FI_EFA_IFACE="$iface" \
	FI_EFA_USE_DEVICE_RDMA=1 \
	FI_EFA_USE_HUGE_PAGE=1 \
	URING_PLAY_OFI_TIMEOUT_MS=60000 \
	URING_PLAY_OFI_BUSY_POLL_ITERS=1000000 \
	URING_PLAY_OFI_CQ_SLEEP_NS=0 \
	URING_PLAY_OFI_RMA_READ_QD="$qd" \
	URING_PLAY_OFI_RMA_WRITE_QD="$qd" \
	URING_PLAY_OFI_RMA_ACCESS_PATTERN=random-permutation \
	URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 \
	URING_PLAY_OFI_RMA_WRITE_MORE=0 \
	URING_PLAY_OFI_EFA_WRITE_HIGH_PPS=0 \
	URING_PLAY_PIN_CPUS=1 \
	URING_PLAY_PIN_CPU_LIST="$cpus" \
	taskset -c "$cpus" target/release/zcutils "$command" \
		efa rdm "$target_ip" "$service" "$lanes" "$bytes" 4K "$lanes"
REMOTE
} 2>&1 | tee "$client_log"

timeout --foreground 60s "${ssh_base[@]}" "$target_host" bash -s -- "$target_pid" <<'REMOTE'
set -euo pipefail
pid="$1"
for ignored in $(seq 1 1200); do
	[ ! -r "/proc/$pid/comm" ] && exit 0
	state="$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null || true)"
	[ "$state" = Z ] && exit 0
	sleep 0.05
done
exit 3
REMOTE
"${ssh_base[@]}" "$target_host" "cat '$target_log'" >"$target_copy"
target_pid=
cat "$target_copy"
printf 'remote-pair-complete: utc=%s card=%s mode=%s qd=%s repeat=%s point_dir=%s\n' \
	"$(date -u +%FT%TZ)" "$card" "$mode" "$qd" "$repeat" "$point_dir"
trap - EXIT INT TERM
