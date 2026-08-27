#!/usr/bin/env bash
set -euo pipefail

usage()
{
	cat <<'EOF'
Validate every Kubernetes node against a published zccusan module profile,
then install the Helm chart with that immutable module image.

Usage:
  scripts/zccusan-install-kmod-fastpath.sh PROFILE [options] [-- HELM_ARGS...]

Options:
  --namespace NAMESPACE  Install namespace (default: zccusan)
  --release RELEASE      Helm release name (default: zccusan)
  --chart PATH           Chart path (default: repository chart)
  --preflight-only       Validate nodes and print the selected artifact only
  -h, --help             Show this help

The installer deliberately rejects mixed or nearby kernels. Extra arguments
after -- are passed to Helm, for example RDMA values or a values file.
EOF
}

die()
{
	printf 'zccusan-kmod-install: ERROR: %s\n' "$*" >&2
	exit 1
}

[ "$#" -gt 0 ] || {
	usage >&2
	exit 2
}
case "$1" in
-h|--help)
	usage
	exit 0
	;;
esac

profile_name="$1"
shift
[[ "$profile_name" =~ ^[a-z0-9][a-z0-9._-]*$ ]] || \
	die "invalid profile name: $profile_name"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile_file="$repo_root/zccusan/deploy/zcblock-csi/kmod-profiles/${profile_name}.profile"
namespace=zccusan
release=zccusan
chart="$repo_root/zccusan/charts/zcblock-csi"
preflight_only=false
helm_args=()

while [ "$#" -gt 0 ]; do
	case "$1" in
	--namespace)
		[ "$#" -ge 2 ] || die "--namespace requires a value"
		namespace="$2"
		shift 2
		;;
	--release)
		[ "$#" -ge 2 ] || die "--release requires a value"
		release="$2"
		shift 2
		;;
	--chart)
		[ "$#" -ge 2 ] || die "--chart requires a value"
		chart="$2"
		shift 2
		;;
	--preflight-only)
		preflight_only=true
		shift
		;;
	--)
		shift
		helm_args=("$@")
		break
		;;
	-h|--help)
		usage
		exit 0
		;;
	*) die "unknown option before --: $1" ;;
	esac
done

