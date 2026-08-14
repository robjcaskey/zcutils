#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
H4D_PREFLIGHT="${H4D_PREFLIGHT:-$ROOT/scripts/gce-h4d-rdma-preflight.sh}"
EXPECTED_KERNEL_RELEASE="${EXPECTED_KERNEL_RELEASE:-}"
REQUIRE_CUSTOM_KERNEL="${REQUIRE_CUSTOM_KERNEL:-0}"
REQUIRE_ZCPROBE="${REQUIRE_ZCPROBE:-$REQUIRE_CUSTOM_KERNEL}"
ZCUTILS="${ZCUTILS:-zcutils}"
problems=0

die() {
	printf 'gce-h4d-kernel-probe: %s\n' "$*" >&2
	exit 1
}

issue() {
	printf 'KERNEL PROBE ERROR: %s\n' "$*" >&2
	problems=$((problems + 1))
}

[[ "$REQUIRE_CUSTOM_KERNEL" =~ ^[01]$ ]] || die "REQUIRE_CUSTOM_KERNEL must be zero or one"
[[ "$REQUIRE_ZCPROBE" =~ ^[01]$ ]] || die "REQUIRE_ZCPROBE must be zero or one"
[ -x "$H4D_PREFLIGHT" ] || die "H4D RDMA preflight is not executable: $H4D_PREFLIGHT"

running="$(uname -r)"
if [ -n "$EXPECTED_KERNEL_RELEASE" ] && [ "$running" != "$EXPECTED_KERNEL_RELEASE" ]; then
	issue "running kernel $running differs from EXPECTED_KERNEL_RELEASE=$EXPECTED_KERNEL_RELEASE"
fi
if [ "$REQUIRE_CUSTOM_KERNEL" = 1 ]; then
	[ -n "$EXPECTED_KERNEL_RELEASE" ] || issue "custom-kernel qualification requires EXPECTED_KERNEL_RELEASE"
	[ -r "/var/lib/zcutils-h4d-kernel/install-$running.txt" ] || \
		issue "no deployment record exists for running kernel $running"
fi

config="/boot/config-$running"
if [ ! -r "$config" ]; then
	issue "running kernel config is unreadable: $config"
else
	for symbol in CONFIG_X86_64 CONFIG_NUMA CONFIG_PCI_MSI CONFIG_GVE CONFIG_IDPF \
		CONFIG_INFINIBAND CONFIG_INFINIBAND_USER_ACCESS CONFIG_INFINIBAND_IRDMA \
		CONFIG_INFINIBAND_ADDR_TRANS CONFIG_BLK_DEV_NVME CONFIG_NVME_MULTIPATH \
		CONFIG_IO_URING CONFIG_NET_RX_BUSY_POLL CONFIG_HUGETLBFS CONFIG_HUGETLB_PAGE; do
		grep -Eq "^${symbol}=(y|m)$" "$config" || issue "$symbol is not enabled"
	done
fi

for module in gve idpf irdma nvme; do
	if ! path="$(modinfo -n "$module" 2>/dev/null)" || [ -z "$path" ]; then
		issue "module $module is unavailable"
		continue
	fi
	vermagic="$(modinfo -F vermagic "$module" 2>/dev/null | awk '{print $1}')"
	[ "$vermagic" = "$running" ] || issue "$module vermagic $vermagic differs from $running"
	printf 'kernel_module module=%s path=%s vermagic=%s loaded=%s\n' \
		"$module" "$path" "$vermagic" \
		"$(lsmod | grep -q "^${module}\\b" && printf yes || printf no)"
done

if command -v "$ZCUTILS" >/dev/null 2>&1 || [ -x "$ZCUTILS" ]; then
	if ! "$ZCUTILS" zcprobe && [ "$REQUIRE_ZCPROBE" = 1 ]; then
		issue "zcutils zcprobe failed"
	fi
else
	[ "$REQUIRE_ZCPROBE" = 0 ] || issue "zcutils is unavailable; set ZCUTILS=/path/to/zcutils"
fi

printf 'kernel_probe running=%s expected=%s require_custom=%s preflight=%s\n' \
	"$running" "${EXPECTED_KERNEL_RELEASE:-any}" "$REQUIRE_CUSTOM_KERNEL" "$H4D_PREFLIGHT"

if ! URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 \
	"$H4D_PREFLIGHT"; then
	issue "strict H4D RDMA/topology qualification failed"
fi

tmp="$(mktemp)"
filtered="$tmp.filtered"
trap 'rm -f "$tmp" "$filtered"' EXIT
if dmesg --level=err,warn >"$tmp" 2>/dev/null || sudo dmesg --level=err,warn >"$tmp" 2>/dev/null; then
	if grep -Ei 'gve|idpf|irdma|infiniband|rdma|nvme' "$tmp" | \
		grep -Ei 'fail|error|timeout|reset|oops|bug:|panic|fatal' >"$filtered"; then
		cat "$filtered" >&2
		issue "running kernel logged an RDMA/NIC/NVMe failure"
	fi
	grep -Ei 'gve|idpf|irdma|infiniband|rdma|nvme' "$tmp" | tail -120 || true
else
	issue "dmesg is not readable"
fi

printf 'kernel_probe_problems=%s qualified=%s\n' \
	"$problems" "$([ "$problems" -eq 0 ] && printf yes || printf no)"
[ "$problems" -eq 0 ] || die "$problems kernel qualification problem(s)"
