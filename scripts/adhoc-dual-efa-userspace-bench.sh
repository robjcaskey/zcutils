#!/usr/bin/env bash
set -euo pipefail

usage() {
	printf 'usage: %s CLIENT_SSH TARGET_SSH TARGET_EFA0_IP TARGET_EFA1_IP OUTPUT_DIR\n' "$0" >&2
	exit 2
}

[ "$#" -eq 5 ] || usage
client=$1
target=$2
target_ip0=$3
target_ip1=$4
out=$5

key=${ADHOC_SSH_KEY:-/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519}
remote_root=${REMOTE_ROOT:-/home/ubuntu/zcutils}
lanes=${LANES_PER_CARD:-40}
qd=${QD_PER_WORKER:-256}
payload=${PAYLOAD_PER_LANE:-4G}
repeats=${REPEATS:-3}
service_base=${SERVICE_BASE:-49000}
domain0=${EFA0_DOMAIN:-rdmap83s0-rdm}
domain1=${EFA1_DOMAIN:-rdmap166s0-rdm}
iface0=${EFA0_DEVICE:-rdmap83s0}
iface1=${EFA1_DEVICE:-rdmap166s0}
ssh=(ssh -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 -i "$key")

[ "$lanes" -le 40 ] || {
	printf 'this topology reserves CPUs for at most 40 lanes per EFA card\n' >&2
	exit 2
}
mkdir -p "$out"

target_pids=()
cleanup_targets() {
	local pid
	for pid in "${target_pids[@]:-}"; do
		[[ "$pid" =~ ^[0-9]+$ ]] || continue
		"${ssh[@]}" "$target" "if [ -r /proc/$pid/comm ] && [ \"\$(cat /proc/$pid/comm)\" = zcutils ]; then kill -TERM '$pid'; fi" >/dev/null 2>&1 || true
	done
}
trap cleanup_targets EXIT INT TERM

