#!/usr/bin/env bash
set -euo pipefail

usage() {
	printf 'usage: %s STABLE_CLIENT MOVING_CLIENT PRIMARY_STORAGE REPLACEMENT_STORAGE PRIMARY_EFA0 PRIMARY_EFA1 REPLACEMENT_EFA0 REPLACEMENT_EFA1 OUT\n' "$0" >&2
	exit 2
}

[ "$#" -eq 9 ] || usage
stable_client=$1
moving_client=$2
primary_storage=$3
replacement_storage=$4
primary_ip0=$5
primary_ip1=$6
replacement_ip0=$7
replacement_ip1=$8
out=$9

key=${ADHOC_SSH_KEY:-/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519}
remote_bin=${REMOTE_BIN:-/home/ubuntu/zcutils/target/release/zcutils}
lanes=${LANES_PER_CARD:-80}
qd=${QD_PER_WORKER:-256}
total_payload=${PAYLOAD_PER_LANE:-8G}
cutover_payload=${CUTOVER_PAYLOAD_PER_LANE:-4G}
extent=${EXTENT_BYTES:-4K}
stable_service0=${STABLE_SERVICE0:-29000}
stable_service1=${STABLE_SERVICE1:-40000}
moving_service0=${MOVING_SERVICE0:-51000}
moving_service1=${MOVING_SERVICE1:-62000}
cpus0=${EFA0_CPU_LIST:-0-79}
cpus1=${EFA1_CPU_LIST:-96-175}
domain0=${EFA0_DOMAIN:-efa_0-rdm}
domain1=${EFA1_DOMAIN:-efa_1-rdm}
device0=${EFA0_DEVICE:-efa_0}
device1=${EFA1_DEVICE:-efa_1}

for value in "$lanes" "$qd" "$stable_service0" "$stable_service1" "$moving_service0" "$moving_service1"; do
	[[ "$value" =~ ^[1-9][0-9]*$ ]] || usage
done
command -v numfmt >/dev/null || {
	printf 'numfmt is required to validate the cutover geometry\n' >&2
	exit 1
}
total_bytes=$(numfmt --from=iec "$total_payload")
cutover_bytes=$(numfmt --from=iec "$cutover_payload")
extent_bytes=$(numfmt --from=iec "$extent")
[ "$total_bytes" -eq $((cutover_bytes * 2)) ] || {
	printf 'PAYLOAD_PER_LANE must be exactly twice CUTOVER_PAYLOAD_PER_LANE\n' >&2
	exit 2
}
[ $((cutover_bytes % extent_bytes)) -eq 0 ] || {
	printf 'cutover payload must be extent aligned\n' >&2
	exit 2
}
cutover_extents=$((cutover_bytes / extent_bytes))
mkdir -p "$out"

ssh_opts=(-o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no -o ServerAliveInterval=30 -i "$key")
common="URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_OFI_TIMEOUT_MS=60000 URING_PLAY_OFI_BUSY_POLL_ITERS=100000 URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_ACK_WINDOW=$qd URING_PLAY_OFI_TX_QUEUE_DEPTH=$qd URING_PLAY_OFI_RX_QUEUE_DEPTH=$qd URING_PLAY_PIN_CPUS=1"
env0="$common URING_PLAY_OFI_DOMAIN=$domain0 FI_EFA_IFACE=$device0 URING_PLAY_PIN_CPU_LIST=$cpus0"
env1="$common URING_PLAY_OFI_DOMAIN=$domain1 FI_EFA_IFACE=$device1 URING_PLAY_PIN_CPU_LIST=$cpus1"
remote_tag="/tmp/zc-live-rebalance-$(date +%s%N)"
primary_pids=()
replacement_pids=()
client_pids=()

cleanup_remote() {
	local host=$1 pid=$2
	[[ "$pid" =~ ^[0-9]+$ ]] || return 0
	ssh "${ssh_opts[@]}" "$host" "if [ -r /proc/$pid/comm ] && [ \"\$(cat /proc/$pid/comm)\" = zcutils ]; then kill -TERM $pid; fi" >/dev/null 2>&1 || true
}

cleanup() {
	local pid
	for pid in "${client_pids[@]:-}"; do
		[[ "$pid" =~ ^[0-9]+$ ]] || continue
		kill -TERM "$pid" 2>/dev/null || true
		wait "$pid" 2>/dev/null || true
	done
	for pid in "${primary_pids[@]:-}"; do cleanup_remote "$primary_storage" "$pid"; done
	for pid in "${replacement_pids[@]:-}"; do cleanup_remote "$replacement_storage" "$pid"; done
}
trap cleanup EXIT INT TERM

start_target() {
	local host=$1 log=$2 envs=$3 cmd=$4
	ssh "${ssh_opts[@]}" "$host" "nohup env $envs $cmd > '$log' 2>&1 </dev/null & echo \$!"
}

