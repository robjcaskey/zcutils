#!/usr/bin/env bash
set -euo pipefail

NS="${NS:-zcblock-local-regions-failover}"
CLEANUP="${CLEANUP:-1}"
SOURCE_REGION="${SOURCE_REGION:-a}"
FIRST_TARGET_REGION="${FIRST_TARGET_REGION:-b}"
SECOND_TARGET_REGION="${SECOND_TARGET_REGION:-c}"
SIZE="${SIZE:-32Mi}"
CONTROL_URL="${CONTROL_URL:-http://127.0.0.1:9788}"
RUN_ID="planned-failover-$(date +%s)"

region_pod() {
  local region="$1"
  local pod
  pod="$(kubectl -n "zcblock-csi-${region}" get pod \
    -l "app.kubernetes.io/name=zcblock-csi,zcutils.io/local-region=${region}" \
    -o jsonpath='{.items[0].metadata.name}')"
  if [ -z "$pod" ]; then
    pod="$(kubectl -n "zcblock-csi-${region}" get pod \
      -l "app.kubernetes.io/name=zcblock-csi" \
      -o jsonpath='{.items[0].metadata.name}')"
  fi
  printf '%s\n' "$pod"
}

region_pod_ip() {
  local region="$1"
  local pod
  pod="$(region_pod "$region")"
  kubectl -n "zcblock-csi-${region}" get pod "$pod" \
    -o jsonpath='{.status.podIP}'
}

region_image() {
  local region="$1"
  kubectl -n "zcblock-csi-${region}" get daemonset "zcblock-csi-${region}-node" \
    -o jsonpath='{.spec.template.spec.containers[?(@.name=="zcblock-csi")].image}'
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

wait_repl() {
  local region="$1"
  local repl_id="$2"
  local response state
  for _ in $(seq 1 120); do
    response="$(control_zcrepl "$region" csi-status --repl-id "$repl_id")"
    state="$(printf '%s' "$response" | kv_field state)"
    case "$state" in
      succeeded) return 0 ;;
      failed)
        echo "replication job ${repl_id} failed in simulated region ${region}" >&2
        return 1
        ;;
    esac
    sleep 1
  done
  echo "replication job ${repl_id} timed out in simulated region ${region}" >&2
  return 1
}

replicate_volume() {
  local source_region="$1"
  local source_volume="$2"
  local target_region="$3"
  local target_volume="$4"
  local response recv_id token port send_id target_ip

  target_ip="$(region_pod_ip "$target_region")"
  response="$(control_zcrepl "$target_region" csi-recv \
    --volume "$target_volume" --listen 0.0.0.0 --port 0 --token auto)"
  recv_id="$(printf '%s' "$response" | kv_field repl_id)"
  token="$(printf '%s' "$response" | kv_field token)"
  port="$(printf '%s' "$response" | kv_field port)"
  if [ -z "$recv_id" ] || [ -z "$token" ] || [ -z "$port" ]; then
    echo "could not establish a receiver in simulated region ${target_region}" >&2
    return 1
  fi

  response="$(control_zcrepl "$source_region" csi-send \
    --volume "$source_volume" --peer "$target_ip" --port "$port" --token "$token")"
  send_id="$(printf '%s' "$response" | kv_field repl_id)"
  if [ -z "$send_id" ]; then
    echo "could not establish a sender in simulated region ${source_region}" >&2
    return 1
  fi
  wait_repl "$source_region" "$send_id"
  wait_repl "$target_region" "$recv_id"
}

cleanup() {
  if [ "$CLEANUP" = "1" ]; then
    kubectl delete namespace "$NS" --ignore-not-found=true --wait=true --timeout=180s >/dev/null || true
  fi
}
trap cleanup EXIT

regions="$SOURCE_REGION $FIRST_TARGET_REGION $SECOND_TARGET_REGION"
for region in $regions; do
  kubectl -n "zcblock-csi-${region}" rollout status \
    "daemonset/zcblock-csi-${region}-node" --timeout=180s
done

kubectl delete namespace "$NS" --ignore-not-found=true --wait=true >/dev/null
kubectl create namespace "$NS" >/dev/null

for region in $regions; do
  kubectl -n "$NS" apply -f - <<YAML
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: volume-${region}
spec:
  accessModes: [ReadWriteOnce]
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
    - name: data
      image: debian:bookworm-slim
      imagePullPolicy: IfNotPresent
      command: ["/bin/sh", "-c", "sync; sleep 3600"]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: volume-${region}
YAML
done

for region in $regions; do
  kubectl -n "$NS" wait --for=condition=Ready "pod/binder-${region}" --timeout=180s
done

kubectl -n "$NS" exec "binder-${SOURCE_REGION}" -- /bin/sh -c \
  "printf '%s\\n' 'source=${SOURCE_REGION} run=${RUN_ID}' > /data/probe; sync"

source_volume="$(pvc_volume_handle "volume-${SOURCE_REGION}")"
first_target_volume="$(pvc_volume_handle "volume-${FIRST_TARGET_REGION}")"
second_target_volume="$(pvc_volume_handle "volume-${SECOND_TARGET_REGION}")"
if [ "$source_volume" = "$first_target_volume" ] \
  || [ "$source_volume" = "$second_target_volume" ] \
  || [ "$first_target_volume" = "$second_target_volume" ]; then
  echo "simulated regions did not provision three distinct volumes" >&2
  exit 1
