#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ZCUTILS_BIN:-$REPO_DIR/target/release/zcutils}"

usage() {
	cat <<'EOF'
usage: scripts/zcnblk-fanwal-plan-bench.sh [local|leaf-node|fan-node]

Run a zcplan-backed zcnblk-send -> zcnblk-fan --engine wal ->
zcnblk-wal-leaf benchmark. The runner records caps, plan JSON, and the actual
lane/CPU maps used by each process. Use local for one-host smoke tests,
leaf-node on the terminal leaf host, and fan-node on the client/fan host.

environment:
  LANES                  lanes/ports/workers, default: 2
  RAID_MODE              mirror, stripe, or raid10, default: mirror
  MIRROR_READ_POLICY     mirror read policy: stripe or extent, default: binary default
  MIRROR_READ_EXTENT_BYTES
                         mirror extent load-balance unit, default: request length
  BYTES_PER_CONNECTION   bytes per lane, default: 256M
  CHUNK_BYTES            logical record size, default: 4K
  FAN_MAX_REQUEST_BYTES  upstream WAL extent size, default: 1M
  BATCH_DEPTH            zcnblk-send batch depth, default: 512
  SEND_WRITE_WINDOW      zcnblk-send write window, default:
                         max(1024, BATCH_DEPTH * WAL_BATCH_WINDOW)
  SEND_READ_WINDOW       zcnblk-send read window, default:
                         max(1024, BATCH_DEPTH * WAL_BATCH_WINDOW)
  WAL_BATCH_WINDOW       fan-to-leaf pipeline window, default: 16
  OUTDIR                 result directory, default: bench-results/...
  STRICT                 set URING_PLAY_TOPOLOGY_STRICT, default: 1
  LEAF_TARGET            terminal leaf target, default: zcleasemem:1G
  LEAF0_BIND             leaf0 bind address, default: 127.0.0.1
  LEAF1_BIND             leaf1 bind address, default: 127.0.0.2
  LEAF_ADDRS             fan leaf address list, default: LEAF0_BIND,LEAF1_BIND
  FAN_BIND               fan client-facing bind address, default: 127.0.0.1
  SEND_ADDR              sender fan address, default: 127.0.0.1
  SEND_OP                zcnblk-send op, default: write-sync-read
  SEND_ACCESS            zcnblk-send access pattern: linear or random,
                         default: URING_PLAY_ZCNBLK_ACCESS or linear
  SEND_RANDOM_RANGE_BYTES
                         random access range, default: binary default
  SEND_RANDOM_SEED       random access seed, default: binary default
  SEND_MIXED_READ_DRAIN_WATERMARK
                         mixed/read-write-same in-flight read watermark before
                         sender drains responses, default: binary default
  VERIFY_READS           verify read payloads, default: 1
  FAN_MEMFD_DIRTY_CACHE  enable fan memfd dirty cache, default: env/default 0
  FAN_INGRESS_MEMFD_PAYLOAD
                         splice upstream write payload into fan memfd, default: env/default 0
  FAN_MEMFD_SEND_COALESCE_BYTES
                         cached-read memfd send coalesce cap in bytes, default: env/default 0
  FAN_LOCAL_INLINE_WRITEBACK
                         for local:zcleasemem leaves, publish local leaf leases on the fan
                         handler thread instead of waking a per-lane writeback worker,
                         default: env/default 0
  FAN_HWM_ONLY_RESULTS   complete write-only range results as branch HWMs instead
                         of expanding per-4K result descriptors, default:
                         URING_PLAY_ZCNBLK_FAN_HWM_ONLY_RESULTS or 0
  ZERO_COPY_STRICT       fail fan/leaf WAL runs on user-payload copy fallback,
                         default: URING_PLAY_ZCNBLK_WAL_ZERO_COPY_STRICT or 0
  PLAN_ZERO_COPY         zcplan zero-copy policy, default: required when
                         ZERO_COPY_STRICT=1, otherwise auto
  FAN_RESULT_WAIT_POLICY fan result wait mode: blocking, adaptive, or greedy;
                         default: adaptive
  FAN_RESULT_SPIN_BUDGET bounded fan result recv spin budget, default: unset
  FAN_RESULT_SPIN_MIN_OUTSTANDING
                         adaptive fan spin threshold, default: binary default
  FAN_UPSTREAM_SPIN_READS
                         spin on client-to-fan reads before blocking, default: 0
  FAN_UPSTREAM_SPIN_BUDGET
                         bounded fan upstream recv spin budget, default: 4096
  SEND_SPIN_BUDGET       sender recv spin budget, default: 4096
  LEAF_SPIN_BUDGET       leaf recv spin budget, default: 4096
  RANGE_DRAIN_BYTES      sender non-verify range drain buffer, default: binary default
  FAN_LEAF_SOURCE_IPS    optional per-leaf fan source IP list, e.g. card0,card1
  LEAF_CPU_DOMAIN        strict topology domain for fan leaf CPU maps; fan-node
                         defaults this to leaf-node so remote CPU ids do not
                         conflict with local fan/client CPU ids
  CLIENT_CPUS            explicit zcnblk-send CPU list, default: 0..LANES-1
  FAN_HANDLER_CPUS       explicit fan client-facing handler CPU list
  FAN_ASYNC_CPUS         explicit fan async writeback CPU list
  LEAF0_CPUS             explicit leaf0 worker CPU list
  LEAF1_CPUS             explicit leaf1 worker CPU list
  TOPOLOGY_PREFLIGHT     record/warn about host perf state, default: 1
  TOPOLOGY_PREFLIGHT_FATAL
                         fail on preflight warnings, default: 0
  TOPOLOGY_BUSY_PCT      warn when a mapped CPU has a thread above pct, default: 1.0

This script never uses a block device as a mirror or stripe primitive.
EOF
}

log() {
	printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"
}

warn() {
	printf '[%s] WARNING: %s\n' "$(date -u +%H:%M:%S)" "$*" >&2
}

die() {
	printf 'ERROR: %s\n' "$*" >&2
	exit 1
}

range_list() {
	local start="$1"
	local count="$2"
	local end
	if [ "$count" -le 0 ]; then
		die "range_list count must be positive"
	fi
	end=$((start + count - 1))
	if [ "$start" -eq "$end" ]; then
		printf '%s\n' "$start"
	else
		printf '%s-%s\n' "$start" "$end"
	fi
}

