#!/usr/bin/env bash
set -euo pipefail

usage()
{
	cat <<'EOF'
Package audited kernel-module matrix artifacts into one DaemonSet image root.

Usage:
  scripts/zccusan-package-daemonset-kmods.sh \
    --architecture amd64|arm64 --artifacts DIRECTORY --output DIRECTORY

The output layout uses uname-compatible architecture names:
  x86_64/KERNEL_RELEASE/zcnblk_client_mod.ko
  aarch64/KERNEL_RELEASE/zcnblk_client_mod.ko

Every artifact is checked against the current target lock, current source
hashes, declared SHA-256, ELF architecture, module name, and vermagic before it
is copied. The output directory must not already exist.
EOF
}

die()
{
	printf 'zccusan-daemonset-kmods: ERROR: %s\n' "$*" >&2
	exit 1
}

architecture=""
artifacts_root=""
output_root=""
while [ "$#" -gt 0 ]; do
	case "$1" in
	--architecture)
		[ "$#" -ge 2 ] || die "--architecture requires a value"
		architecture="$2"
		shift 2
		;;
	--artifacts)
		[ "$#" -ge 2 ] || die "--artifacts requires a value"
		artifacts_root="$2"
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
	*) die "unknown argument: $1" ;;
	esac
done

case "$architecture" in
amd64) uname_architecture=x86_64; expected_elf='Advanced Micro Devices X86-64' ;;
arm64) uname_architecture=aarch64; expected_elf='AArch64' ;;
*) die "--architecture must be amd64 or arm64" ;;
esac
[ -n "$artifacts_root" ] || die "--artifacts is required"
[ -n "$output_root" ] || die "--output is required"
[ -d "$artifacts_root" ] || die "artifact directory does not exist: $artifacts_root"
[ ! -e "$output_root" ] || die "output already exists: $output_root"
artifacts_root="$(cd "$artifacts_root" && pwd -P)"
case "$output_root" in
/*) ;;
*) output_root="$(pwd -P)/$output_root" ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lock_file="$repo_root/kmods/matrix/targets.json"
for command_name in jq sha256sum readelf find install mktemp; do
	command -v "$command_name" >/dev/null 2>&1 || die "$command_name is required"
done
[ -r "$lock_file" ] || die "target lock is not readable: $lock_file"

stage_parent="$(mktemp -d)"
cleanup()
{
	rm -rf -- "$stage_parent"
}
trap cleanup EXIT
stage_root="$stage_parent/bundle"
mkdir -p "$stage_root/$uname_architecture"
index_entries="$stage_parent/index.ndjson"
: > "$index_entries"
artifact_count=0

while IFS= read -r -d '' module; do
	artifact_dir="$(dirname "$module")"
	for required in \
		metadata.env source-files.sha256 target-lock.json vermagic.txt \
		zcnblk_client_mod.ko.sha256; do
		[ -s "$artifact_dir/$required" ] || die "missing $required beside $module"
	done

	target="$(awk -F= '$1 == "TARGET" {print substr($0, index($0, "=") + 1)}' \
		"$artifact_dir/metadata.env")"
	[ -n "$target" ] || die "metadata has no TARGET: $artifact_dir/metadata.env"
	locked_architecture="$(jq -er --arg target "$target" '.targets[$target].architecture' "$lock_file")" || \
		die "artifact target is absent from the current lock: $target"
	[ "$locked_architecture" = "$architecture" ] || continue

	artifact_count=$((artifact_count + 1))
	kernel="$(jq -er '.kernelRelease' "$artifact_dir/target-lock.json")"
	metadata_kernel="$(awk -F= '$1 == "KERNEL_RELEASE" {print substr($0, index($0, "=") + 1)}' \
		"$artifact_dir/metadata.env")"
	[ "$metadata_kernel" = "$kernel" ] || die "kernel metadata mismatch for $target"
	metadata_architecture="$(awk -F= '$1 == "ARCHITECTURE" {print substr($0, index($0, "=") + 1)}' \
		"$artifact_dir/metadata.env")"
	[ "$metadata_architecture" = "$architecture" ] || die "architecture metadata mismatch for $target"
	metadata_name="$(awk -F= '$1 == "MODULE_NAME" {print substr($0, index($0, "=") + 1)}' \
		"$artifact_dir/metadata.env")"
	[ "$metadata_name" = zcnblk_client_mod ] || die "unexpected module name for $target"

	expected_lock="$stage_parent/expected-$artifact_count.json"
	actual_lock="$stage_parent/actual-$artifact_count.json"
	jq -S --arg target "$target" '.targets[$target]' "$lock_file" > "$expected_lock"
	jq -S . "$artifact_dir/target-lock.json" > "$actual_lock"
	cmp "$expected_lock" "$actual_lock" || die "target lock drift for $target"

	(
		cd "$artifact_dir"
		sha256sum -c zcnblk_client_mod.ko.sha256
	)
	(
		cd "$repo_root"
		sha256sum -c "$artifact_dir/source-files.sha256"
	)

	module_sha256="$(awk 'NR == 1 {print $1}' "$artifact_dir/zcnblk_client_mod.ko.sha256")"
	metadata_sha256="$(awk -F= '$1 == "MODULE_SHA256" {print substr($0, index($0, "=") + 1)}' \
		"$artifact_dir/metadata.env")"
	[ "$metadata_sha256" = "$module_sha256" ] || die "module digest metadata mismatch for $target"
	elf_machine="$(readelf -h "$module" | awk -F: '$1 ~ /^[[:space:]]*Machine$/ {sub(/^[[:space:]]+/, "", $2); print $2}')"
	[ "$elf_machine" = "$expected_elf" ] || \
		die "ELF architecture mismatch for $target: $elf_machine"
	vermagic="$(tr -d '\n' < "$artifact_dir/vermagic.txt")"
	metadata_vermagic="$(awk -F= '$1 == "MODULE_VERMAGIC" {print substr($0, index($0, "=") + 1)}' \
		"$artifact_dir/metadata.env")"
	[ "$metadata_vermagic" = "$vermagic" ] || die "vermagic metadata mismatch for $target"
	vermagic_prefix="$(jq -er '.expectedVermagicPrefix' "$artifact_dir/target-lock.json")"
	case "$vermagic" in
	"$vermagic_prefix"*) ;;
	*) die "vermagic mismatch for $target: $vermagic" ;;
	esac

	destination="$stage_root/$uname_architecture/$kernel"
	if [ -e "$destination/zcnblk_client_mod.ko" ]; then
		existing_sha256="$(sha256sum "$destination/zcnblk_client_mod.ko" | awk '{print $1}')"
		[ "$existing_sha256" = "$module_sha256" ] || \
			die "two targets claim $architecture/$kernel with different modules"
	else
		mkdir -p "$destination"
		install -m 0644 "$module" "$destination/zcnblk_client_mod.ko"
		install -m 0644 "$artifact_dir/zcnblk_client_mod.ko.sha256" \
			"$destination/zcnblk_client_mod.ko.sha256"
	fi
	jq -cn \
		--arg target "$target" \
		--arg architecture "$architecture" \
		--arg unameArchitecture "$uname_architecture" \
		--arg kernelRelease "$kernel" \
		--arg sha256 "$module_sha256" \
		--arg vermagic "$vermagic" \
		'{target: $target, architecture: $architecture, unameArchitecture: $unameArchitecture,
		  kernelRelease: $kernelRelease, sha256: $sha256, vermagic: $vermagic}' \
		>> "$index_entries"
done < <(find "$artifacts_root" -type f -name zcnblk_client_mod.ko -print0 | sort -z)

[ "$artifact_count" -gt 0 ] || die "no $architecture module artifacts found under $artifacts_root"
expected_count="$(jq --arg architecture "$architecture" \
	'[.targets[] | select(.architecture == $architecture)] | length' "$lock_file")"
[ "$artifact_count" -eq "$expected_count" ] || \
	die "found $artifact_count $architecture artifacts, expected $expected_count from the target lock"
jq -s \
	--arg architecture "$architecture" \
	--arg unameArchitecture "$uname_architecture" \
	'{schemaVersion: 1, architecture: $architecture, unameArchitecture: $unameArchitecture,
	  modules: (sort_by(.kernelRelease, .target))}' \
	"$index_entries" > "$stage_root/index.json"
install -d -m 0755 "$(dirname "$output_root")"
mv "$stage_root" "$output_root"
printf 'ZCCUSAN_DAEMONSET_KMOD_BUNDLE_PASS arch=%s modules=%s output=%s\n' \
	"$architecture" "$artifact_count" "$output_root"
