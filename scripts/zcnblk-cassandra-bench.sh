#!/usr/bin/env bash
set -euo pipefail

usage() {
	cat <<'EOF'
Usage: zcnblk-cassandra-bench.sh RESULT_DIR DATA_ROOT

Runs single-node Apache Cassandra with all persistent directories below a new
directory in DATA_ROOT, then measures durable writes, reads, and a 50/50 mix
with Cassandra's official cassandra-stress tool.

Required environment:
  CASSANDRA_HOME    Extracted Apache Cassandra distribution
  JAVA_HOME         Supported Java 17 installation

Optional environment:
  SERVER_CPUS=0-7 CLIENT_CPUS=8-15
  NATIVE_PORT=9142 STORAGE_PORT=7100 SSL_STORAGE_PORT=7101 JMX_PORT=7299
  SEED_TOTAL=20000 WRITE_TOTAL=50000 READ_TOTAL=50000 MIXED_TOTAL=50000
  CLIENT_THREADS=32 CONNECTIONS_PER_HOST=8 VALUE_SIZE=256
  MAX_HEAP_SIZE=4G MAX_DIRECT_MEMORY_SIZE=2G
  STARTUP_TIMEOUT_SECONDS=180
  EXPECT_ZCNBLK=0 COORDINATION_RESULT=unknown
EOF
}

[ "$#" -eq 2 ] || { usage >&2; exit 2; }
RESULT_DIR="$1"
DATA_ROOT="$2"
: "${CASSANDRA_HOME:?CASSANDRA_HOME is required}"
: "${JAVA_HOME:?JAVA_HOME is required}"
export CASSANDRA_HOME JAVA_HOME

SERVER_CPUS="${SERVER_CPUS:-0-7}"
CLIENT_CPUS="${CLIENT_CPUS:-8-15}"
NATIVE_PORT="${NATIVE_PORT:-9142}"
STORAGE_PORT="${STORAGE_PORT:-7100}"
SSL_STORAGE_PORT="${SSL_STORAGE_PORT:-7101}"
JMX_PORT="${JMX_PORT:-7299}"
SEED_TOTAL="${SEED_TOTAL:-20000}"
WRITE_TOTAL="${WRITE_TOTAL:-50000}"
READ_TOTAL="${READ_TOTAL:-50000}"
MIXED_TOTAL="${MIXED_TOTAL:-50000}"
CLIENT_THREADS="${CLIENT_THREADS:-32}"
CONNECTIONS_PER_HOST="${CONNECTIONS_PER_HOST:-8}"
VALUE_SIZE="${VALUE_SIZE:-256}"
MAX_HEAP_SIZE="${MAX_HEAP_SIZE:-4G}"
MAX_DIRECT_MEMORY_SIZE="${MAX_DIRECT_MEMORY_SIZE:-2G}"
STARTUP_TIMEOUT_SECONDS="${STARTUP_TIMEOUT_SECONDS:-180}"
EXPECT_ZCNBLK="${EXPECT_ZCNBLK:-0}"
COORDINATION_RESULT="${COORDINATION_RESULT:-unknown}"

CASSANDRA_BIN="$CASSANDRA_HOME/bin/cassandra"
CASSANDRA_STRESS="$CASSANDRA_HOME/tools/bin/cassandra-stress"
NODETOOL="$CASSANDRA_HOME/bin/nodetool"
CQLSH="$CASSANDRA_HOME/bin/cqlsh"

die() { printf 'zcnblk-cassandra-bench: ERROR: %s\n' "$*" >&2; exit 1; }

for tool in "$CASSANDRA_BIN" "$CASSANDRA_STRESS" "$NODETOOL" "$CQLSH" \
	"$JAVA_HOME/bin/java" taskset; do
	[ -x "$tool" ] || command -v "$tool" >/dev/null 2>&1 || \
		die "required executable not found: $tool"
done
[ -d "$DATA_ROOT" ] && [ -w "$DATA_ROOT" ] || \
	die "DATA_ROOT must be an existing writable directory: $DATA_ROOT"