size_to_bytes() {
	local value="$1"
	local lower number suffix multiplier
	lower="${value,,}"
	lower="${lower// /}"
	number="$lower"
	suffix=""
	if [[ "$lower" =~ ^([0-9]+)([kmgt]i?b?|b)?$ ]]; then
		number="${BASH_REMATCH[1]}"
		suffix="${BASH_REMATCH[2]:-}"
	else
		die "invalid byte size: $value"
	fi
	case "$suffix" in
		""|"b") multiplier=1 ;;
		"k"|"kb"|"kib") multiplier=1024 ;;
		"m"|"mb"|"mib") multiplier=$((1024 * 1024)) ;;
		"g"|"gb"|"gib") multiplier=$((1024 * 1024 * 1024)) ;;
		"t"|"tb"|"tib") multiplier=$((1024 * 1024 * 1024 * 1024)) ;;
		*) die "invalid byte suffix: $value" ;;
	esac
	printf '%s\n' "$((number * multiplier))"
}

expand_cpu_list() {
	local list="$1"
	local part start end cpu
	list="${list// /}"
	[ -n "$list" ] || return 0
	IFS=',' read -r -a parts <<<"$list"
	for part in "${parts[@]}"; do
		[ -n "$part" ] || continue
		if [[ "$part" =~ ^([0-9]+)-([0-9]+)$ ]]; then
			start="${BASH_REMATCH[1]}"
			end="${BASH_REMATCH[2]}"
			if [ "$start" -gt "$end" ]; then
				die "invalid descending CPU range: $part"
			fi
			for ((cpu = start; cpu <= end; cpu++)); do
				printf '%s\n' "$cpu"
			done
		elif [[ "$part" =~ ^[0-9]+$ ]]; then
			printf '%s\n' "$part"
		else
			die "invalid CPU list element: $part"
		fi
	done
}

unique_cpu_csv() {
	expand_cpu_list "$1" | awk '!seen[$1]++ { if (out != "") out = out ","; out = out $1 } END { print out }'
}

cpu_count() {
	expand_cpu_list "$1" | awk '!seen[$1]++ { n++ } END { print n + 0 }'
}

preflight_note() {
	printf '%s\n' "$*" >>"$PREFLIGHT_LOG"
}

preflight_warn() {
	PREFLIGHT_WARNINGS=$((PREFLIGHT_WARNINGS + 1))
	printf 'PERF WARNING: %s\n' "$*" >>"$PREFLIGHT_LOG"
	warn "$*"
}

stage_cpu_rows() {
	local stage="$1"
	local cpus="$2"
	expand_cpu_list "$cpus" | awk -v stage="$stage" '{ print $1, stage }'
}

run_topology_preflight() {
	[ "$TOPOLOGY_PREFLIGHT" = "1" ] || {
		printf 'topology_preflight=0\n' >>"$OUTDIR/topology.env"
		return 0
	}

	PREFLIGHT_WARNINGS=0
	PREFLIGHT_LOG="$OUTDIR/topology-preflight.log"
	: >"$PREFLIGHT_LOG"
	preflight_note "topology_preflight=1"
	preflight_note "run_mode=$RUN_MODE"
	preflight_note "strict=$STRICT"
	preflight_note "topology_preflight_fatal=$TOPOLOGY_PREFLIGHT_FATAL"
	preflight_note "topology_busy_pct=$TOPOLOGY_BUSY_PCT"

	local local_rows local_cpu_csv local_cpu_count_value
	case "$RUN_MODE" in
		local)
			if [ "$LOCAL_LEAF_MODE" = "1" ]; then
				local_rows="$(
					stage_cpu_rows client "$CLIENT_CPUS"
					stage_cpu_rows fan-handler "$FAN_HANDLER_CPUS"
					stage_cpu_rows fan-async "$FAN_ASYNC_CPUS"
				)"
			else
				local_rows="$(
					stage_cpu_rows client "$CLIENT_CPUS"
					stage_cpu_rows fan-handler "$FAN_HANDLER_CPUS"
					stage_cpu_rows fan-async "$FAN_ASYNC_CPUS"
					stage_cpu_rows leaf0 "$LEAF0_CPUS"
					stage_cpu_rows leaf1 "$LEAF1_CPUS"
				)"
			fi
			;;
		fan-node)
			local_rows="$(
				stage_cpu_rows client "$CLIENT_CPUS"
				stage_cpu_rows fan-handler "$FAN_HANDLER_CPUS"
				stage_cpu_rows fan-async "$FAN_ASYNC_CPUS"
			)"
			;;
		leaf-node)
			local_rows="$(
				stage_cpu_rows leaf0 "$LEAF0_CPUS"
				stage_cpu_rows leaf1 "$LEAF1_CPUS"
			)"
			;;
	esac
	local_cpu_csv="$(printf '%s\n' "$local_rows" | awk 'NF >= 2 && !seen[$1]++ { if (out != "") out = out ","; out = out $1 } END { print out }')"
	local_cpu_count_value="$(printf '%s\n' "$local_rows" | awk 'NF >= 2 && !seen[$1]++ { n++ } END { print n + 0 }')"
	preflight_note "local_mapped_cpus=$local_cpu_csv"
	preflight_note "local_mapped_cpu_count=$local_cpu_count_value"

	local stage count
	local stage_specs
	if [ "$LOCAL_LEAF_MODE" = "1" ]; then
		stage_specs=(client:"$CLIENT_CPUS" fan-handler:"$FAN_HANDLER_CPUS" fan-async:"$FAN_ASYNC_CPUS")
	else
		stage_specs=(client:"$CLIENT_CPUS" fan-handler:"$FAN_HANDLER_CPUS" fan-async:"$FAN_ASYNC_CPUS" leaf0:"$LEAF0_CPUS" leaf1:"$LEAF1_CPUS")
	fi
	for stage in "${stage_specs[@]}"; do
		local name="${stage%%:*}"
		local cpus="${stage#*:}"
		count="$(cpu_count "$cpus")"
		preflight_note "stage=$name cpus=$cpus unique_cpus=$count"
		if [ "$count" -lt "$LANES" ]; then
			preflight_warn "$name has $count unique CPUs for $LANES lanes; lane workers will share CPUs"
		fi
	done

	local overlap
	overlap="$(printf '%s\n' "$local_rows" | awk '
