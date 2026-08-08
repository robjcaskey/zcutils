#!/usr/bin/env bash
set -euo pipefail

usage() {
	cat <<'EOF'
Usage: zcnblk-kafka-bench.sh RESULT_DIR DATA_ROOT

Runs a single-node Apache Kafka KRaft broker with all persistent directories
below DATA_ROOT. It measures forced-flush production, batched page-cache-
acknowledged production, and consumption with Kafka's bundled performance tools.

Required environment:
  KAFKA_HOME       Extracted Apache Kafka binary distribution
  JAVA_HOME        Supported Java 17 installation

Optional environment:
  BROKER_PORT=19092 CONTROLLER_PORT=19093
  SERVER_CPUS=4-15 CLIENT_CPUS=0,16-23
  DURABLE_RECORDS=2000 STREAM_RECORDS=20000 RECORD_SIZE=4096 PARTITIONS=4
  MAX_HEAP_SIZE=2G STARTUP_TIMEOUT_SECONDS=120
  EXPECT_ZCNBLK=0 COORDINATION_RESULT=unknown KAFKA_ARCHIVE_SHA512=unknown
EOF
}

[ "$#" -eq 2 ] || { usage >&2; exit 2; }
RESULT_DIR="$1"
DATA_ROOT="$2"
: "${KAFKA_HOME:?KAFKA_HOME is required}"
: "${JAVA_HOME:?JAVA_HOME is required}"
export KAFKA_HOME JAVA_HOME

BROKER_PORT="${BROKER_PORT:-19092}"
CONTROLLER_PORT="${CONTROLLER_PORT:-19093}"
SERVER_CPUS="${SERVER_CPUS:-4-15}"
CLIENT_CPUS="${CLIENT_CPUS:-0,16-23}"
DURABLE_RECORDS="${DURABLE_RECORDS:-2000}"
STREAM_RECORDS="${STREAM_RECORDS:-20000}"
RECORD_SIZE="${RECORD_SIZE:-4096}"
PARTITIONS="${PARTITIONS:-4}"
MAX_HEAP_SIZE="${MAX_HEAP_SIZE:-2G}"
STARTUP_TIMEOUT_SECONDS="${STARTUP_TIMEOUT_SECONDS:-120}"
EXPECT_ZCNBLK="${EXPECT_ZCNBLK:-0}"
COORDINATION_RESULT="${COORDINATION_RESULT:-unknown}"
KAFKA_ARCHIVE_SHA512="${KAFKA_ARCHIVE_SHA512:-unknown}"
BOOTSTRAP="127.0.0.1:$BROKER_PORT"

STORAGE="$KAFKA_HOME/bin/kafka-storage.sh"
SERVER="$KAFKA_HOME/bin/kafka-server-start.sh"
TOPICS="$KAFKA_HOME/bin/kafka-topics.sh"
CONFIGS="$KAFKA_HOME/bin/kafka-configs.sh"
OFFSETS="$KAFKA_HOME/bin/kafka-get-offsets.sh"
PRODUCER="$KAFKA_HOME/bin/kafka-producer-perf-test.sh"
CONSUMER="$KAFKA_HOME/bin/kafka-consumer-perf-test.sh"
QUORUM="$KAFKA_HOME/bin/kafka-metadata-quorum.sh"

die() { printf 'zcnblk-kafka-bench: ERROR: %s\n' "$*" >&2; exit 1; }
for tool in "$STORAGE" "$SERVER" "$TOPICS" "$CONFIGS" "$OFFSETS" \
	"$PRODUCER" "$CONSUMER" "$QUORUM" "$JAVA_HOME/bin/java" taskset; do
	[ -x "$tool" ] || command -v "$tool" >/dev/null 2>&1 ||
		die "required executable not found: $tool"
done
[ -d "$DATA_ROOT" ] && [ -w "$DATA_ROOT" ] ||
	die "DATA_ROOT must be an existing writable directory: $DATA_ROOT"

mkdir -p "$RESULT_DIR"
RESULT_DIR="$(realpath "$RESULT_DIR")"
DATA_ROOT="$(realpath "$DATA_ROOT")"
if [ "$EXPECT_ZCNBLK" = 1 ]; then
	mount_source="$(findmnt -T "$DATA_ROOT" -n -o SOURCE)"
	[ "$mount_source" = /dev/zcnblk0 ] ||
		die "EXPECT_ZCNBLK=1 but DATA_ROOT is mounted from $mount_source"
fi
DATA_DIR="$(mktemp -d "$DATA_ROOT/zcutils-kafka-data.XXXXXX")"
CONF="$RESULT_DIR/server.properties"
SERVER_LOG_DIR="$RESULT_DIR/server-logs"
KAFKA_PID=

