#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

need_value()
{
	local name="$1" value="$2"
	[ -n "$value" ] || {
		printf '%s is required\n' "$name" >&2
		exit 2
	}
}

module_file="${MODULE_FILE:-}"
kernel_release="${KERNEL_RELEASE:-}"
module_arch="${MODULE_ARCH:-$(uname -m)}"
image="${IMAGE:-}"
container_tool="${CONTAINER_TOOL:-}"

need_value MODULE_FILE "$module_file"
need_value KERNEL_RELEASE "$kernel_release"
need_value IMAGE "$image"
for command_name in sha256sum readelf modinfo install mktemp awk jq; do
	command -v "$command_name" >/dev/null 2>&1 || {
		printf '%s is required\n' "$command_name" >&2
		exit 127
	}
done
if [ -z "$container_tool" ]; then
	if command -v docker >/dev/null 2>&1; then
		container_tool=docker
	elif command -v podman >/dev/null 2>&1; then
		container_tool=podman
	else
		printf 'docker or podman is required\n' >&2
		exit 127
	fi
fi
command -v "$container_tool" >/dev/null 2>&1 || {
	printf 'CONTAINER_TOOL is not executable: %s\n' "$container_tool" >&2
	exit 127
}
[ -r "$module_file" ] || {
	printf 'MODULE_FILE is not readable: %s\n' "$module_file" >&2
	exit 2
}
case "$module_arch" in
x86_64)
	image_platform=linux/amd64
	image_architecture=amd64
	expected_elf='Advanced Micro Devices X86-64'
	;;
aarch64)
	image_platform=linux/arm64
	image_architecture=arm64
	expected_elf=AArch64
	;;
*)
	printf 'MODULE_ARCH must be x86_64 or aarch64, got: %s\n' "$module_arch" >&2
	exit 2
	;;
esac
case "$kernel_release" in
*[!A-Za-z0-9._+-]*)
	printf 'KERNEL_RELEASE contains unsafe characters: %s\n' "$kernel_release" >&2
	exit 2
	;;
esac

[ "$(modinfo -F name "$module_file")" = zcnblk_client_mod ] || {
	printf 'MODULE_FILE is not zcnblk_client_mod\n' >&2
	exit 2
}
vermagic="$(modinfo -F vermagic "$module_file")"
case "$vermagic" in
"$kernel_release"|"$kernel_release "*) ;;
*)
	printf 'module vermagic does not match KERNEL_RELEASE: %s\n' "$vermagic" >&2
	exit 2
	;;
esac
elf_machine="$(readelf -h "$module_file" | \
	awk -F: '$1 ~ /^[[:space:]]*Machine$/ {sub(/^[[:space:]]+/, "", $2); print $2}')"
[ "$elf_machine" = "$expected_elf" ] || {
	printf 'module ELF architecture mismatch: %s\n' "$elf_machine" >&2
	exit 2
}

mkdir -p "$repo_root/dist"
bundle_root="$(mktemp -d "$repo_root/dist/zccusan-custom-daemonset-kmods.XXXXXX")"
cleanup()
{
	rm -rf -- "$bundle_root"
}
trap cleanup EXIT
destination="$bundle_root/$module_arch/$kernel_release"
install -d -m 0755 "$destination"
install -m 0644 "$module_file" "$destination/zcnblk_client_mod.ko"
(
	cd "$destination"
	sha256sum zcnblk_client_mod.ko > zcnblk_client_mod.ko.sha256
)
module_sha256="$(awk 'NR == 1 {print $1}' "$destination/zcnblk_client_mod.ko.sha256")"
jq -n \
	--arg architecture "$image_architecture" \
	--arg unameArchitecture "$module_arch" \
	--arg kernelRelease "$kernel_release" \
	--arg sha256 "$module_sha256" \
	--arg vermagic "$vermagic" \
	'{schemaVersion: 1, architecture: $architecture,
	  unameArchitecture: $unameArchitecture,
	  modules: [{target: "custom", architecture: $architecture,
	    unameArchitecture: $unameArchitecture, kernelRelease: $kernelRelease,
	    sha256: $sha256, vermagic: $vermagic}]}' \
	> "$bundle_root/index.json"
bundle_relative="${bundle_root#"$repo_root/"}"

"$container_tool" build \
	--platform "$image_platform" \
	--build-arg "KMOD_BUNDLE_ROOT=$bundle_relative" \
	--tag "$image" \
	--file "$repo_root/zccusan/deploy/zcblock-csi/Dockerfile" \
	"$repo_root"

printf 'ZCBLOCK_DAEMONSET_IMAGE_READY image=%s arch=%s kernel=%s module_sha256=%s\n' \
	"$image" "$module_arch" "$kernel_release" "$module_sha256"