NF >= 2 {
	if (seen[$1] == "") {
		seen[$1] = $2
	} else if (index("," seen[$1] ",", "," $2 ",") == 0) {
		seen[$1] = seen[$1] "," $2
		dup[$1] = seen[$1]
	}
}
END {
	for (cpu in dup) {
		print "cpu=" cpu " stages=" dup[cpu]
	}
}' | sort -n)"
	if [ -n "$overlap" ]; then
		preflight_warn "mapped local stages overlap exact CPUs; this is not a representative isolated topology"
		printf '%s\n' "$overlap" >>"$PREFLIGHT_LOG"
	fi

	local cpu online_file online_state gov_file gov bad_governors missing_cpus
	bad_governors=""
	missing_cpus=""
	IFS=',' read -r -a mapped_cpus <<<"$local_cpu_csv"
	for cpu in "${mapped_cpus[@]:-}"; do
		[ -n "$cpu" ] || continue
		online_file="/sys/devices/system/cpu/cpu$cpu/online"
		if [ ! -d "/sys/devices/system/cpu/cpu$cpu" ]; then
			missing_cpus="${missing_cpus}${missing_cpus:+,}$cpu"
			continue
		fi
		if [ -r "$online_file" ]; then
			online_state="$(cat "$online_file" 2>/dev/null || true)"
			if [ "$online_state" = "0" ]; then
				missing_cpus="${missing_cpus}${missing_cpus:+,}$cpu-offline"
			fi
		fi
		gov_file="/sys/devices/system/cpu/cpu$cpu/cpufreq/scaling_governor"
		if [ -r "$gov_file" ]; then
			gov="$(cat "$gov_file" 2>/dev/null || true)"
			preflight_note "cpu=$cpu governor=$gov"
			if [ "$gov" != "performance" ]; then
				bad_governors="${bad_governors}${bad_governors:+,}cpu$cpu=$gov"
			fi
		fi
	done
	if [ -n "$missing_cpus" ]; then
		preflight_warn "mapped CPU list contains unavailable CPUs: $missing_cpus"
	fi
	if [ -n "$bad_governors" ]; then
		preflight_warn "mapped CPUs are not all using performance governor: $bad_governors"
	fi

	local smt_conflicts
	smt_conflicts="$(
		printf '%s\n' "$local_rows" | awk 'NF >= 2 { stage[$1] = stage[$1] "," $2; wanted[$1] = 1 } END { for (cpu in wanted) print cpu, stage[cpu] }' |
		while read -r cpu stages; do
			[ -n "$cpu" ] || continue
			local siblings_file="/sys/devices/system/cpu/cpu$cpu/topology/thread_siblings_list"
			[ -r "$siblings_file" ] || continue
			local siblings sibling
			siblings="$(cat "$siblings_file" 2>/dev/null || true)"
			while read -r sibling; do
				[ -n "$sibling" ] || continue
				[ "$sibling" = "$cpu" ] && continue
				if printf '%s\n' "$local_rows" | awk -v sibling="$sibling" 'NF >= 2 && $1 == sibling { found = 1 } END { exit found ? 0 : 1 }'; then
					printf 'cpu=%s sibling=%s stages=%s\n' "$cpu" "$sibling" "$stages"
				fi
			done < <(expand_cpu_list "$siblings")
		done | sort -u
	)"
	if [ -n "$smt_conflicts" ]; then
		preflight_warn "mapped local stages include SMT siblings; report this as SMT-paired, not isolated physical-core topology"
		printf '%s\n' "$smt_conflicts" >>"$PREFLIGHT_LOG"
	fi

	if command -v lscpu >/dev/null 2>&1 && [ -n "$local_cpu_csv" ]; then
		preflight_note "== mapped cpu mhz sample =="
		lscpu -e=CPU,CORE,SOCKET,NODE,CACHE,ONLINE,MAXMHZ,MINMHZ,MHZ 2>/dev/null |
			awk -v cpus="$local_cpu_csv" '
BEGIN {
	split(cpus, a, ",")
	for (i in a) wanted[a[i]] = 1
}
NR == 1 || wanted[$1] { print }
' >>"$PREFLIGHT_LOG" || true
	fi

	local busy
	busy=""
	if command -v ps >/dev/null 2>&1 && [ -n "$local_cpu_csv" ]; then
		busy="$(ps -eLo psr,pcpu,pid,tid,comm --no-headers 2>/dev/null |
			awk -v cpus="$local_cpu_csv" -v threshold="$TOPOLOGY_BUSY_PCT" '
BEGIN {
	split(cpus, a, ",")
	for (i in a) wanted[a[i]] = 1
}
wanted[$1] && ($2 + 0) >= threshold {
	printf "busy_cpu=%s pcpu=%s pid=%s tid=%s comm=%s\n", $1, $2, $3, $4, $5
	found = 1
	if (++n >= 24) {
		exit
	}
}
END {
	exit found ? 0 : 1
}' || true)"
	fi
	if [ -n "$busy" ]; then
		preflight_warn "mapped CPUs already have threads above ${TOPOLOGY_BUSY_PCT}% CPU; benchmark is noisy unless those cores are isolated"
		printf '%s\n' "$busy" >>"$PREFLIGHT_LOG"
	fi

	local memlock hugepages
	memlock="$(ulimit -l 2>/dev/null || true)"
	preflight_note "memlock_limit=$memlock"
	if [ "$memlock" != "unlimited" ]; then
		preflight_warn "memlock is $memlock; zero-copy/RDMA/hugetlb results are not representative without enough locked-memory headroom"
	fi
	hugepages="$(cat /proc/sys/vm/nr_hugepages 2>/dev/null || printf 0)"
	preflight_note "nr_hugepages=$hugepages"
	if [ "${hugepages:-0}" = "0" ]; then
		preflight_warn "vm.nr_hugepages is 0; this is a smoke run for hugetlb-backed zero-copy/RDMA comparisons"
	fi

	if [ "$PREFLIGHT_WARNINGS" -gt 0 ]; then
		printf 'topology_preflight=1\n' >>"$OUTDIR/topology.env"
		printf 'topology_preflight_representative=0\n' >>"$OUTDIR/topology.env"
		printf 'topology_preflight_warnings=%s\n' "$PREFLIGHT_WARNINGS" >>"$OUTDIR/topology.env"
		warn "topology preflight found $PREFLIGHT_WARNINGS issue(s); see $PREFLIGHT_LOG"
		if [ "$TOPOLOGY_PREFLIGHT_FATAL" = "1" ]; then
			die "topology preflight failed and TOPOLOGY_PREFLIGHT_FATAL=1"
		fi
	else
		printf 'topology_preflight=1\n' >>"$OUTDIR/topology.env"
		printf 'topology_preflight_representative=1\n' >>"$OUTDIR/topology.env"
		printf 'topology_preflight_warnings=0\n' >>"$OUTDIR/topology.env"
	fi
}

append_pid() {
	local pid="$1"
	PIDS+=("$pid")
}

start_bg() {
	local name="$1"
	local log_path="$2"
	shift 2
	log "starting $name"
	"$@" >"$log_path" 2>&1 &
	local pid=$!
	append_pid "$pid"
	printf '%s\n' "$pid" >"$OUTDIR/$name.pid"
	sleep 0.2
	if ! kill -0 "$pid" 2>/dev/null; then
		wait "$pid" || true
		tail -n 60 "$log_path" >&2 || true
		die "$name exited before startup completed"
	fi
}

