#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

CASE="${CASE:-plan}"
RUN_TRAFFIC="${RUN_TRAFFIC:-0}"
DRY_RUN_PREPARE="${DRY_RUN_PREPARE:-0}"
PROFILE_TIME="${PROFILE_TIME:-1}"
LOGICAL_CHUNK_BYTES="${LOGICAL_CHUNK_BYTES:-4k}"

usage() {
	cat <<'EOF'
usage: CASE=<case> [RUN_TRAFFIC=1] scripts/ec2-latency-cpu-profile.sh

Cases:
  plan           print the short-run experiment matrix and commands
  network-zcnc   raw n1->n2 and n2->n3 zcnc leg baseline
  tcpmux-line    encrypted n1->n2->n3 tcpmux + zcforward line
  deepraid-tree  zcraid split/merge tree over the 8-node inventory
  zcbrd-local    local zcbrd 4K io-slot WAL fanout/fanin
  all            run or prepare all cases in the order above

Defaults are intentionally short and 4K-shaped. Cluster cases rely on the
underlying runners to acquire /tmp/cluster.lock before any remote traffic.

Environment:
  RUN_TRAFFIC=1       execute commands; otherwise only print them
  DRY_RUN_PREPARE=1   for dry-run-capable cluster scripts, generate manifests
  PROFILE_TIME=1      emit cpu-profile-time lines through GNU time
  LOGICAL_CHUNK_BYTES default 4k
EOF
}

print_cmd() {
	printf '  '
	printf '%q ' "$@"
	printf '\n'
}

run_or_show() {
	local can_prepare=$1
	shift
	print_cmd "$@"
	if [[ "$RUN_TRAFFIC" == "1" || ( "$can_prepare" == "1" && "$DRY_RUN_PREPARE" == "1" ) ]]; then
		"$@"
	else
		if [[ "$can_prepare" == "1" ]]; then
			echo "dry-run only; set RUN_TRAFFIC=1 to execute or DRY_RUN_PREPARE=1 to generate manifests"
		else
			echo "dry-run only; set RUN_TRAFFIC=1 to execute"
		fi
	fi
}

network_zcnc() {
	run_or_show 1 \
		env PROFILE_TIME="$PROFILE_TIME" RUN_TRAFFIC="$RUN_TRAFFIC" \
		MODE="${MODE:-serial}" CONNECTIONS="${CONNECTIONS:-8}" \
		BYTES_PER_CONNECTION="${BYTES_PER_CONNECTION:-256m}" \
		CHUNK_BYTES="$LOGICAL_CHUNK_BYTES" RECV_BYTES="$LOGICAL_CHUNK_BYTES" \
		PIPELINE="${PIPELINE:-64}" WORKERS="${WORKERS:-16}" RING_ENTRIES="${RING_ENTRIES:-4096}" \
		"$ROOT/qemu-zcrx/replication-line-zcnc-baseline.sh"
}

tcpmux_line() {
	run_or_show 1 \
		env PROFILE_TIME="$PROFILE_TIME" RUN_TRAFFIC="$RUN_TRAFFIC" \
		LANES="${LANES:-8}" BYTES_PER_LANE="${BYTES_PER_LANE:-256m}" \
		CHUNK_BYTES="$LOGICAL_CHUNK_BYTES" BUFFER_BYTES="$LOGICAL_CHUNK_BYTES" \
		QUEUE_DEPTH="${QUEUE_DEPTH:-64}" \
		"$ROOT/qemu-zcrx/replication-line-tcpmux-serial.sh"
}

deepraid_tree() {
	run_or_show 0 \
		env PROFILE_TIME="$PROFILE_TIME" DEEPRAID_PROFILE_TIME="$PROFILE_TIME" \
		DEEPRAID_BYTES="${DEEPRAID_BYTES:-128m}" \
		DEEPRAID_CHUNK_BYTES="$LOGICAL_CHUNK_BYTES" \
		"$ROOT/qemu-zcrx/deepraid_tree_runner.py"
}

zcbrd_local() {
	run_or_show 0 \
		env CHUNK_BYTES="$LOGICAL_CHUNK_BYTES" BYTES_PER_TARGET="${BYTES_PER_TARGET:-256m}" \
		PIPELINE="${PIPELINE:-128}" RING="${RING:-1024}" RUN_TREE_SIM="${RUN_TREE_SIM:-true}" \
		"$ROOT/scripts/zcbrd-fanout-fanin-bench.sh"
}

plan() {
	cat <<EOF
Experiment matrix, 4K logical records:

1. network-zcnc
   Raw two-leg network capacity, n1->n2 then n2->n3. Use this as the transport
   lower bound. Existing zcnc/tcp-bench logs expose throughput and worker CPU.

2. tcpmux-line
   n1 source -> n2 decrypt/local branch + forward -> n3 sink. PROFILE_TIME=1
   gives aggregate process CPU for each role because this pipeline spans several
   small tools.

3. deepraid-tree
   scatter 1->2->4, gather 4->2->1, and shallow scatter 1->4 using
   qemu-zcrx/ec2-c8gn8-deepraid-inventory.json. The runner now takes
   DEEPRAID_BYTES and DEEPRAID_CHUNK_BYTES and locks /tmp/cluster.lock itself.

4. zcbrd-local
   Local io-slot WAL fanout/fanin against zcbrd. This does not create
   network traffic; use it to isolate the RAID/WAL device path.

Post-process logs:
  scripts/zc-profile-summarize.py <run-dir>

Derived metrics:
  4K logical IOPS = bytes / 4096 / seconds
  CPU seconds/GiB = (user_seconds + sys_seconds) / (bytes / 2^30)
  CPU seconds/MIOP = CPU seconds / ((bytes / 4096) / 1e6)

Commands:
EOF
	RUN_TRAFFIC=0 DRY_RUN_PREPARE=0 CASE=network-zcnc "$0"
	RUN_TRAFFIC=0 DRY_RUN_PREPARE=0 CASE=tcpmux-line "$0"
	RUN_TRAFFIC=0 DRY_RUN_PREPARE=0 CASE=deepraid-tree "$0"
	RUN_TRAFFIC=0 DRY_RUN_PREPARE=0 CASE=zcbrd-local "$0"
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || "${1:-}" == "help" ]]; then
	usage
	exit 0
fi

case "$CASE" in
	plan)
		plan
		;;
	network-zcnc)
		network_zcnc
		;;
	tcpmux-line)
		tcpmux_line
		;;
	deepraid-tree)
		deepraid_tree
		;;
	zcbrd-local)
		zcbrd_local
		;;
	all)
		network_zcnc
		tcpmux_line
		deepraid_tree
		zcbrd_local
		;;
	*)
		usage >&2
		exit 2
		;;
esac
