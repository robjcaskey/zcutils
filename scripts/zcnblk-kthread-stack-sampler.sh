#!/usr/bin/env bash
set -euo pipefail

usage() {
	printf 'usage: %s OUTFILE [DURATION_SECONDS] [INTERVAL_SECONDS]\n' "$0" >&2
	exit 2
}

[ "$#" -ge 1 ] && [ "$#" -le 3 ] || usage
outfile=$1
duration=${2:-25}
interval=${3:-0.05}
[[ "$duration" =~ ^[1-9][0-9]*$ ]] || usage
[[ "$interval" =~ ^0\.[0-9]+$|^[1-9][0-9]*(\.[0-9]+)?$ ]] || usage
[ "$(id -u)" -eq 0 ] || {
	printf 'zcnblk kthread stack sampling requires root\n' >&2
	exit 1
}

deadline=$(( $(date +%s) + duration ))
: >"$outfile"
while [ "$(date +%s)" -lt "$deadline" ]; do
	now_ns=$(date +%s%N)
	for comm_path in /proc/[0-9]*/comm; do
		[ -r "$comm_path" ] || continue
		IFS= read -r comm <"$comm_path" || continue
		case "$comm" in
			zcnblk-shm-*) ;;
			*) continue ;;
		esac
		pid=${comm_path#/proc/}
		pid=${pid%/comm}
		{
			printf 'sample_ns=%s pid=%s comm=%s\n' "$now_ns" "$pid" "$comm"
			sed -n '/^State:/p;/^voluntary_ctxt_switches:/p;/^nonvoluntary_ctxt_switches:/p' "/proc/$pid/status" 2>/dev/null || true
			printf 'wchan='
			cat "/proc/$pid/wchan" 2>/dev/null || true
			printf '\nstack:\n'
			cat "/proc/$pid/stack" 2>/dev/null || true
			printf 'end_sample\n'
		} >>"$outfile"
	done
	sleep "$interval"
done