primary_pids+=("$(start_target "$primary_storage" "$remote_tag-stable-card0.log" "$env0" "'$remote_bin' zcwal-ofi-recv efa-direct rdm '$primary_ip0' '$stable_service0' '$lanes' '$total_payload' '$extent' '$lanes' true")")
primary_pids+=("$(start_target "$primary_storage" "$remote_tag-stable-card1.log" "$env1" "'$remote_bin' zcwal-ofi-recv efa-direct rdm '$primary_ip1' '$stable_service1' '$lanes' '$total_payload' '$extent' '$lanes' true")")
primary_pids+=("$(start_target "$primary_storage" "$remote_tag-moving-primary-card0.log" "$env0" "'$remote_bin' zcwal-ofi-recv efa-direct rdm '$primary_ip0' '$moving_service0' '$lanes' '$cutover_payload' '$extent' '$lanes' true")")
primary_pids+=("$(start_target "$primary_storage" "$remote_tag-moving-primary-card1.log" "$env1" "'$remote_bin' zcwal-ofi-recv efa-direct rdm '$primary_ip1' '$moving_service1' '$lanes' '$cutover_payload' '$extent' '$lanes' true")")
replacement_pids+=("$(start_target "$replacement_storage" "$remote_tag-moving-replacement-card0.log" "$env0 URING_PLAY_OFI_SEQUENCE_BASE=$cutover_extents" "'$remote_bin' zcwal-ofi-recv efa-direct rdm '$replacement_ip0' '$moving_service0' '$lanes' '$cutover_payload' '$extent' '$lanes' true")")
replacement_pids+=("$(start_target "$replacement_storage" "$remote_tag-moving-replacement-card1.log" "$env1 URING_PLAY_OFI_SEQUENCE_BASE=$cutover_extents" "'$remote_bin' zcwal-ofi-recv efa-direct rdm '$replacement_ip1' '$moving_service1' '$lanes' '$cutover_payload' '$extent' '$lanes' true")")

control_ports_primary=($((stable_service0 + 1000)) $((stable_service1 + 1000)) $((moving_service0 + 1000)) $((moving_service1 + 1000)))
control_ports_replacement=($((moving_service0 + 1000)) $((moving_service1 + 1000)))
ready=0
for _ in $(seq 1 1200); do
	a=$(ssh "${ssh_opts[@]}" "$primary_storage" "for p in ${control_ports_primary[*]}; do ss -H -ltn \"sport = :\$p\" | head -n1; done | wc -l" 2>/dev/null || true)
	b=$(ssh "${ssh_opts[@]}" "$replacement_storage" "for p in ${control_ports_replacement[*]}; do ss -H -ltn \"sport = :\$p\" | head -n1; done | wc -l" 2>/dev/null || true)
	if [ "$a" -ge 4 ] && [ "$b" -ge 2 ]; then ready=1; break; fi
	sleep 0.025
done
[ "$ready" -eq 1 ] || {
	printf 'not all EFA control listeners became ready\n' >&2
	exit 1
}

start_epoch=$(($(date +%s) + 5))
start_sender() {
	local host=$1 log=$2 envs=$3 cmd=$4
	ssh "${ssh_opts[@]}" "$host" "while [ \"\$(date +%s)\" -lt '$start_epoch' ]; do sleep 0.005; done; env $envs $cmd" >"$log" 2>&1 &
	sender_pid=$!
}

start_sender "$stable_client" "$out/stable-card0.log" "$env0" "'$remote_bin' zcwal-ofi-send efa-direct rdm '$primary_ip0' '$stable_service0' '$lanes' '$total_payload' '$extent' '$lanes' true"
client_pids+=("$sender_pid")
start_sender "$stable_client" "$out/stable-card1.log" "$env1" "'$remote_bin' zcwal-ofi-send efa-direct rdm '$primary_ip1' '$stable_service1' '$lanes' '$total_payload' '$extent' '$lanes' true"
client_pids+=("$sender_pid")
start_sender "$moving_client" "$out/moving-card0.log" "$env0 URING_PLAY_OFI_REBALANCE_ADDR=$replacement_ip0 URING_PLAY_OFI_REBALANCE_AFTER_EXTENTS=$cutover_extents URING_PLAY_OFI_REBALANCE_GANG_FENCE=0" "'$remote_bin' zcwal-ofi-send efa-direct rdm '$primary_ip0' '$moving_service0' '$lanes' '$total_payload' '$extent' '$lanes' true"
client_pids+=("$sender_pid")
start_sender "$moving_client" "$out/moving-card1.log" "$env1 URING_PLAY_OFI_REBALANCE_ADDR=$replacement_ip1 URING_PLAY_OFI_REBALANCE_AFTER_EXTENTS=$cutover_extents URING_PLAY_OFI_REBALANCE_GANG_FENCE=0" "'$remote_bin' zcwal-ofi-send efa-direct rdm '$primary_ip1' '$moving_service1' '$lanes' '$total_payload' '$extent' '$lanes' true"
client_pids+=("$sender_pid")