stop_kafka() {
	local cmdline state
	[ -n "$KAFKA_PID" ] && [ -r "/proc/$KAFKA_PID/cmdline" ] || return 0
	cmdline="$(tr '\0' ' ' <"/proc/$KAFKA_PID/cmdline")"
	[[ "$cmdline" == *kafka.Kafka*"$CONF"* ]] ||
		die "refusing to signal reused pid=$KAFKA_PID cmdline=$cmdline"
	kill -TERM "$KAFKA_PID"
	for _ in $(seq 1 600); do
		[ -r "/proc/$KAFKA_PID/status" ] || break
		state="$(awk '/^State:/{print $2}' "/proc/$KAFKA_PID/status")"
		[ "$state" != Z ] || break
		sleep 0.1
	done
	state="$(awk '/^State:/{print $2}' "/proc/$KAFKA_PID/status" 2>/dev/null || true)"
	if [ -n "$state" ] && [ "$state" != Z ]; then
		kill -KILL "$KAFKA_PID"
	fi
	wait "$KAFKA_PID" 2>/dev/null || true
}

cleanup() {
	local status=$?
	trap - EXIT INT TERM
	stop_kafka
	rm -rf -- "$DATA_DIR"
	exit "$status"
}
trap cleanup EXIT INT TERM

context_snapshot() {
	local output="$1"
	awk '
		/^voluntary_ctxt_switches:/ { voluntary += $2 }
		/^nonvoluntary_ctxt_switches:/ { involuntary += $2 }
		END {
			print "voluntary_ctxt_switches:", voluntary + 0
			print "nonvoluntary_ctxt_switches:", involuntary + 0
		}
	' "/proc/$KAFKA_PID"/task/*/status >"$output"
}

record_cmd() {
	printf '%q ' "$@" >>"$RESULT_DIR/commands.sh"
	printf '\n' >>"$RESULT_DIR/commands.sh"
}

run_phase() {
	local phase="$1"
	shift
	context_snapshot "$RESULT_DIR/context-$phase-before.txt"
	record_cmd taskset -c "$CLIENT_CPUS" "$@"
	/usr/bin/time \
		-f 'elapsed_seconds=%e\nuser_seconds=%U\nsystem_seconds=%S\nmax_rss_kb=%M\nvoluntary_context_switches=%w\ninvoluntary_context_switches=%c' \
		-o "$RESULT_DIR/$phase.time" \
		taskset -c "$CLIENT_CPUS" "$@" >"$RESULT_DIR/$phase.log" 2>&1
	context_snapshot "$RESULT_DIR/context-$phase-after.txt"
}

for port in "$BROKER_PORT" "$CONTROLLER_PORT"; do
	ss -H -ltn "sport = :$port" | grep -q . && die "TCP port $port is already in use"
done
mkdir -p "$DATA_DIR/logs" "$DATA_DIR/metadata" "$SERVER_LOG_DIR"

cat >"$CONF" <<EOF
process.roles=broker,controller
node.id=1
controller.quorum.bootstrap.servers=127.0.0.1:$CONTROLLER_PORT
listeners=PLAINTEXT://127.0.0.1:$BROKER_PORT,CONTROLLER://127.0.0.1:$CONTROLLER_PORT
advertised.listeners=PLAINTEXT://127.0.0.1:$BROKER_PORT,CONTROLLER://127.0.0.1:$CONTROLLER_PORT
inter.broker.listener.name=PLAINTEXT
controller.listener.names=CONTROLLER
listener.security.protocol.map=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT
log.dirs=$DATA_DIR/logs
metadata.log.dir=$DATA_DIR/metadata
num.network.threads=4
num.io.threads=8
num.recovery.threads.per.data.dir=2
socket.send.buffer.bytes=1048576
socket.receive.buffer.bytes=1048576
socket.request.max.bytes=104857600
num.partitions=$PARTITIONS
default.replication.factor=1
min.insync.replicas=1
offsets.topic.replication.factor=1
share.coordinator.state.topic.replication.factor=1
share.coordinator.state.topic.min.isr=1
transaction.state.log.replication.factor=1
transaction.state.log.min.isr=1
group.initial.rebalance.delay.ms=0
auto.create.topics.enable=false
log.cleaner.enable=false
log.retention.hours=1
log.segment.bytes=1073741824
EOF

