#!/usr/bin/env bash
set -euo pipefail

PKG_DIR="${1:-/var/tmp/zcutils-graviton-kernel}"
STAGE_ONE_SHOT="${STAGE_ONE_SHOT:-1}"
STAGE_PERMANENT="${STAGE_PERMANENT:-0}"
INSTALL_LIBC_DEV="${INSTALL_LIBC_DEV:-0}"

if [ "$(uname -m)" != "aarch64" ] && [ "$(uname -m)" != "arm64" ]; then
	echo "warning: this does not look like an arm64/Graviton host: $(uname -m)" >&2
fi

if [ ! -d /sys/firmware/efi ]; then
	echo "warning: /sys/firmware/efi is missing; this instance may not be UEFI-booted" >&2
fi

shopt -s nullglob
images=("$PKG_DIR"/linux-image-*.deb)
headers=("$PKG_DIR"/linux-headers-*.deb)
libc_pkgs=("$PKG_DIR"/linux-libc-dev_*.deb)

if [ "${#images[@]}" -ne 1 ]; then
	echo "expected exactly one linux-image package in $PKG_DIR, found ${#images[@]}" >&2
	exit 1
fi

KREL="$(dpkg-deb -f "${images[0]}" Package | sed 's/^linux-image-//')"
if [ -z "$KREL" ] || [ "$KREL" = "$(dpkg-deb -f "${images[0]}" Package)" ]; then
	echo "could not derive kernel release from ${images[0]}" >&2
	exit 1
fi

echo "installing kernel release: $KREL"
install_pkgs=("${images[@]}" "${headers[@]}")
if [ "$INSTALL_LIBC_DEV" = "1" ]; then
	install_pkgs+=("${libc_pkgs[@]}")
elif [ "${#libc_pkgs[@]}" -gt 0 ]; then
	echo "leaving linux-libc-dev package uninstalled; set INSTALL_LIBC_DEV=1 to install it"
fi
sudo dpkg -i "${install_pkgs[@]}"

if command -v update-initramfs >/dev/null 2>&1; then
	sudo update-initramfs -c -k "$KREL" || sudo update-initramfs -u -k "$KREL"
elif command -v dracut >/dev/null 2>&1; then
	sudo dracut --force "/boot/initramfs-$KREL.img" "$KREL"
fi

if command -v update-grub >/dev/null 2>&1; then
	sudo update-grub
elif command -v grub2-mkconfig >/dev/null 2>&1; then
	if [ -d /boot/grub2 ]; then
		sudo grub2-mkconfig -o /boot/grub2/grub.cfg
	else
		sudo grub2-mkconfig -o /boot/grub/grub.cfg
	fi
fi

grub_cfg_has() {
	local needle="$1"
	if [ -r /boot/grub/grub.cfg ]; then
		grep -Fq "$needle" /boot/grub/grub.cfg
	else
		sudo grep -Fq "$needle" /boot/grub/grub.cfg
	fi
}

entry="Advanced options for Ubuntu>Ubuntu, with Linux $KREL"
if ! grub_cfg_has "menuentry 'Ubuntu, with Linux $KREL'"; then
	entry="Advanced options for Debian GNU/Linux>Debian GNU/Linux, with Linux $KREL"
fi

if [ "$STAGE_ONE_SHOT" = "1" ] && command -v grub-reboot >/dev/null 2>&1; then
	if [ -r /etc/default/grub ] && ! grep -Eq '^GRUB_DEFAULT=saved\b' /etc/default/grub; then
		echo "warning: /etc/default/grub does not set GRUB_DEFAULT=saved; grub-reboot may not be honored" >&2
		echo "warning: Ubuntu cloud images with GRUB_DEFAULT=0 may boot the newest installed kernel as the default" >&2
	fi
	echo "staging one-shot GRUB boot: $entry"
	if ! sudo grub-reboot "$entry"; then
		echo "grub-reboot by submenu name failed; list entries and stage manually before rebooting" >&2
	fi
fi

if [ "$STAGE_PERMANENT" = "1" ]; then
	if command -v grubby >/dev/null 2>&1; then
		sudo grubby --set-default "/boot/vmlinuz-$KREL"
	elif command -v grub-set-default >/dev/null 2>&1; then
		sudo grub-set-default "$entry"
	else
		echo "no supported permanent GRUB default setter found" >&2
	fi
fi

state_dir=/var/lib/zcutils-graviton-kernel
sudo mkdir -p "$state_dir"
{
	echo "installed_at=$(date -Is)"
	echo "kernel_release=$KREL"
	echo "pkg_dir=$PKG_DIR"
	echo "stage_one_shot=$STAGE_ONE_SHOT"
	echo "stage_permanent=$STAGE_PERMANENT"
	uname -a | sed 's/^/pre_reboot_uname=/'
	if command -v grub-editenv >/dev/null 2>&1; then
		grub-editenv list 2>/dev/null | sed 's/^/grubenv=/'
	fi
} | sudo tee "$state_dir/install-$KREL.txt" >/dev/null

echo "installed $KREL"
echo "reboot manually, then run: bash $PKG_DIR/ec2-graviton-kernel-probe.sh"
