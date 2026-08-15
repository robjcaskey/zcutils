#!/usr/bin/env bash
set -euo pipefail

card="${1:?card 0 or 1}"
alignment="${2:?aligned or crossed}"
mode="${3:?read or write}"
repeat="${4:?repeat number}"
lanes="${LANES:-16}"
qd="${QD:-256}"
bytes_per_lane="${BYTES_PER_LANE:-4G}"
base_service="${BASE_SERVICE:-31000}"

client_host=ubuntu@18.225.204.143
target_host=ubuntu@52.14.96.252
ssh_key=/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519
remote_root=/home/ubuntu/zcutils
artifact_root=/home/rob/zcutils/bench-results/zc-lane-align-c8gn48-20260814T221735Z
ssh_base=(ssh -o BatchMode=yes -o ServerAliveInterval=30 -i "$ssh_key")

case "$card" in
	0) domain=efa_0-rdm; iface=efa_0; target_ip=172.31.46.218; local_cpu_base=0; remote_cpu_base=96 ;;
	1) domain=efa_1-rdm; iface=efa_1; target_ip=172.31.45.204; local_cpu_base=96; remote_cpu_base=0 ;;
	*) printf 'invalid card: %s\n' "$card" >&2; exit 2 ;;
esac
case "$alignment" in
	aligned) cpu_base="$local_cpu_base"; strict=1; representative=yes; alignment_offset=0 ;;
	crossed) cpu_base="$remote_cpu_base"; strict=0; representative=no-intentional-cross-numa-control; alignment_offset=1 ;;
	*) printf 'invalid alignment: %s\n' "$alignment" >&2; exit 2 ;;
esac
case "$mode" in
	read) command=zcwal-ofi-rma-read; completion=initiator-local-cq-data-visible; mode_offset=0 ;;
	write) command=zcwal-ofi-rma-write; completion=initiator-delivery-cq-remote-visible; mode_offset=200 ;;
	*) printf 'invalid mode: %s\n' "$mode" >&2; exit 2 ;;
esac

cpu_end=$((cpu_base + lanes - 1))
cpu_list="${cpu_base}-${cpu_end}"
service=$((base_service + card * 400 + mode_offset + repeat * 2 + alignment_offset))
tag="card${card}-${alignment}-${mode}-q${qd}-rep${repeat}"
outdir="$artifact_root/$tag"
mkdir -p "$outdir"
client_log="$outdir/client.log"
target_log="$outdir/target.log"
remote_target_log="/tmp/${tag}-target.log"
target_pid=

cleanup() {
	status=$?
	if [ -n "$target_pid" ]; then
		"${ssh_base[@]}" "$target_host" bash -s -- "$target_pid" "$service" <<'REMOTE' >/dev/null 2>&1 || status=1
set -u
pid="$1"; service="$2"
if [ -r "/proc/$pid/comm" ]; then
	comm="$(cat "/proc/$pid/comm" 2>/dev/null || true)"
	cmdline="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)"
	if [ "$comm" = zcutils ] && [[ "$cmdline" == *zcwal-ofi-rma-target*" $service "* ]]; then
		kill -TERM "$pid" 2>/dev/null || true
	else
		printf 'refusing cleanup pid=%s comm=%s command=%s\n' "$pid" "$comm" "$cmdline" >&2
		exit 1
	fi
fi
REMOTE
	fi
	exit "$status"
}
trap cleanup EXIT INT TERM

target_pid="$("${ssh_base[@]}" "$target_host" bash -s -- "$remote_root" "$remote_target_log" "$domain" "$iface" "$target_ip" "$cpu_list" "$qd" "$service" "$lanes" "$bytes_per_lane" "$strict" <<'REMOTE'
set -euo pipefail
root="$1"; log="$2"; domain="$3"; iface="$4"; bind_ip="$5"; cpus="$6"
qd="$7"; service="$8"; lanes="$9"; bytes="${10}"; strict="${11}"
cd "$root"
nohup sudo prlimit --memlock=unlimited:unlimited -- env \
	LD_LIBRARY_PATH=/opt/amazon/efa/lib:/opt/amazon/efa/lib64 \
	PATH=/opt/amazon/efa/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
	URING_PLAY_TOPOLOGY_STRICT="$strict" URING_PLAY_OFI_DOMAIN="$domain" \
	URING_PLAY_OFI_EFA_FABRIC=efa-direct FI_EFA_IFACE="$iface" \
	FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_OFI_TIMEOUT_MS=180000 \
	URING_PLAY_OFI_BUSY_POLL_ITERS=1000000 URING_PLAY_OFI_CQ_SLEEP_NS=0 \
	URING_PLAY_OFI_RMA_READ_QD="$qd" URING_PLAY_OFI_RMA_WRITE_QD="$qd" \
	URING_PLAY_OFI_RMA_ACCESS_PATTERN=random-permutation \
	URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 URING_PLAY_OFI_RMA_WRITE_MORE=0 \
	URING_PLAY_OFI_EFA_WRITE_HIGH_PPS=0 URING_PLAY_PIN_CPUS=1 \
	URING_PLAY_PIN_CPU_LIST="$cpus" taskset -c "$cpus" target/release/zcutils \
	zcwal-ofi-rma-target efa rdm "$bind_ip" "$service" "$lanes" "$bytes" 4K "$lanes" \
	>"$log" 2>&1 </dev/null &
