#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LINUX_SRC="${LINUX_SRC:-/home/rob/dev-workspace/src/linux}"
BUILD_DIR="${BUILD_DIR:-/home/rob/dev-workspace/build/linux-x86_64-gce-h4d}"
OUT_DIR="${OUT_DIR:-$ROOT/qemu-zcrx/gce-h4d-kernel-out}"
STAGE_DIR="${STAGE_DIR:-$BUILD_DIR/stage}"
SEED_CONFIG="${SEED_CONFIG:-/boot/config-$(uname -r)}"
EXPECTED_SEED_SHA256="${EXPECTED_SEED_SHA256:-}"
JOBS="${JOBS:-$(nproc)}"
KERNEL_SUFFIX="${KERNEL_SUFFIX:--zc-h4d-io-slots}"
KERNEL_PROFILE="${KERNEL_PROFILE:-io-slots}"
EXPECTED_BRANCH="${EXPECTED_BRANCH:-rob/io-slots-v7.0.8-backport-attempt}"
ALLOW_DIRTY_SOURCE="${ALLOW_DIRTY_SOURCE:-0}"
ALLOW_NON_H4D_SEED="${ALLOW_NON_H4D_SEED:-0}"
CONFIG_ONLY="${CONFIG_ONLY:-0}"
ARCH="${ARCH:-x86_64}"

die() {
	printf 'gce-h4d-kernel-build: %s\n' "$*" >&2
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

source_has_symbol() {
	rg -q "^(menuconfig|config)[[:space:]]+$1([[:space:]]|$)" \
		"$LINUX_SRC" -g 'Kconfig*'
}

set_symbol() {
	local mode="$1"
	local symbol="$2"
	source_has_symbol "$symbol" || die "required kernel symbol is absent: CONFIG_$symbol"
	"$LINUX_SRC/scripts/config" --file "$CONFIG" "--$mode" "$symbol"
}

need make
need gcc
need git
need rg
need sha256sum
need tar
need xz
need curl
need cp
need diff
need install

[ "$ARCH" = x86_64 ] || die "ARCH must remain x86_64 for H4D"
[[ "$CONFIG_ONLY" =~ ^[01]$ ]] || die "CONFIG_ONLY must be zero or one"
[[ "$ALLOW_NON_H4D_SEED" =~ ^[01]$ ]] || die "ALLOW_NON_H4D_SEED must be zero or one"
[[ "$ALLOW_DIRTY_SOURCE" =~ ^[01]$ ]] || die "ALLOW_DIRTY_SOURCE must be zero or one"
case "$KERNEL_PROFILE" in
	io-slots | nightly) ;;
	*) die "KERNEL_PROFILE must be io-slots or nightly" ;;
esac

[ -d "$LINUX_SRC" ] || die "LINUX_SRC does not exist: $LINUX_SRC"
git -C "$LINUX_SRC" rev-parse --git-dir >/dev/null 2>&1 || \
	die "LINUX_SRC is not a git checkout: $LINUX_SRC"
[ -x "$LINUX_SRC/scripts/config" ] || die "kernel scripts/config is unavailable"
[ -r "$SEED_CONFIG" ] || die "H4D vendor seed config is unreadable: $SEED_CONFIG"

branch="$(git -C "$LINUX_SRC" branch --show-current 2>/dev/null || true)"
if [ -n "$EXPECTED_BRANCH" ] && [ "$branch" != "$EXPECTED_BRANCH" ]; then
	printf 'warning: expected branch %s, got %s\n' "$EXPECTED_BRANCH" "${branch:-detached}" >&2
fi
if [ "$ALLOW_DIRTY_SOURCE" != 1 ] && \
	[ -n "$(git -C "$LINUX_SRC" status --porcelain --untracked-files=no)" ]; then
	die "LINUX_SRC has tracked changes; set ALLOW_DIRTY_SOURCE=1 only for an intentional build"
fi

machine_type="$(metadata_machine_type)"
if [ "$machine_type" != h4d-standard-192 ] && [ "$ALLOW_NON_H4D_SEED" != 1 ]; then
	die "build is not running on H4D; copy an exact H4D /boot/config and set ALLOW_NON_H4D_SEED=1 only for a config rehearsal"
