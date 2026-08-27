#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

need_value()
{
	local name="$1" value="$2"
	[ -n "$value" ] || {
		echo "$name is required" >&2
		exit 2
	}
}

module_file="${MODULE_FILE:-}"
kernel_release="${KERNEL_RELEASE:-}"
module_arch="${MODULE_ARCH:-$(uname -m)}"
image="${IMAGE:-}"
base_image="${BASE_IMAGE:-public.ecr.aws/amazonlinux/amazonlinux:2023}"
container_tool="${CONTAINER_TOOL:-}"

need_value MODULE_FILE "$module_file"
need_value KERNEL_RELEASE "$kernel_release"
need_value IMAGE "$image"
if [ -z "$container_tool" ]; then
	if command -v docker >/dev/null 2>&1; then
		container_tool=docker
	elif command -v podman >/dev/null 2>&1; then
		container_tool=podman
	else
		echo "docker or podman is required" >&2
		exit 127
	fi
fi
command -v "$container_tool" >/dev/null 2>&1 || {
	echo "CONTAINER_TOOL is not executable: $container_tool" >&2
	exit 127
}
[ -r "$module_file" ] || {
	echo "MODULE_FILE is not readable: $module_file" >&2
	exit 2
}
case "$module_arch" in
	x86_64) image_platform="linux/amd64" ;;
	aarch64) image_platform="linux/arm64" ;;
	*)
		echo "MODULE_ARCH must be x86_64 or aarch64, got: $module_arch" >&2
		exit 2
		;;
esac
case "$kernel_release" in
	*[!A-Za-z0-9._+-]*)
		echo "KERNEL_RELEASE contains unsafe characters: $kernel_release" >&2
		exit 2
		;;
esac

build_context="$(mktemp -d /tmp/zcnblk-kmod-image.XXXXXX)"
trap 'rm -rf "$build_context"' EXIT
cp "$script_dir/Dockerfile.kmod" "$build_context/Dockerfile"
cp "$module_file" "$build_context/zcnblk_client_mod.ko"

"$container_tool" build \
	--platform "$image_platform" \
	--build-arg "BASE_IMAGE=$base_image" \
	--build-arg "MODULE_ARCH=$module_arch" \
	--build-arg "KERNEL_RELEASE=$kernel_release" \
	--tag "$image" \
	"$build_context"

digest="$(sha256sum "$module_file" | awk 'NR == 1 {print $1}')"
printf 'ZCNBLK_KMOD_IMAGE_READY image=%s arch=%s kernel=%s module_sha256=%s\n' \
	"$image" "$module_arch" "$kernel_release" "$digest"
