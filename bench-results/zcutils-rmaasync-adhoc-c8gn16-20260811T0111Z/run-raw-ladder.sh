#!/usr/bin/env bash
set -euo pipefail

SSH_KEY="${SSH_KEY:?set SSH_KEY to the ad-hoc instance key}"
CLIENT_HOST="${CLIENT_HOST:-18.227.100.7}"
LEAF_HOST="${LEAF_HOST:-18.222.231.47}"
CLIENT_PRIVATE="${CLIENT_PRIVATE:-172.31.37.157}"
LEAF_PRIVATE="${LEAF_PRIVATE:-172.31.44.118}"
REMOTE_ROOT="${REMOTE_ROOT:-/home/ubuntu/zcutils}"
ARTIFACT="${ARTIFACT:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
RAW_BYTES_PER_LANE="${RAW_BYTES_PER_LANE:-1G}"
LANES="${LANES:-16}"
WORKERS="${WORKERS:-16}"
REPEATS="${REPEATS:-3}"
QD_LIST="${QD_LIST:-1 2 4 8 16}"

ssh_opts=(-o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 -i "$SSH_KEY")
target_pid=""

cleanup_target() {
	[ -n "$target_pid" ] || return 0
	ssh "${ssh_opts[@]}" "ubuntu@$LEAF_HOST" bash -s -- "$target_pid" <<'REMOTE' || true
set -euo pipefail
pid="$1"
if kill -0 "$pid" 2>/dev/null; then
	command_line="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)"
	case "$command_line" in
		*zcwal-ofi-rma-target*) kill "$pid" ;;
		*) printf 'refusing to stop unexpected pid=%s command=%s\n' "$pid" "$command_line" >&2 ;;
	esac
fi
REMOTE
	target_pid=""
}
trap cleanup_target EXIT

mkdir -p "$ARTIFACT/raw"

for role in client leaf; do
	if [ "$role" = client ]; then
		host="$CLIENT_HOST"
	else
		host="$LEAF_HOST"
	fi
	ssh "${ssh_opts[@]}" "ubuntu@$host" bash -s <<'REMOTE' >"$ARTIFACT/raw/topology-$role.log"
set -euo pipefail
printf 'host=%s private_ip=%s\n' "$(hostname)" "$(hostname -I | awk '{print $1}')"
uname -a
lscpu -e=CPU,NODE,SOCKET,CORE,ONLINE
printf 'memlock_kib=%s\n' "$(ulimit -l)"
awk '/HugePages_(Total|Free)|Hugepagesize/ {print}' /proc/meminfo
ip route get 172.31.37.157 || true
ip route get 172.31.44.118 || true
/opt/amazon/efa/bin/fi_info -p efa -t FI_EP_RDM | sed -n '1,80p'
cat /tmp/zcutils-nic-low-latency/nic-low-latency-confirmed.env
REMOTE
done

qindex=0
for qd in $QD_LIST; do
	case "$qd" in
		1|2|4|8|16) ;;
		*) printf 'unsupported QD: %s\n' "$qd" >&2; exit 2 ;;
	esac
	for rep in $(seq 1 "$REPEATS"); do
		base_service=$((31800 + qindex * 40 + rep))
		remote_log="/tmp/zcutils-rmaasync-ladder/raw-qd${qd}-rep${rep}-target.log"
		printf 'raw-start qd=%s repeat=%s base_service=%s bytes_per_lane=%s\n' \
			"$qd" "$rep" "$base_service" "$RAW_BYTES_PER_LANE"
		target_pid="$(ssh "${ssh_opts[@]}" "ubuntu@$LEAF_HOST" bash -s -- \
			"$REMOTE_ROOT" "$base_service" "$LANES" "$RAW_BYTES_PER_LANE" "$WORKERS" "$remote_log" <<'REMOTE'
set -euo pipefail
root="$1"
base_service="$2"
lanes="$3"
bytes_per_lane="$4"
workers="$5"
log="$6"
mkdir -p "$(dirname "$log")"
cd "$root"
nohup timeout --signal=TERM --kill-after=5s 180s \
	env FI_EFA_USE_DEVICE_RDMA=1 \
	URING_PLAY_OFI_DOMAIN=efa_0-rdm \
	URING_PLAY_OFI_EFA_FABRIC=efa \
	URING_PLAY_OFI_CQ_SLEEP_NS=0 \
	URING_PLAY_OFI_BUSY_POLL_ITERS=1000000 \
	URING_PLAY_PIN_CPUS=1 \
	URING_PLAY_PIN_CPU_LIST=0-15 \
	target/release/zcutils zcwal-ofi-rma-target efa rdm auto \
	"$base_service" "$lanes" "$bytes_per_lane" 4K "$workers" \
	>"$log" 2>&1 < /dev/null &
printf '%s\n' "$!"
REMOTE
)"
		control_port=$((base_service + 1000 + LANES - 1))
		ssh "${ssh_opts[@]}" "ubuntu@$LEAF_HOST" bash -s -- \
			"$target_pid" "$control_port" <<'REMOTE'
set -euo pipefail
pid="$1"
control_port="$2"
for _ in $(seq 1 600); do
	if ss -H -ltn "sport = :$control_port" | grep -q .; then
		exit 0
	fi
	kill -0 "$pid" 2>/dev/null || exit 1
	sleep 0.05
done
printf 'target pid=%s did not open control port=%s\n' "$pid" "$control_port" >&2
exit 1
REMOTE

		ssh "${ssh_opts[@]}" "ubuntu@$CLIENT_HOST" \
			"cd '$REMOTE_ROOT' && env FI_EFA_USE_DEVICE_RDMA=1 URING_PLAY_OFI_DOMAIN=efa_0-rdm URING_PLAY_OFI_EFA_FABRIC=efa URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_BUSY_POLL_ITERS=1000000 URING_PLAY_OFI_RMA_READ_QD='$qd' URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-15 target/release/zcutils zcwal-ofi-rma-read efa rdm '$LEAF_PRIVATE' '$base_service' '$LANES' '$RAW_BYTES_PER_LANE' 4K '$WORKERS'" \
			| tee "$ARTIFACT/raw/qd${qd}-rep${rep}-client.log"

		ssh "${ssh_opts[@]}" "ubuntu@$LEAF_HOST" bash -s -- "$target_pid" <<'REMOTE'
set -euo pipefail
pid="$1"
for _ in $(seq 1 600); do
	kill -0 "$pid" 2>/dev/null || exit 0
	sleep 0.05
done
printf 'target pid=%s did not exit after reader completion\n' "$pid" >&2
exit 1
REMOTE
		ssh "${ssh_opts[@]}" "ubuntu@$LEAF_HOST" "cat '$remote_log'" \
			>"$ARTIFACT/raw/qd${qd}-rep${rep}-target.log"
		target_pid=""
	done
	qindex=$((qindex + 1))
done

printf 'raw-complete artifact=%s/raw client_private=%s leaf_private=%s\n' \
	"$ARTIFACT" "$CLIENT_PRIVATE" "$LEAF_PRIVATE"