mkdir -p "$RESULT_DIR"
RESULT_DIR="$(realpath "$RESULT_DIR")"
DATA_ROOT="$(realpath "$DATA_ROOT")"
DATA_DIR="$(mktemp -d "$DATA_ROOT/zcutils-cassandra-data.XXXXXX")"
CONF_DIR="$RESULT_DIR/conf"
LOG_DIR="$RESULT_DIR/logs"
CASSANDRA_PID=

stop_cassandra() {
	local cmdline state
	[ -n "$CASSANDRA_PID" ] && [ -r "/proc/$CASSANDRA_PID/cmdline" ] || return 0
	cmdline="$(tr '\0' ' ' <"/proc/$CASSANDRA_PID/cmdline")"
	[[ "$cmdline" == *org.apache.cassandra.service.CassandraDaemon* || \
		"$cmdline" == *"$CASSANDRA_BIN"* ]] || \
		die "refusing to signal reused pid=$CASSANDRA_PID cmdline=$cmdline"
	kill -TERM "$CASSANDRA_PID"
	for _ in $(seq 1 300); do
		[ -r "/proc/$CASSANDRA_PID/status" ] || break
		state="$(awk '/^State:/{print $2}' "/proc/$CASSANDRA_PID/status")"
		[ "$state" != Z ] || break
		sleep 0.1
	done
	state="$(awk '/^State:/{print $2}' "/proc/$CASSANDRA_PID/status" 2>/dev/null || true)"
	if [ -n "$state" ] && [ "$state" != Z ]; then
		kill -KILL "$CASSANDRA_PID"
	fi
	wait "$CASSANDRA_PID" 2>/dev/null || true
}

cleanup() {
	local status=$?
	trap - EXIT INT TERM
	stop_cassandra
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
	' "/proc/$CASSANDRA_PID"/task/*/status >"$output"
}

record_cmd() {
	printf '%q ' "$@" >>"$RESULT_DIR/commands.sh"
	printf '\n' >>"$RESULT_DIR/commands.sh"
}

run_stress() {
	local phase="$1"
	shift
	context_snapshot "$RESULT_DIR/context-$phase-before.txt"
	record_cmd taskset -c "$CLIENT_CPUS" "$CASSANDRA_STRESS" "$@"
	/usr/bin/time \
		-f 'elapsed_seconds=%e\nuser_seconds=%U\nsystem_seconds=%S\nmax_rss_kb=%M\nvoluntary_context_switches=%w\ninvoluntary_context_switches=%c' \
		-o "$RESULT_DIR/$phase.time" \
		taskset -c "$CLIENT_CPUS" "$CASSANDRA_STRESS" "$@" \
		>"$RESULT_DIR/$phase.log" 2>&1
	context_snapshot "$RESULT_DIR/context-$phase-after.txt"
}

if [ "$EXPECT_ZCNBLK" = 1 ]; then
	mount_source="$(findmnt -T "$DATA_ROOT" -n -o SOURCE)"
	[ "$mount_source" = /dev/zcnblk0 ] || \
		die "EXPECT_ZCNBLK=1 but DATA_ROOT is mounted from $mount_source"
fi

for port in "$NATIVE_PORT" "$STORAGE_PORT" "$SSL_STORAGE_PORT" "$JMX_PORT"; do
	if ss -H -ltn "sport = :$port" | grep -q .; then
		die "TCP port $port is already in use"
	fi
done

rm -rf "$CONF_DIR" "$LOG_DIR"
cp -a "$CASSANDRA_HOME/conf" "$CONF_DIR"
mkdir -p "$LOG_DIR" "$DATA_DIR/data" "$DATA_DIR/commitlog" \
	"$DATA_DIR/saved_caches" "$DATA_DIR/hints"

sed -i -E \
	-e "s|^cluster_name:.*|cluster_name: 'zcutils-cassandra-bench'|" \
	-e 's|^allocate_tokens_for_local_replication_factor:.*|allocate_tokens_for_local_replication_factor: 1|' \
	-e 's|^commitlog_sync:.*|commitlog_sync: batch|' \
	-e '/^commitlog_sync_period:/d' \
	-e "s|seeds: \"127\.0\.0\.1:[0-9]+\"|seeds: \"127.0.0.1:$STORAGE_PORT\"|" \
	-e "s|^storage_port:.*|storage_port: $STORAGE_PORT|" \
	-e "s|^ssl_storage_port:.*|ssl_storage_port: $SSL_STORAGE_PORT|" \
	-e "s|^native_transport_port:.*|native_transport_port: $NATIVE_PORT|" \
	"$CONF_DIR/cassandra.yaml"