[[ "$namespace" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || \
	die "invalid namespace: $namespace"
[[ "$release" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || \
	die "invalid Helm release: $release"
[ -r "$profile_file" ] || die "unknown or unreadable profile: $profile_name"
[ -d "$chart" ] || die "chart directory does not exist: $chart"

unset ZCCUSAN_KMOD_PROFILE_FORMAT ZCCUSAN_KMOD_PROFILE_NAME
unset ZCCUSAN_KMOD_OS_IMAGE_PATTERN
unset ZCCUSAN_KMOD_NODE_ARCH ZCCUSAN_KMOD_KERNEL_RELEASE
unset ZCCUSAN_KMOD_IMAGE_REPOSITORY ZCCUSAN_KMOD_IMAGE_DIGEST
unset ZCCUSAN_KMOD_MODULE_SHA256
# shellcheck source=/dev/null
source "$profile_file"

required_profile_fields=(
	ZCCUSAN_KMOD_PROFILE_FORMAT
	ZCCUSAN_KMOD_PROFILE_NAME
	ZCCUSAN_KMOD_OS_IMAGE_PATTERN
	ZCCUSAN_KMOD_NODE_ARCH
	ZCCUSAN_KMOD_KERNEL_RELEASE
	ZCCUSAN_KMOD_IMAGE_REPOSITORY
	ZCCUSAN_KMOD_IMAGE_DIGEST
	ZCCUSAN_KMOD_MODULE_SHA256
)
for field in "${required_profile_fields[@]}"; do
	[ -n "${!field:-}" ] || die "profile is missing $field"
done
[ "$ZCCUSAN_KMOD_PROFILE_FORMAT" = 1 ] || die "unsupported profile format"
[ "$ZCCUSAN_KMOD_PROFILE_NAME" = "$profile_name" ] || \
	die "profile name does not match its filename"
[[ "$ZCCUSAN_KMOD_IMAGE_REPOSITORY" =~ ^[A-Za-z0-9._/:+-]+$ ]] || \
	die "profile has an invalid image repository"
[[ "$ZCCUSAN_KMOD_IMAGE_DIGEST" =~ ^sha256:[A-Fa-f0-9]{64}$ ]] || \
	die "profile has an invalid OCI digest"
[[ "$ZCCUSAN_KMOD_MODULE_SHA256" =~ ^[A-Fa-f0-9]{64}$ ]] || \
	die "profile has an invalid module digest"

command -v kubectl >/dev/null 2>&1 || die "kubectl is required"
if [ "$preflight_only" != true ]; then
	command -v helm >/dev/null 2>&1 || die "Helm 3 is required"
fi

node_report="$(kubectl get nodes -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.status.nodeInfo.operatingSystem}{"\t"}{.status.nodeInfo.architecture}{"\t"}{.status.nodeInfo.kernelVersion}{"\t"}{.status.nodeInfo.osImage}{"\t"}{range .status.conditions[?(@.type=="Ready")]}{.status}{end}{"\n"}{end}')" || \
	die "could not read Kubernetes nodes"
[ -n "$node_report" ] || die "the cluster reports no nodes"

node_count=0
while IFS=$'\t' read -r node os architecture kernel os_image ready; do
	[ -n "$node" ] || continue
	node_count=$((node_count + 1))
	[ "$ready" = True ] || die "node $node is not Ready"
	[ "$os" = linux ] || die "node $node is not Linux: $os"
	[ "$architecture" = "$ZCCUSAN_KMOD_NODE_ARCH" ] || \
		die "node $node architecture $architecture does not match $ZCCUSAN_KMOD_NODE_ARCH"
	[ "$kernel" = "$ZCCUSAN_KMOD_KERNEL_RELEASE" ] || \
		die "node $node kernel $kernel does not exactly match $ZCCUSAN_KMOD_KERNEL_RELEASE"
	case "$os_image" in
	*"$ZCCUSAN_KMOD_OS_IMAGE_PATTERN"*) ;;
	*) die "node $node OS image '$os_image' does not match '$ZCCUSAN_KMOD_OS_IMAGE_PATTERN'" ;;
	esac
	printf 'zccusan-kmod-install: compatible node=%s arch=%s kernel=%s\n' \
		"$node" "$architecture" "$kernel"
done <<< "$node_report"
[ "$node_count" -gt 0 ] || die "the cluster reports no usable nodes"

printf 'ZCCUSAN_KMOD_PREFLIGHT_READY profile=%s nodes=%s image=%s@%s module_sha256=%s\n' \
	"$profile_name" "$node_count" "$ZCCUSAN_KMOD_IMAGE_REPOSITORY" \
	"$ZCCUSAN_KMOD_IMAGE_DIGEST" "$ZCCUSAN_KMOD_MODULE_SHA256"
[ "$preflight_only" != true ] || exit 0

helm upgrade --install "$release" "$chart" \
	--namespace "$namespace" \
	--create-namespace \
	--set-string "image.repository=$ZCCUSAN_KMOD_IMAGE_REPOSITORY" \
	--set-string "image.digest=$ZCCUSAN_KMOD_IMAGE_DIGEST" \
	--set-string 'image.pullPolicy=IfNotPresent' \
	--set-string 'nodeSetup.moduleSource.type=image' \
	--set-string "nodeSetup.moduleSource.sha256=$ZCCUSAN_KMOD_MODULE_SHA256" \
	"${helm_args[@]}" \
	--wait \
	--timeout 10m

printf 'ZCCUSAN_KMOD_INSTALL_READY profile=%s release=%s namespace=%s nodes=%s\n' \
	"$profile_name" "$release" "$namespace" "$node_count"
