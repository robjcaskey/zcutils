#!/usr/bin/env bash
set -euo pipefail

INVENTORY="${1:?usage: zcutils-rmawrite-depth-orchestrate.sh INVENTORY}"
ROOT="${ZCUTILS_LOCAL_ROOT:-/home/rob/zcutils}"
HELPER="$ROOT/scripts/ec2_perf_spot.py"
REMOTE_RUNNER=/home/ubuntu/zcutils-rmawrite-depth-node.sh
active_tag=

rexec() {
	"$HELPER" exec --inventory "$INVENTORY" --public-ip -- "$*"
}

run_point() {
	local tag="$1" transport="$2" workers="$3" block_qd="$4" rma_write_qd="$5" fua="${6:-0}"
	active_tag="$tag"
	rexec "$REMOTE_RUNNER leaf-start $tag $transport write $workers $block_qd current $rma_write_qd"
	if ! rexec "$REMOTE_RUNNER zcnblk-run $tag $transport write $workers $block_qd $fua current $rma_write_qd"; then
		rexec "$REMOTE_RUNNER leaf-stop $tag" || true
		return 1
	fi
	rexec "$REMOTE_RUNNER leaf-stop $tag"
	active_tag=
}

cleanup() {
	local status=$?
	set +e
	[ -z "$active_tag" ] || rexec "$REMOTE_RUNNER leaf-stop $active_tag"
	exit "$status"
}
trap cleanup EXIT INT TERM

rexec "$REMOTE_RUNNER prepare"

# Hold the block edge at exactly one worker, one lane, and aggregate QD1 while
# varying only the delivery-complete RMA payload-operation window.
for rma_write_qd in 1 2 4 8 16; do
	run_point "efa-write-w1-blockqd1-rmaqd${rma_write_qd}" efa 1 1 "$rma_write_qd"
done
run_point tcp-write-w1-blockqd1 tcp 1 1 1
run_point efa-write-w1-blockqd1-rmaqd16-repeat efa 1 1 16
run_point efa-fua-w1-blockqd1-rmaqd16 efa 1 1 16 1
run_point tcp-fua-w1-blockqd1 tcp 1 1 1 1

# Two explicit lanes/workers. QD remains per worker and aggregate depth is
# therefore twice the printed block QD; RMA payload QD stays separately named.
for block_qd in 64 256; do
	run_point "efa-write-w2-blockqd${block_qd}-rmaqd16" efa 2 "$block_qd" 16
	run_point "efa-write-w2-blockqd${block_qd}-rmaqd${block_qd}" efa 2 "$block_qd" "$block_qd"
	run_point "tcp-write-w2-blockqd${block_qd}" tcp 2 "$block_qd" 1
done

trap - EXIT INT TERM
cleanup