cleanup() {
	local status=$?
	trap - EXIT INT TERM
	for pid in "${PIDS[@]:-}"; do
		if kill -0 "$pid" 2>/dev/null; then
			kill "$pid" 2>/dev/null || true
		fi
	done
	for pid in "${PIDS[@]:-}"; do
		wait "$pid" 2>/dev/null || true
	done
	exit "$status"
}

wait_listen() {
	local port="$1"
	local label="$2"
	local attempt
	for attempt in $(seq 1 100); do
		if ss -ltn "sport = :$port" | awk 'NR > 1 { found = 1 } END { exit found ? 0 : 1 }'; then
			return 0
		fi
		sleep 0.1
	done
	die "$label did not listen on TCP port $port"
}

extract_plan_id() {
	local plan_json="$1"
	if command -v jq >/dev/null 2>&1; then
		jq -r '.plan_id // empty' "$plan_json"
	else
		sed -n 's/.*"plan_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$plan_json" | head -n 1
	fi
}

summarize_logs() {
	local summary="$OUTDIR/summary.txt"
	{
		printf 'outdir=%s\n' "$OUTDIR"
		printf 'plan_id=%s\n' "$PLAN_ID"
		printf 'placement_epoch=%s\n' "$PLACEMENT_EPOCH"
		printf '\n== topology ==\n'
		cat "$OUTDIR/topology.env"
		printf '\n== plan ==\n'
		if command -v jq >/dev/null 2>&1; then
			jq '.plan_id, .placement_epoch, .representative, .warnings, .compiled.parallel_raid, .descriptor_projection' "$OUTDIR/plan.json"
		else
			sed -n '1,80p' "$OUTDIR/plan.json"
		fi
		printf '\n== topology preflight ==\n'
		cat "$OUTDIR/topology-preflight.log" 2>/dev/null || true
		printf '\n== process summaries ==\n'
		grep -hE 'zcnblk-(send|fan-wal|wal-leaf).*(summary|:)|zcnblk-fan-wal-stage-cpus|PERF WARNING|TOPOLOGY' \
			"$OUTDIR"/send.log "$OUTDIR"/fan.log "$OUTDIR"/leaf0.log "$OUTDIR"/leaf1.log 2>/dev/null || true
	} >"$summary"
	log "wrote $summary"
	tail -n 80 "$summary" || true
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
	usage
	exit 0
fi

RUN_MODE="${1:-local}"
case "$RUN_MODE" in
	local|leaf-node|fan-node) ;;
	*) die "unknown run mode $RUN_MODE; use local, leaf-node, or fan-node" ;;
esac

LANES="${LANES:-2}"
RAID_MODE="${RAID_MODE:-mirror}"
MIRROR_READ_POLICY="${MIRROR_READ_POLICY:-${URING_PLAY_ZCNBLK_FAN_MIRROR_READ_POLICY:-}}"
MIRROR_READ_EXTENT_BYTES="${MIRROR_READ_EXTENT_BYTES:-${URING_PLAY_ZCNBLK_FAN_MIRROR_READ_EXTENT_BYTES:-}}"
BYTES_PER_CONNECTION="${BYTES_PER_CONNECTION:-256M}"
CHUNK_BYTES="${CHUNK_BYTES:-4K}"
FAN_MAX_REQUEST_BYTES="${FAN_MAX_REQUEST_BYTES:-1M}"
FAN_MAX_REQUEST_BYTES_ENV="$(size_to_bytes "$FAN_MAX_REQUEST_BYTES")"
BATCH_DEPTH="${BATCH_DEPTH:-512}"
WAL_BATCH_WINDOW="${WAL_BATCH_WINDOW:-16}"
DEFAULT_SEND_WINDOW=$((BATCH_DEPTH * WAL_BATCH_WINDOW))
if [ "$DEFAULT_SEND_WINDOW" -lt 1024 ]; then
	DEFAULT_SEND_WINDOW=1024
fi
SEND_WRITE_WINDOW="${SEND_WRITE_WINDOW:-${URING_PLAY_ZCNBLK_WRITE_WINDOW:-$DEFAULT_SEND_WINDOW}}"
SEND_READ_WINDOW="${SEND_READ_WINDOW:-${URING_PLAY_ZCNBLK_READ_WINDOW:-$DEFAULT_SEND_WINDOW}}"
STRICT="${STRICT:-1}"
PLACEMENT_EPOCH="${PLACEMENT_EPOCH:-1}"
BASE_PORT="${BASE_PORT:-29000}"
LEAF_BASE_PORT="${LEAF_BASE_PORT:-30000}"
CONNECTIONS_PER_PORT="${CONNECTIONS_PER_PORT:-1}"
SHARD_COUNT="${SHARD_COUNT:-1}"
LEAF_TARGET="${LEAF_TARGET:-zcleasemem:1G}"
LEAF0_BIND="${LEAF0_BIND:-127.0.0.1}"
LEAF1_BIND="${LEAF1_BIND:-127.0.0.2}"
LEAF_ADDRS="${LEAF_ADDRS:-$LEAF0_BIND,$LEAF1_BIND}"
FAN_BIND="${FAN_BIND:-127.0.0.1}"
SEND_ADDR="${SEND_ADDR:-127.0.0.1}"
FAN_LEAF_SOURCE_IPS="${FAN_LEAF_SOURCE_IPS:-}"
LOCAL_LEAF_MODE=0
if [[ ",$LEAF_ADDRS," == *,local:* ]]; then
	LOCAL_LEAF_MODE=1
fi
if [ "$RUN_MODE" = "leaf-node" ] && [ "$LOCAL_LEAF_MODE" = "1" ]; then
	die "leaf-node mode is only for external TCP leaves; remove local: leaf specs"
fi
if [ "$RUN_MODE" = "fan-node" ]; then
	LEAF_CPU_DOMAIN="${LEAF_CPU_DOMAIN:-leaf-node}"
else
	LEAF_CPU_DOMAIN="${LEAF_CPU_DOMAIN:-local}"
