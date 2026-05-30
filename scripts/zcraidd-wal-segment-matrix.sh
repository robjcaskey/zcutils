#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN:-$REPO_DIR/target/release/zcraidd}"
LOG_DIR="${LOG_DIR:-$REPO_DIR/qemu-zcrx/zcraidd-wal-segment-matrix-$(date -u +%Y%m%dT%H%M%SZ)}"

BYTES="${BYTES:-256m}"
RECORD_BYTES="${RECORD_BYTES:-4k}"
SEGMENT_BYTES_LIST="${SEGMENT_BYTES_LIST:-64k 384k 1m}"
STRIPES_LIST="${STRIPES_LIST:-8}"
MIRRORS_LIST="${MIRRORS_LIST:-2}"
SCHEDULERS="${SCHEDULERS:-wave}"
WAVE_SEGMENTS_LIST="${WAVE_SEGMENTS_LIST:-64}"
FANIN_MODES="${FANIN_MODES:-primary tree}"
INTEGRITY_MODES="${INTEGRITY_MODES:-none checksum}"
TREE_FANOUT="${TREE_FANOUT:-4}"
TREE_DEPTH="${TREE_DEPTH:-4}"
REPEATS="${REPEATS:-1}"
BUILD_RELEASE="${BUILD_RELEASE:-true}"
EXTRA_ARGS="${EXTRA_ARGS:-}"

usage() {
	cat <<'EOF'
usage: scripts/zcraidd-wal-segment-matrix.sh

Runs local zcraidd WAL segmentation/reassembly microbenchmarks. This creates
temporary files under /dev/shm or $TMPDIR through zcraidd and does not touch
block devices, cluster hosts, AWS, or /tmp/cluster.lock.

Environment:
  BYTES                 logical WAL bytes per run, default: 256m
  RECORD_BYTES          logical record size, default: 4k
  SEGMENT_BYTES_LIST    extent sizes to sweep, default: "64k 384k 1m"
  STRIPES_LIST          lane counts to sweep, default: "8"
  MIRRORS_LIST          mirror copies to sweep, default: "2"
  SCHEDULERS            zcraidd schedulers, default: "wave"
  WAVE_SEGMENTS_LIST    wave reaping credits, default: "64"
  FANIN_MODES           primary|tree, default: "primary tree"
  INTEGRITY_MODES       none|checksum|payload, default: "none checksum"
  TREE_FANOUT           descriptor tree fanout, default: 4
  TREE_DEPTH            descriptor tree depth, default: 4
  REPEATS               repeats per case, default: 1
  BUILD_RELEASE         cargo build --release --bin zcraidd first, default: true
  EXTRA_ARGS            appended to every zcraidd invocation
  LOG_DIR               output directory
EOF
}

log() {
	printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"
}

slug() {
	printf '%s' "$*" | tr ' (),/' '_____' | tr -cs 'A-Za-z0-9_.=-' '-' | sed 's/^-//; s/-$//'
}

emit_header() {
	local path="$1"
	printf 'case\trepeat\tscheduler\tfanin\tintegrity\tstripes\tmirrors\tsegment_bytes\twave_segments\trecords_per_segment\trecords\tsegments\tfanout_record_iops\tfanin_record_iops\teffective_record_iops\ttotal_seconds\tdescriptor_batches\tcomposite_batches\troot_batches\ttree_nodes\tlog\n' >"$path"
}

emit_summary_row() {
	local log_file="$1"
	local case_name="$2"
	local repeat="$3"
	local scheduler="$4"
	local fanin="$5"
	local integrity="$6"
	local stripes="$7"
	local mirrors="$8"
	local segment_bytes="$9"
	local wave_segments="${10}"
	local summary_file="${11}"

	python3 - "$log_file" "$case_name" "$repeat" "$scheduler" "$fanin" "$integrity" "$stripes" "$mirrors" "$segment_bytes" "$wave_segments" <<'PY' >>"$summary_file"
import sys

log_file, case_name, repeat, scheduler, fanin, integrity, stripes, mirrors, segment_bytes, wave_segments = sys.argv[1:]
result = None
with open(log_file, "r", encoding="utf-8", errors="replace") as handle:
    for line in handle:
        if line.startswith("zcraidd-wal-result:"):
            result = line.strip()
if result is None:
    print("\t".join([
        case_name, repeat, scheduler, fanin, integrity, stripes, mirrors,
        segment_bytes, wave_segments, "", "", "", "", "", "", "", "", "",
        "", "", log_file,
    ]))
    raise SystemExit(0)

fields = {}
for part in result.split():
    if "=" in part:
        key, value = part.split("=", 1)
        fields[key.rstrip(":")] = value

def get(name):
    return fields.get(name, "")

print("\t".join([
    case_name,
    repeat,
    get("scheduler") or scheduler,
    get("fanin_mode") or fanin,
    integrity,
    get("stripes") or stripes,
    get("mirrors") or mirrors,
    get("segment_bytes") or segment_bytes,
    get("wave_segments") or wave_segments,
    get("records_per_segment"),
    get("records"),
    get("segments"),
    get("fanout_record_iops"),
    get("fanin_record_iops"),
    get("effective_record_iops"),
    get("total_seconds"),
    get("descriptor_batches"),
    get("composite_batches"),
    get("root_batches"),
    get("tree_nodes"),
    log_file,
]))
PY
}

