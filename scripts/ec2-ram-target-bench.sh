#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN:-$REPO_DIR/target/release/zcutils}"
LOG_DIR="${LOG_DIR:-$REPO_DIR/qemu-zcrx/ec2-ramtarget-$(date -u +%Y%m%dT%H%M%SZ)}"

CPUS="$(nproc)"
ZCBRD_DEVS="${ZCBRD_DEVS:-2}"
ZCBRD_SIZE_MIB="${ZCBRD_SIZE_MIB:-131072}"
ZCBRD_BLOCKSIZE="${ZCBRD_BLOCKSIZE:-4096}"
ZCBRD_QUEUES="${ZCBRD_QUEUES:-$CPUS}"
ZCBRD_QUEUE_DEPTH="${ZCBRD_QUEUE_DEPTH:-2048}"
ZCBRD_SHARDS="${ZCBRD_SHARDS:-$CPUS}"

LOCAL_WORKERS="${LOCAL_WORKERS:-$CPUS}"
LOCAL_BYTES_PER_WORKER="${LOCAL_BYTES_PER_WORKER:-268435456}"
LOCAL_CHUNK="${LOCAL_CHUNK:-65536}"
LOCAL_PIPELINE="${LOCAL_PIPELINE:-128}"
LOCAL_RING="${LOCAL_RING:-1024}"

BASE_PORT="${BASE_PORT:-19400}"
PORTS="${PORTS:-16}"
CONNS_PER_PORT="${CONNS_PER_PORT:-8}"
BYTES_PER_CONN="${BYTES_PER_CONN:-1073741824}"
NET_CHUNK="${NET_CHUNK:-65536}"
NET_PIPELINE="${NET_PIPELINE:-128}"
NET_WORKERS="${NET_WORKERS:-64}"
NET_RING="${NET_RING:-2048}"
SERVER_WRITE_MODE="${SERVER_WRITE_MODE:-fixed-file}"
SERVER_PIPELINE_MODE="${SERVER_PIPELINE_MODE:-same-core}"
WAL_BASE_OFFSET_BYTES="${WAL_BASE_OFFSET_BYTES:-0}"
WAL_REGION_BYTES="${WAL_REGION_BYTES:-}"
SOURCE_IP="${SOURCE_IP:-}"
SEND_MODE="${SEND_MODE:-send}"

usage() {
	cat <<'EOF'
usage: scripts/ec2-ram-target-bench.sh <command> [target-ip]

commands:
  bootstrap       install build dependencies and rustup
  build           build zcutils and the zcbrd lab module
  target-setup    load modules and create /dev/zcbrd[0..]
  target-local    run local RAM-device write benchmarks on the target node
  target-server   start tcp-wal-mux-server in tmux on the target node
  target-stop     stop the target tmux server if it is still running
  source-client   run tcp-bench-uring-mux-send to the target private IP
  probe           print kernel, NIC, block, and zcprobe details
EOF
}

log() {
	printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"
}

tee_attr() {
	local path="$1"
	local value="$2"
	printf '%s\n' "$value" | sudo tee "$path" >/dev/null
}

ensure_rust() {
	export PATH="$HOME/.cargo/bin:$PATH"
	if command -v rustc >/dev/null 2>&1; then
		local version major minor
		version="$(rustc -V | awk '{print $2}')"
		major="${version%%.*}"
		minor="${version#*.}"
		minor="${minor%%.*}"
		if [ "$major" -gt 1 ] || { [ "$major" -eq 1 ] && [ "$minor" -ge 85 ]; }; then
			return
		fi
	fi
	curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
		sh -s -- -y --profile minimal --default-toolchain stable
	export PATH="$HOME/.cargo/bin:$PATH"
}

