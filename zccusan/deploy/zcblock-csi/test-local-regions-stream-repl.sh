#!/usr/bin/env bash
set -euo pipefail

NS="${NS:-zcblock-stream-repl-test}"
CLEANUP="${CLEANUP:-1}"
SOURCE_REGION="${SOURCE_REGION:-a}"
TARGET_REGIONS="${TARGET_REGIONS:-b c}"
SIZE="${SIZE:-16Mi}"
CONTROL_URL="${CONTROL_URL:-http://127.0.0.1:9788}"
RUN_ID="stream-repl-$(date +%s)"

region_pod() {
  local region="$1"
  kubectl -n "zcblock-csi-${region}" get pod \
    -l "app.kubernetes.io/name=zcblock-csi,zcutils.io/local-region=${region}" \
    -o jsonpath='{.items[0].metadata.name}'
}

region_pod_ip() {
  local region="$1"
  kubectl -n "zcblock-csi-${region}" get pod \
    -l "app.kubernetes.io/name=zcblock-csi,zcutils.io/local-region=${region}" \
    -o jsonpath='{.items[0].status.podIP}'
}

control_zcrepl() {
  local region="$1"
  shift
  local pod
  pod="$(region_pod "$region")"
  kubectl -n "zcblock-csi-${region}" exec "$pod" -c zcblock-csi -- \
    zcrepl "$@" --control-url "$CONTROL_URL"
}

kv_field() {
  local key="$1"
  tr ' ' '\n' | sed -n "s/^${key}=//p" | tail -n 1
}

pvc_volume_handle() {
  local pvc="$1"
  local pv
  pv="$(kubectl -n "$NS" get pvc "$pvc" -o jsonpath='{.spec.volumeName}')"
  kubectl get pv "$pv" -o jsonpath='{.spec.csi.volumeHandle}'
}

snapshot_handle() {
  local snap="$1"
  local content
  content="$(kubectl -n "$NS" get volumesnapshot "$snap" \
    -o jsonpath='{.status.boundVolumeSnapshotContentName}')"
  kubectl get volumesnapshotcontent "$content" -o jsonpath='{.status.snapshotHandle}'
}

wait_repl() {
  local region="$1"
  local repl_id="$2"
  local response state
  for _ in $(seq 1 90); do
    response="$(control_zcrepl "$region" csi-status --repl-id "$repl_id")"
    state="$(printf '%s' "$response" | kv_field state)"
    case "$state" in
      succeeded)
        printf '%s' "$response"
        return 0
        ;;
      failed)
        printf '%s\n' "$response" >&2
        return 1
        ;;
    esac
    sleep 1
  done
  echo "replication job ${repl_id} in region ${region} did not finish" >&2
  control_zcrepl "$region" csi-status --repl-id "$repl_id" >&2 || true
  return 1
}

cleanup() {
  if [ "$CLEANUP" = "1" ]; then
    kubectl delete namespace "$NS" --ignore-not-found=true --wait=true --timeout=180s >/dev/null || true
  fi
}
trap cleanup EXIT

kubectl delete namespace "$NS" --ignore-not-found=true --wait=true >/dev/null
kubectl create namespace "$NS" >/dev/null

cat <<YAML | kubectl -n "$NS" apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: source-${SOURCE_REGION}
spec:
  accessModes:
    - ReadWriteOnce
  storageClassName: zcfile-${SOURCE_REGION}
  resources:
    requests:
      storage: ${SIZE}
---
apiVersion: v1
kind: Pod
metadata:
  name: writer-${SOURCE_REGION}
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
          printf '%s\n' 'source=${SOURCE_REGION} run=${RUN_ID}' > /data/probe
          sync
          sleep 3600
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: source-${SOURCE_REGION}
YAML

for region in $TARGET_REGIONS; do
  cat <<YAML | kubectl -n "$NS" apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: target-${region}
