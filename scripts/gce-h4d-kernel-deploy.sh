#!/usr/bin/env bash
set -euo pipefail

PKG_DIR="${1:-/var/tmp/zcutils-h4d-kernel}"
STAGE_ONE_SHOT="${STAGE_ONE_SHOT:-0}"
FORCE_NON_H4D="${FORCE_NON_H4D:-0}"

die() {
	printf 'gce-h4d-kernel-deploy: %s\n' "$*" >&2
	exit 1
}

need() {
	command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

metadata_machine_type() {
	curl --fail --silent --connect-timeout 1 --max-time 3 --noproxy '*' \
		-H 'Metadata-Flavor: Google' \
		http://169.254.169.254/computeMetadata/v1/instance/machine-type \
		2>/dev/null | sed 's#.*/##' || true
}

[[ "$STAGE_ONE_SHOT" =~ ^[01]$ ]] || die "STAGE_ONE_SHOT must be zero or one"
[[ "$FORCE_NON_H4D" =~ ^[01]$ ]] || die "FORCE_NON_H4D must be zero or one"
for command in curl depmod dracut find grep lsinitrd sha256sum sudo tar; do
	need "$command"
done
[ -d "$PKG_DIR" ] || die "package directory is absent: $PKG_DIR"

machine_type="$(metadata_machine_type)"
if [ "$machine_type" != h4d-standard-192 ] && [ "$FORCE_NON_H4D" != 1 ]; then
	die "refusing to deploy outside h4d-standard-192"
fi
if command -v mokutil >/dev/null 2>&1 && mokutil --sb-state 2>/dev/null | grep -qi enabled; then
	die "Secure Boot is enabled; do not install this unsigned custom kernel"
fi

shopt -s nullglob
archives=("$PKG_DIR"/gce-h4d-kernel-*.tar.xz)
manifests=("$PKG_DIR"/manifest-*.txt)
[ "${#archives[@]}" -eq 1 ] || die "expected exactly one H4D kernel archive, found ${#archives[@]}"
[ "${#manifests[@]}" -eq 1 ] || die "expected exactly one H4D kernel manifest, found ${#manifests[@]}"
if [ -r "$PKG_DIR/SHA256SUMS" ]; then
	(cd "$PKG_DIR" && sha256sum -c SHA256SUMS)
else
	die "SHA256SUMS is required"
fi

KREL="$(sed -n 's/^kernel_release=//p' "${manifests[0]}" | head -n 1)"
[[ "$KREL" =~ ^[A-Za-z0-9._+-]+$ ]] || die "manifest has an invalid kernel release"
tar -tf "${archives[0]}" | grep -qx "boot/vmlinuz-$KREL" || \
	die "archive lacks boot/vmlinuz-$KREL"
for module in gve idpf irdma nvme; do
	tar -tf "${archives[0]}" | grep -Eq "^lib/modules/$KREL/.*/${module}\.ko(\.(xz|zst))?$" || \
		die "archive lacks $module for $KREL"
done

state_dir=/var/lib/zcutils-h4d-kernel
pre_state="/tmp/zcutils-h4d-pre-state.$$"
{
	printf 'installed_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
	printf 'machine_type=%s\n' "${machine_type:-forced-non-h4d}"
	printf 'vendor_kernel=%s\n' "$(uname -r)"
	printf 'custom_kernel=%s\n' "$KREL"
	printf 'stage_one_shot=%s\n' "$STAGE_ONE_SHOT"
	printf 'archive_sha256=%s\n' "$(sha256sum "${archives[0]}" | awk '{print $1}')"
	uname -a | sed 's/^/pre_install_uname=/'
	lsmod | grep -E '^(gve|idpf|irdma|ib_core|iw_cm|nvme|nvme_core)\b' | \
		sed 's/^/pre_install_module=/' || true
	for interface in /sys/class/net/*; do
		[ "$(basename "$interface")" = lo ] && continue
		printf 'pre_install_netdev=%s driver=%s\n' "$(basename "$interface")" \
			"$(basename "$(readlink -f "$interface/device/driver" 2>/dev/null || printf unreported)")"
	done
} >"$pre_state"

sudo mkdir -p "$state_dir"
sudo cp "$pre_state" "$state_dir/install-$KREL.txt"
rm -f "$pre_state"
sudo tar --same-owner -C / -xJf "${archives[0]}"
sudo depmod "$KREL"
sudo dracut --force --add-drivers 'gve idpf irdma nvme nvme_core' \
	"/boot/initramfs-$KREL.img" "$KREL"

initramfs_contents="$(sudo lsinitrd "/boot/initramfs-$KREL.img")"
for module in gve idpf irdma nvme; do
	grep -Eq "/${module}\.ko(\.(xz|zst))?$" <<<"$initramfs_contents" || \
		die "generated initramfs lacks $module; do not reboot"
done

if command -v grub2-mkconfig >/dev/null 2>&1; then
	if [ -e /etc/grub2-efi.cfg ]; then
		grub_cfg="$(readlink -f /etc/grub2-efi.cfg)"
	elif [ -e /etc/grub2.cfg ]; then
		grub_cfg="$(readlink -f /etc/grub2.cfg)"
	elif [ -d /boot/grub2 ]; then
		grub_cfg=/boot/grub2/grub.cfg
	else
		die "cannot locate a GRUB2 configuration target"
	fi
	sudo grub2-mkconfig -o "$grub_cfg"
else
	die "grub2-mkconfig is required"
fi

if command -v grubby >/dev/null 2>&1; then
	sudo grubby --info="/boot/vmlinuz-$KREL" | sudo tee "$state_dir/grubby-$KREL.txt" >/dev/null
fi

if [ "$STAGE_ONE_SHOT" = 1 ]; then
	need grub2-editenv
	need grub2-reboot
	grub_env="$(sudo grub2-editenv list 2>/dev/null || true)"
	if [ -r /etc/default/grub ] && ! grep -Eq '^GRUB_DEFAULT=saved\b' /etc/default/grub; then
		die "GRUB_DEFAULT is not saved; one-shot boot cannot be trusted"
	fi
	entry_title=""
	if command -v grubby >/dev/null 2>&1; then
		entry_title="$(sudo grubby --info="/boot/vmlinuz-$KREL" | sed -n 's/^title="\(.*\)"$/\1/p' | head -n 1)"
	fi
	[ -n "$entry_title" ] || die "could not resolve the exact GRUB title for $KREL"
	sudo grub2-reboot "$entry_title"
	staged_env="$(sudo grub2-editenv list 2>/dev/null || true)"
	grep -Fq 'next_entry=' <<<"$staged_env" || die "GRUB did not retain a next_entry"
	{
		printf 'grub_env_before=%q\n' "$grub_env"
		printf 'one_shot_entry=%s\n' "$entry_title"
		printf 'grub_env_after=%q\n' "$staged_env"
	} | sudo tee -a "$state_dir/install-$KREL.txt" >/dev/null
	printf 'one-shot boot staged for %s; reboot manually, then run gce-h4d-kernel-probe.sh\n' "$KREL"
else
	printf 'installed %s without changing the boot target\n' "$KREL"
	printf 'audit first; then rerun with STAGE_ONE_SHOT=1 to stage one reboot\n'
fi
printf 'vendor kernel remains the permanent baseline: %s\n' "$(uname -r)"
