#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LINUX_SRC="${LINUX_SRC:-/home/rob/dev-workspace/src/linux}"
BUILD_DIR="${BUILD_DIR:-/home/rob/dev-workspace/build/linux-arm64-ec2-graviton}"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/qemu-zcrx/ec2-graviton-kernel-out}"
JOBS="${JOBS:-$(nproc)}"
KERNEL_SUFFIX="${KERNEL_SUFFIX:--io-slots-graviton}"
PKG_VERSION="${PKG_VERSION:-7.0.8.io-slots-graviton1}"
KERNEL_PROFILE="${KERNEL_PROFILE:-io-slots}"
CROSS_COMPILE="${CROSS_COMPILE:-aarch64-linux-gnu-}"
ARCH="${ARCH:-arm64}"
KBUILD_DEBARCH="${KBUILD_DEBARCH:-arm64}"
if [ "$KERNEL_PROFILE" = "io-slots" ]; then
	EXPECTED_BRANCH="${EXPECTED_BRANCH:-rob/io-slots-v7.0.8-backport-attempt}"
else
	EXPECTED_BRANCH="${EXPECTED_BRANCH:-}"
fi
ALLOW_DIRTY_SOURCE="${ALLOW_DIRTY_SOURCE:-0}"
CONFIG_ONLY="${CONFIG_ONLY:-0}"
SOURCE_REMOTE_URL="${SOURCE_REMOTE_URL:-}"
SOURCE_REF="${SOURCE_REF:-}"
SOURCE_COMMIT="${SOURCE_COMMIT:-}"
BUILD_KEY="${BUILD_KEY:-}"

need() {
	if ! command -v "$1" >/dev/null 2>&1; then
		echo "missing required command: $1" >&2
		exit 1
	fi
}

need make
need "${CROSS_COMPILE}gcc"

if [ ! -d "$LINUX_SRC" ]; then
	echo "LINUX_SRC does not exist: $LINUX_SRC" >&2
	exit 1
fi

if ! git -C "$LINUX_SRC" rev-parse --git-dir >/dev/null 2>&1; then
	echo "LINUX_SRC is not a git checkout: $LINUX_SRC" >&2
	exit 1
fi

case "$KERNEL_PROFILE" in
	io-slots | nightly) ;;
	*)
		echo "KERNEL_PROFILE must be io-slots or nightly, got: $KERNEL_PROFILE" >&2
		exit 1
		;;
esac

branch="$(git -C "$LINUX_SRC" branch --show-current 2>/dev/null || true)"
if [ -n "$EXPECTED_BRANCH" ] && [ "$branch" != "$EXPECTED_BRANCH" ]; then
	echo "warning: expected $EXPECTED_BRANCH, got ${branch:-unknown}" >&2
fi

if [ "$ALLOW_DIRTY_SOURCE" != "1" ] &&
	[ -n "$(git -C "$LINUX_SRC" status --porcelain --untracked-files=no)" ]; then
	echo "LINUX_SRC has tracked changes; set ALLOW_DIRTY_SOURCE=1 to build anyway" >&2
	git -C "$LINUX_SRC" status --short --untracked-files=no >&2
	exit 1
fi

for path in \
	include/uapi/linux/io_uring.h \
	io_uring/Kconfig \
	drivers/net/ethernet/amazon/ena/ena_netdev.c
do
	if [ ! -e "$LINUX_SRC/$path" ]; then
		echo "required kernel source file missing: $LINUX_SRC/$path" >&2
		exit 1
	fi
done

required_tokens=(IORING_OP_SEND_ZC)
if [ "$KERNEL_PROFILE" = "io-slots" ]; then
	required_tokens+=(IORING_OP_SLOT_RW IORING_REGISTER_ZCRX_IFQ IORING_REGISTER_IO_SLOT)
fi
for token in "${required_tokens[@]}"; do
	if ! grep -q "$token" "$LINUX_SRC/include/uapi/linux/io_uring.h"; then
		echo "required io_uring UAPI token missing: $token" >&2
		exit 1
	fi
done

source_has_symbol() {
	grep -RqsE "^(menuconfig|config)[[:space:]]+$1([[:space:]]|$)" \
		"$LINUX_SRC"/arch/arm64/Kconfig* \
		"$LINUX_SRC"/drivers \
		"$LINUX_SRC"/fs \
		"$LINUX_SRC"/io_uring \
		"$LINUX_SRC"/kernel \
		"$LINUX_SRC"/lib \
		"$LINUX_SRC"/mm \
		"$LINUX_SRC"/net 2>/dev/null
}

enable_if_present() {
	local mode="$1"
	local symbol="$2"
	if source_has_symbol "$symbol"; then
		"$LINUX_SRC/scripts/config" --file "$CONFIG" "--$mode" "$symbol"
		OPTIONAL_CONFIG_SYMBOLS+=("CONFIG_$symbol")
	else
		echo "optional config symbol absent in this source: CONFIG_$symbol"
	fi
}

