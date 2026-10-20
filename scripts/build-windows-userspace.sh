#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$(realpath "$0")")/.."
command -v x86_64-w64-mingw32-gcc >/dev/null
command -v nasm >/dev/null
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
cargo build --locked --target x86_64-pc-windows-gnu \
  --bin zcnblk-portable-target --bin zcnblk-client --bin zcnblk-windows-client \
  --bin zccusan-crdgen --bin zccusan-operator --bin zccusan-telemetry-server "$@"
