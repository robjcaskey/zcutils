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
remote_bin=${REMOTE_BIN:-$remote_root/target/release/zcutils}
lanes=${LANES_PER_CARD:-40}
qd=${QD_PER_WORKER:-256}
payload=${PAYLOAD_PER_LANE:-4G}
repeats=${REPEATS:-3}
service_base=${SERVICE_BASE:-49000}
virtual_volumes=${VIRTUAL_VOLUMES:-}
domain0=${EFA0_DOMAIN:-rdmap83s0-rdm}
domain1=${EFA1_DOMAIN:-rdmap166s0-rdm}
iface0=${EFA0_DEVICE:-rdmap83s0}
iface1=${EFA1_DEVICE:-rdmap166s0}
client_cpus0=${EFA0_CLIENT_CPU_LIST:-0-39}
target_cpus0=${EFA0_TARGET_CPU_LIST:-40-79}
client_cpus1=${EFA1_CLIENT_CPU_LIST:-96-135}
target_cpus1=${EFA1_TARGET_CPU_LIST:-136-175}
cloud_region=${CLOUD_REGION:-unreported}
availability_zone=${AVAILABILITY_ZONE:-unreported}
instance_type=${INSTANCE_TYPE:-unreported}
placement_group=${PLACEMENT_GROUP:-unconfirmed}
ssh=(ssh -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no -o ServerAliveInterval=30 -i "$key")

for numeric in "$lanes" "$qd" "$repeats" "$service_base"; do
	[[ "$numeric" =~ ^[1-9][0-9]*$ ]] || {
		printf 'lanes, QD, repeats, and service base must be positive integers\n' >&2
		exit 2
	}
done
last_data_port=$((service_base + 10000 + repeats * 50))
last_control_port=$((last_data_port + 1000))
if (( last_control_port > 65535 )); then
	printf 'service range overflows TCP ports: last_data_port=%s control_offset=1000 last_control_port=%s\n' \
		"$last_data_port" "$last_control_port" >&2
	exit 2
fi

cpu_list_count() {
	awk -v list="$1" 'BEGIN {
		n = split(list, parts, ","); total = 0
		for (i = 1; i <= n; i++) {
			if (parts[i] ~ /^[0-9]+$/) total++
			else if (parts[i] ~ /^[0-9]+-[0-9]+$/) {
				split(parts[i], range, "-")
				if (range[2] < range[1]) exit 2
				total += range[2] - range[1] + 1
			} else exit 2
		}
		print total
	}'
}

cpu_list_at() {
	awk -v list="$1" -v wanted="$2" 'BEGIN {
		n = split(list, parts, ","); position = 0
		for (i = 1; i <= n; i++) {
			if (parts[i] ~ /^[0-9]+$/) { first = parts[i]; last = parts[i] }
			else { split(parts[i], range, "-"); first = range[1]; last = range[2] }
			for (cpu = first; cpu <= last; cpu++) {
				if (position == wanted) { print cpu; exit }
				position++
			}
		}
		exit 2
	}'
}

virtual_volume_env=
if [ -n "$virtual_volumes" ]; then
	[[ "$virtual_volumes" =~ ^[1-9][0-9]*$ ]] || {
		printf 'VIRTUAL_VOLUMES must be a positive integer\n' >&2
		exit 2
	}
	virtual_volume_env="URING_PLAY_ZCOFI_VIRTUAL_VOLUMES=$virtual_volumes"
fi

for cpus in "$client_cpus0" "$target_cpus0" "$client_cpus1" "$target_cpus1"; do
	count=$(cpu_list_count "$cpus") || {
		printf 'invalid CPU list: %s\n' "$cpus" >&2
		exit 2
	}
	[ "$count" -ge "$lanes" ] || {
		printf 'CPU list %s has %s CPUs but %s lanes were requested\n' "$cpus" "$count" "$lanes" >&2
		exit 2
	}
done
mkdir -p "$out"