sed -i -E "s|^JMX_PORT=\"[0-9]+\"|JMX_PORT=\"$JMX_PORT\"|" \
	"$CONF_DIR/cassandra-env.sh"
cat >>"$CONF_DIR/cassandra.yaml" <<EOF

# zcutils benchmark-owned persistent paths.
data_file_directories:
  - '$DATA_DIR/data'
commitlog_directory: '$DATA_DIR/commitlog'
saved_caches_directory: '$DATA_DIR/saved_caches'
hints_directory: '$DATA_DIR/hints'
EOF

{
	printf 'timestamp_utc=%s\n' "$(date -u +%FT%TZ)"
	printf 'hostname=%s\n' "$(hostname)"
	printf 'kernel=%s\n' "$(uname -srvo)"
	printf 'data_root=%s\n' "$DATA_ROOT"
	printf 'data_mount=%s\n' "$(findmnt -T "$DATA_ROOT" -n -o SOURCE,TARGET,FSTYPE,OPTIONS)"
	printf 'server_cpus=%s\nclient_cpus=%s\n' "$SERVER_CPUS" "$CLIENT_CPUS"
	printf 'client_threads=%s\nconnections_per_host=%s\n' "$CLIENT_THREADS" "$CONNECTIONS_PER_HOST"
	printf 'seed_total=%s\nwrite_total=%s\nread_total=%s\nmixed_total=%s\n' \
		"$SEED_TOTAL" "$WRITE_TOTAL" "$READ_TOTAL" "$MIXED_TOTAL"
	printf 'value_size=%s\n' "$VALUE_SIZE"
	printf 'commitlog_sync=batch\ncompletion=write-ack-after-commitlog-fsync\n'
	printf 'coordination=%s\n' "$COORDINATION_RESULT"
	printf 'zcnblk_device=%s\n' "$(test -b /dev/zcnblk0 && printf present || printf absent)"
	printf 'cassandra_core_jar_sha256=%s\n' \
		"$(sha256sum "$CASSANDRA_HOME/lib/apache-cassandra-"*.jar | awk 'NR == 1 {print $1}')"
	lscpu
} >"$RESULT_DIR/topology.txt"
"$JAVA_HOME/bin/java" -version >"$RESULT_DIR/java-version.txt" 2>&1
env JAVA_HOME="$JAVA_HOME" "$CASSANDRA_BIN" -v >"$RESULT_DIR/cassandra-version.txt" 2>&1
env JAVA_HOME="$JAVA_HOME" "$CASSANDRA_STRESS" version >"$RESULT_DIR/stress-version.txt" 2>&1

record_cmd taskset -c "$SERVER_CPUS" "$CASSANDRA_BIN" -f
taskset -c "$SERVER_CPUS" env \
	JAVA_HOME="$JAVA_HOME" CASSANDRA_HOME="$CASSANDRA_HOME" \
	CASSANDRA_CONF="$CONF_DIR" CASSANDRA_LOG_DIR="$LOG_DIR" \
	CASSANDRA_HEAPDUMP_DIR="$LOG_DIR" JMX_PORT="$JMX_PORT" LOCAL_JMX=yes \
	MAX_HEAP_SIZE="$MAX_HEAP_SIZE" MAX_DIRECT_MEMORY_SIZE="$MAX_DIRECT_MEMORY_SIZE" \
	"$CASSANDRA_BIN" -f >"$RESULT_DIR/cassandra-console.log" 2>&1 &
CASSANDRA_PID=$!
printf '%s\n' "$CASSANDRA_PID" >"$RESULT_DIR/cassandra.pid"

