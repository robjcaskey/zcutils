#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zcvhost-direct-ofi-smoke}"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/qemu-zcvhost-direct-ofi-cargo}"
TARGET_BIN="${TARGET_BIN:-$CARGO_TARGET_DIR/release/zcvhost-ofi-volume}"
TARGET_LOG="${TARGET_LOG:-$WORK_DIR/direct-ofi-target.log}"
DIRECT_OFI_ADDRESS="${DIRECT_OFI_ADDRESS:-127.0.0.1}"
DIRECT_OFI_PROVIDER="${DIRECT_OFI_PROVIDER:-sockets}"
DIRECT_OFI_ENDPOINT="${DIRECT_OFI_ENDPOINT:-rdm}"
DIRECT_OFI_DOMAIN="${DIRECT_OFI_DOMAIN:-}"
DIRECT_OFI_BASE_SERVICE="${DIRECT_OFI_BASE_SERVICE:-37000}"
DIRECT_OFI_CAPACITY_BYTES="${DIRECT_OFI_CAPACITY_BYTES:-67108864}"
QUEUES="${QUEUES:-4}"
TARGET_LANE_CPUS="${TARGET_LANE_CPUS:-}"
BUILD="${BUILD:-1}"

mkdir -p "$WORK_DIR"
if [[ "$BUILD" == 1 ]]; then
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release --bin zcvhost-user-blk --bin zcvhost-ofi-volume
fi
[[ -x "$TARGET_BIN" ]] || { printf 'target binary is not executable: %s\n' "$TARGET_BIN" >&2; exit 1; }

target_args=(
    --bind "$DIRECT_OFI_ADDRESS"
    --provider "$DIRECT_OFI_PROVIDER"
    --endpoint "$DIRECT_OFI_ENDPOINT"
    --base-service "$DIRECT_OFI_BASE_SERVICE"
    --lanes "$QUEUES"
    --capacity-bytes "$DIRECT_OFI_CAPACITY_BYTES"
)
if [[ -n "$DIRECT_OFI_DOMAIN" ]]; then
    target_args+=(--domain "$DIRECT_OFI_DOMAIN")
fi
if [[ -n "$TARGET_LANE_CPUS" ]]; then
    target_args+=(--lane-cpus "$TARGET_LANE_CPUS")
fi
if [[ "${DIRECT_OFI_REQUIRE_HUGETLB:-0}" == 1 ]]; then
    target_args+=(--require-hugetlb)
fi

rm -f -- "$TARGET_LOG"
"$TARGET_BIN" "${target_args[@]}" >"$TARGET_LOG" 2>&1 &
target_pid=$!
cleanup() {
    if kill -0 "$target_pid" 2>/dev/null; then
        kill -TERM "$target_pid"
        wait "$target_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

for _ in $(seq 1 400); do
    if grep -Fq 'zcvhost-ofi-volume:' "$TARGET_LOG"; then
        break
    fi
    kill -0 "$target_pid" 2>/dev/null || {
        printf 'direct OFI target exited during startup\n' >&2
        sed -n '1,240p' "$TARGET_LOG" >&2
        exit 1
    }
    sleep 0.025
done

env \
    WORK_DIR="$WORK_DIR" \
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
    BACKEND="$CARGO_TARGET_DIR/release/zcvhost-user-blk" \
    BUILD=0 \
    QUEUES="$QUEUES" \
    DIRECT_OFI_ADDRESS="$DIRECT_OFI_ADDRESS" \
    DIRECT_OFI_PROVIDER="$DIRECT_OFI_PROVIDER" \
    DIRECT_OFI_ENDPOINT="$DIRECT_OFI_ENDPOINT" \
    DIRECT_OFI_DOMAIN="$DIRECT_OFI_DOMAIN" \
    DIRECT_OFI_BASE_SERVICE="$DIRECT_OFI_BASE_SERVICE" \
    DIRECT_OFI_CAPACITY_BYTES="$DIRECT_OFI_CAPACITY_BYTES" \
    DIRECT_OFI_SLOT_BYTES="${DIRECT_OFI_SLOT_BYTES:-4096}" \
    DIRECT_OFI_REQUIRE_HUGETLB="${DIRECT_OFI_REQUIRE_HUGETLB:-0}" \
    "$ROOT/scripts/zcvhost-user-blk-qemu-smoke.sh"

wait "$target_pid"
target_pid=''
trap - EXIT INT TERM
grep -Fq 'status=closed' "$TARGET_LOG"
grep -Fq 'kernel_block_edge=no' "$WORK_DIR/backend.log"
grep -Fq 'direct_ofi=' "$WORK_DIR/backend.log"
grep -Eq 'zcvhost-user-blk-summary: .*io_errors=0([[:space:]]|$)' "$WORK_DIR/backend.log"
if grep -Fq 'zcnblk_edge=' "$WORK_DIR/backend.log"; then
    printf 'direct OFI backend unexpectedly used the zcnblk kernel block edge\n' >&2
    exit 1
fi
printf 'zcvhost direct-OFI QEMU smoke passed\ntarget_log=%s\n' "$TARGET_LOG"