{
	printf 'timestamp_utc=%s\n' "$(date -u +%FT%TZ)"
	printf 'hostname=%s\nkernel=%s\n' "$(hostname)" "$(uname -srvo)"
	printf 'data_root=%s\ndata_mount=%s\n' "$DATA_ROOT" "$(findmnt -T "$DATA_ROOT" -n -o SOURCE,TARGET,FSTYPE,OPTIONS)"
	printf 'server_cpus=%s\nclient_cpus=%s\n' "$SERVER_CPUS" "$CLIENT_CPUS"
	printf 'durable_records=%s\nstream_records=%s\nrecord_size=%s\npartitions=%s\n' \
		"$DURABLE_RECORDS" "$STREAM_RECORDS" "$RECORD_SIZE" "$PARTITIONS"
	printf 'durable_completion=acks-all+topic-flush-every-record+one-record-request\n'
	printf 'stream_completion=leader-page-cache-ack+batched-request\n'
	printf 'coordination=%s\nkafka_archive_sha512=%s\n' "$COORDINATION_RESULT" "$KAFKA_ARCHIVE_SHA512"
	printf 'zcnblk_device=%s\n' "$(test -b /dev/zcnblk0 && printf present || printf absent)"
	lscpu
} >"$RESULT_DIR/topology.txt"
"$JAVA_HOME/bin/java" -version >"$RESULT_DIR/java-version.txt" 2>&1
env JAVA_HOME="$JAVA_HOME" "$PRODUCER" --help >"$RESULT_DIR/producer-version.txt" 2>&1 || true

cluster_id="$(env JAVA_HOME="$JAVA_HOME" "$STORAGE" random-uuid)"
printf '%s\n' "$cluster_id" >"$RESULT_DIR/cluster-id.txt"
record_cmd env JAVA_HOME="$JAVA_HOME" "$STORAGE" format --standalone -t "$cluster_id" -c "$CONF"
env JAVA_HOME="$JAVA_HOME" "$STORAGE" format --standalone -t "$cluster_id" -c "$CONF" \
	>"$RESULT_DIR/format.log" 2>&1

record_cmd taskset -c "$SERVER_CPUS" "$SERVER" "$CONF"
taskset -c "$SERVER_CPUS" env JAVA_HOME="$JAVA_HOME" KAFKA_HEAP_OPTS="-Xms$MAX_HEAP_SIZE -Xmx$MAX_HEAP_SIZE" \
	LOG_DIR="$SERVER_LOG_DIR" "$SERVER" "$CONF" >"$RESULT_DIR/server.log" 2>&1 &
KAFKA_PID=$!
printf '%s\n' "$KAFKA_PID" >"$RESULT_DIR/kafka.pid"

ready=false
startup_deadline=$((SECONDS + STARTUP_TIMEOUT_SECONDS))
while (( SECONDS < startup_deadline )); do
	if taskset -c "$CLIENT_CPUS" env JAVA_HOME="$JAVA_HOME" "$TOPICS" \
		--bootstrap-server "$BOOTSTRAP" --list >/dev/null 2>&1; then
		ready=true
		break
	fi
	kill -0 "$KAFKA_PID" 2>/dev/null || {
		tail -n 100 "$RESULT_DIR/server.log" >&2
		die 'Kafka exited during startup'
	}
	sleep 0.2
done
[ "$ready" = true ] || die 'Kafka readiness timeout'

taskset -c "$CLIENT_CPUS" env JAVA_HOME="$JAVA_HOME" "$TOPICS" \
	--bootstrap-server "$BOOTSTRAP" --create --topic zcutils-durable \
	--partitions 1 --replication-factor 1 --config flush.messages=1 \
	>"$RESULT_DIR/create-durable.log" 2>&1
taskset -c "$CLIENT_CPUS" env JAVA_HOME="$JAVA_HOME" "$TOPICS" \
	--bootstrap-server "$BOOTSTRAP" --create --topic zcutils-stream \
	--partitions "$PARTITIONS" --replication-factor 1 \
	>"$RESULT_DIR/create-stream.log" 2>&1
env JAVA_HOME="$JAVA_HOME" "$CONFIGS" --bootstrap-server "$BOOTSTRAP" \
	--describe --entity-type topics --entity-name zcutils-durable \
	>"$RESULT_DIR/durable-topic-config.txt" 2>&1
grep -q 'flush.messages=1' "$RESULT_DIR/durable-topic-config.txt" ||
	die 'durable topic did not retain flush.messages=1'

run_phase producer-durable "$PRODUCER" --bootstrap-server "$BOOTSTRAP" \
	--topic zcutils-durable --num-records "$DURABLE_RECORDS" --record-size "$RECORD_SIZE" \
	--throughput -1 --reporting-interval 1000 --command-property \
	acks=all enable.idempotence=false batch.size=0 linger.ms=0 \
	max.in.flight.requests.per.connection=1 compression.type=none

