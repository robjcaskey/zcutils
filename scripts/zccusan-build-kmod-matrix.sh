#!/usr/bin/env bash
# shellcheck disable=SC2016 # Container-side scripts intentionally expand there.
set -euo pipefail

usage()
{
	cat <<'EOF'
Build zcnblk_client_mod.ko for the exact, researched kernel ABI matrix.

Usage:
  scripts/zccusan-build-kmod-matrix.sh --list
  scripts/zccusan-build-kmod-matrix.sh --target TARGET [options]
  scripts/zccusan-build-kmod-matrix.sh --all [options]

Options:
  --target TARGET       Build one target from kmods/matrix/targets.json
  --all                 Build every pinned target, sequentially
  --list                List pinned targets without building
  --output DIRECTORY    Artifact root (default: ./dist/zccusan-kmod-matrix)
  --cache DIRECTORY     Download/toolchain cache (default: ./.cache/zccusan-kmod-matrix)
  --engine NAME         docker or podman (default: docker if present, else podman)
  --podman-root PATH    Optional Podman graph root (requires --engine podman)
  --podman-runroot PATH Optional Podman run root (requires --engine podman)
  --replace             Replace the exact destination for a completed target
  -h, --help            Show this help

The script creates no VM, cluster, or cloud resource. Arm container targets use
the local engine's binfmt/QEMU support; the GKE Arm target cross-compiles with
Google's published COS toolchain. Versions and research sources are locked in
kmods/matrix/targets.json.
EOF
}

die()
{
	printf 'zccusan-kmod-matrix: ERROR: %s\n' "$*" >&2
	exit 1
}

log()
{
	printf 'zccusan-kmod-matrix: %s\n' "$*" >&2
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lock_file="$repo_root/kmods/matrix/targets.json"
module_source="$repo_root/kmods/zcnblk_client_mod.c"
abi_header="$repo_root/kmods/zcnblk_shm_abi.h"
module_makefile="$repo_root/zccusan/charts/zcblock-csi/files/kmod/Makefile"
module_kbuild="$repo_root/zccusan/deploy/zcblock-csi/zcnblk-client-only.kbuild"
cos_containerfile="$repo_root/kmods/matrix/cos-kernel-devenv.Containerfile"
cos_arm_patch="$repo_root/kmods/matrix/cos-kernel-devenv-arm-bucket.patch"
nixos_expression="$repo_root/kmods/matrix/nixos-module.nix"

output_root="$repo_root/dist/zccusan-kmod-matrix"
cache_root="$repo_root/.cache/zccusan-kmod-matrix"
engine_name=""
podman_root=""
podman_runroot=""
replace=false
list_only=false
build_all=false
target=""

while [ "$#" -gt 0 ]; do
	case "$1" in
	--target)
		[ "$#" -ge 2 ] || die "--target requires a value"
		target="$2"
		shift 2
		;;
	--all)
		build_all=true
		shift
		;;
	--list)
		list_only=true
		shift
		;;
	--output)
		[ "$#" -ge 2 ] || die "--output requires a value"
		output_root="$2"
		shift 2
		;;
	--cache)
		[ "$#" -ge 2 ] || die "--cache requires a value"
		cache_root="$2"
		shift 2
		;;
	--engine)
		[ "$#" -ge 2 ] || die "--engine requires a value"
		engine_name="$2"
		shift 2
		;;
	--podman-root)
		[ "$#" -ge 2 ] || die "--podman-root requires a value"
		podman_root="$2"
		shift 2
		;;
	--podman-runroot)
		[ "$#" -ge 2 ] || die "--podman-runroot requires a value"
		podman_runroot="$2"
		shift 2
		;;
	--replace)
		replace=true
		shift
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

for required_file in \
	"$lock_file" \
	"$module_source" \
	"$abi_header" \
	"$module_makefile" \
	"$module_kbuild" \
	"$cos_containerfile" \
	"$cos_arm_patch" \
	"$nixos_expression"; do
	[ -r "$required_file" ] || die "required file is not readable: $required_file"