integrity_args() {
	case "$1" in
		none)
			printf '%s\n' "--no-checksum --verify none"
			;;
		checksum)
			printf '%s\n' "--checksum --verify checksum"
			;;
		payload)
			printf '%s\n' "--checksum --verify payload"
			;;
		*)
			printf 'unknown INTEGRITY_MODES entry: %s\n' "$1" >&2
			return 1
			;;
	esac
}

main() {
	if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ] || [ "${1:-}" = "help" ]; then
		usage
		exit 0
	fi

	if [ "$BUILD_RELEASE" = true ]; then
		log "building release zcraidd"
		cargo build --release --bin zcraidd --manifest-path "$REPO_DIR/Cargo.toml"
	fi
	if [ ! -x "$BIN" ]; then
		printf 'missing executable: %s\n' "$BIN" >&2
		exit 1
	fi

	mkdir -p "$LOG_DIR"
	local summary_file="$LOG_DIR/summary.tsv"
	emit_header "$summary_file"
	printf 'zcraidd-wal-segment-matrix-config bytes=%q record_bytes=%q segments=%q stripes=%q mirrors=%q schedulers=%q wave_segments=%q fanin_modes=%q integrity_modes=%q repeats=%q log_dir=%q\n' \
		"$BYTES" "$RECORD_BYTES" "$SEGMENT_BYTES_LIST" "$STRIPES_LIST" "$MIRRORS_LIST" \
		"$SCHEDULERS" "$WAVE_SEGMENTS_LIST" "$FANIN_MODES" "$INTEGRITY_MODES" "$REPEATS" "$LOG_DIR" |
		tee "$LOG_DIR/config.log"

	local repeat stripes mirrors segment_bytes scheduler wave_segments fanin integrity shape args case_name log_file
	for repeat in $(seq 1 "$REPEATS"); do
		for stripes in $STRIPES_LIST; do
			for mirrors in $MIRRORS_LIST; do
				shape="stripe($stripes,mirror($mirrors))"
				for scheduler in $SCHEDULERS; do
					for wave_segments in $WAVE_SEGMENTS_LIST; do
						for segment_bytes in $SEGMENT_BYTES_LIST; do
							for fanin in $FANIN_MODES; do
								for integrity in $INTEGRITY_MODES; do
									if [ "$fanin" = tree ] && [ "$integrity" != none ]; then
										log "skip incompatible fanin=tree integrity=$integrity; tree fanin is descriptor-only"
										continue
									fi
									args="$(integrity_args "$integrity")"
									case_name="$(slug "r${repeat}-${scheduler}-${fanin}-${integrity}-${shape}-${segment_bytes}-w${wave_segments}")"
									log_file="$LOG_DIR/${case_name}.log"
									log "run case=$case_name"
									# shellcheck disable=SC2086
									"$BIN" wal-bench \
										--shape "$shape" \
										--bytes "$BYTES" \
										--record-bytes "$RECORD_BYTES" \
										--segment-bytes "$segment_bytes" \
										--scheduler "$scheduler" \
										--wave-segments "$wave_segments" \
										--fanin-mode "$fanin" \
										--tree-fanout "$TREE_FANOUT" \
										--tree-depth "$TREE_DEPTH" \
										$args $EXTRA_ARGS >"$log_file" 2>&1
									emit_summary_row "$log_file" "$case_name" "$repeat" "$scheduler" "$fanin" \
										"$integrity" "$stripes" "$mirrors" "$segment_bytes" "$wave_segments" "$summary_file"
								done
							done
						done
					done
				done
			done
		done
	done

	log "summary: $summary_file"
	if command -v column >/dev/null 2>&1; then
		column -t -s $'\t' "$summary_file" | tee "$LOG_DIR/summary.txt"
	else
		cp "$summary_file" "$LOG_DIR/summary.txt"
		cat "$summary_file"
	fi
}

main "$@"
