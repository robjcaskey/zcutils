#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT=$(cd "$(dirname "$0")" && pwd)
BYTES=${BYTES:-128M}
PORTS=${PORTS:-8}
CONNS=${CONNS:-1}
CHUNK=${CHUNK:-4K}
BATCH=${BATCH:-64}
WINDOW=${WINDOW:-1024}
FAN_WINDOW=${FAN_WINDOW:-64}
run_case() {
  local name=$1
  local pin=$2
  local plan=$3
  local fan_base=$4
  local leaf_base=$5
  local casedir="$OUT/$name"
  mkdir -p "$casedir"
  local pids=()
  cleanup_case() {
    for pid in "${pids[@]:-}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
  }
  trap cleanup_case RETURN
  if [[ "$pin" == 1 ]]; then
    URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-7 URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
      "$ROOT/target/release/zcnblk-wal-leaf" zcdevnull0 127.0.0.1 "$leaf_base" "$PORTS" "$CONNS" "$CHUNK" "$PORTS" true blocking >"$casedir/leaf0.log" 2>&1 &
    pids+=("$!")
    URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=8-15 URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
      "$ROOT/target/release/zcnblk-wal-leaf" zcdevnull1 127.0.0.2 "$leaf_base" "$PORTS" "$CONNS" "$CHUNK" "$PORTS" true blocking >"$casedir/leaf1.log" 2>&1 &
    pids+=("$!")
  else
    URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
      "$ROOT/target/release/zcnblk-wal-leaf" zcdevnull0 127.0.0.1 "$leaf_base" "$PORTS" "$CONNS" "$CHUNK" "$PORTS" false blocking >"$casedir/leaf0.log" 2>&1 &
    pids+=("$!")
    URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
      "$ROOT/target/release/zcnblk-wal-leaf" zcdevnull1 127.0.0.2 "$leaf_base" "$PORTS" "$CONNS" "$CHUNK" "$PORTS" false blocking >"$casedir/leaf1.log" 2>&1 &
    pids+=("$!")
  fi
  sleep 0.4
  local fan_env=(URING_PLAY_ZCNBLK_WRITE_ACKS=1 URING_PLAY_ZCNBLK_BATCH_DEPTH="$BATCH" URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW="$FAN_WINDOW" URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1)
  if [[ "$pin" == 1 ]]; then
    fan_env+=(URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=16-23)
  fi
  if [[ "$plan" == 1 ]]; then
    fan_env+=(URING_PLAY_ZC_PLAN_ID="local-aligned-plan" URING_PLAY_ZC_PLACEMENT_EPOCH=1001)
  fi
  env "${fan_env[@]}" \
    "$ROOT/target/release/zcnblk-fan" --engine wal --leaves 127.0.0.1,127.0.0.2 --bind 127.0.0.1 \
      --base-port "$fan_base" --ports "$PORTS" --connections-per-port "$CONNS" \
      --bytes-per-connection "$BYTES" --chunk-bytes "$CHUNK" --stripe-bytes "$CHUNK" \
      --leaf-base-port "$leaf_base" --pin-handlers "$([[ "$pin" == 1 ]] && printf true || printf false)" --mode stripe >"$casedir/fan.log" 2>&1 &
  pids+=("$!")
  sleep 0.4
  local send_env=(URING_PLAY_ZCNBLK_OP=write URING_PLAY_ZCNBLK_WAIT_WRITE_ACKS=1 URING_PLAY_ZCNBLK_BATCH_DEPTH="$BATCH" URING_PLAY_ZCNBLK_WRITE_WINDOW="$WINDOW")
  if [[ "$pin" == 1 ]]; then
    send_env+=(URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=24-31)
  fi
  env "${send_env[@]}" \
    "$ROOT/target/release/zcutils" zcnblk-send 127.0.0.1 1 "$fan_base" "$PORTS" "$CONNS" "$BYTES" "$CHUNK" "$PORTS" >"$casedir/send.log" 2>&1
  wait "${pids[2]}"
  wait "${pids[0]}"
  wait "${pids[1]}"
  trap - RETURN
}
run_case unpinned-no-plan 0 0 39100 40100
run_case pinned-no-plan 1 0 39200 40200
run_case pinned-plan 1 1 39300 40300
for name in unpinned-no-plan pinned-no-plan pinned-plan; do
  casedir="$OUT/$name"
  {
    printf 'case=%s\n' "$name"
    rg -n 'zcnblk-fan-wal: bytes=|zcnblk-fan-wal-summary|zcnblk-send-summary|zcnblk-send: bytes=|zcnblk-wal-leaf-summary|zcnblk-wal-leaf-plan|plan_hash|PERF WARNING' "$casedir"/*.log || true
  } >"$casedir/summary.txt"
  cat "$casedir/summary.txt"
done
