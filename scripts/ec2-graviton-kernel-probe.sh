#!/usr/bin/env bash
set -euo pipefail

ZCUTILS="${ZCUTILS:-zcutils}"
if ! command -v "$ZCUTILS" >/dev/null 2>&1 && [ -x /home/ubuntu/zcutils/bin/zcutils ]; then
	ZCUTILS=/home/ubuntu/zcutils/bin/zcutils
fi

section() {
	printf '\n== %s ==\n' "$1"
}

section system
date -Is
uname -a
printf 'machine=%s\n' "$(uname -m)"
printf 'efi_booted=%s\n' "$([ -d /sys/firmware/efi ] && echo yes || echo no)"
if command -v nproc >/dev/null 2>&1; then
	printf 'nproc=%s\n' "$(nproc)"
fi
if command -v lscpu >/dev/null 2>&1; then
	lscpu | sed -n '1,24p'
fi
if [ -r /etc/os-release ]; then
	sed -n '1,12p' /etc/os-release
fi

section kernel-config
config="/boot/config-$(uname -r)"
if [ -r "$config" ]; then
	grep -E '^(CONFIG_IO_URING|CONFIG_IO_URING_ZCRX|CONFIG_IO_URING_SLOT_RW|CONFIG_NET_RX_BUSY_POLL|CONFIG_ENA_ETHERNET|CONFIG_BLK_DEV_NVME|CONFIG_NVME_MULTIPATH|CONFIG_EFI|CONFIG_EFI_STUB|CONFIG_ACPI|CONFIG_PCI)=' "$config" || true
elif [ -r /proc/config.gz ]; then
	zcat /proc/config.gz | grep -E '^(CONFIG_IO_URING|CONFIG_IO_URING_ZCRX|CONFIG_IO_URING_SLOT_RW|CONFIG_NET_RX_BUSY_POLL|CONFIG_ENA_ETHERNET|CONFIG_BLK_DEV_NVME|CONFIG_NVME_MULTIPATH|CONFIG_EFI|CONFIG_EFI_STUB|CONFIG_ACPI|CONFIG_PCI)=' || true
else
	echo "no readable kernel config found"
fi

section modules
lsmod | grep -E '^(ena|nvme|nvme_core|netdevsim|null_blk)\b' || true
for mod in ena nvme nvme_core netdevsim null_blk; do
	if command -v modinfo >/dev/null 2>&1; then
		modinfo "$mod" 2>/dev/null | sed -n '1,12p' || true
	fi
done

section block
lsblk -o NAME,TYPE,SIZE,MODEL,MOUNTPOINTS

section network
ip -brief link
for iface in $(ls /sys/class/net | grep -v '^lo$'); do
	echo "-- $iface"
	readlink -f "/sys/class/net/$iface/device/driver" 2>/dev/null || true
	if command -v ethtool >/dev/null 2>&1; then
		ethtool -i "$iface" || true
		ethtool -l "$iface" || true
		ethtool -x "$iface" 2>/dev/null || true
		ethtool -n "$iface" 2>/dev/null || true
		ethtool -k "$iface" | grep -E 'tcp-data-split|rx-gro|rx-checksumming|tx-checksumming|scatter-gather|tcp-segmentation-offload|generic-segmentation-offload|generic-receive-offload' || true
	fi
done

section io-uring-probe
if command -v "$ZCUTILS" >/dev/null 2>&1 || [ -x "$ZCUTILS" ]; then
	"$ZCUTILS" zcprobe || true
else
	echo "zcutils binary not found; set ZCUTILS=/path/to/zcutils"
fi

section dmesg
if dmesg --level=err,warn >/tmp/zcutils-dmesg-warn.$$ 2>/dev/null; then
	tail -120 /tmp/zcutils-dmesg-warn.$$
	rm -f /tmp/zcutils-dmesg-warn.$$
elif command -v sudo >/dev/null 2>&1; then
	sudo dmesg --level=err,warn | tail -120 || true
else
	echo "dmesg not readable"
fi
