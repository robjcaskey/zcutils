#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export ZCGLOBAL_KUBERNETES=1
export ZCGLOBAL_REPLICATION_MODE="${ZCGLOBAL_REPLICATION_MODE:-async}"
export WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zcglobal-kubernetes-failover}"
export TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-300}"
exec "$ROOT/scripts/zcglobal-volume-failover-qemu.sh" "$@"
