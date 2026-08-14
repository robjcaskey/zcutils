#!/usr/bin/env bash
set -euo pipefail

INVENTORY="${1:?usage: zcutils-million-iops-orchestrate.sh INVENTORY}"
ROOT="${ZCUTILS_LOCAL_ROOT:-/home/rob/zcutils}"
HELPER="$ROOT/scripts/ec2_perf_spot.py"
REMOTE_RUNNER=/home/ubuntu/zcutils-rmawrite-depth-node.sh
active_tag=

rexec() {
	"$HELPER" exec --inventory "$INVENTORY" --public-ip -- "$*"
}

run_point() {
	local tag="$1" transport="$2" mode="$3" workers="$4" block_qd="$5" rma_write_qd="$6" ops="$7"
	active_tag="$tag"
	rexec "$REMOTE_RUNNER leaf-start $tag $transport $mode $workers $block_qd current $rma_write_qd"
	if ! rexec "$REMOTE_RUNNER zcnblk-run $tag $transport $mode $workers $block_qd 0 current $rma_write_qd $ops"; then
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

# Each point is four repeats. QD is per worker; aggregate outstanding depth is
# workers * QD. The 4-worker points establish scaling and the 8-worker points
# probe both a moderate and a very-high aggregate-depth saturation regime.
for workers in 4 8; do
	for mode in write read; do
		run_point "efa-${mode}-w${workers}-blockqd64-rmaqd16" efa "$mode" "$workers" 64 16 250000
		run_point "tcp-${mode}-w${workers}-blockqd64" tcp "$mode" "$workers" 64 1 250000
	done
done

trap - EXIT INT TERM
cleanup