done
for command_name in jq git curl sha256sum install mktemp; do
	command -v "$command_name" >/dev/null 2>&1 || die "$command_name is required"
done
jq -e '.schemaVersion == 1 and (.targets | type == "object")' "$lock_file" >/dev/null || \
	die "invalid target lock: $lock_file"

if "$list_only"; then
	jq -r '.targets | to_entries[] | [.key, .value.architecture, .value.kernelRelease, .value.distribution] | @tsv' \
		"$lock_file"
	exit 0
fi
if "$build_all" && [ -n "$target" ]; then
	die "choose either --all or --target"
fi
if ! "$build_all" && [ -z "$target" ]; then
	die "one of --target, --all, or --list is required"
fi

if [ -z "$engine_name" ]; then
	if command -v docker >/dev/null 2>&1; then
		engine_name=docker
	elif command -v podman >/dev/null 2>&1; then
		engine_name=podman
	else
		die "docker or podman is required"
	fi
fi
case "$engine_name" in
docker|podman) ;;
*) die "--engine must be docker or podman" ;;
esac
command -v "$engine_name" >/dev/null 2>&1 || die "$engine_name is not installed"
if [ "$engine_name" != podman ] && { [ -n "$podman_root" ] || [ -n "$podman_runroot" ]; }; then
	die "Podman storage options require --engine podman"
fi

mkdir -p "$output_root" "$cache_root"
output_root="$(cd "$output_root" && pwd -P)"
cache_root="$(cd "$cache_root" && pwd -P)"

active_build_root=""
cleanup()
{
	if [ -n "$active_build_root" ] && [ -d "$active_build_root" ]; then
		rm -rf -- "$active_build_root"
	fi
}
trap cleanup EXIT

engine=("$engine_name")
if [ -n "$podman_root" ]; then
	engine+=(--root "$podman_root")
fi
if [ -n "$podman_runroot" ]; then
	engine+=(--runroot "$podman_runroot")
fi

container_run()
{
	local platform="$1"
	shift
	local options=(run --rm --platform "$platform")
	if [ "$engine_name" = podman ]; then
		options+=(--security-opt label=disable)
	fi
	"${engine[@]}" "${options[@]}" "$@"
}

json_value()
{
	local target_name="$1"
	local field="$2"
	jq -er --arg target "$target_name" --arg field "$field" \
		'.targets[$target][$field] // empty' "$lock_file"
}

json_optional()
{
	local target_name="$1"
	local field="$2"
	jq -r --arg target "$target_name" --arg field "$field" \
		'.targets[$target][$field] // ""' "$lock_file"
}

prepare_source()
{
	local source_dir="$1"
	mkdir -p "$source_dir"
	install -m 0644 "$module_source" "$source_dir/zcnblk_client_mod.c"
	install -m 0644 "$abi_header" "$source_dir/zcnblk_shm_abi.h"
	install -m 0644 "$module_makefile" "$source_dir/Makefile"
	install -m 0644 "$module_kbuild" "$source_dir/Kbuild"
}

build_amazon_linux()
{
	local platform="$1" image="$2" kernel_release="$3" kernel_package="$4"
	local source_dir="$5" stage_dir="$6"
	container_run "$platform" \
		-e KERNEL_RELEASE="$kernel_release" \
		-e KERNEL_PACKAGE="$kernel_package" \
		-v "$source_dir:/src" -v "$stage_dir:/out" \
		"$image" bash -lc '
set -euo pipefail
dnf -q -y install gcc make elfutils-libelf-devel kmod "$KERNEL_PACKAGE"
make -C "/usr/src/kernels/$KERNEL_RELEASE" M=/src clean modules
install -m 0644 /src/zcnblk_client_mod.ko /out/zcnblk_client_mod.ko
modinfo -F name /out/zcnblk_client_mod.ko > /out/module-name.txt
modinfo -F vermagic /out/zcnblk_client_mod.ko > /out/vermagic.txt
rpm -q gcc make elfutils-libelf-devel kmod "$KERNEL_PACKAGE" > /out/build-environment.txt
'
}