fi
LEAF_SUBMIT_MODE="${LEAF_SUBMIT_MODE:-blocking}"
ASYNC_WRITEBACK="${ASYNC_WRITEBACK:-1}"
DEFER_UNSYNCED_WRITEBACK="${DEFER_UNSYNCED_WRITEBACK:-0}"
DIRTY_HARD_BYTES="${DIRTY_HARD_BYTES:-4G}"
DIRTY_SOFT_BYTES="${DIRTY_SOFT_BYTES:-3G}"
FAN_MEMFD_DIRTY_CACHE="${FAN_MEMFD_DIRTY_CACHE:-${URING_PLAY_ZCNBLK_FAN_MEMFD_DIRTY_CACHE:-0}}"
FAN_INGRESS_MEMFD_PAYLOAD="${FAN_INGRESS_MEMFD_PAYLOAD:-${URING_PLAY_ZCNBLK_FAN_INGRESS_MEMFD_PAYLOAD:-0}}"
FAN_MEMFD_SEND_COALESCE_BYTES="${FAN_MEMFD_SEND_COALESCE_BYTES:-${URING_PLAY_ZCNBLK_FAN_MEMFD_SEND_COALESCE_BYTES:-0}}"
FAN_LOCAL_INLINE_WRITEBACK="${FAN_LOCAL_INLINE_WRITEBACK:-${URING_PLAY_ZCNBLK_FAN_LOCAL_INLINE_WRITEBACK:-0}}"
FAN_HWM_ONLY_RESULTS="${FAN_HWM_ONLY_RESULTS:-${URING_PLAY_ZCNBLK_FAN_HWM_ONLY_RESULTS:-0}}"
FAN_MEMFD_SEND_COALESCE_BYTES_ENV="$(size_to_bytes "$FAN_MEMFD_SEND_COALESCE_BYTES")"
ZERO_COPY_STRICT="${ZERO_COPY_STRICT:-${URING_PLAY_ZCNBLK_WAL_ZERO_COPY_STRICT:-${URING_PLAY_ZCNBLK_ZERO_COPY_STRICT:-0}}}"
PLAN_ZERO_COPY="${PLAN_ZERO_COPY:-auto}"
if [ "$ZERO_COPY_STRICT" = "1" ] && [ "$PLAN_ZERO_COPY" = "auto" ]; then
	PLAN_ZERO_COPY="required"
fi
SEND_SPIN_BUDGET="${SEND_SPIN_BUDGET:-${URING_PLAY_ZCNBLK_SEND_SPIN_BUDGET:-4096}}"
LEAF_SPIN_BUDGET="${LEAF_SPIN_BUDGET:-${URING_PLAY_ZCNBLK_WAL_LEAF_SPIN_BUDGET:-4096}}"
FAN_RESULT_WAIT_POLICY="${FAN_RESULT_WAIT_POLICY:-${URING_PLAY_ZCNBLK_FAN_RESULT_WAIT_POLICY:-adaptive}}"
FAN_RESULT_SPIN_BUDGET="${FAN_RESULT_SPIN_BUDGET:-${URING_PLAY_ZCNBLK_FAN_RESULT_SPIN_BUDGET:-}}"
FAN_RESULT_SPIN_MIN_OUTSTANDING="${FAN_RESULT_SPIN_MIN_OUTSTANDING:-${URING_PLAY_ZCNBLK_FAN_RESULT_SPIN_MIN_OUTSTANDING:-}}"
FAN_UPSTREAM_SPIN_READS="${FAN_UPSTREAM_SPIN_READS:-${URING_PLAY_ZCNBLK_FAN_UPSTREAM_SPIN_READS:-0}}"
FAN_UPSTREAM_SPIN_BUDGET="${FAN_UPSTREAM_SPIN_BUDGET:-${URING_PLAY_ZCNBLK_FAN_UPSTREAM_SPIN_BUDGET:-4096}}"
RANGE_DRAIN_BYTES="${RANGE_DRAIN_BYTES:-${URING_PLAY_ZCNBLK_RANGE_DRAIN_BYTES:-}}"
ASYNC_RESULT_WINDOW="${ASYNC_RESULT_WINDOW:-4096}"
OBJECTIVE="${OBJECTIVE:-max-gbit}"
SEND_OP="${SEND_OP:-write-sync-read}"
SEND_ACCESS="${SEND_ACCESS:-${URING_PLAY_ZCNBLK_ACCESS:-linear}}"
SEND_RANDOM_RANGE_BYTES="${SEND_RANDOM_RANGE_BYTES:-${URING_PLAY_ZCNBLK_RANDOM_RANGE_BYTES:-}}"
SEND_RANDOM_SEED="${SEND_RANDOM_SEED:-${URING_PLAY_ZCNBLK_RANDOM_SEED:-}}"
SEND_MIXED_READ_DRAIN_WATERMARK="${SEND_MIXED_READ_DRAIN_WATERMARK:-${URING_PLAY_ZCNBLK_MIXED_READ_DRAIN_WATERMARK:-}}"
VERIFY_READS="${VERIFY_READS:-1}"
WAIT_WRITE_ACKS="${WAIT_WRITE_ACKS:-0}"
case "${SEND_ACCESS,,}" in
	""|"linear"|"sequential"|"seq")
		SEND_ACCESS="linear"
		WRITE_EXTENTS="${WRITE_EXTENTS:-1}"
		READ_EXTENTS="${READ_EXTENTS:-1}"
		;;
	"random"|"rand"|"rand4k"|"random-4k")
		SEND_ACCESS="random"
		WRITE_EXTENTS="${WRITE_EXTENTS:-0}"
		READ_EXTENTS="${READ_EXTENTS:-0}"
		;;
	*)
		die "unknown SEND_ACCESS=$SEND_ACCESS; use linear or random"
		;;
esac
if [ "$SEND_ACCESS" = "random" ] && { [ "$WRITE_EXTENTS" != "0" ] || [ "$READ_EXTENTS" != "0" ]; }; then
	die "SEND_ACCESS=random requires WRITE_EXTENTS=0 and READ_EXTENTS=0; random 4K placement must stay as individual descriptors"
fi
TOPOLOGY_PREFLIGHT="${TOPOLOGY_PREFLIGHT:-1}"
TOPOLOGY_PREFLIGHT_FATAL="${TOPOLOGY_PREFLIGHT_FATAL:-0}"
TOPOLOGY_BUSY_PCT="${TOPOLOGY_BUSY_PCT:-1.0}"
RUN_ID="${RUN_ID:-fanwal-plan-$(date -u +%Y%m%dT%H%M%SZ)}"
OUTDIR="${OUTDIR:-$REPO_DIR/bench-results/$RUN_ID}"

[ "$LANES" -gt 0 ] || die "LANES must be positive"
[ "$CONNECTIONS_PER_PORT" -eq 1 ] || die "this local runner expects one connection per port"
[ "$SHARD_COUNT" -eq 1 ] || die "zcnblk-fan accepts only client edge shard 0; keep SHARD_COUNT=1 and express lanes with ports/workers"

if [ ! -x "$BIN" ]; then
	log "building zcutils release binary"
	( cd "$REPO_DIR" && cargo build --release --bin zcutils )
fi
command -v ss >/dev/null 2>&1 || die "ss is required"

mkdir -p "$OUTDIR"
PIDS=()
trap cleanup EXIT INT TERM

