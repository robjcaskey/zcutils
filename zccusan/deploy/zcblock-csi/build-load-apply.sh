#!/usr/bin/env bash
set -euo pipefail

IMAGE_VARIANT="${IMAGE_VARIANT:-nonfips}"
IMAGE_NONFIPS_DEFAULT="${IMAGE_NONFIPS:-localhost/zcblock-csi:dev}"
IMAGE_FIPS_ASPIRING_DEFAULT="${IMAGE_FIPS_ASPIRING:-${IMAGE_FIPS:-localhost/zcblock-csi-fips:dev}}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
ARCHIVE="$(mktemp --suffix=.tar)"

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

trap 'rm -f "$ARCHIVE"' EXIT

cd "$ROOT"

if [[ "$IMAGE_VARIANT" == fips-aspiring && "$DOCKERFILE" == zccusan/deploy/zcblock-csi/Dockerfile.fips ]]; then
  podman build --build-arg "FIPS_DISTRO=${FIPS_DISTRO:-amzn2023}" \
    --build-arg "FIPS_PROVIDER_ROOT=${FIPS_PROVIDER_ROOT:-zccusan/deploy/zcblock-csi/fips/provider}" \
    -t "$IMAGE" -f "$DOCKERFILE" .
else
  podman build -t "$IMAGE" -f "$DOCKERFILE" .
fi
podman save "$IMAGE" -o "$ARCHIVE"

if [ "$(id -u)" -eq 0 ]; then
  ctr -n k8s.io images import "$ARCHIVE"
else
  sudo ctr -n k8s.io images import "$ARCHIVE"
fi

if [ "${INSTALL_SNAPSHOT_API:-1}" = "1" ]; then
  zccusan/deploy/zcblock-csi/install-snapshot-api.sh
fi

# Render the selected image before applying; the source manifest defaults to non-FIPS.
python3 - "$IMAGE" <<'PY' | kubectl apply -f -
import json
from pathlib import Path
import sys
manifest = Path("zccusan/deploy/zcblock-csi/zcblock-csi.yaml").read_text()
placeholder = "image: localhost/zcblock-csi:dev"
if manifest.count(placeholder) != 2:
    raise SystemExit("expected two first-party image placeholders in CSI manifest")
print(manifest.replace(placeholder, "image: " + json.dumps(sys.argv[1])))
PY
kubectl apply -f zccusan/deploy/zcblock-csi/snapshot-class.yaml
kubectl -n zcblock-csi rollout restart daemonset/zcblock-csi-node
kubectl -n zcblock-csi rollout status daemonset/zcblock-csi-node --timeout=180s