cmd_bootstrap() {
	log "installing OS dependencies"
	if sudo grep -Rqs 'ec2\.ports\.ubuntu\.com' /etc/apt/sources.list /etc/apt/sources.list.d; then
		sudo sed -i \
			's|http://[^ ]*ec2\.ports\.ubuntu\.com/ubuntu-ports|http://ports.ubuntu.com/ubuntu-ports|g' \
			/etc/apt/sources.list /etc/apt/sources.list.d/*.sources 2>/dev/null || true
		sudo rm -rf /var/lib/apt/lists/*
	fi
	for attempt in 1 2 3; do
		if sudo apt-get update; then
			break
		fi
		if [ "$attempt" -eq 3 ]; then
			return 1
		fi
		sleep $((attempt * 5))
	done
	DEBIAN_FRONTEND=noninteractive sudo apt-get install -y \
		bc build-essential ca-certificates clang cmake curl dwarves flex bison \
		ethtool git jq libssl-dev liburing-dev linux-headers-"$(uname -r)" ninja-build \
		perl pkg-config rsync tmux
	ensure_rust
	rustc -V
	cargo -V
}

cmd_build() {
	ensure_rust
	log "building zcutils release binary"
	(cd "$REPO_DIR" && cargo build --release --bin zcutils --bin zcprobe)
	log "building zcbrd RAM backing module"
	(cd "$REPO_DIR" && make -C kmods)
}

wait_for_path() {
	local path="$1"
	local tries="${2:-100}"
	for _ in $(seq 1 "$tries"); do
		[ -e "$path" ] && return 0
		sleep 0.1
	done
	return 1
}

reset_configfs_dir() {
	local dir="$1"
	if [ -e "$dir/power" ]; then
		tee_attr "$dir/power" 0 || true
	fi
	if [ -d "$dir" ]; then
		sudo rmdir "$dir" 2>/dev/null || true
	fi
}

load_modules() {
	sudo modprobe configfs
	if ! mountpoint -q /sys/kernel/config; then
		sudo mount -t configfs configfs /sys/kernel/config
	fi
	if ! lsmod | awk '{print $1}' | grep -qx zcbrd_mod; then
		sudo insmod "$REPO_DIR/kmods/zcbrd_mod.ko"
	fi
}

create_zcbrd() {
	local name="$1"
	local dir="/sys/kernel/config/zcbrd/$name"
	reset_configfs_dir "$dir"
	sudo mkdir "$dir"
	tee_attr "$dir/size_mib" "$ZCBRD_SIZE_MIB"
	tee_attr "$dir/blocksize" "$ZCBRD_BLOCKSIZE"
	tee_attr "$dir/queues" "$ZCBRD_QUEUES"
	tee_attr "$dir/queue_depth" "$ZCBRD_QUEUE_DEPTH"
	tee_attr "$dir/shards" "$ZCBRD_SHARDS"
	tee_attr "$dir/descriptor_mode" advertise
	tee_attr "$dir/power" 1
	wait_for_path "/dev/$name"
}

cmd_target_setup() {
	cmd_build
	load_modules
	for idx in $(seq 0 $((ZCBRD_DEVS - 1))); do
		create_zcbrd "zcbrd$idx"
	done

	mkdir -p "$LOG_DIR"
	lsblk -b -o NAME,SIZE,LOG-SEC,PHY-SEC,ROTA,TYPE /dev/zcbrd* |
		tee "$LOG_DIR/target-block-devices.log"
}

cmd_probe() {
	mkdir -p "$LOG_DIR"
	{
		date -u
		uname -a
		lscpu
		ip -br addr
		ethtool -i "$(ip route show default | awk '{print $5; exit}')" 2>/dev/null || true
		"$BIN" zcprobe || true
		lsblk -b -o NAME,SIZE,LOG-SEC,PHY-SEC,ROTA,TYPE || true
	} | tee "$LOG_DIR/probe.log"
}

cmd_target_local() {
	mkdir -p "$LOG_DIR"
	cmd_probe
	for dev in /dev/zcbrd*; do
		[ -b "$dev" ] || continue
		log "local uring write bench $dev"
		URING_PLAY_URING_WRITE_COMPLETION_BATCH="${URING_PLAY_URING_WRITE_COMPLETION_BATCH:-64}" \
			"$BIN" uring-write-bench "$dev" "$LOCAL_WORKERS" "$LOCAL_BYTES_PER_WORKER" \
			"$LOCAL_CHUNK" "$LOCAL_PIPELINE" "$LOCAL_RING" small-pages fixed-file true |
			tee "$LOG_DIR/local-$(basename "$dev").log"
	done
}

cmd_target_server() {
	mkdir -p "$LOG_DIR"
	local session="${TMUX_SESSION:-zc-ram-target-$BASE_PORT}"
	tmux kill-session -t "$session" 2>/dev/null || true
	local wal_region_env=""
	if [ -n "$WAL_REGION_BYTES" ]; then
		wal_region_env="URING_PLAY_WAL_REGION_BYTES=$WAL_REGION_BYTES"
	fi
	local cmd
	printf -v cmd \
		'cd %q && mkdir -p %q && URING_PLAY_TCP_WAL_MODE=%q URING_PLAY_TCP_WAL_WRITE_MODE=%q URING_PLAY_WAL_BASE_OFFSET_BYTES=%q %s %q tcp-wal-mux-server /dev/zcbrd0 0.0.0.0 %q %q %q %q %q %q %q %q small-pages true 2>&1 | tee %q' \
		"$REPO_DIR" "$LOG_DIR" "$SERVER_PIPELINE_MODE" "$SERVER_WRITE_MODE" \
		"$WAL_BASE_OFFSET_BYTES" "$wal_region_env" "$BIN" "$BASE_PORT" "$PORTS" \
		"$CONNS_PER_PORT" "$BYTES_PER_CONN" "$NET_CHUNK" "$NET_PIPELINE" \
		"$NET_WORKERS" "$NET_RING" \
		"$LOG_DIR/server-${SERVER_WRITE_MODE}-${BASE_PORT}.log"
	log "starting target server in tmux session $session"
	tmux new-session -d -s "$session" "$cmd"
	sleep 2
	tmux capture-pane -pt "$session" -S -80 || true
}

cmd_target_stop() {
	local session="${TMUX_SESSION:-zc-ram-target-$BASE_PORT}"
	tmux kill-session -t "$session" 2>/dev/null || true
}

cmd_source_client() {
	local target_ip="${1:-}"
	if [ -z "$target_ip" ]; then
		printf 'source-client requires target private IP\n' >&2
		exit 2
	fi
	mkdir -p "$LOG_DIR"
	cmd_probe
	local source_label="kernel-route"
	if [ -n "$SOURCE_IP" ]; then
		export URING_PLAY_SOURCE_IP="$SOURCE_IP"
		source_label="$SOURCE_IP"
	fi
	log "client send target=$target_ip source=$source_label"
	"$BIN" tcp-bench-uring-mux-send "$target_ip" "$BASE_PORT" "$PORTS" "$CONNS_PER_PORT" \
		"$BYTES_PER_CONN" "$NET_CHUNK" "$NET_PIPELINE" "$NET_WORKERS" "$NET_RING" "$SEND_MODE" |
		tee "$LOG_DIR/client-${target_ip}-${BASE_PORT}.log"
}

cmd="${1:-}"
case "$cmd" in
	bootstrap) cmd_bootstrap ;;
	build) cmd_build ;;
	target-setup) cmd_target_setup ;;
	target-local) cmd_target_local ;;
	target-server) cmd_target_server ;;
	target-stop) cmd_target_stop ;;
	source-client) shift; cmd_source_client "$@" ;;
	probe) cmd_probe ;;
	-h|--help|help|"") usage ;;
	*) usage >&2; exit 2 ;;
esac