DEFAULT_CLIENT_CPUS="$(range_list 0 "$LANES")"
DEFAULT_FAN_HANDLER_CPUS="$(range_list "$LANES" "$LANES")"
DEFAULT_FAN_ASYNC_CPUS="$(range_list "$((2 * LANES))" "$LANES")"
DEFAULT_LEAF0_CPUS="$(range_list "$((3 * LANES))" "$LANES")"
DEFAULT_LEAF1_CPUS="$(range_list "$((4 * LANES))" "$LANES")"
CLIENT_CPUS="${CLIENT_CPUS:-$DEFAULT_CLIENT_CPUS}"
FAN_HANDLER_CPUS="${FAN_HANDLER_CPUS:-$DEFAULT_FAN_HANDLER_CPUS}"
FAN_ASYNC_CPUS="${FAN_ASYNC_CPUS:-$DEFAULT_FAN_ASYNC_CPUS}"
LEAF0_CPUS="${LEAF0_CPUS:-$DEFAULT_LEAF0_CPUS}"
LEAF1_CPUS="${LEAF1_CPUS:-$DEFAULT_LEAF1_CPUS}"
FAN_CPUS="$FAN_HANDLER_CPUS,$FAN_ASYNC_CPUS"
if [ "$LOCAL_LEAF_MODE" = "1" ]; then
	PLAN_CPUS="$FAN_HANDLER_CPUS,$FAN_ASYNC_CPUS"
else
	PLAN_CPUS="$FAN_HANDLER_CPUS,$FAN_ASYNC_CPUS,$LEAF0_CPUS,$LEAF1_CPUS"
fi
if [ "$LOCAL_LEAF_MODE" = "1" ]; then
	FAN_LEAF_CPU_LISTS="${FAN_LEAF_CPU_LISTS:-in-process-local-leaves}"
elif [ "$LEAF_CPU_DOMAIN" = "local" ]; then
	FAN_LEAF_CPU_LISTS="${FAN_LEAF_CPU_LISTS:-leaf0=$LEAF0_CPUS;leaf1=$LEAF1_CPUS}"
else
	FAN_LEAF_CPU_LISTS="${FAN_LEAF_CPU_LISTS:-leaf0@$LEAF_CPU_DOMAIN=$LEAF0_CPUS;leaf1@$LEAF_CPU_DOMAIN=$LEAF1_CPUS}"
fi

{
	printf 'run_mode=%s\n' "$RUN_MODE"
	printf 'run_id=%s\n' "$RUN_ID"
	printf 'lanes=%s\n' "$LANES"
	printf 'raid_mode=%s\n' "$RAID_MODE"
	printf 'mirror_read_policy=%s\n' "${MIRROR_READ_POLICY:-binary-default}"
	printf 'mirror_read_extent_bytes=%s\n' "${MIRROR_READ_EXTENT_BYTES:-request-len}"
	printf 'fan_bind=%s\n' "$FAN_BIND"
	printf 'send_addr=%s\n' "$SEND_ADDR"
	printf 'leaf0_bind=%s\n' "$LEAF0_BIND"
	printf 'leaf1_bind=%s\n' "$LEAF1_BIND"
	printf 'leaf_addrs=%s\n' "$LEAF_ADDRS"
	printf 'local_leaf_mode=%s\n' "$LOCAL_LEAF_MODE"
	printf 'leaf_cpu_domain=%s\n' "$LEAF_CPU_DOMAIN"
	printf 'client_cpus=%s\n' "$CLIENT_CPUS"
	printf 'fan_handler_cpus=%s\n' "$FAN_HANDLER_CPUS"
	printf 'fan_async_cpus=%s\n' "$FAN_ASYNC_CPUS"
	printf 'leaf0_cpus=%s\n' "$LEAF0_CPUS"
	printf 'leaf1_cpus=%s\n' "$LEAF1_CPUS"
	printf 'fan_leaf_cpu_lists=%s\n' "$FAN_LEAF_CPU_LISTS"
	printf 'fan_leaf_source_ips=%s\n' "${FAN_LEAF_SOURCE_IPS:-kernel-route}"
	printf 'plan_cpus=%s\n' "$PLAN_CPUS"
	printf 'bytes_per_connection=%s\n' "$BYTES_PER_CONNECTION"
	printf 'chunk_bytes=%s\n' "$CHUNK_BYTES"
	printf 'fan_max_request_bytes=%s\n' "$FAN_MAX_REQUEST_BYTES"
	printf 'fan_max_request_bytes_env=%s\n' "$FAN_MAX_REQUEST_BYTES_ENV"
	printf 'shard_count=%s\n' "$SHARD_COUNT"
	printf 'batch_depth=%s\n' "$BATCH_DEPTH"
	printf 'default_send_window=%s\n' "$DEFAULT_SEND_WINDOW"
	printf 'send_write_window=%s\n' "$SEND_WRITE_WINDOW"
	printf 'send_read_window=%s\n' "$SEND_READ_WINDOW"
	printf 'wal_batch_window=%s\n' "$WAL_BATCH_WINDOW"
	printf 'leaf_target=%s\n' "$LEAF_TARGET"
	printf 'fan_memfd_dirty_cache=%s\n' "$FAN_MEMFD_DIRTY_CACHE"
	printf 'fan_ingress_memfd_payload=%s\n' "$FAN_INGRESS_MEMFD_PAYLOAD"
	printf 'fan_memfd_send_coalesce_bytes=%s\n' "$FAN_MEMFD_SEND_COALESCE_BYTES"
	printf 'fan_memfd_send_coalesce_bytes_env=%s\n' "$FAN_MEMFD_SEND_COALESCE_BYTES_ENV"
	printf 'fan_local_inline_writeback=%s\n' "$FAN_LOCAL_INLINE_WRITEBACK"
	printf 'fan_hwm_only_results=%s\n' "$FAN_HWM_ONLY_RESULTS"
	printf 'zero_copy_strict=%s\n' "$ZERO_COPY_STRICT"
	printf 'plan_zero_copy=%s\n' "$PLAN_ZERO_COPY"
	printf 'fan_result_wait_policy=%s\n' "$FAN_RESULT_WAIT_POLICY"
	printf 'fan_result_spin_budget=%s\n' "${FAN_RESULT_SPIN_BUDGET:-unset}"
	printf 'fan_result_spin_min_outstanding=%s\n' "${FAN_RESULT_SPIN_MIN_OUTSTANDING:-binary-default}"
	printf 'fan_upstream_spin_reads=%s\n' "$FAN_UPSTREAM_SPIN_READS"
	printf 'fan_upstream_spin_budget=%s\n' "$FAN_UPSTREAM_SPIN_BUDGET"
	printf 'send_spin_budget=%s\n' "$SEND_SPIN_BUDGET"
	printf 'leaf_spin_budget=%s\n' "$LEAF_SPIN_BUDGET"
	printf 'range_drain_bytes=%s\n' "${RANGE_DRAIN_BYTES:-binary-default}"
	printf 'send_op=%s\n' "$SEND_OP"
	printf 'send_access=%s\n' "$SEND_ACCESS"
	printf 'send_random_range_bytes=%s\n' "${SEND_RANDOM_RANGE_BYTES:-binary-default}"
	printf 'send_random_seed=%s\n' "${SEND_RANDOM_SEED:-binary-default}"
	printf 'send_mixed_read_drain_watermark=%s\n' "${SEND_MIXED_READ_DRAIN_WATERMARK:-binary-default}"
	printf 'verify_reads=%s\n' "$VERIFY_READS"
	printf 'write_extents=%s\n' "$WRITE_EXTENTS"
	printf 'read_extents=%s\n' "$READ_EXTENTS"
	printf 'strict=%s\n' "$STRICT"
} >"$OUTDIR/topology.env"