printf '%s\n' "$!"
REMOTE
)"
[[ "$target_pid" =~ ^[0-9]+$ ]] || { printf 'invalid target pid: %s\n' "$target_pid" >&2; exit 1; }

control_port=$((service + 1000))
timeout --foreground 45s "${ssh_base[@]}" "$target_host" bash -s -- "$target_pid" "$control_port" <<'REMOTE'
set -euo pipefail
pid="$1"; port="$2"
for ignored in $(seq 1 1500); do
	ss -H -ltn | awk -v p=":$port" '$4 ~ p "$" {found=1} END{exit !found}' && exit 0
	[ -r "/proc/$pid/comm" ] || exit 2
	state="$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null || true)"
	[ "$state" != Z ] || exit 2
	sleep 0.025
done
exit 3
REMOTE

{
	printf 'lane-alignment-start: utc=%s card=%s alignment=%s representative=%s mode=%s lanes=%s workers=%s per_worker_qd=%s aggregate_outstanding_depth=%s bytes_per_lane=%s domain=%s iface=%s cpu_list=%s nic_numa=%s worker_numa=%s completion=%s zero_copy=registered-rma-direct shared_system=yes\n' \
		"$(date -u +%FT%TZ)" "$card" "$alignment" "$representative" "$mode" "$lanes" "$lanes" "$qd" "$((lanes * qd))" \
		"$bytes_per_lane" "$domain" "$iface" "$cpu_list" "$card" "$([ "$cpu_base" -lt 96 ] && printf 0 || printf 1)" "$completion"
	"${ssh_base[@]}" "$client_host" bash -s -- "$remote_root" "$command" "$domain" "$iface" "$target_ip" "$cpu_list" "$qd" "$service" "$lanes" "$bytes_per_lane" "$strict" <<'REMOTE'
set -euo pipefail
root="$1"; command="$2"; domain="$3"; iface="$4"; target_ip="$5"; cpus="$6"
qd="$7"; service="$8"; lanes="$9"; bytes="${10}"; strict="${11}"
cd "$root"
exec sudo prlimit --memlock=unlimited:unlimited -- env \
	LD_LIBRARY_PATH=/opt/amazon/efa/lib:/opt/amazon/efa/lib64 \
	PATH=/opt/amazon/efa/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
	URING_PLAY_TOPOLOGY_STRICT="$strict" URING_PLAY_OFI_DOMAIN="$domain" \
	URING_PLAY_OFI_EFA_FABRIC=efa-direct FI_EFA_IFACE="$iface" \
	FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_OFI_TIMEOUT_MS=180000 \
	URING_PLAY_OFI_BUSY_POLL_ITERS=1000000 URING_PLAY_OFI_CQ_SLEEP_NS=0 \
	URING_PLAY_OFI_RMA_READ_QD="$qd" URING_PLAY_OFI_RMA_WRITE_QD="$qd" \
	URING_PLAY_OFI_RMA_ACCESS_PATTERN=random-permutation \
	URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 URING_PLAY_OFI_RMA_WRITE_MORE=0 \
	URING_PLAY_OFI_EFA_WRITE_HIGH_PPS=0 URING_PLAY_PIN_CPUS=1 \
	URING_PLAY_PIN_CPU_LIST="$cpus" taskset -c "$cpus" target/release/zcutils \
	"$command" efa rdm "$target_ip" "$service" "$lanes" "$bytes" 4K "$lanes"
REMOTE
} 2>&1 | tee "$client_log"

timeout --foreground 90s "${ssh_base[@]}" "$target_host" bash -s -- "$target_pid" <<'REMOTE'
set -euo pipefail
pid="$1"
for ignored in $(seq 1 1800); do
	[ ! -r "/proc/$pid/comm" ] && exit 0
	state="$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null || true)"
	[ "$state" = Z ] && exit 0
	sleep 0.05
done
exit 3
REMOTE
"${ssh_base[@]}" "$target_host" "cat '$remote_target_log'" >"$target_log"
target_pid=
cat "$target_log"
printf 'lane-alignment-complete: utc=%s tag=%s artifact=%s\n' "$(date -u +%FT%TZ)" "$tag" "$outdir"
trap - EXIT INT TERM