fi

# Planned failover begins with an explicit source-writer fence. Sending a live
# mounted filesystem is intentionally not accepted as an application-consistent cut.
for region in $regions; do
  kubectl -n "$NS" delete "pod/binder-${region}" --wait=true --timeout=180s >/dev/null
done
sleep 2

replicate_volume "$SOURCE_REGION" "$source_volume" \
  "$FIRST_TARGET_REGION" "$first_target_volume"
replicate_volume "$SOURCE_REGION" "$source_volume" \
  "$SECOND_TARGET_REGION" "$second_target_volume"

expected="source=${SOURCE_REGION} run=${RUN_ID}"
for region in "$FIRST_TARGET_REGION" "$SECOND_TARGET_REGION"; do
  kubectl -n "$NS" apply -f - <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: verify-${region}
spec:
  restartPolicy: Never
  containers:
    - name: verify
      image: debian:bookworm-slim
      imagePullPolicy: IfNotPresent
      command: ["/bin/sh", "-c", "cat /data/probe"]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: volume-${region}
YAML
  kubectl -n "$NS" wait --for=jsonpath='{.status.phase}'=Succeeded \
    "pod/verify-${region}" --timeout=180s
  actual="$(kubectl -n "$NS" logs "verify-${region}")"
  [ "$actual" = "$expected" ] || {
    echo "replicated contents in simulated region ${region} did not match the fenced source" >&2
    exit 1
  }
  kubectl -n "$NS" delete "pod/verify-${region}" --wait=true --timeout=180s >/dev/null
done

kubectl -n "$NS" apply -f - <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: promote-${FIRST_TARGET_REGION}
spec:
  restartPolicy: Never
  containers:
    - name: promote
      image: debian:bookworm-slim
      imagePullPolicy: IfNotPresent
      command:
        - /bin/sh
        - -c
        - |
          set -eu
          test "\$(cat /data/probe)" = "${expected}"
          printf '%s\n' 'promoted=${FIRST_TARGET_REGION} run=${RUN_ID}' > /data/promotion
          sync
          cat /data/promotion
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: volume-${FIRST_TARGET_REGION}
YAML
kubectl -n "$NS" wait --for=jsonpath='{.status.phase}'=Succeeded \
  "pod/promote-${FIRST_TARGET_REGION}" --timeout=180s
kubectl -n "$NS" logs "promote-${FIRST_TARGET_REGION}" | \
  grep -q "promoted=${FIRST_TARGET_REGION} run=${RUN_ID}"
kubectl -n "$NS" delete "pod/promote-${FIRST_TARGET_REGION}" \
  --wait=true --timeout=180s >/dev/null
sleep 2

# The second transfer proves a stair-step path: the middle release is now the
# source and the newest release receives the already-promoted state.
replicate_volume "$FIRST_TARGET_REGION" "$first_target_volume" \
  "$SECOND_TARGET_REGION" "$second_target_volume"

kubectl -n "$NS" apply -f - <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: promote-${SECOND_TARGET_REGION}
spec:
  restartPolicy: Never
  containers:
    - name: promote
      image: debian:bookworm-slim
      imagePullPolicy: IfNotPresent
      command:
        - /bin/sh
        - -c
        - |
          set -eu
          test "\$(cat /data/probe)" = "${expected}"
          test "\$(cat /data/promotion)" = "promoted=${FIRST_TARGET_REGION} run=${RUN_ID}"
          printf '%s\n' 'promoted=${SECOND_TARGET_REGION} run=${RUN_ID}' >> /data/promotion
          sync
          cat /data/promotion
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: volume-${SECOND_TARGET_REGION}
YAML
kubectl -n "$NS" wait --for=jsonpath='{.status.phase}'=Succeeded \
  "pod/promote-${SECOND_TARGET_REGION}" --timeout=180s
kubectl -n "$NS" logs "promote-${SECOND_TARGET_REGION}" | \
  grep -q "promoted=${SECOND_TARGET_REGION} run=${RUN_ID}"

source_mounting_pods="$(kubectl -n "$NS" get pod -o json | jq \
  '[.items[] | select(.spec.volumes[]?.persistentVolumeClaim.claimName == "volume-'"$SOURCE_REGION"'") | select(.status.phase == "Running")] | length')"
[ "$source_mounting_pods" = 0 ] || {
  echo "source writer was not fenced during promotion" >&2
  exit 1
}

image_source="$(region_image "$SOURCE_REGION")"
image_first="$(region_image "$FIRST_TARGET_REGION")"
image_second="$(region_image "$SECOND_TARGET_REGION")"
printf '%s\n' \
  "ZCCUSAN_LOCAL_REGIONS_FAILOVER_PASS namespaces=zcblock-csi-${SOURCE_REGION},zcblock-csi-${FIRST_TARGET_REGION},zcblock-csi-${SECOND_TARGET_REGION} volumes=3 volume_handles_distinct=true source_writer_fenced=true first_promotion=${FIRST_TARGET_REGION} second_promotion=${SECOND_TARGET_REGION} images=${image_source},${image_first},${image_second} replication=aes-256-authenticated-tcp placement=userspace block_raid=false"
