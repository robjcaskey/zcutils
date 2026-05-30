#!/usr/bin/env bash
set -euo pipefail

NS="${NS:-zcblock-concurrent-c-receive-test}"
CLEANUP="${CLEANUP:-1}"
A_REGION="${A_REGION:-a}"
B_REGION="${B_REGION:-b}"
C_REGION="${C_REGION:-c}"
SIZE="${SIZE:-16Mi}"
CONTROL_URL="${CONTROL_URL:-http://127.0.0.1:9788}"
RUN_ID="concurrent-c-receive-$(date +%s)"
declare -A CONTROL_FLAG
declare -A CONTROL_VALUE

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
  resolve_control_endpoint "$region" "$pod"
  kubectl -n "zcblock-csi-${region}" exec "$pod" -c zcblock-csi -- \
    zcrepl "$@" "${CONTROL_FLAG[$region]}" "${CONTROL_VALUE[$region]}"
}

resolve_control_endpoint() {
  local region="$1"
  local pod="$2"
  if [ -n "${CONTROL_FLAG[$region]:-}" ]; then
    return 0
  fi
  local help
  help="$(kubectl -n "zcblock-csi-${region}" exec "$pod" -c zcblock-csi -- \
    zcrepl --help 2>&1 || true)"
  if printf '%s' "$help" | grep -q -- '--control-url'; then
    CONTROL_FLAG[$region]="--control-url"
    CONTROL_VALUE[$region]="$CONTROL_URL"
  else
    CONTROL_FLAG[$region]="--socket"
    CONTROL_VALUE[$region]="/var/lib/zcblock-csi-${region}/control.sock"
  fi
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

region_pod "$A_REGION" >/dev/null
region_pod "$B_REGION" >/dev/null
region_pod "$C_REGION" >/dev/null

kubectl delete namespace "$NS" --ignore-not-found=true --wait=true >/dev/null
kubectl create namespace "$NS" >/dev/null

for region in "$A_REGION" "$B_REGION"; do
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
      storage: ${SIZE}
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
          printf '%s\n' 'source=${region} run=${RUN_ID}' > /data/probe
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

for source in "$A_REGION" "$B_REGION"; do
  cat <<YAML | kubectl -n "$NS" apply -f -
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: target-${source}-to-${C_REGION}
spec:
  accessModes:
    - ReadWriteOnce
  storageClassName: zcfile-${C_REGION}
  resources:
    requests:
      storage: ${SIZE}
---
apiVersion: v1
kind: Pod
metadata:
  name: binder-${source}-to-${C_REGION}
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
        claimName: target-${source}-to-${C_REGION}
YAML
done

for region in "$A_REGION" "$B_REGION"; do
  kubectl -n "$NS" wait --for=condition=Ready "pod/writer-${region}" --timeout=180s
done
for source in "$A_REGION" "$B_REGION"; do
  kubectl -n "$NS" wait --for=condition=Ready "pod/binder-${source}-to-${C_REGION}" --timeout=180s
done

cat <<YAML | kubectl -n "$NS" apply -f -
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshot
metadata:
  name: source-${B_REGION}
spec:
  volumeSnapshotClassName: zcblock-${B_REGION}
  source:
    persistentVolumeClaimName: source-${B_REGION}
YAML

kubectl -n "$NS" wait --for=jsonpath='{.status.readyToUse}'=true \
  "volumesnapshot/source-${B_REGION}" --timeout=180s

source_a_volume="$(pvc_volume_handle "source-${A_REGION}")"
snapshot_b="$(snapshot_handle "source-${B_REGION}")"
target_a_to_c="$(pvc_volume_handle "target-${A_REGION}-to-${C_REGION}")"
target_b_to_c="$(pvc_volume_handle "target-${B_REGION}-to-${C_REGION}")"
c_ip="$(region_pod_ip "$C_REGION")"

for source in "$A_REGION" "$B_REGION"; do
  kubectl -n "$NS" delete "pod/binder-${source}-to-${C_REGION}" \
    --wait=true --timeout=180s >/dev/null
done
sleep 3

recv_a_response="$(control_zcrepl "$C_REGION" csi-recv \
  --volume "$target_a_to_c" --listen 0.0.0.0 --port 0 --token auto)"
recv_b_response="$(control_zcrepl "$C_REGION" csi-recv \
  --volume "$target_b_to_c" --listen 0.0.0.0 --port 0 --token auto)"

recv_a_repl="$(printf '%s' "$recv_a_response" | kv_field repl_id)"
recv_a_token="$(printf '%s' "$recv_a_response" | kv_field token)"
recv_a_port="$(printf '%s' "$recv_a_response" | kv_field port)"
recv_b_repl="$(printf '%s' "$recv_b_response" | kv_field repl_id)"
recv_b_token="$(printf '%s' "$recv_b_response" | kv_field token)"
recv_b_port="$(printf '%s' "$recv_b_response" | kv_field port)"

if [ -z "$recv_a_repl" ] || [ -z "$recv_a_token" ] || [ -z "$recv_a_port" ]; then
  echo "could not parse A->C receiver response: ${recv_a_response}" >&2
  exit 1
fi
if [ -z "$recv_b_repl" ] || [ -z "$recv_b_token" ] || [ -z "$recv_b_port" ]; then
  echo "could not parse B->C receiver response: ${recv_b_response}" >&2
  exit 1
fi

tmp_dir="$(mktemp -d /tmp/zccusan-concurrent-c.XXXXXX)"
trap 'rm -rf "$tmp_dir"; cleanup' EXIT

control_zcrepl "$A_REGION" csi-send \
  --volume "$source_a_volume" \
  --peer "$c_ip" \
  --port "$recv_a_port" \
  --token "$recv_a_token" >"${tmp_dir}/send-a.out" &
send_a_pid="$!"

control_zcrepl "$B_REGION" csi-send \
  --snapshot "$snapshot_b" \
  --peer "$c_ip" \
  --port "$recv_b_port" \
  --token "$recv_b_token" >"${tmp_dir}/send-b.out" &
send_b_pid="$!"

wait "$send_a_pid"
wait "$send_b_pid"

send_a_response="$(cat "${tmp_dir}/send-a.out")"
send_b_response="$(cat "${tmp_dir}/send-b.out")"
send_a_repl="$(printf '%s' "$send_a_response" | kv_field repl_id)"
send_b_repl="$(printf '%s' "$send_b_response" | kv_field repl_id)"
if [ -z "$send_a_repl" ]; then
  echo "could not parse A->C sender response: ${send_a_response}" >&2
  exit 1
fi
if [ -z "$send_b_repl" ]; then
  echo "could not parse B->C sender response: ${send_b_response}" >&2
  exit 1
fi

wait_repl "$A_REGION" "$send_a_repl" >/dev/null
wait_repl "$B_REGION" "$send_b_repl" >/dev/null
wait_repl "$C_REGION" "$recv_a_repl" >/dev/null
wait_repl "$C_REGION" "$recv_b_repl" >/dev/null

for source in "$A_REGION" "$B_REGION"; do
  cat <<YAML | kubectl -n "$NS" apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: reader-${source}-to-${C_REGION}
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
        claimName: target-${source}-to-${C_REGION}
YAML
done

for source in "$A_REGION" "$B_REGION"; do
  kubectl -n "$NS" wait --for=condition=Ready "pod/reader-${source}-to-${C_REGION}" --timeout=180s
  expected="source=${source} run=${RUN_ID}"
  actual="$(kubectl -n "$NS" logs "reader-${source}-to-${C_REGION}")"
  if [ "$actual" != "$expected" ]; then
    echo "concurrent C receive verification failed for ${source}->${C_REGION}" >&2
    echo "expected: $expected" >&2
    echo "actual:   $actual" >&2
    exit 1
  fi
done

echo "concurrent C receive passed: ${A_REGION}->${C_REGION} live volume stream and ${B_REGION}->${C_REGION} snapshot stream"
