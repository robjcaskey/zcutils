#!/usr/bin/env bash
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
BIN=/home/ubuntu/uring-play/target/release/zcutils
RUN_DIR=${RUN_DIR:?}
PORTS=${PORTS:?}
CONNS=${CONNS:?}
BYTES=${BYTES:?}
CHUNK=${CHUNK:?}
WORKERS=${WORKERS:?}
BATCH=${BATCH:?}
WINDOW=${WINDOW:?}
DIRTY_HARD=${DIRTY_HARD:?}
DIRTY_SOFT=${DIRTY_SOFT:?}
common_fan_env=(
  URING_PLAY_PIN_CPUS=1
  URING_PLAY_SOCKET_BUFFER_BYTES=134217728
  URING_PLAY_ZCNBLK_WRITE_ACKS=1
  URING_PLAY_ZCNBLK_BATCH_DEPTH=$BATCH
  URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW=$WINDOW
  URING_PLAY_ZCNBLK_WAL_WRITE_ACK_MODE=admit
  URING_PLAY_ZCNBLK_FAN_WAL_HARD_DIRTY_BYTES=$DIRTY_HARD
  URING_PLAY_ZCNBLK_FAN_WAL_SOFT_DIRTY_BYTES=$DIRTY_SOFT
  URING_PLAY_ZCNBLK_FAN_RESULT_WAIT_POLICY=adaptive
)
common_send_env=(
  URING_PLAY_PIN_CPUS=1
  URING_PLAY_SOCKET_BUFFER_BYTES=134217728
  URING_PLAY_ZCNBLK_OP=write-read-same
  URING_PLAY_ZCNBLK_VERIFY_READS=1
  URING_PLAY_ZCNBLK_WAIT_WRITE_ACKS=1
  URING_PLAY_ZCNBLK_BATCH_DEPTH=$BATCH
  URING_PLAY_ZCNBLK_READ_WINDOW=16384
  URING_PLAY_ZCNBLK_WRITE_WINDOW=16384
  URING_PLAY_ZCNBLK_LATENCY=1
)
(
  export "${common_fan_env[@]}"
  export URING_PLAY_PIN_CPU_LIST=0-31
  export URING_PLAY_SOURCE_IP=172.31.41.87
  export URING_PLAY_ROUTE_PROBE=1
  export URING_PLAY_EXPECT_ROUTE_DEV=ens68
  export URING_PLAY_EXPECT_ROUTE_SRC=172.31.41.87
  /usr/bin/time -v "$BIN" zcnblk-fan --engine wal --leaves 172.31.42.81:39100 --bind 172.31.41.87 --base-port 38100 --ports "$PORTS" --connections-per-port "$CONNS" --bytes-per-connection "$BYTES" --chunk-bytes "$CHUNK" --stripe-bytes "$CHUNK" --leaf-base-port 39100 --pin-handlers true --mode stripe
) > "$RUN_DIR/fan-card0.log" 2>&1 &
echo $! > "$RUN_DIR/fan-card0.pid"
(
  export "${common_fan_env[@]}"
  export URING_PLAY_PIN_CPU_LIST=96-127
  export URING_PLAY_SOURCE_IP=172.31.41.126
  export URING_PLAY_ROUTE_PROBE=1
  export URING_PLAY_EXPECT_ROUTE_DEV=ens146
  export URING_PLAY_EXPECT_ROUTE_SRC=172.31.41.126
  /usr/bin/time -v "$BIN" zcnblk-fan --engine wal --leaves 172.31.40.9:39200 --bind 172.31.41.126 --base-port 38200 --ports "$PORTS" --connections-per-port "$CONNS" --bytes-per-connection "$BYTES" --chunk-bytes "$CHUNK" --stripe-bytes "$CHUNK" --leaf-base-port 39200 --pin-handlers true --mode stripe
) > "$RUN_DIR/fan-card1.log" 2>&1 &
echo $! > "$RUN_DIR/fan-card1.pid"
sleep 3
(
  export "${common_send_env[@]}"
  export URING_PLAY_PIN_CPU_LIST=32-63
  export URING_PLAY_SOURCE_IP=172.31.41.87
  /usr/bin/time -v "$BIN" zcnblk-send 172.31.41.87 1 38100 "$PORTS" "$CONNS" "$BYTES" "$CHUNK" "$WORKERS"
) > "$RUN_DIR/send-card0.log" 2>&1 &
echo $! > "$RUN_DIR/send-card0.pid"
(
  export "${common_send_env[@]}"
  export URING_PLAY_PIN_CPU_LIST=128-159
  export URING_PLAY_SOURCE_IP=172.31.41.126
  /usr/bin/time -v "$BIN" zcnblk-send 172.31.41.126 1 38200 "$PORTS" "$CONNS" "$BYTES" "$CHUNK" "$WORKERS"
) > "$RUN_DIR/send-card1.log" 2>&1 &
echo $! > "$RUN_DIR/send-card1.pid"
wait "$(cat "$RUN_DIR/send-card0.pid")" "$(cat "$RUN_DIR/send-card1.pid")"
wait "$(cat "$RUN_DIR/fan-card0.pid")" "$(cat "$RUN_DIR/fan-card1.pid")"