mkdir -p "$BUILD_DIR" "$OUT_DIR"

make_args=(
	-C "$LINUX_SRC"
	O="$BUILD_DIR"
	ARCH="$ARCH"
	CROSS_COMPILE="$CROSS_COMPILE"
	LOCALVERSION=
)

if [ "$CONFIG_ONLY" != "1" ]; then
	need dpkg-buildpackage
	need dpkg-deb
fi

echo "building arm64 kernel from $LINUX_SRC"
echo "kernel profile: $KERNEL_PROFILE"
echo "build dir: $BUILD_DIR"
echo "output dir: $OUT_DIR"

make "${make_args[@]}" defconfig

CONFIG="$BUILD_DIR/.config"
OPTIONAL_CONFIG_SYMBOLS=()
"$LINUX_SRC/scripts/config" --file "$CONFIG" \
	--set-str LOCALVERSION "$KERNEL_SUFFIX" \
	--disable LOCALVERSION_AUTO \
	--enable IKCONFIG \
	--enable IKCONFIG_PROC \
	--enable KALLSYMS \
	--enable KALLSYMS_ALL \
	--enable MODULES \
	--enable MODULE_UNLOAD \
	--enable EFI \
	--enable EFI_STUB \
	--enable EFIVAR_FS \
	--enable ACPI \
	--enable PCI \
	--enable HOTPLUG_PCI \
	--enable DEVTMPFS \
	--enable DEVTMPFS_MOUNT \
	--enable BINFMT_ELF \
	--enable PROC_FS \
	--enable SYSFS \
	--enable TMPFS \
	--enable BLK_DEV_INITRD \
	--enable RD_GZIP \
	--enable RD_XZ \
	--enable XZ_DEC \
	--enable BLK_DEV_NVME \
	--enable NVME_MULTIPATH \
	--enable ENA_ETHERNET \
	--enable INET \
	--enable IO_URING \
	--enable PAGE_POOL \
	--enable DMA_SHARED_BUFFER \
	--enable NET_RX_BUSY_POLL \
	--module NETDEVSIM \
	--module BLK_DEV_NULL_BLK \
	--enable CONFIGFS_FS \
	--enable EXT4_FS \
	--enable XFS_FS \
	--enable SQUASHFS \
	--enable SQUASHFS_XATTR \
	--enable SQUASHFS_XZ \
	--enable FW_LOADER_COMPRESS \
	--enable FW_LOADER_COMPRESS_XZ \
	--enable BPF \
	--enable BPF_SYSCALL \
	--enable DEBUG_FS \
	--enable SERIAL_AMBA_PL011 \
	--enable SERIAL_AMBA_PL011_CONSOLE \
	--enable VIRTIO \
	--enable VIRTIO_PCI \
	--enable VIRTIO_MMIO \
	--enable VIRTIO_BLK \
	--enable VIRTIO_NET

enable_if_present enable IO_URING_ZCRX
enable_if_present enable IO_URING_SLOT_RW
enable_if_present module NETDEVSIM
enable_if_present module BLK_DEV_NULL_BLK

make "${make_args[@]}" olddefconfig

KREL="$(make -s "${make_args[@]}" kernelrelease)"
echo "kernel release: $KREL"

for sym in \
	CONFIG_IO_URING \
	CONFIG_NET_RX_BUSY_POLL \
	CONFIG_PAGE_POOL \
	CONFIG_DMA_SHARED_BUFFER \
	CONFIG_ENA_ETHERNET \
	CONFIG_BLK_DEV_NVME \
	CONFIG_BLK_DEV_INITRD \
	CONFIG_BINFMT_ELF \
	CONFIG_PROC_FS \
	CONFIG_SYSFS \
	CONFIG_TMPFS \
	CONFIG_RD_GZIP \
	CONFIG_RD_XZ \
	CONFIG_XZ_DEC \
	CONFIG_SQUASHFS \
	CONFIG_SQUASHFS_XATTR \
	CONFIG_SQUASHFS_XZ \
	CONFIG_FW_LOADER_COMPRESS \
	CONFIG_FW_LOADER_COMPRESS_XZ \
	CONFIG_EFI_STUB \
	CONFIG_SERIAL_AMBA_PL011 \
	CONFIG_SERIAL_AMBA_PL011_CONSOLE \
	CONFIG_IKCONFIG_PROC
do
	if ! grep -Eq "^${sym}=(y|m)$" "$CONFIG"; then
		echo "required symbol not enabled after olddefconfig: $sym" >&2
		exit 1
	fi
done

for sym in "${OPTIONAL_CONFIG_SYMBOLS[@]}"; do
	if grep -Eq "^${sym}=(y|m)$" "$CONFIG"; then
		echo "optional symbol enabled: $sym"
	elif [ "$KERNEL_PROFILE" = "io-slots" ] &&
		{ [ "$sym" = CONFIG_IO_URING_ZCRX ] || [ "$sym" = CONFIG_IO_URING_SLOT_RW ]; }; then
		echo "required io-slots symbol not enabled after olddefconfig: $sym" >&2
		exit 1
	else
		echo "optional symbol unavailable after dependency resolution: $sym"
	fi
