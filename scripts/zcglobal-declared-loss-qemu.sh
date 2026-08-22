#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export ZCGLOBAL_REPLICATION_MODE=async
export ZCGLOBAL_SCENARIO=declared-loss
export OPERATIONS="${OPERATIONS:-64}"
export DECLARED_LOSS_CHECKPOINT="${DECLARED_LOSS_CHECKPOINT:-32}"
export MOVE_END="${MOVE_END:-96}"
export WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zcglobal-declared-loss}"
export TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-150}"
exec "$ROOT/scripts/zcglobal-volume-failover-qemu.sh" "$@"
