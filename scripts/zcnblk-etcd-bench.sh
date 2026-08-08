#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: zcnblk-etcd-bench.sh RESULT_DIR DATA_ROOT

Runs a single-member etcd benchmark in a newly-created directory below
DATA_ROOT. DATA_ROOT must already be on the filesystem being measured.

Required environment:
  ETCD_BIN       Path to etcd
  ETCDCTL_BIN    Path to etcdctl
  BENCHMARK_BIN  Path to etcd's official tools/benchmark binary

Optional environment:
  CLIENT_PORT=22379 PEER_PORT=22380
  SERVER_CPUS=0-3 CLIENT_CPUS=4-11
  PUT_TOTAL=100000 RANGE_TOTAL=100000 MIXED_TOTAL=50000
  CLIENTS=64 CONNS=8 KEY_SPACE_SIZE=100000 VALUE_SIZE=256
  EXPECT_ZCNBLK=0
  COORDINATION_RESULT=unknown
EOF
}

[[ $# -eq 2 ]] || { usage >&2; exit 2; }
RESULT_DIR=$1
DATA_ROOT=$2
: "${ETCD_BIN:?ETCD_BIN is required}"
: "${BENCHMARK_BIN:?BENCHMARK_BIN is required}"

: "${ETCDCTL_BIN:?ETCDCTL_BIN is required to seed and verify range data}"
CLIENT_PORT=${CLIENT_PORT:-22379}
PEER_PORT=${PEER_PORT:-22380}
SERVER_CPUS=${SERVER_CPUS:-0-3}
CLIENT_CPUS=${CLIENT_CPUS:-4-11}
PUT_TOTAL=${PUT_TOTAL:-100000}
RANGE_TOTAL=${RANGE_TOTAL:-100000}
MIXED_TOTAL=${MIXED_TOTAL:-50000}
CLIENTS=${CLIENTS:-64}
CONNS=${CONNS:-8}
KEY_SPACE_SIZE=${KEY_SPACE_SIZE:-100000}
VALUE_SIZE=${VALUE_SIZE:-256}
COORDINATION_RESULT=${COORDINATION_RESULT:-unknown}
ETCD_BENCHMARK_SOURCE=${ETCD_BENCHMARK_SOURCE:-unknown}
EXPECT_ZCNBLK=${EXPECT_ZCNBLK:-0}
ENDPOINT="127.0.0.1:${CLIENT_PORT}"

for tool in "$ETCD_BIN" "$ETCDCTL_BIN" "$BENCHMARK_BIN" taskset; do
    [[ -x "$tool" ]] || command -v "$tool" >/dev/null 2>&1 || {
        echo "required executable not found: $tool" >&2
        exit 2
    }
done
[[ -d "$DATA_ROOT" && -w "$DATA_ROOT" ]] || {
    echo "DATA_ROOT must be an existing writable directory: $DATA_ROOT" >&2
    exit 2
}
mkdir -p "$RESULT_DIR"
RESULT_DIR=$(realpath "$RESULT_DIR")
DATA_ROOT=$(realpath "$DATA_ROOT")
if [[ "$EXPECT_ZCNBLK" == 1 ]]; then
    mount_source=$(findmnt -T "$DATA_ROOT" -n -o SOURCE)
    [[ "$mount_source" == /dev/zcnblk0 ]] || {
        echo "EXPECT_ZCNBLK=1 but DATA_ROOT is mounted from $mount_source" >&2
        exit 1
    }
fi
DATA_DIR=$(mktemp -d "$DATA_ROOT/zcutils-etcd-data.XXXXXX")
ETCD_PID=

cleanup() {
    if [[ -n "$ETCD_PID" ]] && kill -0 "$ETCD_PID" 2>/dev/null; then
        kill -TERM "$ETCD_PID" 2>/dev/null || true
        for _ in $(seq 1 50); do
            kill -0 "$ETCD_PID" 2>/dev/null || break
            sleep 0.1
        done
        if kill -0 "$ETCD_PID" 2>/dev/null; then
            kill -KILL "$ETCD_PID" 2>/dev/null || true
        fi
        wait "$ETCD_PID" 2>/dev/null || true
    fi
    rm -rf -- "$DATA_DIR"
}
trap cleanup EXIT INT TERM

context_snapshot() {
    local label=$1
    awk '
        /^voluntary_ctxt_switches:/ { voluntary += $2 }
        /^nonvoluntary_ctxt_switches:/ { involuntary += $2 }
        END {
            print "voluntary_ctxt_switches:", voluntary + 0
            print "nonvoluntary_ctxt_switches:", involuntary + 0
        }
    ' "/proc/$ETCD_PID"/task/*/status >"$RESULT_DIR/context-${label}.txt"
}

record_cmd() {
    printf '%q ' "$@" >>"$RESULT_DIR/commands.sh"
    printf '\n' >>"$RESULT_DIR/commands.sh"
}

run_benchmark() {
    local name=$1
    shift
    context_snapshot "${name}-before"
    record_cmd taskset -c "$CLIENT_CPUS" "$BENCHMARK_BIN" --endpoints "$ENDPOINT" \
        --clients "$CLIENTS" --conns "$CONNS" --precise "$@"
    /usr/bin/time -f 'elapsed_seconds=%e\nuser_seconds=%U\nsystem_seconds=%S\nmax_rss_kb=%M\nvoluntary_context_switches=%w\ninvoluntary_context_switches=%c' \
        -o "$RESULT_DIR/${name}.time" \
        taskset -c "$CLIENT_CPUS" "$BENCHMARK_BIN" --endpoints "$ENDPOINT" \
        --clients "$CLIENTS" --conns "$CONNS" --precise "$@" \
        >"$RESULT_DIR/${name}.log" 2>&1
    context_snapshot "${name}-after"
}

{
    echo "timestamp_utc=$(date -u +%FT%TZ)"
    echo "hostname=$(hostname)"
    echo "kernel=$(uname -srvo)"
    echo "data_root=$DATA_ROOT"
    echo "data_mount=$(findmnt -T "$DATA_ROOT" -n -o SOURCE,TARGET,FSTYPE,OPTIONS)"
    echo "result_dir=$RESULT_DIR"
    echo "server_cpus=$SERVER_CPUS"
    echo "client_cpus=$CLIENT_CPUS"
    echo "clients=$CLIENTS"
    echo "connections=$CONNS"
    echo "key_space_size=$KEY_SPACE_SIZE"
    echo "value_size=$VALUE_SIZE"
    echo "coordination=$COORDINATION_RESULT"
    echo "benchmark_source=$ETCD_BENCHMARK_SOURCE"
    echo "benchmark_sha256=$(sha256sum "$BENCHMARK_BIN" | awk '{print $1}')"
    echo "zcnblk_device=$(test -b /dev/zcnblk0 && echo present || echo absent)"
    lscpu
} >"$RESULT_DIR/topology.txt"
"$ETCD_BIN" --version >"$RESULT_DIR/etcd-version.txt" 2>&1
"$BENCHMARK_BIN" version >"$RESULT_DIR/benchmark-version.txt" 2>&1 || \
    "$BENCHMARK_BIN" --help >"$RESULT_DIR/benchmark-version.txt" 2>&1

record_cmd taskset -c "$SERVER_CPUS" "$ETCD_BIN" --data-dir "$DATA_DIR"
taskset -c "$SERVER_CPUS" "$ETCD_BIN" \
    --name zcutils-etcd-bench \
    --data-dir "$DATA_DIR" \
    --listen-client-urls "http://$ENDPOINT" \
    --advertise-client-urls "http://$ENDPOINT" \
    --listen-peer-urls "http://127.0.0.1:$PEER_PORT" \
    --initial-advertise-peer-urls "http://127.0.0.1:$PEER_PORT" \
    --initial-cluster "zcutils-etcd-bench=http://127.0.0.1:$PEER_PORT" \
    --initial-cluster-state new \
    --logger zap --log-level warn \
    >"$RESULT_DIR/etcd.log" 2>&1 &
ETCD_PID=$!
echo "$ETCD_PID" >"$RESULT_DIR/etcd.pid"

ready=false
for _ in $(seq 1 100); do
    if env -u ETCDCTL_BIN "$ETCDCTL_BIN" --endpoints="http://$ENDPOINT" endpoint health >/dev/null 2>&1; then
        ready=true
        break
    fi
    kill -0 "$ETCD_PID" 2>/dev/null || {
        echo "etcd exited during startup" >&2
        exit 1
    }
    sleep 0.1
done
[[ "$ready" == true ]] || { echo "etcd readiness timeout" >&2; exit 1; }
record_cmd env -u ETCDCTL_BIN "$ETCDCTL_BIN" --endpoints="http://$ENDPOINT" put zcutils-bench-key benchmark-value
env -u ETCDCTL_BIN "$ETCDCTL_BIN" --endpoints="http://$ENDPOINT" put zcutils-bench-key benchmark-value \
    >"$RESULT_DIR/seed.log" 2>&1
[[ $(env -u ETCDCTL_BIN "$ETCDCTL_BIN" --endpoints="http://$ENDPOINT" get zcutils-bench-key --print-value-only) == benchmark-value ]] || {
    echo "seed key verification failed" >&2
    exit 1
}

run_benchmark put put --total "$PUT_TOTAL" --key-size 16 \
    --key-space-size "$KEY_SPACE_SIZE" --val-size "$VALUE_SIZE" --sequential-keys
run_benchmark range-linearizable range --total "$RANGE_TOTAL" \
    --consistency l --limit 1 zcutils-bench-key

run_benchmark mixed txn-mixed --total "$MIXED_TOTAL" --rw-ratio 1 \
    --consistency l --limit 1 --key-size 16 --key-space-size "$KEY_SPACE_SIZE" \
    --val-size "$VALUE_SIZE" zcutils-bench-key

python3 - "$RESULT_DIR" <<'PY'
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
lines = []
for path in sorted(root.glob("*.log")):
    if path.name in {"coordination.log", "etcd.log", "seed.log"}:
        continue
    text = path.read_text(errors="replace")
    total = re.search(r"Total:\s+([0-9.]+)", text)
    rps = re.search(r"Requests/sec:\s+([0-9.]+)", text)
    avg = re.search(r"Average:\s+([0-9.]+) secs", text)
    p50 = re.search(r"50% in ([0-9.]+) secs", text)
    p90 = re.search(r"90% in ([0-9.]+) secs", text)
    p99 = re.search(r"99% in ([0-9.]+) secs", text)
    lines.append(",".join([
        path.stem,
        total.group(1) if total else "",
        rps.group(1) if rps else "",
        avg.group(1) if avg else "",
        p50.group(1) if p50 else "",
        p90.group(1) if p90 else "",
        p99.group(1) if p99 else "",
    ]))
(root / "summary.csv").write_text(
    "phase,total_seconds,requests_per_second,average_seconds,p50_seconds,p90_seconds,p99_seconds\n"
    + "\n".join(lines) + "\n"
)

def ctxt(path):
    values = {}
    for line in path.read_text().splitlines():
        key, value = line.split(":", 1)
        values[key] = int(value.strip())
    return values

out = ["phase,voluntary_context_switches,nonvoluntary_context_switches"]
for before in sorted(root.glob("context-*-before.txt")):
    phase = before.name.removeprefix("context-").removesuffix("-before.txt")
    after = root / f"context-{phase}-after.txt"
    if not after.exists():
        continue
    a, b = ctxt(before), ctxt(after)
    out.append(f"{phase},{b['voluntary_ctxt_switches']-a['voluntary_ctxt_switches']},"
               f"{b['nonvoluntary_ctxt_switches']-a['nonvoluntary_ctxt_switches']}")
(root / "context-switches.csv").write_text("\n".join(out) + "\n")
PY

echo "results=$RESULT_DIR"
cat "$RESULT_DIR/summary.csv"
cat "$RESULT_DIR/context-switches.csv"