done

if [ "$CONFIG_ONLY" = "1" ]; then
	echo "CONFIG_ONLY=1: config generated and audited at $CONFIG"
	exit 0
fi

rm -f \
	"$OUT_DIR"/linux-image-"$KREL"_*.deb \
	"$OUT_DIR"/linux-headers-"$KREL"_*.deb \
	"$OUT_DIR"/linux-libc-dev_"$PKG_VERSION"_"$KBUILD_DEBARCH".deb \
	"$OUT_DIR"/config-"$KREL" \
	"$OUT_DIR"/manifest-"$KREL".txt

make "${make_args[@]}" \
	KBUILD_DEBARCH="$KBUILD_DEBARCH" KDEB_PKGVERSION="$PKG_VERSION" \
	DPKG_FLAGS="-d" \
	-j"$JOBS" bindeb-pkg

shopt -s nullglob
image_candidates=()
for pkg in \
	"$(dirname "$BUILD_DIR")"/linux-image-*_${PKG_VERSION}_${KBUILD_DEBARCH}.deb \
	"$(dirname "$LINUX_SRC")"/linux-image-*_${PKG_VERSION}_${KBUILD_DEBARCH}.deb
do
	case "$pkg" in
		*-dbg_*.deb) ;;
		*) image_candidates+=("$pkg") ;;
	esac
done
if [ "${#image_candidates[@]}" -ne 1 ]; then
	echo "expected exactly one final linux-image package, found ${#image_candidates[@]}" >&2
	printf '  %s\n' "${image_candidates[@]}" >&2
	exit 1
fi
final_package="$(dpkg-deb -f "${image_candidates[0]}" Package)"
FINAL_KREL="${final_package#linux-image-}"
if [ -z "$FINAL_KREL" ] || [ "$FINAL_KREL" = "$final_package" ]; then
	echo "could not derive final kernel release from ${image_candidates[0]}" >&2
	exit 1
fi
if [ "$KREL" != "$FINAL_KREL" ]; then
	echo "kbuild finalized kernel release after packaging: $KREL -> $FINAL_KREL"
	KREL="$FINAL_KREL"
fi

manifest="$OUT_DIR/manifest-$KREL.txt"
: > "$manifest"
{
	echo "kernel_release=$KREL"
	echo "package_version=$PKG_VERSION"
	echo "kernel_profile=$KERNEL_PROFILE"
	echo "source_remote_url=$SOURCE_REMOTE_URL"
	echo "source_ref=$SOURCE_REF"
	echo "source_commit=${SOURCE_COMMIT:-$(git -C "$LINUX_SRC" rev-parse HEAD)}"
	echo "build_key=$BUILD_KEY"
	echo "linux_src=$LINUX_SRC"
	echo "build_dir=$BUILD_DIR"
	git -C "$LINUX_SRC" rev-parse HEAD 2>/dev/null | sed 's/^/git_head=/'
	git -C "$LINUX_SRC" status --short --branch 2>/dev/null | sed 's/^/git_status=/'
} >> "$manifest"

packages=()
add_packages() {
	local pattern="$1"
	local pkg

	while IFS= read -r pkg; do
		[ -n "$pkg" ] && packages+=("$pkg")
	done < <(compgen -G "$pattern" | sort)
}

add_packages "$(dirname "$BUILD_DIR")/linux-image-${KREL}_*.deb"
add_packages "$(dirname "$BUILD_DIR")/linux-headers-${KREL}_*.deb"
add_packages "$(dirname "$BUILD_DIR")/linux-libc-dev_${PKG_VERSION}_${KBUILD_DEBARCH}.deb"
add_packages "$(dirname "$LINUX_SRC")/linux-image-${KREL}_*.deb"
add_packages "$(dirname "$LINUX_SRC")/linux-headers-${KREL}_*.deb"
add_packages "$(dirname "$LINUX_SRC")/linux-libc-dev_${PKG_VERSION}_${KBUILD_DEBARCH}.deb"

deduped=()
for pkg in "${packages[@]}"; do
	seen=0
	for existing in "${deduped[@]}"; do
		if [ "$pkg" = "$existing" ]; then
			seen=1
			break
		fi
	done
	if [ "$seen" = "0" ]; then
		deduped+=("$pkg")
	fi
done
packages=("${deduped[@]}")

if [ "${#packages[@]}" -eq 0 ]; then
	echo "no generated .deb packages found for $KREL / $PKG_VERSION" >&2
	exit 1
fi

for pkg in "${packages[@]}"; do
	cp -av "$pkg" "$OUT_DIR"/
done

cp -av "$CONFIG" "$OUT_DIR/config-$KREL"
(cd "$OUT_DIR" && sha256sum ./*.deb > SHA256SUMS)

echo "built packages:"
printf '  %s\n' "$OUT_DIR"/*.deb
echo "manifest: $manifest"
