#!/usr/bin/env bash
set -euo pipefail

mode="${1:?usage: adhoc-nic-low-latency.sh <apply|verify> OUTDIR}"
outdir="${2:?usage: adhoc-nic-low-latency.sh <apply|verify> OUTDIR}"
manifest="${ZCUTILS_BOOTSTRAP_MANIFEST:-${HOME:?}/.local/state/zcutils/adhoc-bootstrap.env}"

die() {
	printf 'adhoc-nic-low-latency: ERROR: %s\n' "$*" >&2
	exit 1
}

[ "$mode" = apply ] || [ "$mode" = verify ] || die "mode must be apply or verify"
[ -r "$manifest" ] || die "missing ad-hoc bootstrap manifest: $manifest"
grep -qx 'coordination_scope=dedicated-adhoc-instance' "$manifest" || \
	die "refusing to change NIC state outside a dedicated ad-hoc instance"
command -v ethtool >/dev/null || die "ethtool is required"
command -v sudo >/dev/null || die "sudo is required"
sudo -n true || die "passwordless sudo is required"

declare -a interfaces=()
if [ -n "${URING_PLAY_ADHOC_NIC_INTERFACES:-}" ]; then
	IFS=, read -r -a interfaces <<<"$URING_PLAY_ADHOC_NIC_INTERFACES"
else
	for netdev in /sys/class/net/*; do
		iface="${netdev##*/}"
		[ "$iface" != lo ] || continue
		driver="$(ethtool -i "$iface" 2>/dev/null | awk '$1 == "driver:" { print $2; exit }')"
		[ "$driver" = ena ] || continue
		interfaces+=("$iface")
	done
fi
[ "${#interfaces[@]}" -gt 0 ] || die "no ENA interfaces found"

mkdir -p "$outdir"
log="$outdir/nic-low-latency.log"
: >"$log"

snapshot() {
	local label="$1" iface="$2"
	{
		printf 'run_id=%s node_index=%s mode=%s label=%s interface=%s\n' \
			"${URING_RUN_ID:-unknown}" "${URING_NODE_INDEX:-unknown}" "$mode" "$label" "$iface"
		ethtool -i "$iface"
		ethtool -c "$iface"
	} >>"$log"
}

verify_interface() {
	local iface="$1" coalesce
	coalesce="$(ethtool -c "$iface")"
	grep -Eq '^Adaptive RX:[[:space:]]+off([[:space:]]|$)' <<<"$coalesce" || \
		die "$iface still has adaptive RX enabled"
	awk '$1 == "rx-usecs:" { found=1; good=($2 == 0) } END { exit !(found && good) }' \
		<<<"$coalesce" || die "$iface rx-usecs is not zero"
	awk '$1 == "tx-usecs:" { found=1; good=($2 == 0) } END { exit !(found && good) }' \
		<<<"$coalesce" || die "$iface tx-usecs is not zero"
}

for iface in "${interfaces[@]}"; do
	[[ "$iface" =~ ^[[:alnum:]_.:-]+$ ]] || die "invalid interface name: $iface"
	[ -e "/sys/class/net/$iface" ] || die "interface does not exist: $iface"
	snapshot before "$iface"
	if [ "$mode" = apply ]; then
		sudo -n ethtool -C "$iface" adaptive-rx off rx-usecs 0 tx-usecs 0
	fi
	verify_interface "$iface"
	snapshot after "$iface"
done

{
	printf 'nic_low_latency_confirmed=true\n'
	printf 'scope=local-dedicated-adhoc-node\n'
	printf 'interfaces=%s\n' "$(IFS=,; printf '%s' "${interfaces[*]}")"
	printf 'settings=adaptive-rx-off,rx-usecs-0,tx-usecs-0\n'
} | tee "$outdir/nic-low-latency-confirmed.env"