ready=false
startup_deadline=$((SECONDS + STARTUP_TIMEOUT_SECONDS))
while (( SECONDS < startup_deadline )); do
	if grep -q 'Startup complete' "$LOG_DIR/system.log" 2>/dev/null && \
		ss -H -ltn "sport = :$NATIVE_PORT" | grep -q .; then
		ready=true
		break
	fi
	kill -0 "$CASSANDRA_PID" 2>/dev/null || {
		tail -n 100 "$RESULT_DIR/cassandra-console.log" >&2
		die 'Cassandra exited during startup'
	}
	sleep 0.2
done
[ "$ready" = true ] || die 'Cassandra readiness timeout'

common_args=(
	-schema 'replication(factor=1)'
	-col "n=FIXED(1)" "size=FIXED($VALUE_SIZE)"
	-rate "threads=$CLIENT_THREADS"
	-mode prepared "connectionsPerHost=$CONNECTIONS_PER_HOST"
	-node 127.0.0.1
	-port "native=$NATIVE_PORT" "jmx=$JMX_PORT"
	-log interval=1s
)

record_cmd taskset -c "$CLIENT_CPUS" "$CASSANDRA_STRESS" write \
	"n=$SEED_TOTAL" no-warmup truncate=never cl=ONE "${common_args[@]}"
taskset -c "$CLIENT_CPUS" "$CASSANDRA_STRESS" write \
	"n=$SEED_TOTAL" no-warmup truncate=never cl=ONE "${common_args[@]}" \
	>"$RESULT_DIR/seed.log" 2>&1

"$CQLSH" 127.0.0.1 "$NATIVE_PORT" -e \
	'SELECT key FROM keyspace1.standard1 LIMIT 1;' >"$RESULT_DIR/seed-verify.txt" 2>&1
grep -q '0x' "$RESULT_DIR/seed-verify.txt" || die 'seeded row verification failed'

run_stress write write "n=$WRITE_TOTAL" no-warmup truncate=never cl=ONE \
	-pop "dist=UNIFORM(1..$SEED_TOTAL)" "${common_args[@]}"
run_stress read read "n=$READ_TOTAL" no-warmup cl=ONE \
	-pop "dist=UNIFORM(1..$SEED_TOTAL)" "${common_args[@]}"
run_stress mixed mixed "n=$MIXED_TOTAL" no-warmup cl=ONE 'ratio(read=1,write=1)' \
	-pop "dist=UNIFORM(1..$SEED_TOTAL)" "${common_args[@]}"

env JAVA_HOME="$JAVA_HOME" JMX_PORT="$JMX_PORT" "$NODETOOL" -p "$JMX_PORT" \
	tablestats keyspace1.standard1 >"$RESULT_DIR/nodetool-tablestats.txt" 2>&1
env JAVA_HOME="$JAVA_HOME" JMX_PORT="$JMX_PORT" "$NODETOOL" -p "$JMX_PORT" \
	tpstats >"$RESULT_DIR/nodetool-tpstats.txt" 2>&1

{
	printf 'phase,op_rate_per_second,latency_mean_ms,latency_median_ms,latency_p95_ms,latency_p99_ms,latency_p999_ms,total_errors\n'
	for phase in write read mixed; do
		awk -F: -v phase="$phase" '
			function value() { v=$2; gsub(/^[[:space:]]+|[[:space:]]+$/, "", v); split(v, p, /[[:space:]]+/); gsub(/,/, "", p[1]); return p[1] }
			/^Op rate/ { rate=value() }
			/^Latency mean/ { mean=value() }
			/^Latency median/ { median=value() }
			/^Latency 95th percentile/ { p95=value() }
			/^Latency 99th percentile/ { p99=value() }
			/^Latency 99.9th percentile/ { p999=value() }
			/^Total errors/ { errors=value() }
			END { printf "%s,%s,%s,%s,%s,%s,%s,%s\n", phase, rate, mean, median, p95, p99, p999, errors }
		' "$RESULT_DIR/$phase.log"
	done
} >"$RESULT_DIR/summary.csv"

{
	printf 'phase,voluntary_context_switches,nonvoluntary_context_switches\n'
	for phase in write read mixed; do
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
cat "$RESULT_DIR/context-switches.csv"