build_apt_family()
{
	local platform="$1" image="$2" kernel_release="$3" kernel_package="$4"
	local kernel_package_version="$5" source_dir="$6" stage_dir="$7"
	container_run "$platform" \
		-e DEBIAN_FRONTEND=noninteractive \
		-e KERNEL_RELEASE="$kernel_release" \
		-e KERNEL_PACKAGE="$kernel_package" \
		-e KERNEL_PACKAGE_VERSION="$kernel_package_version" \
		-v "$source_dir:/src" -v "$stage_dir:/out" \
		"$image" bash -lc '
set -euo pipefail
apt-get -qq update
apt-get -qq -y install gcc make kmod libelf-dev "${KERNEL_PACKAGE}=${KERNEL_PACKAGE_VERSION}"
make -C "/usr/src/linux-headers-$KERNEL_RELEASE" M=/src clean modules
install -m 0644 /src/zcnblk_client_mod.ko /out/zcnblk_client_mod.ko
modinfo -F name /out/zcnblk_client_mod.ko > /out/module-name.txt
modinfo -F vermagic /out/zcnblk_client_mod.ko > /out/vermagic.txt
dpkg-query -W gcc make kmod libelf-dev "$KERNEL_PACKAGE" > /out/build-environment.txt
'
}

verify_centos_key()
{
	local key_file="$1" expected_fingerprint="$2"
	command -v gpg >/dev/null 2>&1 || die "gpg is required for the UBI target"
	local actual_fingerprint
	actual_fingerprint="$(gpg --batch --show-keys --with-colons "$key_file" | \
		awk -F: '$1 == "fpr" {print $10; exit}')"
	[ "$actual_fingerprint" = "$expected_fingerprint" ] || \
		die "CentOS key fingerprint mismatch: $actual_fingerprint"
}

build_ubi_el()
{
	local platform="$1" image="$2" kernel_release="$3" kernel_package="$4"
	local baseos="$5" appstream="$6" key_url="$7" key_fingerprint="$8"
	local source_dir="$9" stage_dir="${10}"
	local key_dir="$cache_root/centos-keys"
	local key_file="$key_dir/RPM-GPG-KEY-CentOS-Official-SHA256"
	mkdir -p "$key_dir"
	if [ ! -s "$key_file" ]; then
		curl --fail --location --silent --show-error "$key_url" --output "$key_file"
	fi
	verify_centos_key "$key_file" "$key_fingerprint"
	container_run "$platform" \
		-e KERNEL_RELEASE="$kernel_release" \
		-e KERNEL_PACKAGE="$kernel_package" \
		-e CENTOS_BASEOS="$baseos" \
		-e CENTOS_APPSTREAM="$appstream" \
		-v "$key_file:/tmp/RPM-GPG-KEY-CentOS-Official-SHA256:ro" \
		-v "$source_dir:/src" -v "$stage_dir:/out" \
		"$image" bash -lc '
set -euo pipefail
dnf -q -y install gcc make elfutils-libelf-devel kmod
rpm --import /tmp/RPM-GPG-KEY-CentOS-Official-SHA256
dnf -q -y \
  --repofrompath="centos-baseos,$CENTOS_BASEOS" \
  --repofrompath="centos-appstream,$CENTOS_APPSTREAM" \
  --setopt=centos-baseos.gpgcheck=1 \
  --setopt=centos-baseos.gpgkey=file:///tmp/RPM-GPG-KEY-CentOS-Official-SHA256 \
  --setopt=centos-appstream.gpgcheck=1 \
  --setopt=centos-appstream.gpgkey=file:///tmp/RPM-GPG-KEY-CentOS-Official-SHA256 \
  install "$KERNEL_PACKAGE"
make -C "/usr/src/kernels/$KERNEL_RELEASE" M=/src clean modules
install -m 0644 /src/zcnblk_client_mod.ko /out/zcnblk_client_mod.ko
modinfo -F name /out/zcnblk_client_mod.ko > /out/module-name.txt
modinfo -F vermagic /out/zcnblk_client_mod.ko > /out/vermagic.txt
rpm -q gcc make elfutils-libelf-devel kmod "$KERNEL_PACKAGE" > /out/build-environment.txt
'
}

