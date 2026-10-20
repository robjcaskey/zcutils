#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin ]] || { echo 'Run on macOS with Xcode command line tools and Rust installed.' >&2; exit 1; }
# prost-build 0.8 otherwise selects its bundled Intel protoc on Apple Silicon.
export PROTOC="${PROTOC:-$(command -v protoc)}"
# Linux kernel/io_uring/OFI binaries deliberately remain Linux-only.
cargo build --locked \
  --bin zcnblk-client --bin zcnblk-portable-target --bin zcracing-mirror \
  --bin zcglobal-policy-node --bin zcglobal-kubernetes-adapter \
  --bin zccusan-crdgen --bin zccusan-operator --bin zccusan-telemetry-server "$@"