target_pids=()
client_pids=()
cleanup_targets() {
	local pid
	for pid in "${client_pids[@]:-}"; do
		[[ "$pid" =~ ^[0-9]+$ ]] || continue
		if kill -0 "$pid" 2>/dev/null; then
			kill -TERM "$pid" 2>/dev/null || true
		fi
		wait "$pid" 2>/dev/null || true
	done
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
	benchmark_env="ZCCUSAN_BENCHMARK_RUN_ID=$tag ZCCUSAN_BENCHMARK_RAIL_COUNT=2 ZCCUSAN_TOPOLOGY_PATH_COUNT=2 ZCCUSAN_TOPOLOGY_CLASS=direct ZCCUSAN_PLACEMENT_SCOPE=same-placement-group ZCCUSAN_TOPOLOGY_NUMA_NODE_COUNT=2 ZCCUSAN_TOPOLOGY_NUMA_LOCAL=1"
	remote_dir="$remote_root/bench-results/$(basename "$out")/$tag"
	env0="$virtual_volume_env URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 URING_PLAY_OFI_DOMAIN=$domain0 FI_EFA_IFACE=$iface0 FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_OFI_TIMEOUT_MS=60000 URING_PLAY_OFI_BUSY_POLL_ITERS=100000 URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_ACK_WINDOW=$qd URING_PLAY_OFI_TX_QUEUE_DEPTH=$qd URING_PLAY_OFI_RX_QUEUE_DEPTH=$qd"
	env1="$virtual_volume_env URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 URING_PLAY_OFI_DOMAIN=$domain1 FI_EFA_IFACE=$iface1 FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_OFI_TIMEOUT_MS=60000 URING_PLAY_OFI_BUSY_POLL_ITERS=100000 URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_ACK_WINDOW=$qd URING_PLAY_OFI_TX_QUEUE_DEPTH=$qd URING_PLAY_OFI_RX_QUEUE_DEPTH=$qd"

	pids=$("${ssh[@]}" "$target" "mkdir -p '$remote_dir'; nohup env $env0 URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST='$target_cpus0' '$remote_bin' zcwal-ofi-recv efa-direct rdm '$target_ip0' '$service0' '$lanes' '$payload' 4K '$lanes' true >'$remote_dir/target0.log' 2>&1 </dev/null & p0=\$!; nohup env $env1 URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST='$target_cpus1' '$remote_bin' zcwal-ofi-recv efa-direct rdm '$target_ip1' '$service1' '$lanes' '$payload' 4K '$lanes' true >'$remote_dir/target1.log' 2>&1 </dev/null & p1=\$!; printf '%s %s\\n' \$p0 \$p1")
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
		printf 'cloud_region=%s availability_zone=%s instance_type=%s placement_group=%s\n' "$cloud_region" "$availability_zone" "$instance_type" "$placement_group"
		printf 'virtual_volumes=%s volume_state=lane-local-hwm volume_state_merge=post-interval hotpath_shared_locks=0 hotpath_atomics=0\n' "${virtual_volumes:-disabled}"
		printf 'cards=2 lanes_per_card=%s workers_per_card=%s per_worker_qd=%s aggregate_outstanding_depth=%s\n' "$lanes" "$lanes" "$qd" "$((lanes * qd * 2))"
		printf 'card0_domain=%s card0_device=%s card0_client_cpus=%s card0_target_cpus=%s target_ip=%s\n' "$domain0" "$iface0" "$client_cpus0" "$target_cpus0" "$target_ip0"
		printf 'card1_domain=%s card1_device=%s card1_client_cpus=%s card1_target_cpus=%s target_ip=%s\n' "$domain1" "$iface1" "$client_cpus1" "$target_cpus1" "$target_ip1"
		for ((lane = 0; lane < lanes; lane++)); do
			printf 'card=0 lane=%s worker=%s client_cpu=%s target_cpu=%s\n' "$lane" "$lane" "$(cpu_list_at "$client_cpus0" "$lane")" "$(cpu_list_at "$target_cpus0" "$lane")"
			printf 'card=1 lane=%s worker=%s client_cpu=%s target_cpu=%s\n' "$lane" "$lane" "$(cpu_list_at "$client_cpus1" "$lane")" "$(cpu_list_at "$target_cpus1" "$lane")"
		done
	} >"$out/$tag-topology.log"

	"${ssh[@]}" "$client" "env $env0 $benchmark_env ZCCUSAN_BENCHMARK_RAIL_INDEX=0 URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST='$client_cpus0' '$remote_bin' zcwal-ofi-send efa-direct rdm '$target_ip0' '$service0' '$lanes' '$payload' 4K '$lanes' true" >"$out/$tag-client0.log" 2>&1 &
	client_pid0=$!
	"${ssh[@]}" "$client" "env $env1 $benchmark_env ZCCUSAN_BENCHMARK_RAIL_INDEX=1 URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST='$client_cpus1' '$remote_bin' zcwal-ofi-send efa-direct rdm '$target_ip1' '$service1' '$lanes' '$payload' 4K '$lanes' true" >"$out/$tag-client1.log" 2>&1 &
	client_pid1=$!
	client_pids=("$client_pid0" "$client_pid1")
	client_status0=0
	client_status1=0
	wait "$client_pid0" || client_status0=$?
	wait "$client_pid1" || client_status1=$?
	client_pids=()

	for _ in $(seq 1 1200); do
		live=$("${ssh[@]}" "$target" "for pid in '$target_pid0' '$target_pid1'; do [ -r /proc/\$pid/comm ] && [ \"\$(cat /proc/\$pid/comm)\" = zcutils ] && printf x; done" 2>/dev/null || true)
		[ -z "$live" ] && break
		sleep 0.025
	done
	if [[ -n "$live" ]]; then
		"${ssh[@]}" "$target" "for pid in '$target_pid0' '$target_pid1'; do if [ -r /proc/\$pid/comm ] && [ \"\$(cat /proc/\$pid/comm)\" = zcutils ]; then kill -TERM \$pid; fi; done" >/dev/null 2>&1 || true
		for _ in $(seq 1 200); do
			live=$("${ssh[@]}" "$target" "for pid in '$target_pid0' '$target_pid1'; do [ -r /proc/\$pid/comm ] && [ \"\$(cat /proc/\$pid/comm)\" = zcutils ] && printf x; done" 2>/dev/null || true)
			[ -z "$live" ] && break
			sleep 0.025
		done
	fi
	"${ssh[@]}" "$target" "cat '$remote_dir/target0.log'" >"$out/$tag-target0.log"
	"${ssh[@]}" "$target" "cat '$remote_dir/target1.log'" >"$out/$tag-target1.log"
	target_pids=()
	if (( client_status0 != 0 || client_status1 != 0 )); then
		printf 'dual-EFA client failure: repeat=%s card0_status=%s card1_status=%s; target logs retained in %s\n' \
			"$rep" "$client_status0" "$client_status1" "$out" >&2
		exit 1
	fi

	line0=$(rg 'zcofi-wal-send-summary:' "$out/$tag-client0.log" | tail -1)
	line1=$(rg 'zcofi-wal-send-summary:' "$out/$tag-client1.log" | tail -1)
	[[ -n "$line0" && -n "$line1" ]] || {
		printf 'dual-EFA run completed without both summary records: repeat=%s\n' "$rep" >&2
		exit 1
	}
	ops0=$(sed -n 's/.*logical_records=\([0-9][0-9]*\).*/\1/p' <<<"$line0")
	ops1=$(sed -n 's/.*logical_records=\([0-9][0-9]*\).*/\1/p' <<<"$line1")
	sec0=$(sed -n 's/.*seconds=\([0-9.][0-9.]*\).*/\1/p' <<<"$line0")
	sec1=$(sed -n 's/.*seconds=\([0-9.][0-9.]*\).*/\1/p' <<<"$line1")
	awk -v rep="$rep" -v a="$ops0" -v b="$ops1" -v sa="$sec0" -v sb="$sec1" 'BEGIN { slow=(sa>sb?sa:sb); printf "repeat=%d completed_ops=%d synchronized_seconds=%.6f conservative_iops=%.0f payload_Gbitps=%.3f\n", rep, a+b, slow, (a+b)/slow, ((a+b)*4096*8)/slow/1e9 }' | tee -a "$out/summary.log"
done

trap - EXIT INT TERM
