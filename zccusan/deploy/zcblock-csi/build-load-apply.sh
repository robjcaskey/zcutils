#!/usr/bin/env bash
set -euo pipefail

IMAGE="${IMAGE:-localhost/zcblock-csi:dev}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
ARCHIVE="$(mktemp --suffix=.tar)"
trap 'rm -f "$ARCHIVE"' EXIT

cd "$ROOT"

podman build -t "$IMAGE" -f zccusan/deploy/zcblock-csi/Dockerfile .
podman save "$IMAGE" -o "$ARCHIVE"

if [ "$(id -u)" -eq 0 ]; then
  ctr -n k8s.io images import "$ARCHIVE"
else
  sudo ctr -n k8s.io images import "$ARCHIVE"
fi

if [ "${INSTALL_SNAPSHOT_API:-1}" = "1" ]; then
  zccusan/deploy/zcblock-csi/install-snapshot-api.sh
fi

kubectl apply -f zccusan/deploy/zcblock-csi/zcblock-csi.yaml
kubectl apply -f zccusan/deploy/zcblock-csi/snapshot-class.yaml
kubectl -n zcblock-csi rollout restart daemonset/zcblock-csi-node
kubectl -n zcblock-csi rollout status daemonset/zcblock-csi-node --timeout=180s
