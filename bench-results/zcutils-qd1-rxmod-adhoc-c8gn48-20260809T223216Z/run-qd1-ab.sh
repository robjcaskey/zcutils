#!/usr/bin/env bash
set -euo pipefail

condition="${1:?usage: run-qd1-ab.sh <adaptive|fixed-zero> <prepare|leaf|client|collect>}"
phase="${2:?usage: run-qd1-ab.sh <adaptive|fixed-zero> <prepare|leaf|client|collect>}"
root=/home/ubuntu/zcutils
run_root="$root/bench-results/qd1-rxmod-ab/$condition"
node_index="${URING_NODE_INDEX:?URING_NODE_INDEX is required}"

interface_for_ip() {
	ip -o -4 addr show | awk -v address="$1" '$4 ~ ("^" address "/") { print $2; exit }'
}

nic0="$(interface_for_ip "$([ "$node_index" = 1 ] && printf 172.31.35.138 || printf 172.31.37.66)")"
nic1="$(interface_for_ip "$([ "$node_index" = 1 ] && printf 172.31.43.235 || printf 172.31.32.228)")"
[ -n "$nic0" ] && [ -n "$nic1" ]

snapshot_network() {
	local label="$1" iface
	mkdir -p "$run_root/node$node_index"
	{
		printf 'condition=%s node=%s label=%s nic0=%s nic1=%s\n' \
			"$condition" "$node_index" "$label" "$nic0" "$nic1"
		for iface in "$nic0" "$nic1"; do
			printf '[interface=%s]\n' "$iface"
			ethtool -i "$iface"
			ethtool -c "$iface"
			ethtool -l "$iface"
			ethtool -x "$iface" 2>/dev/null || true
		done
		printf '[interrupts]\n'
		grep -E 'ena|Elastic Network Adapter' /proc/interrupts || true
	} >"$run_root/node$node_index/network-$label.log"
}

case "$phase" in
	prepare)
		if [ "$condition" = fixed-zero ]; then
			sudo -n ethtool -C "$nic0" adaptive-rx off rx-usecs 0 tx-usecs 0
			sudo -n ethtool -C "$nic1" adaptive-rx off rx-usecs 0 tx-usecs 0
		elif [ "$condition" != adaptive ]; then
			printf 'unknown condition: %s\n' "$condition" >&2
			exit 1
		fi
		snapshot_network before
		;;
	leaf)
		[ "$node_index" = 2 ] || exit 0
		mkdir -p "$run_root/leaf"
		pid_file="$run_root/leaf/leaf.pid"
		if [ -s "$pid_file" ] && kill -0 "$(cat "$pid_file")" 2>/dev/null; then
			printf 'leaf already running pid=%s\n' "$(cat "$pid_file")" >&2
			exit 1
		fi
		nohup env \
			URING_PLAY_PIN_CPU_LIST=0-15,96-111 \
			URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1 \
			URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MIN=256 \
			URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MAX=65536 \
			URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_WAIT_NS=50000 \
			URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_HYSTERESIS_NS=10000000 \
			URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
			"$root/target/release/zcnblk-wal-leaf" \
			zcmem:4096M 0.0.0.0 29000 32 1 4096 32 true blocking \
			>"$run_root/leaf/leaf.log" 2>&1 </dev/null &
		leaf_pid=$!
		printf '%s\n' "$leaf_pid" >"$pid_file"
		for _ in $(seq 1 200); do
			listeners="$(ss -ltnH '( sport >= 29000 and sport <= 29031 )' | wc -l)"
			[ "$listeners" -eq 32 ] && break
			kill -0 "$leaf_pid" 2>/dev/null || {
				tail -n 80 "$run_root/leaf/leaf.log" >&2
				exit 1
			}
			sleep 0.05
		done
		[ "$(ss -ltnH '( sport >= 29000 and sport <= 29031 )' | wc -l)" -eq 32 ]
		printf 'leaf_ready=true pid=%s\n' "$leaf_pid"
		;;
	client)
		[ "$node_index" = 1 ] || exit 0
		mkdir -p "$run_root/client"
		leaf_addrs="$(printf '172.31.37.66:29000,%.0s' {1..15})172.31.37.66:29000,$(printf '172.31.32.228:29000,%.0s' {1..15})172.31.32.228:29000"
		source_addrs="$(printf '172.31.35.138,%.0s' {1..15})172.31.35.138,$(printf '172.31.43.235,%.0s' {1..15})172.31.43.235"
		env \
			COORDINATION_SCOPE=dedicated-adhoc REPRESENTATIVE=1 BUILD=0 \
			BACKEND=wal-tcp START_LOCAL_LEAF=0 \
			LEAF_ADDR=172.31.37.66 LEAF_ADDRS="$leaf_addrs" \
			LEAF_SOURCE_ADDRS="$source_addrs" LEAF_PORT=29000 \
			LANES=32 REPEATS=3 OPS_PER_WORKER=20000 IODEPTH=1 \
			KERNEL_QUEUE_DEPTH=1 KERNEL_PIPELINE_DEPTH=1 \
			MODE=read READ_PERCENT=100 BUFFER_MODE=hugetlb \
			PERF_STAT=0 KERNEL_POLL_US=1000 \
			URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE=1 \
			URING_PLAY_BLOCKBENCH_CQE_HOT_POLL=1 \
			URING_PLAY_BLOCKBENCH_CQE_HOT_POLL_PROGRESS_SPINS=256 \
			URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_POLICY=adaptive \
			URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MIN=0 \
			URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MAX=65536 \
			URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_WAIT_NS=50000 \
			URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_HYSTERESIS_NS=10000000 \
			OUTDIR="$run_root/client" \
			"$root/scripts/zcnblk-shm-block-bench.sh"
		;;
	collect)
		snapshot_network after
		if [ "$node_index" = 2 ]; then
			pid_file="$run_root/leaf/leaf.pid"
			if [ -s "$pid_file" ]; then
				leaf_pid="$(cat "$pid_file")"
				for _ in $(seq 1 100); do
					kill -0 "$leaf_pid" 2>/dev/null || break
					sleep 0.05
				done
				if kill -0 "$leaf_pid" 2>/dev/null; then
					comm="$(cat "/proc/$leaf_pid/comm")"
					[ "$comm" = zcnblk-wal-lea ]
					kill -TERM "$leaf_pid"
				fi
			fi
		fi
		;;
	*)
		printf 'unknown phase: %s\n' "$phase" >&2
		exit 1
		;;
esac
