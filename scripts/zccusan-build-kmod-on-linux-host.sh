#!/usr/bin/env bash
set -euo pipefail

usage()
{
	cat <<'EOF'
Build zcnblk_client_mod.ko against this Linux host's running kernel.

Usage:
  scripts/zccusan-build-kmod-on-linux-host.sh [options]

Options:
  --kernel-build-dir PATH  Prepared kernel tree (default: /lib/modules/%KERNEL_RELEASE%/build)
  --output DIRECTORY       Artifact root (default: ./dist/zccusan-kmods)
  -h, --help               Show this help

The host must already provide make, a C compiler, modinfo, sha256sum, and the
prepared external-module build tree for its exact running kernel. The script
does not install packages, load the module, or change the running host.
EOF
}

die()
{
	printf 'zccusan-kmod-build: ERROR: %s\n' "$*" >&2
	exit 1
}

kernel_build_template='/lib/modules/%KERNEL_RELEASE%/build'
output_root='./dist/zccusan-kmods'

while [ "$#" -gt 0 ]; do
	case "$1" in
	--kernel-build-dir)
		[ "$#" -ge 2 ] || die "--kernel-build-dir requires a value"
		kernel_build_template="$2"
		shift 2
		;;
	--output)
		[ "$#" -ge 2 ] || die "--output requires a value"
		output_root="$2"
		shift 2
		;;
	-h|--help)
		usage
		exit 0
		;;
	*)
		die "unknown argument: $1"
		;;
	esac
done

for command_name in make cc modinfo sha256sum install sed awk; do
	command -v "$command_name" >/dev/null 2>&1 || die "$command_name is required"
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
module_source="$repo_root/kmods/zcnblk_client_mod.c"
abi_header="$repo_root/kmods/zcnblk_shm_abi.h"
module_makefile="$repo_root/zccusan/charts/zcblock-csi/files/kmod/Makefile"
module_kbuild="$repo_root/zccusan/deploy/zcblock-csi/zcnblk-client-only.kbuild"
for source_file in "$module_source" "$abi_header" "$module_makefile" "$module_kbuild"; do
	[ -r "$source_file" ] || die "required source is not readable: $source_file"
done

kernel_release="$(uname -r)"
machine_arch="$(uname -m)"
case "$machine_arch" in
x86_64|aarch64) ;;
*) die "host architecture must be x86_64 or aarch64, got: $machine_arch" ;;
esac
kernel_build_dir="${kernel_build_template//%KERNEL_RELEASE%/$kernel_release}"
[ -r "$kernel_build_dir/Makefile" ] || \
	die "missing prepared kernel build tree: $kernel_build_dir/Makefile"

temporary_root="$(mktemp -d /tmp/zccusan-kmod-host-build.XXXXXX)"
cleanup()
{
	rm -rf "$temporary_root"
}
trap cleanup EXIT

install -m 0644 "$module_source" "$temporary_root/zcnblk_client_mod.c"
install -m 0644 "$abi_header" "$temporary_root/zcnblk_shm_abi.h"
install -m 0644 "$module_makefile" "$temporary_root/Makefile"
install -m 0644 "$module_kbuild" "$temporary_root/Kbuild"
make -C "$kernel_build_dir" M="$temporary_root" modules

module_file="$temporary_root/zcnblk_client_mod.ko"
[ "$(modinfo -F name "$module_file")" = zcnblk_client_mod ] || \
	die "built module has the wrong name"
module_vermagic="$(modinfo -F vermagic "$module_file")"
case "$module_vermagic" in
"$kernel_release "*) ;;
*) die "module vermagic does not start with running kernel: $module_vermagic" ;;
esac

destination="$output_root/$machine_arch/$kernel_release"
mkdir -p "$destination"
for artifact in \
	zcnblk_client_mod.ko \
	zcnblk_client_mod.ko.sha256 \
	metadata.env \
	build-environment.txt \
	source-files.sha256; do
	[ ! -e "$destination/$artifact" ] || \
		die "refusing to overwrite existing artifact: $destination/$artifact"
done

install -m 0644 "$module_file" "$destination/zcnblk_client_mod.ko"
(
	cd "$destination"
	sha256sum zcnblk_client_mod.ko > zcnblk_client_mod.ko.sha256
)
(
	cd "$repo_root"
	sha256sum \
		kmods/zcnblk_client_mod.c \
		kmods/zcnblk_shm_abi.h \
		zccusan/charts/zcblock-csi/files/kmod/Makefile \
		zccusan/deploy/zcblock-csi/zcnblk-client-only.kbuild \
		> "$destination/source-files.sha256"
)
module_sha256="$(awk 'NR == 1 {print $1}' "$destination/zcnblk_client_mod.ko.sha256")"
os_id=""
os_version=""
if [ -r /etc/os-release ]; then
	os_id="$(awk -F= '$1 == "ID" {gsub(/^"|"$/, "", $2); print $2; exit}' /etc/os-release)"
	os_version="$(awk -F= '$1 == "VERSION_ID" {gsub(/^"|"$/, "", $2); print $2; exit}' /etc/os-release)"
fi
{
	printf 'KERNEL_RELEASE=%s\n' "$kernel_release"
	printf 'MACHINE_ARCH=%s\n' "$machine_arch"
	printf 'OS_ID=%s\n' "$os_id"
	printf 'OS_VERSION_ID=%s\n' "$os_version"
	printf 'BUILD_PROFILE=local-host\n'
	printf 'MODULE_NAME=zcnblk_client_mod\n'
	printf 'MODULE_SHA256=%s\n' "$module_sha256"
	printf 'MODULE_VERMAGIC=%s\n' "$module_vermagic"
	printf 'KERNEL_BUILD_DIR=%s\n' "$kernel_build_dir"
} > "$destination/metadata.env"
{
	test -r /etc/os-release && cat /etc/os-release
	cc --version | sed -n '1p'
	make --version | sed -n '1p'
	modinfo --version | sed -n '1p'
} > "$destination/build-environment.txt"

printf 'ZCCUSAN_KMOD_BUILD_READY host=%s arch=%s kernel=%s profile=local-host sha256=%s directory=%s\n' \
	"$(hostname)" "$machine_arch" "$kernel_release" "$module_sha256" "$destination"