run_topology_preflight

"$BIN" zcplan caps \
	--role fan-local \
	--node-id "$RUN_ID-local" \
	--cpu-list "$PLAN_CPUS" >"$OUTDIR/caps.local.json"

"$BIN" zcplan plan \
	--mode "$RAID_MODE" \
	--lanes "$LANES" \
	--workers "$LANES" \
	--branches 2 \
	--cpu-list "$PLAN_CPUS" \
	--record-bytes "$CHUNK_BYTES" \
	--extent-bytes "$FAN_MAX_REQUEST_BYTES" \
	--batch-window "$BATCH_DEPTH" \
	--objective "$OBJECTIVE" \
	--transport tcp-mux \
	--zero-copy "$PLAN_ZERO_COPY" \
	--placement-epoch "$PLACEMENT_EPOCH" >"$OUTDIR/plan.json"

PLAN_ID="$(extract_plan_id "$OUTDIR/plan.json")"
[ -n "$PLAN_ID" ] || die "plan.json did not contain plan_id"
log "plan_id=$PLAN_ID placement_epoch=$PLACEMENT_EPOCH"

PLAN_ENV=(
	"URING_PLAY_ZC_PLAN_ID=$PLAN_ID"
	"URING_PLAY_ZC_PLACEMENT_EPOCH=$PLACEMENT_EPOCH"
)

FAN_ENV=(
	"${PLAN_ENV[@]}"
	"URING_PLAY_TOPOLOGY_STRICT=$STRICT"
	"URING_PLAY_PIN_CPUS=1"
	"URING_PLAY_PIN_CPU_LIST=$FAN_CPUS"
	"URING_PLAY_ZCNBLK_FAN_CLIENT_CPU_LIST=$CLIENT_CPUS"
	"URING_PLAY_ZCNBLK_BATCH_DEPTH=$BATCH_DEPTH"
	"URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW=$WAL_BATCH_WINDOW"
	"URING_PLAY_ZCNBLK_WRITE_WINDOW=$SEND_WRITE_WINDOW"
	"URING_PLAY_ZCNBLK_READ_WINDOW=$SEND_READ_WINDOW"
	"URING_PLAY_ZCNBLK_FAN_MAX_REQUEST_BYTES=$FAN_MAX_REQUEST_BYTES_ENV"
	"URING_PLAY_ZCNBLK_FAN_WAL_HARD_DIRTY_BYTES=$DIRTY_HARD_BYTES"
	"URING_PLAY_ZCNBLK_FAN_WAL_SOFT_DIRTY_BYTES=$DIRTY_SOFT_BYTES"
	"URING_PLAY_ZCNBLK_FAN_MEMFD_DIRTY_CACHE=$FAN_MEMFD_DIRTY_CACHE"
	"URING_PLAY_ZCNBLK_FAN_INGRESS_MEMFD_PAYLOAD=$FAN_INGRESS_MEMFD_PAYLOAD"
	"URING_PLAY_ZCNBLK_FAN_MEMFD_SEND_COALESCE_BYTES=$FAN_MEMFD_SEND_COALESCE_BYTES_ENV"
	"URING_PLAY_ZCNBLK_FAN_LOCAL_INLINE_WRITEBACK=$FAN_LOCAL_INLINE_WRITEBACK"
	"URING_PLAY_ZCNBLK_FAN_HWM_ONLY_RESULTS=$FAN_HWM_ONLY_RESULTS"
	"URING_PLAY_ZCNBLK_WAL_ZERO_COPY_STRICT=$ZERO_COPY_STRICT"
	"URING_PLAY_ZCNBLK_WRITE_ACKS=0"
	"URING_PLAY_ZCNBLK_WAL_WRITE_ACK_MODE=disabled"
	"URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1"
	"URING_PLAY_ZCNBLK_FAN_ASYNC_WRITEBACK=$ASYNC_WRITEBACK"
	"URING_PLAY_ZCNBLK_FAN_DEFER_UNSYNCED_WRITEBACK=$DEFER_UNSYNCED_WRITEBACK"
	"URING_PLAY_ZCNBLK_FAN_ASYNC_RESULT_WINDOW=$ASYNC_RESULT_WINDOW"
	"URING_PLAY_ZCNBLK_FAN_ASYNC_WRITE_EXTENTS=1"
	"URING_PLAY_ZCNBLK_FAN_RESULT_WAIT_POLICY=$FAN_RESULT_WAIT_POLICY"
	"URING_PLAY_ZCNBLK_FAN_UPSTREAM_SPIN_READS=$FAN_UPSTREAM_SPIN_READS"
	"URING_PLAY_ZCNBLK_FAN_UPSTREAM_SPIN_BUDGET=$FAN_UPSTREAM_SPIN_BUDGET"
)
if [ "$LOCAL_LEAF_MODE" = "0" ]; then
	FAN_ENV+=("URING_PLAY_ZCNBLK_FAN_LEAF_CPU_LISTS=$FAN_LEAF_CPU_LISTS")
fi
if [ -n "$FAN_LEAF_SOURCE_IPS" ]; then
	FAN_ENV+=("URING_PLAY_ZCNBLK_FAN_LEAF_SOURCE_IPS=$FAN_LEAF_SOURCE_IPS")
fi
if [ -n "$FAN_RESULT_SPIN_BUDGET" ]; then
	FAN_ENV+=("URING_PLAY_ZCNBLK_FAN_RESULT_SPIN_BUDGET=$FAN_RESULT_SPIN_BUDGET")
fi
if [ -n "$FAN_RESULT_SPIN_MIN_OUTSTANDING" ]; then
	FAN_ENV+=("URING_PLAY_ZCNBLK_FAN_RESULT_SPIN_MIN_OUTSTANDING=$FAN_RESULT_SPIN_MIN_OUTSTANDING")
fi
if [ -n "$MIRROR_READ_POLICY" ]; then
	FAN_ENV+=("URING_PLAY_ZCNBLK_FAN_MIRROR_READ_POLICY=$MIRROR_READ_POLICY")
