#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFLIGHT="${H4D_PREFLIGHT:-$ROOT/scripts/gce-h4d-rdma-preflight.sh}"
ROLE="${1:-}"
PEER="${2:-${H4D_RDMA_PEER_IPV4:-}}"
WORKER_CPU="${H4D_RDMA_QUALIFY_CPU:-}"
RDMA_DEVICE="${H4D_RDMA_DEVICE:-${ZCOFI_RDMA_DEVICE:-}}"
RDMA_NETDEV="${H4D_RDMA_NETDEV:-${ZCOFI_RDMA_NETDEV:-}}"
RDMA_PORT="${H4D_RDMA_PORT:-${ZCOFI_RDMA_PORT:-1}}"
GID_INDEX="${FI_VERBS_GID_IDX:-}"
CONTROL_PORT="${H4D_RDMA_QUALIFY_PORT:-18515}"
LOG="${H4D_RDMA_QUALIFY_LOG:-}"
MIN_MBPS="${H4D_RDMA_QUALIFY_MIN_MBPS:-11000}"

die() {
	printf 'gce-h4d-rdma-qualify: %s\n' "$*" >&2
	exit 1
}

case "$ROLE" in
	server | client) ;;
	*) die "usage: $0 server|client PEER_IRDMA_IPV4" ;;
esac
[[ "$PEER" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]] || \
	die "pass the peer's validated IRDMA IPv4 address, including for server preflight"
[[ "$WORKER_CPU" =~ ^[0-9]+$ ]] || die "set H4D_RDMA_QUALIFY_CPU to one explicit CPU"
[[ "$RDMA_PORT" =~ ^[1-9][0-9]*$ ]] || die "H4D_RDMA_PORT must be positive"
[[ "$CONTROL_PORT" =~ ^[1-9][0-9]*$ ]] && [ "$CONTROL_PORT" -le 65535 ] || \
	die "H4D_RDMA_QUALIFY_PORT must be in 1..65535"
[[ "$MIN_MBPS" =~ ^[1-9][0-9]*$ ]] || die "H4D_RDMA_QUALIFY_MIN_MBPS must be positive"
[ -n "$RDMA_DEVICE" ] || die "set H4D_RDMA_DEVICE explicitly"
[ -n "$RDMA_NETDEV" ] || die "set H4D_RDMA_NETDEV explicitly"
[ -x "$PREFLIGHT" ] || die "H4D preflight is not executable: $PREFLIGHT"
for command in awk grep ib_send_bw ip mkdir taskset tee; do
	command -v "$command" >/dev/null 2>&1 || die "missing required command: $command"
done

if [ -z "$LOG" ]; then
	LOG="h4d-ib-send-bw-$ROLE-$(date -u +%Y%m%dT%H%M%SZ).log"
fi
mkdir -p "$(dirname "$LOG")"

printf 'qualification=google-cloud-irdma-ib-send-bw role=%s peer_irdma_ipv4=%s worker_cpu=%s rdma_device=%s rdma_port=%s gid_index=%s control_port=%s completion=send-receive-work-request semantics=verbs-rdma-not-tcp-payload bulk_cross_region=no bulk_internet=no\n' \
	"$ROLE" "$PEER" "$WORKER_CPU" "$RDMA_DEVICE" "$RDMA_PORT" \
	"${GID_INDEX:-provider-default}" "$CONTROL_PORT" | tee "$LOG"

H4D_RDMA_PEER_IPV4="$PEER" \
H4D_RDMA_LANES=1 \
H4D_RDMA_WORKER_CPUS="$WORKER_CPU" \
URING_PLAY_PIN_CPU_LIST="$WORKER_CPU" \
URING_PLAY_PIN_CPUS=1 \
H4D_REQUIRE_LIBFABRIC=0 \
URING_PLAY_TOPOLOGY_STRICT=1 \
URING_PLAY_TOPOLOGY_FATAL=1 \
	"$PREFLIGHT" 2>&1 | tee -a "$LOG"

common=(-d "$RDMA_DEVICE" -i "$RDMA_PORT" -p "$CONTROL_PORT" -a -F)
if [ -n "$GID_INDEX" ]; then
	[[ "$GID_INDEX" =~ ^[0-9]+$ ]] || die "FI_VERBS_GID_IDX must be numeric"
	common+=(-x "$GID_INDEX")
fi

if [ "$ROLE" = server ]; then
	printf 'qualification_server_ready command=ib_send_bw transport=verbs-rdma payload_path=irdma-falcon\n' | tee -a "$LOG"
	taskset -c "$WORKER_CPU" ib_send_bw "${common[@]}" 2>&1 | tee -a "$LOG"
	exit "${PIPESTATUS[0]}"
fi

route="$(ip -4 route get "$PEER" | head -n 1)"
grep -Eq "(^|[[:space:]])dev ${RDMA_NETDEV}([[:space:]]|$)" <<<"$route" || \
	die "peer route is no longer bound to H4D_RDMA_NETDEV=$RDMA_NETDEV"
printf 'qualification_client_start route=%s\n' "$route" | tee -a "$LOG"
taskset -c "$WORKER_CPU" ib_send_bw "${common[@]}" "$PEER" 2>&1 | tee -a "$LOG"
perftest_rc="${PIPESTATUS[0]}"
[ "$perftest_rc" -eq 0 ] || die "ib_send_bw failed with exit $perftest_rc"

read -r qualifying_rows max_average_mbps < <(awk -v minimum="$MIN_MBPS" '
	$1 ~ /^[0-9]+$/ && $2 ~ /^[0-9]+$/ && $4 ~ /^[0-9]+([.][0-9]+)?$/ && $1 > 4096 {
		if ($4 >= minimum) qualifying++
		if ($4 > maximum) maximum=$4
	}
	END { printf "%d %.2f\n", qualifying, maximum }
' "$LOG")
if [ "$qualifying_rows" -lt 1 ]; then
	die "Cloud RDMA qualification failed: no >4096-byte row reached $MIN_MBPS MB/s; max=$max_average_mbps MB/s"
fi
printf 'qualification_result passed=yes qualifying_rows=%s max_average_MBps=%s minimum_MBps=%s path=ib_send_bw-verbs-over-irdma-falcon representative=cloud-rdma-health-check-only\n' \
	"$qualifying_rows" "$max_average_mbps" "$MIN_MBPS" | tee -a "$LOG"
