#!/usr/bin/env bash
set -euo pipefail

VERSION="${SNAPSHOTTER_VERSION:-v8.3.0}"
REQUIRED_SNAPSHOT_CRD_VERSION="${REQUIRED_SNAPSHOT_CRD_VERSION:-v1}"
SUPPORTED_SNAPSHOT_CRD_VERSIONS="${SUPPORTED_SNAPSHOT_CRD_VERSIONS:-v1}"
SUPPORTED_N_MINUS_1_SNAPSHOT_CRD_VERSION="${SUPPORTED_N_MINUS_1_SNAPSHOT_CRD_VERSION:-v1}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT
SNAPSHOT_CRDS=(
  volumesnapshotclasses.snapshot.storage.k8s.io
  volumesnapshotcontents.snapshot.storage.k8s.io
  volumesnapshots.snapshot.storage.k8s.io
)

cd "$ROOT"

version_supported() {
  local version="$1"
  local supported
  for supported in $SUPPORTED_SNAPSHOT_CRD_VERSIONS; do
    if [ "$version" = "$supported" ]; then
      return 0
    fi
  done
  return 1
}

for crd in "${SNAPSHOT_CRDS[@]}"; do
  storage_version="$(kubectl get crd "$crd" -o jsonpath='{range .spec.versions[?(@.storage==true)]}{.name}{end}' 2>/dev/null || true)"
  if [ -n "$storage_version" ] && ! version_supported "$storage_version"; then
    cat >&2 <<EOF
Installed $crd storage version is $storage_version, but this installer supports:
  $SUPPORTED_SNAPSHOT_CRD_VERSIONS

Do a stair-step upgrade through a zcblock CSI/snapshot-controller release that
supports both $storage_version and $REQUIRED_SNAPSHOT_CRD_VERSION before changing
the CRD storage version.
EOF
    exit 1
  fi
done

kubectl kustomize "https://github.com/kubernetes-csi/external-snapshotter/client/config/crd?ref=${VERSION}" \
  | kubectl apply -f -

for crd in "${SNAPSHOT_CRDS[@]}"; do
  storage_version="$(kubectl get crd "$crd" -o jsonpath='{range .spec.versions[?(@.storage==true)]}{.name}{end}')"
  if [ "$storage_version" != "$REQUIRED_SNAPSHOT_CRD_VERSION" ]; then
    echo "expected $crd storage version $REQUIRED_SNAPSHOT_CRD_VERSION, got $storage_version" >&2
    exit 1
  fi
  kubectl annotate crd "$crd" \
    zcutils.io/snapshotter-version="$VERSION" \
    zcutils.io/snapshot-crd-storage-version="$storage_version" \
    zcutils.io/snapshot-crd-supported-versions="$SUPPORTED_SNAPSHOT_CRD_VERSIONS" \
    zcutils.io/snapshot-crd-n-minus-1="$SUPPORTED_N_MINUS_1_SNAPSHOT_CRD_VERSION" \
    zcutils.io/snapshot-crd-stair-step-required="true" \
    --overwrite
done

kubectl kustomize "https://github.com/kubernetes-csi/external-snapshotter/deploy/kubernetes/snapshot-controller?ref=${VERSION}" > "$TMP"
sed -E "s#registry.k8s.io/sig-storage/snapshot-controller:v[0-9.]+#registry.k8s.io/sig-storage/snapshot-controller:${VERSION}#g" "$TMP" \
  | awk '
      /        - --leader-election=true/ {
        print
        print "        - --enable-distributed-snapshotting=true"
        next
      }
      { print }
    ' \
  | kubectl apply -f -

if [ "$(kubectl auth can-i list nodes --as=system:serviceaccount:kube-system:snapshot-controller)" != "yes" ]; then
  kubectl patch clusterrole snapshot-controller-runner --type=json \
    -p='[{"op":"add","path":"/rules/-","value":{"apiGroups":[""],"resources":["nodes"],"verbs":["get","list","watch"]}}]'
fi