fi
if [ -n "$MIRROR_READ_EXTENT_BYTES" ]; then
	FAN_ENV+=("URING_PLAY_ZCNBLK_FAN_MIRROR_READ_EXTENT_BYTES=$MIRROR_READ_EXTENT_BYTES")
fi

SEND_ENV=(
	"${PLAN_ENV[@]}"
	"URING_PLAY_TOPOLOGY_STRICT=$STRICT"
	"URING_PLAY_PIN_CPUS=1"
	"URING_PLAY_PIN_CPU_LIST=$CLIENT_CPUS"
	"URING_PLAY_ZCNBLK_OP=$SEND_OP"
	"URING_PLAY_ZCNBLK_ACCESS=$SEND_ACCESS"
	"URING_PLAY_ZCNBLK_BATCH_DEPTH=$BATCH_DEPTH"
	"URING_PLAY_ZCNBLK_WRITE_WINDOW=$SEND_WRITE_WINDOW"
	"URING_PLAY_ZCNBLK_READ_WINDOW=$SEND_READ_WINDOW"
	"URING_PLAY_ZCNBLK_SEND_WRITE_EXTENTS=$WRITE_EXTENTS"
	"URING_PLAY_ZCNBLK_SEND_READ_EXTENTS=$READ_EXTENTS"
	"URING_PLAY_ZCNBLK_SEND_MAX_EXTENT_BYTES=$FAN_MAX_REQUEST_BYTES_ENV"
	"URING_PLAY_ZCNBLK_VERIFY_READS=$VERIFY_READS"
	"URING_PLAY_ZCNBLK_WAIT_WRITE_ACKS=$WAIT_WRITE_ACKS"
	"URING_PLAY_ZCNBLK_SEND_SPIN_READS=1"
	"URING_PLAY_ZCNBLK_SEND_SPIN_BUDGET=$SEND_SPIN_BUDGET"
)
if [ -n "$RANGE_DRAIN_BYTES" ]; then
	SEND_ENV+=("URING_PLAY_ZCNBLK_RANGE_DRAIN_BYTES=$RANGE_DRAIN_BYTES")
fi
if [ -n "$SEND_RANDOM_RANGE_BYTES" ]; then
	SEND_ENV+=("URING_PLAY_ZCNBLK_RANDOM_RANGE_BYTES=$SEND_RANDOM_RANGE_BYTES")
fi
if [ -n "$SEND_RANDOM_SEED" ]; then
	SEND_ENV+=("URING_PLAY_ZCNBLK_RANDOM_SEED=$SEND_RANDOM_SEED")
fi
if [ -n "$SEND_MIXED_READ_DRAIN_WATERMARK" ]; then
	SEND_ENV+=("URING_PLAY_ZCNBLK_MIXED_READ_DRAIN_WATERMARK=$SEND_MIXED_READ_DRAIN_WATERMARK")
fi

LEAF_COMMON_ENV=(
	"${PLAN_ENV[@]}"
	"URING_PLAY_TOPOLOGY_STRICT=$STRICT"
	"URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1"
	"URING_PLAY_ZCNBLK_WAL_LEAF_SPIN_READS=1"
	"URING_PLAY_ZCNBLK_WAL_LEAF_SPIN_BUDGET=$LEAF_SPIN_BUDGET"
	"URING_PLAY_ZCNBLK_WAL_LEAF_ZCLEASEMEM_MEMFD=1"
	"URING_PLAY_ZCNBLK_WAL_ZERO_COPY_STRICT=$ZERO_COPY_STRICT"
)

if { [ "$RUN_MODE" = "local" ] || [ "$RUN_MODE" = "leaf-node" ]; } && [ "$LOCAL_LEAF_MODE" = "0" ]; then
	start_bg leaf0 "$OUTDIR/leaf0.log" \
		env "${LEAF_COMMON_ENV[@]}" \
			URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST="$LEAF0_CPUS" \
			"$BIN" zcnblk-wal-leaf "$LEAF_TARGET" "$LEAF0_BIND" \
			"$LEAF_BASE_PORT" "$LANES" "$CONNECTIONS_PER_PORT" "$CHUNK_BYTES" "$LANES" true "$LEAF_SUBMIT_MODE"

	start_bg leaf1 "$OUTDIR/leaf1.log" \
		env "${LEAF_COMMON_ENV[@]}" \
			URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST="$LEAF1_CPUS" \
			"$BIN" zcnblk-wal-leaf "$LEAF_TARGET" "$LEAF1_BIND" \
			"$LEAF_BASE_PORT" "$LANES" "$CONNECTIONS_PER_PORT" "$CHUNK_BYTES" "$LANES" true "$LEAF_SUBMIT_MODE"

	wait_listen "$LEAF_BASE_PORT" "leaf"
elif [ "$LOCAL_LEAF_MODE" = "1" ]; then
	log "using in-process local zcleasemem leaves; no external zcnblk-wal-leaf daemons"
fi

if [ "$RUN_MODE" = "leaf-node" ]; then
	log "leaf-node mode listening on $LEAF0_BIND,$LEAF1_BIND; waiting for fan EOF"
	wait_status=0
	for pid in "${PIDS[@]}"; do
		if ! wait "$pid"; then
			wait_status=1
		fi
	done
	PIDS=()
	summarize_logs
	exit "$wait_status"
fi

start_bg fan "$OUTDIR/fan.log" \
	env "${FAN_ENV[@]}" \
		"$BIN" zcnblk-fan --engine wal --leaves "$LEAF_ADDRS" --bind "$FAN_BIND" \
		--base-port "$BASE_PORT" --ports "$LANES" --connections-per-port "$CONNECTIONS_PER_PORT" \
		--bytes-per-connection "$BYTES_PER_CONNECTION" --chunk-bytes "$CHUNK_BYTES" \
		--stripe-bytes "$CHUNK_BYTES" --leaf-base-port "$LEAF_BASE_PORT" \
		--pin-handlers true --mode "$RAID_MODE"

wait_listen "$BASE_PORT" "fan"

log "running zcnblk-send"
env "${SEND_ENV[@]}" \
	"$BIN" zcnblk-send "$SEND_ADDR" "$SHARD_COUNT" "$BASE_PORT" "$LANES" \
	"$CONNECTIONS_PER_PORT" "$BYTES_PER_CONNECTION" "$CHUNK_BYTES" "$LANES" \
	>"$OUTDIR/send.log" 2>&1

if [ "$RUN_MODE" = "local" ]; then
	log "sender completed; waiting for fan and local leaves"
else
	log "sender completed; waiting for fan"
fi
wait_status=0
for pid in "${PIDS[@]}"; do
	if ! wait "$pid"; then
		wait_status=1
	fi
done
PIDS=()
summarize_logs
exit "$wait_status"
