#!/usr/bin/env bash
set -euo pipefail

IMAGE="${IMAGE:-localhost/zcblock-csi:dev}"
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
  IMAGE=localhost/zcblock-csi:dev
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
  podman build -t "$IMAGE" -f zccusan/deploy/zcblock-csi/Dockerfile .
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
  IMAGE="$IMAGE" zccusan/deploy/zcblock-csi/render-region-install.sh "$region" | kubectl apply -f -
done

for region in "${REGIONS[@]}"; do
  kubectl -n "zcblock-csi-${region}" rollout status "daemonset/zcblock-csi-${region}-node" --timeout=180s
done

kubectl get volumesnapshotclasses.snapshot.storage.k8s.io \
  -l zcutils.io/local-region \
  -o custom-columns=NAME:.metadata.name,DRIVER:.driver,REGION:.metadata.labels.zcutils\\.io/local-region
