#!/usr/bin/env bash
set -euo pipefail

REGIONS="${REGIONS:-a b c}"
SUBSET="${SUBSET:-a c}"
NS="${NS:-zcblock-cohesive-test}"
TTL_MS="${TTL_MS:-5000}"
SNAPSHOT_UNDER_FREEZE_TIMEOUT="${SNAPSHOT_UNDER_FREEZE_TIMEOUT:-4s}"
CLEANUP="${CLEANUP:-1}"
CONTROL_URL="${CONTROL_URL:-http://127.0.0.1:9788}"
BARRIER="cohesive-$(date +%s)"
RELEASE_NEEDED=0

region_pod() {
  local region="$1"
  kubectl -n "zcblock-csi-${region}" get pod \
    -l "app.kubernetes.io/name=zcblock-csi,zcutils.io/local-region=${region}" \
    -o jsonpath='{.items[0].metadata.name}'
}

release_all() {
  local region pod
  for region in $REGIONS; do
    pod="$(region_pod "$region" 2>/dev/null || true)"
    if [ -n "$pod" ]; then
      kubectl -n "zcblock-csi-${region}" exec "$pod" -c zcblock-csi -- \
        zcblock-freeze --control-url "$CONTROL_URL" release --barrier "$BARRIER" >/dev/null 2>&1 || true
    fi
  done
}

cleanup() {
  if [ "$RELEASE_NEEDED" = "1" ]; then
    release_all
  fi
  if [ "$CLEANUP" = "1" ]; then
    kubectl delete namespace "$NS" --ignore-not-found=true --wait=true --timeout=180s >/dev/null || true
  fi
}
trap cleanup EXIT

kubectl delete namespace "$NS" --ignore-not-found=true --wait=true >/dev/null
kubectl create namespace "$NS" >/dev/null

for region in $REGIONS; do
  cat <<YAML | kubectl -n "$NS" apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: source-${region}
spec:
  accessModes:
    - ReadWriteOnce
  storageClassName: zcfile-${region}
  resources:
    requests:
      storage: 16Mi
---
apiVersion: v1
kind: Pod
metadata:
  name: writer-${region}
spec:
  restartPolicy: Never
  containers:
    - name: writer
      image: debian:bookworm-slim
      command:
        - /bin/sh
        - -c
        - |
          set -eu
          printf '%s\n' 'region=${region} barrier=${BARRIER}' > /data/probe
          sync
          sleep 3600
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: source-${region}
YAML
done

for region in $REGIONS; do
  kubectl -n "$NS" wait --for=condition=Ready "pod/writer-${region}" --timeout=180s
done

for region in $REGIONS; do
  pod="$(region_pod "$region")"
  kubectl -n "zcblock-csi-${region}" exec "$pod" -c zcblock-csi -- \
    zcblock-freeze --control-url "$CONTROL_URL" freeze --barrier "$BARRIER" --ttl-ms "$TTL_MS"
done
RELEASE_NEEDED=1

for region in $SUBSET; do
  cat <<YAML | kubectl -n "$NS" apply -f -
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshot
metadata:
  name: snap-${region}
spec:
  volumeSnapshotClassName: zcblock-${region}
  source:
    persistentVolumeClaimName: source-${region}
YAML
done

wait_args=()
for region in $SUBSET; do
  wait_args+=("volumesnapshot/snap-${region}")
done
kubectl -n "$NS" wait --for=jsonpath='{.status.readyToUse}'=true "${wait_args[@]}" \
  --timeout="$SNAPSHOT_UNDER_FREEZE_TIMEOUT"

release_all
RELEASE_NEEDED=0

for region in $SUBSET; do
  cat <<YAML | kubectl -n "$NS" apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: restore-${region}
spec:
  accessModes:
    - ReadWriteOnce
  storageClassName: zcfile-${region}
  dataSource:
    apiGroup: snapshot.storage.k8s.io
    kind: VolumeSnapshot
    name: snap-${region}
  resources:
    requests:
      storage: 16Mi
---
apiVersion: v1
kind: Pod
metadata:
  name: reader-${region}
spec:
  restartPolicy: Never
  containers:
    - name: reader
      image: debian:bookworm-slim
      command:
        - /bin/sh
        - -c
        - |
          set -eu
          cat /data/probe
          sleep 10
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: restore-${region}
YAML
done

for region in $SUBSET; do
  kubectl -n "$NS" wait --for=condition=Ready "pod/reader-${region}" --timeout=180s
  expected="region=${region} barrier=${BARRIER}"
  actual="$(kubectl -n "$NS" logs "reader-${region}")"
  if [ "$actual" != "$expected" ]; then
    echo "restore verification failed for region ${region}" >&2
    echo "expected: $expected" >&2
    echo "actual:   $actual" >&2
    exit 1
  fi
done

for region in $REGIONS; do
  pod="$(region_pod "$region")"
  kubectl -n "zcblock-csi-${region}" exec "$pod" -c zcblock-csi -- \
    zcblock-freeze --control-url "$CONTROL_URL" status
done

echo "cohesive snapshot subset passed: regions=[$REGIONS] subset=[$SUBSET] barrier=$BARRIER"
