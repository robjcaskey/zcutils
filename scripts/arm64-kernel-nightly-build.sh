#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ACTION="${1:-build}"
KERNEL_REMOTE_URL="${KERNEL_REMOTE_URL:-https://git.kernel.org/pub/scm/linux/kernel/git/axboe/linux.git}"
KERNEL_REF="${KERNEL_REF:-refs/heads/for-next}"
CACHE_ROOT="${CACHE_ROOT:-/home/rob/dev-workspace/cache/linux-arm64-nightly}"
ARTIFACT_ROOT="${ARTIFACT_ROOT:-$REPO_ROOT/qemu-zcrx/arm64-kernel-nightly}"
JOBS="${JOBS:-16}"
CROSS_COMPILE="${CROSS_COMPILE:-aarch64-linux-gnu-}"
BUILD_POLICY="$REPO_ROOT/scripts/ec2-graviton-kernel-build.sh"

die() {
	echo "arm64-kernel-nightly: $*" >&2
	exit 1
}

need() {
	command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

case "$ACTION" in
	resolve | status | build) ;;
	*) die "usage: $0 [resolve|status|build]" ;;
esac

for cmd in git sha256sum sed; do
	need "$cmd"
done
[ -x "$BUILD_POLICY" ] || die "build policy is not executable: $BUILD_POLICY"

resolved="$(git ls-remote "$KERNEL_REMOTE_URL" "$KERNEL_REF" | awk 'NR == 1 { print $1 }')"
[[ "$resolved" =~ ^[0-9a-f]{40}$ ]] || die "could not resolve $KERNEL_REF from $KERNEL_REMOTE_URL"

remote_id="$(printf '%s' "$KERNEL_REMOTE_URL" | sha256sum | cut -c1-16)"
remote_slug="$(basename "$KERNEL_REMOTE_URL" .git | sed -E 's/[^A-Za-z0-9._-]+/-/g')-$remote_id"
ref_slug="$(printf '%s' "$KERNEL_REF" | sed -E 's#^refs/(heads|tags)/##; s#[^A-Za-z0-9._-]+#-#g')"
short="${resolved:0:12}"
policy_hash="$(sha256sum "$BUILD_POLICY" "$0" | sha256sum | awk '{print $1}')"
build_key="$(printf '%s\n' \
	"source_commit=$resolved" \
	"source_remote_url=$KERNEL_REMOTE_URL" \
	"source_ref=$KERNEL_REF" \
	"kernel_profile=nightly" \
	"arch=arm64" \
	"cross_compile=$CROSS_COMPILE" \
	"policy_hash=$policy_hash" | sha256sum | awk '{print $1}')"
artifact_dir="$ARTIFACT_ROOT/$remote_slug/$ref_slug/${short}-${build_key:0:12}"
env_file="$artifact_dir/nightly.env"

print_resolution() {
	printf 'kernel_remote_url=%s\n' "$KERNEL_REMOTE_URL"
	printf 'kernel_ref=%s\n' "$KERNEL_REF"
	printf 'source_commit=%s\n' "$resolved"
	printf 'build_key=%s\n' "$build_key"
	printf 'artifact_dir=%s\n' "$artifact_dir"
}

cache_valid() {
	[ -f "$artifact_dir/COMPLETE" ] || return 1
	[ -f "$env_file" ] || return 1
	[ -f "$artifact_dir/SHA256SUMS" ] || return 1
	grep -Fqx "SOURCE_COMMIT=$resolved" "$env_file" || return 1
	grep -Fqx "BUILD_KEY=$build_key" "$env_file" || return 1
	(cd "$artifact_dir" && sha256sum -c SHA256SUMS >/dev/null) || return 1
	compgen -G "$artifact_dir/linux-image-*.deb" >/dev/null || return 1
}

print_resolution
if [ "$ACTION" = resolve ]; then
	exit 0