spec:
  accessModes:
    - ReadWriteOnce
  storageClassName: zcfile-${region}
  resources:
    requests:
      storage: ${SIZE}
---
apiVersion: v1
kind: Pod
metadata:
  name: binder-${region}
spec:
  restartPolicy: Never
  containers:
    - name: binder
      image: debian:bookworm-slim
      command: ["/bin/sh", "-c", "sync; sleep 3600"]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: target-${region}
YAML
done

kubectl -n "$NS" wait --for=condition=Ready "pod/writer-${SOURCE_REGION}" --timeout=180s
for region in $TARGET_REGIONS; do
  kubectl -n "$NS" wait --for=condition=Ready "pod/binder-${region}" --timeout=180s
done

cat <<YAML | kubectl -n "$NS" apply -f -
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshot
metadata:
  name: source-${SOURCE_REGION}
spec:
  volumeSnapshotClassName: zcblock-${SOURCE_REGION}
  source:
    persistentVolumeClaimName: source-${SOURCE_REGION}
YAML

kubectl -n "$NS" wait --for=jsonpath='{.status.readyToUse}'=true \
  "volumesnapshot/source-${SOURCE_REGION}" --timeout=180s
snap_id="$(snapshot_handle "source-${SOURCE_REGION}")"

for region in $TARGET_REGIONS; do
  kubectl -n "$NS" delete "pod/binder-${region}" --wait=true --timeout=180s >/dev/null
done
sleep 3

declare -A target_volume
declare -A target_ip
declare -A recv_repl
declare -A recv_token
declare -A recv_port
declare -A send_repl

for region in $TARGET_REGIONS; do
  target_volume[$region]="$(pvc_volume_handle "target-${region}")"
  target_ip[$region]="$(region_pod_ip "$region")"
  response="$(control_zcrepl "$region" csi-recv --volume "${target_volume[$region]}" --listen 0.0.0.0 --port 0 --token auto)"
  recv_repl[$region]="$(printf '%s' "$response" | kv_field repl_id)"
  recv_token[$region]="$(printf '%s' "$response" | kv_field token)"
  recv_port[$region]="$(printf '%s' "$response" | kv_field port)"
  if [ -z "${recv_repl[$region]}" ] || [ -z "${recv_token[$region]}" ] || [ -z "${recv_port[$region]}" ]; then
    echo "could not parse receiver response for region ${region}: ${response}" >&2
    exit 1
  fi
done

for region in $TARGET_REGIONS; do
  response="$(control_zcrepl "$SOURCE_REGION" csi-send --snapshot "$snap_id" --peer "${target_ip[$region]}" --port "${recv_port[$region]}" --token "${recv_token[$region]}")"
  send_repl[$region]="$(printf '%s' "$response" | kv_field repl_id)"
  if [ -z "${send_repl[$region]}" ]; then
    echo "could not parse sender response for region ${region}: ${response}" >&2
    exit 1
  fi
done

for region in $TARGET_REGIONS; do
  wait_repl "$SOURCE_REGION" "${send_repl[$region]}" >/dev/null
  wait_repl "$region" "${recv_repl[$region]}" >/dev/null
done

for region in $TARGET_REGIONS; do
  cat <<YAML | kubectl -n "$NS" apply -f -
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
        claimName: target-${region}
YAML
done

expected="source=${SOURCE_REGION} run=${RUN_ID}"
for region in $TARGET_REGIONS; do
  kubectl -n "$NS" wait --for=condition=Ready "pod/reader-${region}" --timeout=180s
  actual="$(kubectl -n "$NS" logs "reader-${region}")"
  if [ "$actual" != "$expected" ]; then
    echo "stream replication verification failed for region ${region}" >&2
    echo "expected: $expected" >&2
    echo "actual:   $actual" >&2
    exit 1
  fi
done

echo "stream replication passed: source=${SOURCE_REGION} targets=[$TARGET_REGIONS] snapshot=${snap_id}"
