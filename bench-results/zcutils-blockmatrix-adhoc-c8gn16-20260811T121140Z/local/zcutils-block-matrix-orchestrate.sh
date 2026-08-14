#!/usr/bin/env bash
set -euo pipefail

INVENTORY="${1:?usage: zcutils-block-matrix-orchestrate.sh INVENTORY}"
ROOT="${ZCUTILS_LOCAL_ROOT:-/home/rob/zcutils}"
HELPER="$ROOT/scripts/ec2_perf_spot.py"
REMOTE_RUNNER=/tmp/zcutils-block-matrix-node.sh

rexec() {
	"$HELPER" exec --inventory "$INVENTORY" --public-ip -- "$*"
}

run_zcnblk_point() {
	local tree="$1" transport="$2" mode="$3" workers="$4" qd="$5" fua="${6:-0}"
	local tag="${tree}-${transport}-${mode}-w${workers}-qd${qd}"
	[ "$fua" != 1 ] || tag+="-fua"
	rexec "$REMOTE_RUNNER leaf-start $tag $transport $mode $workers $qd $tree"
	if ! rexec "$REMOTE_RUNNER zcnblk-run $tag $transport $mode $workers $qd $fua $tree"; then
		rexec "$REMOTE_RUNNER leaf-stop $tag" || true
		return 1
	fi
	rexec "$REMOTE_RUNNER leaf-stop $tag"
}

run_nvme_point() {
	local mode="$1" workers="$2" qd="$3" fua="${4:-0}"
	local tag="current-nvmet-${mode}-w${workers}-qd${qd}"
	[ "$fua" != 1 ] || tag+="-fua"
	rexec "$REMOTE_RUNNER nvme-run $tag $mode $workers $qd $fua"
}

cleanup() {
	local status=$?
	set +e
	rexec "$REMOTE_RUNNER nvmet-client-disconnect"
	rexec "$REMOTE_RUNNER nvmet-target-cleanup"
	exit "$status"
}

trap cleanup EXIT INT TERM

rexec "$REMOTE_RUNNER prepare"
rexec "$REMOTE_RUNNER nvmet-target-setup"
rexec "$REMOTE_RUNNER nvmet-client-connect 1"

# The acceptance gate is genuinely one worker at per-worker QD1 and aggregate QD1.
for mode in read write rw; do
	run_zcnblk_point current efa "$mode" 1 1 0
	run_zcnblk_point current tcp "$mode" 1 1 0
	run_nvme_point "$mode" 1 1 0
done
run_zcnblk_point current efa write 1 1 1
run_zcnblk_point current tcp write 1 1 1
run_nvme_point write 1 1 1

# Complete the single-worker low-depth efficiency curve.
for qd in 2 4 8 16; do
	for mode in read write rw; do
		if (( qd % 4 == 0 )); then
			run_zcnblk_point current tcp "$mode" 1 "$qd" 0
			run_zcnblk_point current efa "$mode" 1 "$qd" 0
		else
			run_zcnblk_point current efa "$mode" 1 "$qd" 0
			run_zcnblk_point current tcp "$mode" 1 "$qd" 0
		fi
		run_nvme_point "$mode" 1 "$qd" 0
	done
done

# Two explicit lanes/workers for the saturation curve; QD remains per worker.
rexec "$REMOTE_RUNNER nvmet-client-disconnect"
rexec "$REMOTE_RUNNER nvmet-client-connect 2"
for qd in 32 64 128 256; do
	for mode in read write rw; do
		if (( qd == 64 || qd == 256 )); then
			run_zcnblk_point current tcp "$mode" 2 "$qd" 0
			run_zcnblk_point current efa "$mode" 2 "$qd" 0
		else
			run_zcnblk_point current efa "$mode" 2 "$qd" 0
			run_zcnblk_point current tcp "$mode" 2 "$qd" 0
		fi
		run_nvme_point "$mode" 2 "$qd" 0
	done
done

# Small same-host-version regression gate against the pre-refactor base tree.
for mode in read write rw; do
	run_zcnblk_point base tcp "$mode" 1 1 0
	run_zcnblk_point base tcp "$mode" 1 16 0
	run_zcnblk_point base tcp "$mode" 2 128 0
done
for qd in 1 16 128; do
	run_zcnblk_point base efa read "$([ "$qd" -ge 32 ] && printf 2 || printf 1)" "$qd" 0
done

trap - EXIT INT TERM
cleanup