prepare_cos_devenv_image()
{
	local revision="$1" source_url="$2" base_image="$3" build_root="$4"
	local checkout="$build_root/cos-tools"
	local image_tag="localhost/zc-cos-kernel-devenv:${revision:0:12}-zc1"
	if ! "${engine[@]}" image inspect "$image_tag" >/dev/null 2>&1; then
		git clone -q "$source_url" "$checkout"
		git -C "$checkout" checkout -q --detach "$revision"
		[ "$(git -C "$checkout" rev-parse HEAD)" = "$revision" ] || \
			die "COS tools checkout did not resolve to $revision"
		git -C "$checkout" apply "$cos_arm_patch"
		"${engine[@]}" build \
			--build-arg COS_DEVENV_BASE="$base_image" \
			--tag "$image_tag" \
			--file "$cos_containerfile" \
			"$checkout/src/cmd/cos_kernel_devenv" >&2
	fi
	printf '%s\n' "$image_tag"
}

build_cos()
{
	local architecture="$1" kernel_release="$2" cos_release="$3" cos_bucket="$4"
	local revision="$5" source_url="$6" base_image="$7" source_dir="$8"
	local stage_dir="$9" build_root="${10}"
	local image_tag cos_arch cos_cache
	image_tag="$(prepare_cos_devenv_image "$revision" "$source_url" "$base_image" "$build_root")"
	case "$architecture" in
	amd64) cos_arch=x86_64 ;;
	arm64) cos_arch=arm64 ;;
	*) die "unsupported COS architecture: $architecture" ;;
	esac
	cos_cache="$cache_root/cos-build/$architecture/$cos_release"
	mkdir -p "$cos_cache"
	# The gcloud multiprocessing coordinator can strand a fully downloaded
	# sliced object under rootless Podman/QEMU. One stream is slower but makes
	# the pinned toolchain fetch deterministic across Docker and Podman CI.
	if [ "$architecture" = amd64 ]; then
		container_run linux/amd64 \
			-e CLOUDSDK_AUTH_DISABLE_CREDENTIALS=true \
			-e CLOUDSDK_STORAGE_SLICED_OBJECT_DOWNLOAD_THRESHOLD=0 \
			-v "$cos_cache:/build" -v "$source_dir:/src" -w /src \
			"$image_tag" -m -A "$cos_arch" -R "$cos_release"
	else
		container_run linux/amd64 \
			-e CLOUDSDK_AUTH_DISABLE_CREDENTIALS=true \
			-e CLOUDSDK_STORAGE_SLICED_OBJECT_DOWNLOAD_THRESHOLD=0 \
			-v "$cos_cache:/build" -v "$source_dir:/src" -w /src \
			"$image_tag" -m -A "$cos_arch" -G "$cos_bucket"
	fi
	container_run linux/amd64 \
		--entrypoint /bin/bash \
		-e KERNEL_RELEASE="$kernel_release" \
		-v "$source_dir:/src:ro" -v "$stage_dir:/out" \
		"$image_tag" -lc '
set -euo pipefail
install -m 0644 /src/zcnblk_client_mod.ko /out/zcnblk_client_mod.ko
modinfo -F name /out/zcnblk_client_mod.ko > /out/module-name.txt
modinfo -F vermagic /out/zcnblk_client_mod.ko > /out/vermagic.txt
printf "cos-kernel-release=%s\n" "$KERNEL_RELEASE" > /out/build-environment.txt
'
}