fi
seed_sha256="$(sha256sum "$SEED_CONFIG" | awk '{print $1}')"
if [ -n "$EXPECTED_SEED_SHA256" ] && [ "$seed_sha256" != "$EXPECTED_SEED_SHA256" ]; then
	die "seed config SHA-256 differs from EXPECTED_SEED_SHA256"
fi

for path in include/uapi/linux/io_uring.h drivers/net/ethernet/google/Kconfig \
	drivers/net/ethernet/intel/idpf/Kconfig drivers/infiniband/hw/irdma/Kconfig; do
	[ -e "$LINUX_SRC/$path" ] || die "required source path is absent: $path"
done
grep -q IORING_OP_SEND_ZC "$LINUX_SRC/include/uapi/linux/io_uring.h" || \
	die "kernel tree lacks IORING_OP_SEND_ZC"
if [ "$KERNEL_PROFILE" = io-slots ]; then
	for token in IORING_OP_SLOT_RW IORING_REGISTER_ZCRX_IFQ IORING_REGISTER_IO_SLOT; do
		grep -q "$token" "$LINUX_SRC/include/uapi/linux/io_uring.h" || \
			die "io-slots kernel tree lacks $token"
	done
fi

mkdir -p "$BUILD_DIR" "$OUT_DIR"
cp "$SEED_CONFIG" "$BUILD_DIR/.config"
CONFIG="$BUILD_DIR/.config"
make_args=(-C "$LINUX_SRC" O="$BUILD_DIR" ARCH="$ARCH" LOCALVERSION=)

"$LINUX_SRC/scripts/config" --file "$CONFIG" --set-str LOCALVERSION "$KERNEL_SUFFIX"
"$LINUX_SRC/scripts/config" --file "$CONFIG" --disable LOCALVERSION_AUTO

for symbol in \
	X86_64 MODULES MODULE_UNLOAD IKCONFIG IKCONFIG_PROC KALLSYMS KALLSYMS_ALL \
	EFI EFI_STUB ACPI PCI PCI_MSI NUMA HYPERVISOR_GUEST KVM_GUEST PARAVIRT \
	DEVTMPFS DEVTMPFS_MOUNT BLK_DEV_INITRD BLK_DEV_NVME NVME_MULTIPATH \
	VIRTIO VIRTIO_PCI VIRTIO_BLK SCSI SCSI_VIRTIO INET IPV6 IO_URING \
	NET_RX_BUSY_POLL HUGETLBFS HUGETLB_PAGE EXT4_FS XFS_FS BPF BPF_SYSCALL \
	DEBUG_FS; do
	set_symbol enable "$symbol"
done
for symbol in GVE IDPF INFINIBAND INFINIBAND_USER_ACCESS \
	INFINIBAND_USER_MAD INFINIBAND_IRDMA; do
	set_symbol module "$symbol"
done
set_symbol enable INFINIBAND_ADDR_TRANS
if source_has_symbol IO_URING_ZCRX; then
	set_symbol enable IO_URING_ZCRX
fi
if source_has_symbol IO_URING_SLOT_RW; then
	set_symbol enable IO_URING_SLOT_RW
fi

make "${make_args[@]}" olddefconfig
KREL="$(make -s "${make_args[@]}" kernelrelease)"

required_symbols=(
	CONFIG_X86_64 CONFIG_MODULES CONFIG_EFI_STUB CONFIG_ACPI CONFIG_PCI
	CONFIG_PCI_MSI CONFIG_NUMA CONFIG_BLK_DEV_INITRD CONFIG_BLK_DEV_NVME
	CONFIG_NVME_MULTIPATH CONFIG_GVE CONFIG_IDPF CONFIG_INFINIBAND
	CONFIG_INFINIBAND_USER_ACCESS CONFIG_INFINIBAND_IRDMA
	CONFIG_INFINIBAND_ADDR_TRANS CONFIG_IO_URING CONFIG_NET_RX_BUSY_POLL
	CONFIG_HUGETLBFS CONFIG_HUGETLB_PAGE CONFIG_EXT4_FS CONFIG_XFS_FS
)
for symbol in "${required_symbols[@]}"; do
	grep -Eq "^${symbol}=(y|m)$" "$CONFIG" || \
		die "required symbol is not enabled after olddefconfig: $symbol"
