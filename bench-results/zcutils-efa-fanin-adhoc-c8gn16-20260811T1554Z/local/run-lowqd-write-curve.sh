#!/usr/bin/env bash
set -euo pipefail

ROOT=/home/rob/zcutils
INVENTORY="$ROOT/bench-results/zcutils-efa-fanin-adhoc-c8gn16-20260811T1554Z-inventory.json"
HELPER="$ROOT/scripts/ec2_perf_spot.py"
RUNNER=/home/ubuntu/fanin-node.sh
LOCAL="$ROOT/bench-results/zcutils-efa-fanin-adhoc-c8gn16-20260811T1554Z/local"
active_tag=

rexec() {
	"$HELPER" exec --inventory "$INVENTORY" --public-ip "$*"
}

cleanup() {
	local status=$?
	set +e
	if [ -n "$active_tag" ]; then
		rexec "$RUNNER leaf-stop $active_tag" >/dev/null 2>&1
	fi
	exit "$status"
}
trap cleanup EXIT INT TERM

run_point() {
	local transport="$1" qd="$2" tag="${transport}-write-w1-q${qd}-curve"
	local owner_mode=placement rma_qd=1 pipeline=16 log="$LOCAL/$tag.console.log"
	if [ "$transport" = efa ]; then
		owner_mode=single-domain-fan-in
		rma_qd=64
		pipeline=1
	fi
	active_tag="$tag"
	rexec "$RUNNER leaf-start $tag $transport write 1 $rma_qd" >"$log" 2>&1
	rexec "$RUNNER zcnblk-run $tag $transport write 1 $qd 1 $owner_mode $rma_qd 300000 $pipeline 1 0" >>"$log" 2>&1
	rexec "$RUNNER leaf-stop $tag" >>"$log" 2>&1
	active_tag=
	grep -E 'repeat=[0-9]+ zcblockbench-result|runs=|target-summary' "$log"
}

for transport in efa tcp; do
	for qd in 1 2 4 8 16; do
		run_point "$transport" "$qd"
	done
done

trap - EXIT INT TERM
