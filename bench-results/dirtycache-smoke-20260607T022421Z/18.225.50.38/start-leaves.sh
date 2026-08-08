#!/usr/bin/env bash
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
BIN=/home/ubuntu/uring-play/target/release/zcutils
RUN_DIR=${RUN_DIR:?}
PORTS=${PORTS:?}
CONNS=${CONNS:?}
CHUNK=${CHUNK:?}
WORKERS=${WORKERS:?}
ZCMEM=${ZCMEM:?}
COMMON_ENV=(
  URING_PLAY_PIN_CPUS=1
  URING_PLAY_SOCKET_BUFFER_BYTES=67108864
  URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_THP=1
  URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_FIRST_TOUCH=1
)
(
  export "${COMMON_ENV[@]}"
  export URING_PLAY_PIN_CPU_LIST=0-7
  /usr/bin/time -v "$BIN" zcnblk-wal-leaf "zcmem:$ZCMEM" 172.31.42.81 39100 "$PORTS" "$CONNS" "$CHUNK" "$WORKERS" true blocking
) > "$RUN_DIR/leaf-card0.log" 2>&1 &
echo $! > "$RUN_DIR/leaf-card0.pid"
(
  export "${COMMON_ENV[@]}"
  export URING_PLAY_PIN_CPU_LIST=96-103
  /usr/bin/time -v "$BIN" zcnblk-wal-leaf "zcmem:$ZCMEM" 172.31.40.9 39200 "$PORTS" "$CONNS" "$CHUNK" "$WORKERS" true blocking
) > "$RUN_DIR/leaf-card1.log" 2>&1 &
echo $! > "$RUN_DIR/leaf-card1.pid"