prepare_nixpkgs()
{
	local revision="$1" url="$2" expected_sha256="$3"
	local archive_dir="$cache_root/nixpkgs-archives"
	local archive="$archive_dir/$revision.tar.xz"
	local unpacked="$cache_root/nixpkgs-$revision"
	mkdir -p "$archive_dir"
	if [ ! -s "$archive" ]; then
		curl --fail --location --silent --show-error "$url" --output "$archive"
	fi
	printf '%s  %s\n' "$expected_sha256" "$archive" | sha256sum -c - >/dev/null
	if [ ! -r "$unpacked/default.nix" ]; then
		local temporary_unpack
		temporary_unpack="$(mktemp -d "$cache_root/.nixpkgs-$revision.XXXXXX")"
		tar -xJf "$archive" -C "$temporary_unpack" --strip-components=1
		mv "$temporary_unpack" "$unpacked"
	fi
	printf '%s\n' "$unpacked"
}

build_nixos()
{
	local platform="$1" image="$2" kernel_release="$3" revision="$4"
	local url="$5" expected_sha256="$6" source_dir="$7" stage_dir="$8"
	local nixpkgs_path
	nixpkgs_path="$(prepare_nixpkgs "$revision" "$url" "$expected_sha256")"
	container_run "$platform" \
		-e NIX_CONFIG='sandbox = false' \
		-e KERNEL_RELEASE="$kernel_release" \
		-v "$nixpkgs_path:/nixpkgs:ro" \
		-v "$source_dir:/src:ro" \
		-v "$nixos_expression:/module.nix:ro" \
		-v "$stage_dir:/out" \
		"$image" bash -lc '
set -euo pipefail
result="$(nix-build /module.nix --no-out-link --option sandbox false \
  --arg nixpkgsPath /nixpkgs --arg moduleSource /src)"
install -m 0644 "$result/zcnblk_client_mod.ko" /out/zcnblk_client_mod.ko
install -m 0644 "$result/module-name.txt" /out/module-name.txt
install -m 0644 "$result/vermagic.txt" /out/vermagic.txt
printf "nixos-kernel-release=%s\n" "$KERNEL_RELEASE" > /out/build-environment.txt
'
}

publish_stage()
{
	local target_name="$1" architecture="$2" kernel_release="$3"
	local expected_vermagic_prefix="$4" stage_dir="$5"
	local destination="$output_root/$target_name/$architecture/$kernel_release"
	[ -r "$stage_dir/zcnblk_client_mod.ko" ] || die "target produced no module"
	[ "$(tr -d '\n' < "$stage_dir/module-name.txt")" = zcnblk_client_mod ] || \
		die "target produced a module with the wrong name"
	local vermagic
	vermagic="$(tr -d '\n' < "$stage_dir/vermagic.txt")"
	case "$vermagic" in
	"$expected_vermagic_prefix"*) ;;
	*) die "vermagic '$vermagic' does not start with '$expected_vermagic_prefix'" ;;
	esac
	if [ -e "$destination" ]; then
		if ! "$replace"; then
			die "destination exists (use --replace): $destination"
		fi
		rm -rf -- "$destination"
	fi
	mkdir -p "$destination"
	install -m 0644 "$stage_dir/zcnblk_client_mod.ko" "$destination/zcnblk_client_mod.ko"
	install -m 0644 "$stage_dir/build-environment.txt" "$destination/build-environment.txt"
	printf '%s\n' "$vermagic" > "$destination/vermagic.txt"
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
	jq --arg target "$target_name" '.targets[$target]' "$lock_file" \
		> "$destination/target-lock.json"
	local module_sha256 researched_at
	module_sha256="$(awk 'NR == 1 {print $1}' "$destination/zcnblk_client_mod.ko.sha256")"
	researched_at="$(jq -r '.researchedAt' "$lock_file")"
	{
		printf 'TARGET=%s\n' "$target_name"
		printf 'ARCHITECTURE=%s\n' "$architecture"
		printf 'KERNEL_RELEASE=%s\n' "$kernel_release"
		printf 'MODULE_NAME=zcnblk_client_mod\n'
		printf 'MODULE_SHA256=%s\n' "$module_sha256"
		printf 'MODULE_VERMAGIC=%s\n' "$vermagic"
		printf 'VERSION_RESEARCHED_AT=%s\n' "$researched_at"
	} > "$destination/metadata.env"
	printf 'ZCCUSAN_KMOD_MATRIX_READY target=%s arch=%s kernel=%s sha256=%s directory=%s\n' \
		"$target_name" "$architecture" "$kernel_release" "$module_sha256" "$destination"
}