run_phase producer-stream "$PRODUCER" --bootstrap-server "$BOOTSTRAP" \
	--topic zcutils-stream --num-records "$STREAM_RECORDS" --record-size "$RECORD_SIZE" \
	--throughput -1 --reporting-interval 1000 --command-property \
	acks=1 enable.idempotence=false batch.size=1048576 linger.ms=5 \
	max.in.flight.requests.per.connection=5 compression.type=none buffer.memory=268435456

env JAVA_HOME="$JAVA_HOME" "$OFFSETS" --bootstrap-server "$BOOTSTRAP" \
	--topic zcutils-durable >"$RESULT_DIR/durable-offsets.txt" 2>&1
durable_offsets="$(awk -F: '{sum += $3} END {print sum + 0}' "$RESULT_DIR/durable-offsets.txt")"
[ "$durable_offsets" -eq "$DURABLE_RECORDS" ] ||
	die "durable topic offset count $durable_offsets != $DURABLE_RECORDS"
env JAVA_HOME="$JAVA_HOME" "$OFFSETS" --bootstrap-server "$BOOTSTRAP" \
	--topic zcutils-stream >"$RESULT_DIR/stream-offsets.txt" 2>&1
stream_offsets="$(awk -F: '{sum += $3} END {print sum + 0}' "$RESULT_DIR/stream-offsets.txt")"
[ "$stream_offsets" -eq "$STREAM_RECORDS" ] ||
	die "stream topic offset count $stream_offsets != $STREAM_RECORDS"

run_phase consumer-stream "$CONSUMER" --bootstrap-server "$BOOTSTRAP" \
	--topic zcutils-stream --num-records "$STREAM_RECORDS" \
	--group "zcutils-consumer-$(date +%s%N)" --timeout 30000 --hide-header

env JAVA_HOME="$JAVA_HOME" "$QUORUM" --bootstrap-server "$BOOTSTRAP" \
	describe --status >"$RESULT_DIR/quorum-status.txt" 2>&1

python3 - "$RESULT_DIR" <<'PY'
import csv
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
producer_pattern = re.compile(
    r"(?P<records>[0-9]+) records sent, (?P<rate>[0-9.]+) records/sec "
    r"\((?P<mbps>[0-9.]+) MB/sec\), (?P<avg>[0-9.]+) ms avg latency, "
    r"(?P<max>[0-9.]+) ms max latency, (?P<p50>[0-9.]+) ms 50th, "
    r"(?P<p95>[0-9.]+) ms 95th, (?P<p99>[0-9.]+) ms 99th, "
    r"(?P<p999>[0-9.]+) ms 99.9th"
)
rows = []
for phase in ("producer-durable", "producer-stream"):
    matches = list(producer_pattern.finditer((root / f"{phase}.log").read_text(errors="replace")))
    if not matches:
        raise SystemExit(f"could not parse {phase} output")
    values = matches[-1].groupdict()
    rows.append([phase, values["records"], values["rate"], values["mbps"],
                 values["avg"], values["p50"], values["p95"], values["p99"],
                 values["p999"], values["max"]])
with (root / "summary.csv").open("w", newline="") as handle:
    writer = csv.writer(handle)
    writer.writerow(["phase", "records", "records_per_second", "MB_per_second",
                     "latency_avg_ms", "latency_p50_ms", "latency_p95_ms",
                     "latency_p99_ms", "latency_p999_ms", "latency_max_ms"])
    writer.writerows(rows)

consumer_lines = [line for line in (root / "consumer-stream.log").read_text().splitlines()
                  if line.strip() and not line.startswith("start.time")]
if not consumer_lines:
    raise SystemExit("could not parse consumer output")
(root / "consumer-summary.csv").write_text(
    "start_time,end_time,data_consumed_MB,MB_per_second,messages,messages_per_second,"
    "rebalance_time_ms,fetch_time_ms,fetch_MB_per_second,fetch_messages_per_second\n"
    + consumer_lines[-1] + "\n"
)
PY

{
	printf 'phase,voluntary_context_switches,nonvoluntary_context_switches\n'
	for phase in producer-durable producer-stream consumer-stream; do
		awk -F: -v phase="$phase" '
			NR == FNR { before[$1]=$2 + 0; next }
			{ after[$1]=$2 + 0 }
			END {
				printf "%s,%d,%d\n", phase,
					after["voluntary_ctxt_switches"]-before["voluntary_ctxt_switches"],
					after["nonvoluntary_ctxt_switches"]-before["nonvoluntary_ctxt_switches"]
			}
		' "$RESULT_DIR/context-$phase-before.txt" "$RESULT_DIR/context-$phase-after.txt"
	done
} >"$RESULT_DIR/context-switches.csv"

printf 'results=%s\n' "$RESULT_DIR"
cat "$RESULT_DIR/summary.csv"
cat "$RESULT_DIR/consumer-summary.csv"
cat "$RESULT_DIR/context-switches.csv"