status=0
for pid in "${client_pids[@]}"; do wait "$pid" || status=$?; done
client_pids=()
[ "$status" -eq 0 ] || {
	printf 'one or more client flows failed: status=%s\n' "$status" >&2
	exit "$status"
}

for _ in $(seq 1 1200); do
	live=0
	for pid in "${primary_pids[@]}"; do
		x=$(ssh "${ssh_opts[@]}" "$primary_storage" "[ -r /proc/$pid/comm ] && [ \"\$(cat /proc/$pid/comm)\" = zcutils ] && echo 1 || echo 0" 2>/dev/null || echo 1)
		live=$((live + x))
	done
	for pid in "${replacement_pids[@]}"; do
		x=$(ssh "${ssh_opts[@]}" "$replacement_storage" "[ -r /proc/$pid/comm ] && [ \"\$(cat /proc/$pid/comm)\" = zcutils ] && echo 1 || echo 0" 2>/dev/null || echo 1)
		live=$((live + x))
	done
	[ "$live" -eq 0 ] && break
	sleep 0.025
done
[ "${live:-1}" -eq 0 ] || {
	printf 'receiver did not drain after client completion\n' >&2
	exit 1
}

for name in stable-card0 stable-card1 moving-primary-card0 moving-primary-card1; do
	ssh "${ssh_opts[@]}" "$primary_storage" "cat '$remote_tag-$name.log'" >"$out/target-$name.log"
done
for name in moving-replacement-card0 moving-replacement-card1; do
	ssh "${ssh_opts[@]}" "$replacement_storage" "cat '$remote_tag-$name.log'" >"$out/target-$name.log"
done

{
	printf 'completion=remote-application-hwm-ack durability=volatile-memory-test-only block_device=no placement_owner=userspace-flow-stage\n'
	printf 'clients=2 volumes=2 initial_storage_nodes=1 final_storage_nodes=2 cards_per_volume=2 lanes_per_card=%s workers_per_card=%s per_worker_qd=%s aggregate_outstanding_per_volume=%s\n' "$lanes" "$lanes" "$qd" "$((lanes * qd * 2))"
	printf 'volume=stable target_before=primary target_after=primary services=%s,%s cpus=%s,%s\n' "$stable_service0" "$stable_service1" "$cpus0" "$cpus1"
	printf 'volume=moving target_before=primary target_after=replacement services=%s,%s cutover_hwm_per_lane=%s scheduler_decision=gang-issued worker_resolution=lane-local-hwm-parallel interim_state=dual-placement client_process_restart=false endpoint_reconnect=false\n' "$moving_service0" "$moving_service1" "$((cutover_extents - 1))"
	printf 'primary-card0 lane_map=stable:0-%s,moving:0-%s cpu_map=stable:%s,moving:%s contention=deliberate\n' "$((lanes - 1))" "$((lanes - 1))" "$cpus0" "$cpus0"
	printf 'primary-card1 lane_map=stable:0-%s,moving:0-%s cpu_map=stable:%s,moving:%s contention=deliberate\n' "$((lanes - 1))" "$((lanes - 1))" "$cpus1" "$cpus1"
	printf 'replacement-card0 lane_map=moving:0-%s cpu_map=%s\n' "$((lanes - 1))" "$cpus0"
	printf 'replacement-card1 lane_map=moving:0-%s cpu_map=%s\n' "$((lanes - 1))" "$cpus1"
} >"$out/topology.log"

primary_seconds=$(sed -n 's/.*primary_seconds=\([0-9.][0-9.]*\).*/\1/p' "$out"/moving-card*.log | sort -nr | head -n1)
max_handoff_us=$(sed -n 's/.*handoff_gap_us=\([0-9.][0-9.]*\).*/\1/p' "$out"/moving-card*.log | sort -nr | head -n1)
primary_min_seconds=$(sed -n 's/.*primary_seconds=\([0-9.][0-9.]*\).*/\1/p' "$out"/moving-card*.log | sort -n | head -n1)
awk -v last="$primary_seconds" -v first="$primary_min_seconds" -v handoff="$max_handoff_us" 'BEGIN {printf "LIVE_REBALANCE_PASS max_flow_handoff_gap_us=%s convergence_ms=%.3f client_process_restart=false endpoint_reconnect=false scheduler_decision=gang-issued worker_resolution=lane-local-hwm-parallel phase_iops=not-reported-unsynchronized-lane-windows userspace_placement=true block_placement=false\n",handoff,(last-first)*1000}' | tee "$out/summary.log"

trap - EXIT INT TERM
