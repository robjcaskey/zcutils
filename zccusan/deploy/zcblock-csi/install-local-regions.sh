#!/usr/bin/env bash
set -euo pipefail

IMAGE_VARIANT="${IMAGE_VARIANT:-nonfips}"
IMAGE_NONFIPS_DEFAULT="${IMAGE_NONFIPS:-localhost/zcblock-csi:dev}"
IMAGE_FIPS_ASPIRING_DEFAULT="${IMAGE_FIPS_ASPIRING:-${IMAGE_FIPS:-localhost/zcblock-csi-fips:dev}}"
IMAGE="${IMAGE:-}"
DOCKERFILE="${DOCKERFILE:-}"
BUILD_IMAGE="${BUILD_IMAGE:-1}"
INSTALL_SNAPSHOT_API="${INSTALL_SNAPSHOT_API:-1}"
REGIONS=()
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
ARCHIVE=""

usage() {
  cat <<'EOF'
usage: install-local-regions.sh [-a] [-b] [-c] [--region NAME ...]

Installs regionized zcblock CSI node deployments into one local Kubernetes
cluster. Snapshot CRDs/controller are shared cluster-wide; each region gets its
own namespace, CSIDriver name, StorageClasses, VolumeSnapshotClass, kubelet
plugin path, and /var/lib state directory.

Environment:
  IMAGE_VARIANT=nonfips|fips-aspiring
  IMAGE_NONFIPS=localhost/zcblock-csi:dev
  IMAGE_FIPS_ASPIRING=localhost/zcblock-csi-fips:dev
  DOCKERFILE (optional)
  IMAGE (optional custom override)
  IMAGE_A / IMAGE_B / IMAGE_C (optional per-region image overrides)
  REGION_IMAGES='a=IMAGE b=IMAGE c=IMAGE' (arbitrary per-region overrides)
  BUILD_IMAGE=1
  INSTALL_SNAPSHOT_API=1
  SNAPSHOT_MODE=auto
  SNAPSHOTTER_VERSION=v8.3.0
EOF
}

cleanup() {
  if [ -n "$ARCHIVE" ] && [ -f "$ARCHIVE" ]; then
    rm -f "$ARCHIVE"
  fi
}

case "$IMAGE_VARIANT" in
  nonfips|non-fips)
    IMAGE_DEFAULT="$IMAGE_NONFIPS_DEFAULT"
    DOCKERFILE_DEFAULT="zccusan/deploy/zcblock-csi/Dockerfile"
    ;;
  fips-aspiring)
    IMAGE_DEFAULT="$IMAGE_FIPS_ASPIRING_DEFAULT"
    DOCKERFILE_DEFAULT="zccusan/deploy/zcblock-csi/Dockerfile.fips"
    ;;
  *)
    echo "unsupported IMAGE_VARIANT=$IMAGE_VARIANT (expected: nonfips, non-fips, fips-aspiring). Use IMAGE_VARIANT=fips-aspiring for the non-validated FIPS track." >&2
    exit 1
    ;;
esac

IMAGE="${IMAGE:-$IMAGE_DEFAULT}"
DOCKERFILE="${DOCKERFILE:-$DOCKERFILE_DEFAULT}"

trap cleanup EXIT

add_region() {
  local region="$1"
  local existing
  for existing in "${REGIONS[@]}"; do
    if [ "$existing" = "$region" ]; then
      return
    fi
  done
  REGIONS+=("$region")
}

image_for_region() {
  local region="$1"
  local token token_region token_image
  local variable="IMAGE_${region^^}"
  variable="${variable//-/_}"
  if [ -n "${!variable:-}" ]; then
    printf '%s\n' "${!variable}"
    return
  fi
  for token in ${REGION_IMAGES:-}; do
    token_region="${token%%=*}"
    token_image="${token#*=}"
    if [ "$token" = "$token_region" ] || [ -z "$token_image" ]; then
      echo "invalid REGION_IMAGES entry: $token (expected region=image)" >&2
      return 1
    fi
    if [ "$token_region" = "$region" ]; then
      printf '%s\n' "$token_image"
      return
    fi
  done
  printf '%s\n' "$IMAGE"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    -a) add_region a ;;
    -b) add_region b ;;
    -c) add_region c ;;
    --region)
      shift
      add_region "${1:?--region requires a value}"
      ;;
    --regions)
      shift
      for region in ${1:?--regions requires a value}; do
        add_region "$region"
      done
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
  shift
done

if [ "${#REGIONS[@]}" -eq 0 ]; then
  REGIONS=(a b c)
fi

cd "$ROOT"

if [ "$BUILD_IMAGE" = "1" ]; then
  ARCHIVE="$(mktemp --suffix=.tar)"
  podman build -t "$IMAGE" -f "$DOCKERFILE" .
  podman save "$IMAGE" -o "$ARCHIVE"
  if [ "$(id -u)" -eq 0 ]; then
    ctr -n k8s.io images import "$ARCHIVE"
  else
    sudo ctr -n k8s.io images import "$ARCHIVE"
  fi
fi

if [ "$INSTALL_SNAPSHOT_API" = "1" ]; then
  zccusan/deploy/zcblock-csi/install-snapshot-api.sh
fi

for region in "${REGIONS[@]}"; do
  region_image="$(image_for_region "$region")"
  printf 'installing local region %s with image %s\n' "$region" "$region_image"
  IMAGE="$region_image" zccusan/deploy/zcblock-csi/render-region-install.sh "$region" | kubectl apply -f -
done

for region in "${REGIONS[@]}"; do
  kubectl -n "zcblock-csi-${region}" rollout status "daemonset/zcblock-csi-${region}-node" --timeout=180s
done

kubectl get volumesnapshotclasses.snapshot.storage.k8s.io \
  -l zcutils.io/local-region \
  -o custom-columns=NAME:.metadata.name,DRIVER:.driver,REGION:.metadata.labels.zcutils\\.io/local-region