done
for symbol in CONFIG_GVE CONFIG_IDPF CONFIG_INFINIBAND CONFIG_INFINIBAND_IRDMA; do
	grep -Eq "^${symbol}=m$" "$CONFIG" || \
		die "$symbol must remain modular so the H4D driver/initramfs path is auditable"
done
if [ "$KERNEL_PROFILE" = io-slots ]; then
	for symbol in CONFIG_IO_URING_ZCRX CONFIG_IO_URING_SLOT_RW; do
		grep -Eq "^${symbol}=y$" "$CONFIG" || die "io-slots profile requires $symbol=y"
	done
fi

config_copy="$OUT_DIR/config-$KREL"
diff_copy="$OUT_DIR/config-diff-$KREL.txt"
manifest="$OUT_DIR/manifest-$KREL.txt"
cp "$CONFIG" "$config_copy"
if [ -x "$LINUX_SRC/scripts/diffconfig" ]; then
	"$LINUX_SRC/scripts/diffconfig" "$SEED_CONFIG" "$CONFIG" >"$diff_copy"
else
	diff -u "$SEED_CONFIG" "$CONFIG" >"$diff_copy" || true
fi
{
	printf 'platform=gce-h4d-standard-192\n'
	printf 'kernel_release=%s\n' "$KREL"
	printf 'kernel_profile=%s\n' "$KERNEL_PROFILE"
	printf 'source_commit=%s\n' "$(git -C "$LINUX_SRC" rev-parse HEAD)"
	printf 'source_branch=%s\n' "${branch:-detached}"
	printf 'source_tree=%s\n' "$LINUX_SRC"
	printf 'seed_config=%s\n' "$SEED_CONFIG"
	printf 'seed_config_sha256=%s\n' "$seed_sha256"
	printf 'seed_machine_type=%s\n' "${machine_type:-non-h4d-rehearsal}"
	printf 'config_only=%s\n' "$CONFIG_ONLY"
	printf 'driver_contract=gve-control,idpf-netdev,irdma-verbs,nvme-hyperdisk\n'
	printf 'benchmark_contract=stock-hpc-kernel-first,custom-kernel-only-after-rdma-requalification\n'
} >"$manifest"

printf 'kernel_release=%s\nseed_config_sha256=%s\nconfig=%s\nconfig_diff=%s\n' \
	"$KREL" "$seed_sha256" "$config_copy" "$diff_copy"
if [ "$CONFIG_ONLY" = 1 ]; then
	(cd "$OUT_DIR" && sha256sum "$(basename "$config_copy")" \
		"$(basename "$diff_copy")" "$(basename "$manifest")" >SHA256SUMS-config)
	printf 'CONFIG_ONLY=1: H4D config generated and audited; no kernel was compiled\n'
	exit 0
fi

rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR/root/boot"
make "${make_args[@]}" -j"$JOBS" bzImage modules
make "${make_args[@]}" INSTALL_MOD_PATH="$STAGE_DIR/root" modules_install
install -m 0644 "$BUILD_DIR/arch/x86/boot/bzImage" "$STAGE_DIR/root/boot/vmlinuz-$KREL"
install -m 0644 "$BUILD_DIR/System.map" "$STAGE_DIR/root/boot/System.map-$KREL"
install -m 0644 "$CONFIG" "$STAGE_DIR/root/boot/config-$KREL"

for module in gve idpf irdma nvme; do
	find "$STAGE_DIR/root/lib/modules/$KREL" -type f \
		\( -name "$module.ko" -o -name "$module.ko.xz" -o -name "$module.ko.zst" \) \
		-print -quit | grep -q . || die "staged kernel is missing module $module"
done

archive="$OUT_DIR/gce-h4d-kernel-$KREL.tar.xz"
tar --numeric-owner --owner=0 --group=0 -C "$STAGE_DIR/root" -cJf "$archive" boot lib/modules
{
	printf 'archive=%s\n' "$(basename "$archive")"
	printf 'built_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} >>"$manifest"
(cd "$OUT_DIR" && sha256sum "$(basename "$archive")" "$(basename "$config_copy")" \
	"$(basename "$diff_copy")" "$(basename "$manifest")" >SHA256SUMS)
printf 'built_h4d_kernel_archive=%s\nmanifest=%s\n' "$archive" "$manifest"