build_target()
{
	local target_name="$1"
	case "$target_name" in
	*[!a-z0-9._-]*|'') die "invalid target identifier: $target_name" ;;
	esac
	jq -e --arg target "$target_name" '.targets[$target] != null' "$lock_file" >/dev/null || \
		die "unknown target: $target_name"
	local family architecture platform image kernel_release kernel_package
	local kernel_package_version expected_vermagic_prefix
	family="$(json_value "$target_name" family)"
	architecture="$(json_value "$target_name" architecture)"
	platform="$(json_value "$target_name" platform)"
	image="$(json_optional "$target_name" containerImage)"
	kernel_release="$(json_value "$target_name" kernelRelease)"
	kernel_package="$(json_optional "$target_name" kernelPackage)"
	kernel_package_version="$(json_optional "$target_name" kernelPackageVersion)"
	expected_vermagic_prefix="$(json_value "$target_name" expectedVermagicPrefix)"
	case "$architecture" in
	amd64|arm64) ;;
	*) die "unsupported target architecture: $architecture" ;;
	esac
	case "$kernel_release" in
	*[!A-Za-z0-9._+-]*|'') die "unsafe kernel release in target lock: $kernel_release" ;;
	esac
	mkdir -p "$cache_root/tmp" "$output_root"
	local build_root source_dir stage_dir
	build_root="$(mktemp -d "$cache_root/tmp/$target_name.XXXXXX")"
	active_build_root="$build_root"
	source_dir="$build_root/source"
	stage_dir="$build_root/stage"
	prepare_source "$source_dir"
	mkdir -p "$stage_dir"
	log "building target=$target_name family=$family arch=$architecture kernel=$kernel_release"
	case "$family" in
	amazon-linux)
		build_amazon_linux "$platform" "$image" "$kernel_release" "$kernel_package" \
			"$source_dir" "$stage_dir"
		;;
	debian|ubuntu)
		build_apt_family "$platform" "$image" "$kernel_release" "$kernel_package" \
			"$kernel_package_version" "$source_dir" "$stage_dir"
		;;
	ubi-el)
		build_ubi_el "$platform" "$image" "$kernel_release" "$kernel_package" \
			"$(json_value "$target_name" centosBaseOs)" \
			"$(json_value "$target_name" centosAppStream)" \
			"$(json_value "$target_name" centosKey)" \
			"$(json_value "$target_name" centosKeyFingerprint)" \
			"$source_dir" "$stage_dir"
		;;
	cos)
		build_cos "$architecture" "$kernel_release" \
			"$(json_value "$target_name" cosRelease)" \
			"$(json_value "$target_name" cosBucket)" \
			"$(json_value "$target_name" cosToolRevision)" \
			"$(json_value "$target_name" cosToolSource)" \
			"$(json_value "$target_name" devenvBaseImage)" \
			"$source_dir" "$stage_dir" "$build_root"
		;;
	nixos)
		build_nixos "$platform" "$image" "$kernel_release" \
			"$(json_value "$target_name" nixpkgsRevision)" \
			"$(json_value "$target_name" nixpkgsUrl)" \
			"$(json_value "$target_name" nixpkgsSha256)" \
			"$source_dir" "$stage_dir"
		;;
	*) die "unsupported target family: $family" ;;
	esac
	publish_stage "$target_name" "$architecture" "$kernel_release" \
		"$expected_vermagic_prefix" "$stage_dir"
	rm -rf -- "$build_root"
	active_build_root=""
}

if "$build_all"; then
	mapfile -t targets < <(jq -r '.targets | keys[]' "$lock_file")
	for target_name in "${targets[@]}"; do
		build_target "$target_name"
	done
else
	build_target "$target"
fi
