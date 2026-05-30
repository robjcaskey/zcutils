#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${PORT:-$((28000 + RANDOM % 10000))}"
STATE_DIR="$(mktemp -d /tmp/zccusan-live-switch.XXXXXX)"
LOG_FILE="${STATE_DIR}/zcblock-control.log"
CONTROL_URL="http://127.0.0.1:${PORT}"
CONTROL_PID=""

cleanup() {
  if [ -n "${CONTROL_PID}" ] && kill -0 "${CONTROL_PID}" 2>/dev/null; then
    kill "${CONTROL_PID}" 2>/dev/null || true
    wait "${CONTROL_PID}" 2>/dev/null || true
  fi
  rm -rf "${STATE_DIR}"
}
trap cleanup EXIT

start_control() {
  "${ROOT}/target/debug/zcblock-control" \
    --listen "127.0.0.1:${PORT}" \
    --state-dir "${STATE_DIR}/state" \
    --logstream-path "${STATE_DIR}/state/logstream/zccusan-state.log" \
    >"${LOG_FILE}" 2>&1 &
  CONTROL_PID="$!"
  for _ in $(seq 1 100); do
    if "${ROOT}/target/debug/zcrepl" csi-mode --control-url "${CONTROL_URL}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.05
  done
  cat "${LOG_FILE}" >&2 || true
  echo "zcblock-control did not become ready on ${CONTROL_URL}" >&2
  return 1
}

stop_control() {
  if [ -n "${CONTROL_PID}" ] && kill -0 "${CONTROL_PID}" 2>/dev/null; then
    kill "${CONTROL_PID}" 2>/dev/null || true
    wait "${CONTROL_PID}" 2>/dev/null || true
  fi
  CONTROL_PID=""
}

require_contains() {
  local haystack="$1"
  local needle="$2"
  if [[ "${haystack}" != *"${needle}"* ]]; then
    echo "expected output to contain ${needle}" >&2
    echo "actual: ${haystack}" >&2
    return 1
  fi
}

csi() {
  "${ROOT}/target/debug/zcrepl" "$@" --control-url "${CONTROL_URL}"
}

cd "${ROOT}"
cargo build --bin zcblock-control --bin zcrepl >/dev/null

start_control

out="$(csi csi-mode --mode async)"
require_contains "${out}" "mode=async"

out="$(csi csi-route \
  --scope volume:vol-a \
  --target-cluster b \
  --gateway-endpoint 127.0.0.1:9 \
  --spillover-tier spill-b)"
require_contains "${out}" "target_cluster=b"
require_contains "${out}" "gateway_endpoint=127.0.0.1:9"

out="$(csi csi-route \
  --scope volume:vol-a \
  --target-cluster c \
  --gateway-endpoint c-gateway.zccusan.svc.cluster.local:9443 \
  --spillover-tier spill-c)"
require_contains "${out}" "target_cluster=c"
require_contains "${out}" "gateway_endpoint=c-gateway.zccusan.svc.cluster.local:9443"
require_contains "${out}" "spillover_tier=spill-c"

out="$(csi csi-mode --mode sync)"
require_contains "${out}" "mode=sync"

out="$(csi csi-route --scope volume:vol-a)"
require_contains "${out}" "target_cluster=c"
require_contains "${out}" "gateway_endpoint=c-gateway.zccusan.svc.cluster.local:9443"

out="$(csi csi-mode --scope global)"
require_contains "${out}" "mode=sync"

stop_control
start_control

out="$(csi csi-route --scope volume:vol-a)"
require_contains "${out}" "target_cluster=c"
require_contains "${out}" "gateway_endpoint=c-gateway.zccusan.svc.cluster.local:9443"
require_contains "${out}" "spillover_tier=spill-c"

out="$(csi csi-mode --scope global)"
require_contains "${out}" "mode=sync"

echo "zccusan live switch passed: volume:vol-a route b(unresponsive)->c and async->sync survived restart"