for ((rep = 1; rep <= repeats; rep++)); do
	service0=$((service_base + rep * 50))
	service1=$((service_base + 10000 + rep * 50))
	tag="dual-efa-lanes${lanes}x2-qd${qd}-${payload}-rep${rep}"
	remote_dir="$remote_root/bench-results/$(basename "$out")/$tag"
	env0="URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 URING_PLAY_OFI_DOMAIN=$domain0 FI_EFA_IFACE=$iface0 FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_OFI_TIMEOUT_MS=60000 URING_PLAY_OFI_BUSY_POLL_ITERS=100000 URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_ACK_WINDOW=$qd URING_PLAY_OFI_TX_QUEUE_DEPTH=$qd URING_PLAY_OFI_RX_QUEUE_DEPTH=$qd"
	env1="URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 URING_PLAY_OFI_DOMAIN=$domain1 FI_EFA_IFACE=$iface1 FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_OFI_TIMEOUT_MS=60000 URING_PLAY_OFI_BUSY_POLL_ITERS=100000 URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_ACK_WINDOW=$qd URING_PLAY_OFI_TX_QUEUE_DEPTH=$qd URING_PLAY_OFI_RX_QUEUE_DEPTH=$qd"

	pids=$("${ssh[@]}" "$target" "mkdir -p '$remote_dir'; nohup env $env0 URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=40-79 '$remote_root/target/release/zcutils' zcwal-ofi-recv efa-direct rdm '$target_ip0' '$service0' '$lanes' '$payload' 4K '$lanes' true >'$remote_dir/target0.log' 2>&1 </dev/null & p0=\$!; nohup env $env1 URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=136-175 '$remote_root/target/release/zcutils' zcwal-ofi-recv efa-direct rdm '$target_ip1' '$service1' '$lanes' '$payload' 4K '$lanes' true >'$remote_dir/target1.log' 2>&1 </dev/null & p1=\$!; printf '%s %s\\n' \$p0 \$p1")
	read -r target_pid0 target_pid1 <<<"$pids"
	[[ "$target_pid0" =~ ^[0-9]+$ && "$target_pid1" =~ ^[0-9]+$ ]] || {
		printf 'target did not return valid PIDs: %s\n' "$pids" >&2
		exit 1
	}
	target_pids=("$target_pid0" "$target_pid1")

	ready=0
	for _ in $(seq 1 1200); do
		listeners=$("${ssh[@]}" "$target" "ss -H -ltn | awk -v p0=':$((service0 + 1000))' -v p1=':$((service1 + 1000))' '\$4 ~ p0 \"\$\" || \$4 ~ p1 \"\$\" {n++} END{print n+0}'" 2>/dev/null || true)
		if [[ "$listeners" =~ ^[0-9]+$ ]] && [ "$listeners" -ge 2 ]; then
			ready=1
			break
		fi
		sleep 0.025
	done
	[ "$ready" -eq 1 ] || {
		printf 'target control listeners did not become ready\n' >&2
		exit 1
	}

	{
		printf 'completion=remote-application-hwm-ack durability=volatile-memory-test-only block_device=no\n'
		printf 'cards=2 lanes_per_card=%s workers_per_card=%s per_worker_qd=%s aggregate_outstanding_depth=%s\n' "$lanes" "$lanes" "$qd" "$((lanes * qd * 2))"
		printf 'card0_domain=%s card0_device=%s card0_client_cpus=0-39 card0_target_cpus=40-79 target_ip=%s\n' "$domain0" "$iface0" "$target_ip0"
		printf 'card1_domain=%s card1_device=%s card1_client_cpus=96-135 card1_target_cpus=136-175 target_ip=%s\n' "$domain1" "$iface1" "$target_ip1"
		for ((lane = 0; lane < lanes; lane++)); do
			printf 'card=0 lane=%s worker=%s client_cpu=%s target_cpu=%s\n' "$lane" "$lane" "$lane" "$((40 + lane))"
			printf 'card=1 lane=%s worker=%s client_cpu=%s target_cpu=%s\n' "$lane" "$lane" "$((96 + lane))" "$((136 + lane))"
		done
	} >"$out/$tag-topology.log"

	"${ssh[@]}" "$client" "env $env0 URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-39 '$remote_root/target/release/zcutils' zcwal-ofi-send efa-direct rdm '$target_ip0' '$service0' '$lanes' '$payload' 4K '$lanes' true" >"$out/$tag-client0.log" 2>&1 &
	client_pid0=$!
	"${ssh[@]}" "$client" "env $env1 URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=96-135 '$remote_root/target/release/zcutils' zcwal-ofi-send efa-direct rdm '$target_ip1' '$service1' '$lanes' '$payload' 4K '$lanes' true" >"$out/$tag-client1.log" 2>&1 &
	client_pid1=$!
	wait "$client_pid0"
	wait "$client_pid1"

	for _ in $(seq 1 1200); do
		live=$("${ssh[@]}" "$target" "for pid in '$target_pid0' '$target_pid1'; do [ -r /proc/\$pid/comm ] && [ \"\$(cat /proc/\$pid/comm)\" = zcutils ] && printf x; done" 2>/dev/null || true)
		[ -z "$live" ] && break
		sleep 0.025
	done
	"${ssh[@]}" "$target" "cat '$remote_dir/target0.log'" >"$out/$tag-target0.log"
	"${ssh[@]}" "$target" "cat '$remote_dir/target1.log'" >"$out/$tag-target1.log"
	target_pids=()

	line0=$(rg 'zcofi-wal-send-summary:' "$out/$tag-client0.log" | tail -1)
	line1=$(rg 'zcofi-wal-send-summary:' "$out/$tag-client1.log" | tail -1)
	ops0=$(sed -n 's/.*logical_records=\([0-9][0-9]*\).*/\1/p' <<<"$line0")
	ops1=$(sed -n 's/.*logical_records=\([0-9][0-9]*\).*/\1/p' <<<"$line1")
	sec0=$(sed -n 's/.*seconds=\([0-9.][0-9.]*\).*/\1/p' <<<"$line0")
	sec1=$(sed -n 's/.*seconds=\([0-9.][0-9.]*\).*/\1/p' <<<"$line1")
	awk -v rep="$rep" -v a="$ops0" -v b="$ops1" -v sa="$sec0" -v sb="$sec1" 'BEGIN { slow=(sa>sb?sa:sb); printf "repeat=%d completed_ops=%d synchronized_seconds=%.6f conservative_iops=%.0f payload_Gbitps=%.3f\n", rep, a+b, slow, (a+b)/slow, ((a+b)*4096*8)/slow/1e9 }' | tee -a "$out/summary.log"
done

trap - EXIT INT TERM