fi
if cache_valid; then
	echo "action=reuse"
	exit 0
fi
echo "action=build"
[ "$ACTION" = status ] && exit 1

need flock
need "${CROSS_COMPILE}gcc"
mkdir -p "$CACHE_ROOT/git" "$CACHE_ROOT/build" "$CACHE_ROOT/worktrees" "$ARTIFACT_ROOT"
exec 9>"$CACHE_ROOT/build-$build_key.lock"
flock 9
if cache_valid; then
	echo "action=reuse-after-lock"
	exit 0
fi

mirror="$CACHE_ROOT/git/$remote_id.git"
if [ ! -d "$mirror" ]; then
	git init --bare "$mirror"
fi
git --git-dir="$mirror" config remote.origin.url "$KERNEL_REMOTE_URL"
git --git-dir="$mirror" fetch --force --depth=1 origin "$KERNEL_REF:refs/zcutils/nightly"
fetched="$(git --git-dir="$mirror" rev-parse refs/zcutils/nightly)"
[ "$fetched" = "$resolved" ] || die "branch moved while fetching ($resolved -> $fetched); retry to build the new exact hash"

worktree="$CACHE_ROOT/worktrees/$resolved"
git --git-dir="$mirror" worktree prune
if [ -d "$worktree/.git" ] || [ -f "$worktree/.git" ]; then
	[ "$(git -C "$worktree" rev-parse HEAD)" = "$resolved" ] || die "owned worktree has unexpected HEAD: $worktree"
else
	[ ! -e "$worktree" ] || die "worktree path exists but is not a git worktree: $worktree"
	git --git-dir="$mirror" worktree add --detach "$worktree" "$resolved"
fi

build_dir="$CACHE_ROOT/build/$build_key"
mkdir -p "$artifact_dir"
suffix="-zcnext-$short-b${build_key:0:8}"
pkg_version="1.0~zcnext.${short}.${build_key:0:12}.1"

KERNEL_PROFILE=nightly \
LINUX_SRC="$worktree" \
BUILD_DIR="$build_dir" \
OUT_DIR="$artifact_dir" \
JOBS="$JOBS" \
KERNEL_SUFFIX="$suffix" \
PKG_VERSION="$pkg_version" \
CROSS_COMPILE="$CROSS_COMPILE" \
EXPECTED_BRANCH= \
SOURCE_REMOTE_URL="$KERNEL_REMOTE_URL" \
SOURCE_REF="$KERNEL_REF" \
SOURCE_COMMIT="$resolved" \
BUILD_KEY="$build_key" \
	"$BUILD_POLICY"

kernel_release="$(sed -n 's/^kernel_release=//p' "$artifact_dir"/manifest-*.txt | head -n1)"
[ -n "$kernel_release" ] || die "build manifest did not report kernel_release"

tmp_env="$env_file.tmp.$$"
{
	printf 'KERNEL_REMOTE_URL=%s\n' "$KERNEL_REMOTE_URL"
	printf 'KERNEL_REF=%s\n' "$KERNEL_REF"
	printf 'SOURCE_COMMIT=%s\n' "$resolved"
	printf 'BUILD_KEY=%s\n' "$build_key"
	printf 'POLICY_HASH=%s\n' "$policy_hash"
	printf 'KERNEL_RELEASE=%s\n' "$kernel_release"
	printf 'KERNEL_SUFFIX=%s\n' "$suffix"
	printf 'PACKAGE_VERSION=%s\n' "$pkg_version"
	printf 'BUILT_AT=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$tmp_env"
mv "$tmp_env" "$env_file"
(cd "$artifact_dir" && sha256sum ./*.deb config-* manifest-*.txt nightly.env > SHA256SUMS)
printf 'source_commit=%s\nbuild_key=%s\n' "$resolved" "$build_key" > "$artifact_dir/COMPLETE"
cache_valid || die "new artifact failed its own cache validation"
echo "action=built"
