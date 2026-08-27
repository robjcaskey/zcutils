#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
chart="${ZCCUSAN_HELM_CHART:-$repo_root/zccusan/charts/zcblock-csi}"

need()
{
	command -v "$1" >/dev/null 2>&1 || {
		echo "missing required command: $1" >&2
		exit 127
	}
}

need helm
need rg

helm lint "$chart"

for architecture in amd64 arm64; do
	for transport in tcp rdma; do
		rendered="$(mktemp "/tmp/zccusan-helm-${architecture}-${transport}.XXXXXX.yaml")"
		arguments=(
			template zccusan "$chart"
			--namespace zccusan
			--set-string "daemonset.nodeSelector.kubernetes\\.io/arch=$architecture"
		)
		if [ "$transport" = rdma ]; then
			arguments+=(
				--set backplane.rdma.enabled=true
				--set-string backplane.rdma.provider=efa
			)
		fi
		helm "${arguments[@]}" >"$rendered"

		rg -q "kubernetes.io/arch: $architecture" "$rendered"
		rg -q '^    transport=shm$' "$rendered"
		if [ "$transport" = rdma ]; then
			rg -q '^        - name: rdma-preflight$' "$rendered"
			rg -q '^            - rdma-preflight$' "$rendered"
			rg -q '^            - "efa"$' "$rendered"
			rg -q '^      hostNetwork: true$' "$rendered"
			rg -q '^      dnsPolicy: ClusterFirstWithHostNet$' "$rendered"
		else
			if rg -q 'name: rdma-preflight' "$rendered"; then
				echo "TCP render unexpectedly contains the RDMA preflight" >&2
				exit 1
			fi
			if rg -q '^      hostNetwork: true$' "$rendered"; then
				echo "TCP render unexpectedly enables host networking" >&2
				exit 1
			fi
		fi
		printf 'ZCCUSAN_HELM_INSTALL_MATRIX_PASS arch=%s backplane=%s kernel_edge=shm rendered=%s\n' \
			"$architecture" "$transport" "$rendered"
	done
done

if helm template invalid-kernel-transport "$chart" \
	--set-string 'nodeSetup.module.parameters[0]=transport=tcp' \
	--set-string 'nodeSetup.module.parameters[1]=lanes=1' \
	>/dev/null 2>&1; then
	echo "Helm accepted transport=tcp in the kernel client edge" >&2
	exit 1
fi
echo "ZCCUSAN_HELM_KERNEL_EDGE_GUARD_PASS required=transport=shm"

artifact_digest="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
module_digest="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
image_render="$(mktemp /tmp/zccusan-helm-kmod-image.XXXXXX.yaml)"
helm template image-module "$chart" \
	--set nodeSetup.moduleSource.type=image \
	--set-string nodeSetup.image.repository=registry.example.invalid/zcnblk-kmod \
	--set-string "nodeSetup.image.digest=$artifact_digest" \
	--set-string "nodeSetup.moduleSource.sha256=$module_digest" \
	>"$image_render"
rg -q "registry.example.invalid/zcnblk-kmod@$artifact_digest" "$image_render"
if rg -q 'name: image-module-zcblock-csi-zcnblk-source' "$image_render"; then
	echo "production image mode unexpectedly rendered kernel source" >&2
	exit 1
fi
echo "ZCCUSAN_HELM_MODULE_IMAGE_PASS rendered=$image_render"

http_url='https://modules.example.invalid/%ARCH%/%KERNEL_RELEASE%/zcnblk_client_mod.ko'
http_render="$(mktemp /tmp/zccusan-helm-kmod-http-cache.XXXXXX.yaml)"
helm template http-cache "$chart" \
	--set nodeSetup.moduleSource.type=http \
	--set-string "nodeSetup.moduleSource.http.urlTemplate=$http_url" \
	--set-string "nodeSetup.moduleSource.http.sha256=$module_digest" \
	>"$http_render"
rg -q '^  name: http-cache-zcblock-csi-module-cache$' "$http_render"
rg -q '^      hostNetwork: false$' "$http_render"
rg -q '^      automountServiceAccountToken: false$' "$http_render"
echo "ZCCUSAN_HELM_MODULE_HTTP_CACHE_PASS rendered=$http_render"

http_direct_render="$(mktemp /tmp/zccusan-helm-kmod-http-direct.XXXXXX.yaml)"
helm template http-direct "$chart" \
	--set nodeSetup.moduleSource.type=http \
	--set nodeSetup.moduleSource.http.delivery=direct \
	--set-string "nodeSetup.moduleSource.http.urlTemplate=$http_url" \
	--set-string "nodeSetup.moduleSource.http.sha256=$module_digest" \
	>"$http_direct_render"
if rg -q 'zcblock-csi-module-cache' "$http_direct_render"; then
	echo "direct HTTP mode unexpectedly rendered the artifact-cache DaemonSet" >&2
	exit 1
fi
echo "ZCCUSAN_HELM_MODULE_HTTP_DIRECT_PASS rendered=$http_direct_render"

build_render="$(mktemp /tmp/zccusan-helm-kmod-build.XXXXXX.yaml)"
helm template development-build "$chart" \
	--set nodeSetup.moduleSource.type=build \
	--set nodeSetup.developmentBuild.enabled=true \
	>"$build_render"
rg -q '^  name: development-build-zcblock-csi-zcnblk-source$' "$build_render"
if helm template invalid-build "$chart" \
	--set nodeSetup.moduleSource.type=build >/dev/null 2>&1; then
	echo "Helm accepted development build mode without its explicit enable gate" >&2
	exit 1
fi
if helm template invalid-http "$chart" \
	--set nodeSetup.moduleSource.type=http \
	--set-string "nodeSetup.moduleSource.http.urlTemplate=$http_url" \
	>/dev/null 2>&1; then
	echo "Helm accepted an HTTP module without a digest or checksum URL" >&2
	exit 1
fi
if helm template invalid-plain-http "$chart" \
	--set nodeSetup.moduleSource.type=http \
	--set-string 'nodeSetup.moduleSource.http.urlTemplate=http://modules.example.invalid/zcnblk_client_mod.ko' \
	--set-string "nodeSetup.moduleSource.http.sha256=$module_digest" \
	>/dev/null 2>&1; then
	echo "Helm accepted plain HTTP without explicit opt-in" >&2
	exit 1
fi
echo "ZCCUSAN_HELM_MODULE_BUILD_DEV_ONLY_PASS rendered=$build_render"
