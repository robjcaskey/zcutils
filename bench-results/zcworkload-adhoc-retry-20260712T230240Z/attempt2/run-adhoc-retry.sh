#!/usr/bin/env bash
set -euo pipefail

ROOT=/home/rob/zcutils
OUTDIR="$ROOT/bench-results/zcworkload-adhoc-retry-20260712T230240Z"
HELPER=/home/rob/spot-helper/ec2_perf_spot.py
PROFILE=tf
REGION=us-east-2
AZ=us-east-2c
SUBNET=subnet-c66ddd8b
SG=sg-025a50a35d3073a8a
RUN_ID=zcworkload-adhoc-retry-20260712T230240Z-a2
INVENTORY="$OUTDIR/inventory.json"
KEY=/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519
SSH_BASE=(ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 -o ServerAliveInterval=15 -i "$KEY")

launched=0
teardown_started=0
ready_epoch=0
client_public=
target_public=

log() {
	printf '[%s] %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$OUTDIR/driver.log"
}

remote_stop_pidfile() {
	local host=$1 pidfile=$2 expected=$3 label=$4
	"${SSH_BASE[@]}" "ubuntu@$host" bash -s -- "$pidfile" "$expected" "$label" <<'REMOTE'
set -euo pipefail
pidfile=$1
expected=$2
label=$3
if [ ! -s "$pidfile" ]; then
	printf '%s pid_file=%s state=absent\n' "$label" "$pidfile"
	exit 0
fi
pid=$(cat "$pidfile")
case "$pid" in
	''|*[!0-9]*) printf '%s pid_file=%s invalid_pid=%s\n' "$label" "$pidfile" "$pid"; exit 1 ;;
esac
if [ ! -r "/proc/$pid/comm" ]; then
	printf '%s pid=%s state=exited\n' "$label" "$pid"
	exit 0
fi
comm=$(cat "/proc/$pid/comm")
printf '%s pid=%s comm=%s state=live\n' "$label" "$pid" "$comm"
if [ "$comm" != "$expected" ]; then
	printf 'refusing signal: expected_comm=%s actual_comm=%s\n' "$expected" "$comm" >&2
	exit 1
fi
sudo -n kill -INT "$pid" 2>/dev/null || kill -INT "$pid"
for _ in $(seq 1 100); do
	[ ! -e "/proc/$pid" ] && exit 0
	sleep 0.05
done
sudo -n kill -TERM "$pid" 2>/dev/null || kill -TERM "$pid"
REMOTE
}

cleanup_remote() {
	[ -n "$client_public" ] || return 0
	[ -n "$target_public" ] || return 0
	{
		remote_stop_pidfile "$client_public" /tmp/zcworkload-target.pid zcnblk-shm-targ target || true
		remote_stop_pidfile "$target_public" /tmp/zcworkload-leaf.pid zcnblk-wal-lea leaf || true
		"${SSH_BASE[@]}" "ubuntu@$client_public" 'if grep -q "^zcnblk_client_mod " /proc/modules; then sudo -n rmmod zcnblk_client_mod; fi' || true
	} >>"$OUTDIR/process-cleanup.log" 2>&1
	"${SSH_BASE[@]}" "ubuntu@$client_public" 'cat /tmp/zcworkload-target.log' >"$OUTDIR/target-final.log" 2>&1 || true
	"${SSH_BASE[@]}" "ubuntu@$target_public" 'cat /tmp/zcworkload-leaf.log' >"$OUTDIR/leaf-final.log" 2>&1 || true
}

terminate_cloud() {
	[ "$launched" -eq 1 ] || return 0
	if [ "$teardown_started" -eq 0 ]; then
		teardown_started=1
		printf 'teardown_started_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" | tee "$OUTDIR/teardown.env"
		if [ "$ready_epoch" -gt 0 ]; then
			printf 'seconds_since_ready=%s\n' "$(( $(date +%s) - ready_epoch ))" | tee -a "$OUTDIR/teardown.env"
		fi
	fi
	cleanup_remote
	"$HELPER" terminate --profile "$PROFILE" --region "$REGION" --run-id "$RUN_ID" --yes >>"$OUTDIR/terminate.log" 2>&1 || true
}

cleanup() {
	local status=$?
	set +e
	terminate_cloud
	printf 'driver_exit_status=%s\n' "$status" >>"$OUTDIR/teardown.env"
	exit "$status"
}

# The trap is armed before launch so every post-launch exit path terminates the pair.
trap cleanup EXIT INT TERM
printf 'trap_installed_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$OUTDIR/trap.log"

drop_dead=$(date -u -d '+85 minutes' +%Y-%m-%dT%H:%M:%SZ)
cat >"$OUTDIR/run.env" <<EOF
run_id=$RUN_ID
region=$REGION
availability_zone=$AZ
subnet_id=$SUBNET
security_group_id=$SG
instance_type=c8gn.2xlarge
nodes=2
max_spot_price=0.15
max_total_cost=1.00
root_gb=32
drop_dead_utc=$drop_dead
frame_bytes=4096
logical_request_sizes=4096,8192,16384,32768,65536
EOF

log "launching two c8gn.2xlarge Spot nodes"
launched=1
"$HELPER" launch --profile "$PROFILE" --region "$REGION" --availability-zone "$AZ" \
	--subnet-id "$SUBNET" --security-group-id "$SG" --key-name adhocMasterKeypair \
	--instance-type c8gn.2xlarge --nodes 2 --drop-dead-utc "$drop_dead" \
	--max-spot-price 0.15 --max-total-cost 1.00 --root-gb 32 --no-enable-efa \
	--network-card-count 1 --associate-public-ip --no-ena-express \
	--run-id "$RUN_ID" --inventory "$INVENTORY" --yes >"$OUTDIR/launch.log" 2>&1
printf 'launch_completed_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >>"$OUTDIR/trap.log"

jq -e --arg az "$AZ" --arg subnet "$SUBNET" '
  (.instances | length) == 2 and
  all(.instances[];
	.az == $az and
    .instance_type == "c8gn.2xlarge" and
    (.public_ip | type == "string") and (.private_ip | type == "string") and
    (.network_interfaces | length) == 1)
' "$INVENTORY" >"$OUTDIR/inventory-validation.log"

mapfile -t public_ips < <(jq -r '.instances[].public_ip' "$INVENTORY")
mapfile -t private_ips < <(jq -r '.instances[].private_ip' "$INVENTORY")
mapfile -t instance_ids < <(jq -r '.instances[].instance_id' "$INVENTORY")
client_public=${public_ips[0]}
target_public=${public_ips[1]}
client_private=${private_ips[0]}
target_private=${private_ips[1]}
printf 'role\tpublic_management\tprivate_data\tinstance_id\nclient\t%s\t%s\t%s\ntarget\t%s\t%s\t%s\n' \
	"$client_public" "$client_private" "${instance_ids[0]}" \
	"$target_public" "$target_private" "${instance_ids[1]}" >"$OUTDIR/role-map.tsv"

log "waiting for public SSH management paths"
for index in 0 1; do
	host=${public_ips[$index]}
	for _ in $(seq 1 60); do
		if "${SSH_BASE[@]}" "ubuntu@$host" true >/dev/null 2>&1; then
			printf 'node=%s public_management=%s ready_utc=%s\n' "$index" "$host" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >>"$OUTDIR/ssh-ready.log"
			break
		fi
		sleep 2
	done
	"${SSH_BASE[@]}" "ubuntu@$host" true
done
ready_epoch=$(date +%s)
printf 'instances_ready_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >>"$OUTDIR/trap.log"
deadline_epoch=$((ready_epoch + 780))

check_deadline() {
	if [ "$(date +%s)" -ge "$deadline_epoch" ]; then
		log "13-minute ready-to-teardown deadline reached"
		exit 124
	fi
}

aws ec2 describe-instances --profile "$PROFILE" --region "$REGION" \
	--filters Name=tag:uringPlayRunId,Values="$RUN_ID" >"$OUTDIR/describe-instances.json"
jq -e --arg az "$AZ" --arg subnet "$SUBNET" '
	([.Reservations[].Instances[]] | length) == 2 and
	all(.Reservations[].Instances[]; .Placement.AvailabilityZone == $az and .SubnetId == $subnet)
' "$OUTDIR/describe-instances.json" >"$OUTDIR/aws-topology-validation.log"

log "syncing dirty source state over public management addresses"
"$HELPER" sync --inventory "$INVENTORY" --repo "$ROOT" --remote-dir /home/ubuntu/zcutils \
	--public-ip >"$OUTDIR/sync.log" 2>&1
printf 'source_head=%s\n' "$(git -C "$ROOT" rev-parse HEAD)" >"$OUTDIR/source-state.log"
git -C "$ROOT" status --short >>"$OUTDIR/source-state.log"
check_deadline

log "bootstrapping and building both dedicated nodes in parallel"
"${SSH_BASE[@]}" "ubuntu@$client_public" \
	'env ZCUTILS_CLOUD_DAILY_BUDGET_USD=1 ZCUTILS_HUGEPAGES=64 ZCUTILS_BOOTSTRAP_BINS="zcworkload zcnblk-shm-target" bash /home/ubuntu/zcutils/scripts/welcome-to-the-team.sh --hugepages 64' \
	>"$OUTDIR/client-bootstrap.log" 2>&1 &
client_build_pid=$!
"${SSH_BASE[@]}" "ubuntu@$target_public" \
	'env ZCUTILS_CLOUD_DAILY_BUDGET_USD=1 ZCUTILS_HUGEPAGES=64 ZCUTILS_BOOTSTRAP_BINS="zcnblk-wal-leaf" bash /home/ubuntu/zcutils/scripts/welcome-to-the-team.sh --hugepages 64' \
	>"$OUTDIR/target-bootstrap.log" 2>&1 &
target_build_pid=$!
printf 'client_build_pid=%s\ntarget_build_pid=%s\n' "$client_build_pid" "$target_build_pid" >"$OUTDIR/local-build-pids.log"
wait "$client_build_pid"
wait "$target_build_pid"
check_deadline

log "building client kernel edge module"
"${SSH_BASE[@]}" "ubuntu@$client_public" 'make -C /home/ubuntu/zcutils/kmods all' \
	>"$OUTDIR/client-module-build.log" 2>&1

for index in 0 1; do
	"${SSH_BASE[@]}" "ubuntu@${public_ips[$index]}" \
		'printf "host=%s\n" "$(hostname)"; lscpu -e=CPU,NODE,SOCKET,CORE,ONLINE; ip -br addr; ip route; ulimit -l; cat /proc/sys/vm/nr_hugepages; cat ~/.local/state/zcutils/adhoc-bootstrap.env' \
		>"$OUTDIR/node${index}-topology.log" 2>&1
done
check_deadline

log "validating workload sample includes every 4-64 KiB logical size"
"${SSH_BASE[@]}" "ubuntu@$client_public" \
	'/home/ubuntu/zcutils/target/release/zcworkload sample --capacity 128M --requests 200000 --seed 7640891576956012809 --json' \
	>"$OUTDIR/sample.json" 2>"$OUTDIR/sample.stderr.log"
jq -e '
	.requests == 200000 and .counters.reads > 0 and .counters.writes > 0 and
	([.sizes[] | select(.operations > 0) | .bytes] | sort) == [4096,8192,16384,32768,65536]
' "$OUTDIR/sample.json" >"$OUTDIR/sample-validation.log"

log "starting one private-address userspace WAL/memory terminal leaf"
"${SSH_BASE[@]}" "ubuntu@$target_public" bash -s -- "$target_private" <<'REMOTE'
set -euo pipefail
target_private=$1
rm -f /tmp/zcworkload-leaf.pid /tmp/zcworkload-leaf.log
nohup env URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=1 URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
  /home/ubuntu/zcutils/target/release/zcnblk-wal-leaf \
  zcmem:256M "$target_private" 29000 1 1 4096 1 true blocking \
  >/tmp/zcworkload-leaf.log 2>&1 &
echo $! >/tmp/zcworkload-leaf.pid
REMOTE
for _ in $(seq 1 100); do
	if "${SSH_BASE[@]}" "ubuntu@$target_public" "ss -H -ltn | awk -v p=':$((29000))' '\$4 ~ p \"\$\" {found=1} END {exit !found}'"; then
		break
	fi
	sleep 0.1
done
"${SSH_BASE[@]}" "ubuntu@$target_public" 'pid=$(cat /tmp/zcworkload-leaf.pid); printf "pid=%s comm=%s affinity=" "$pid" "$(cat /proc/$pid/comm)"; taskset -pc "$pid"; ss -ltnp | grep -E ":29000|:29001"' \
	>"$OUTDIR/leaf-topology.log" 2>&1

log "measuring private data route and RTT"
"${SSH_BASE[@]}" "ubuntu@$client_public" bash -s -- "$target_private" <<'REMOTE' >"$OUTDIR/private-rtt.log" 2>&1
set -euo pipefail
target_private=$1
ip route get "$target_private"
ping -n -c 40 -i 0.05 "$target_private"
REMOTE
rtt_ns=$(awk -F'[ =/]' '/^rtt min\/avg\/max/ {printf "%.0f\n", $9 * 1000000}' "$OUTDIR/private-rtt.log")
case "$rtt_ns" in ''|*[!0-9]*) log "could not parse RTT"; exit 1 ;; esac
printf 'transport_rtt_ns=%s\n' "$rtt_ns" >"$OUTDIR/rtt.env"

log "loading one placement-free /dev/zcnblk0 client edge with 4096-byte frames"
"${SSH_BASE[@]}" "ubuntu@$client_public" bash -s <<'REMOTE'
set -euo pipefail
cd /home/ubuntu/zcutils
if grep -q '^zcnblk_client_mod ' /proc/modules; then
	echo 'refusing to replace an existing zcnblk_client_mod owner' >&2
	exit 1
fi
sudo -n insmod kmods/zcnblk_client_mod.ko transport=shm lanes=1 connections_per_lane=1 \
  size_mib=256 queues=1 queue_depth=64 logical_block_size=4096 max_frame_bytes=4096 \
  pipeline_depth=128 shm_ring_entries=128 shm_payload_entries=4096 shm_poll_us=50 \
  write_acks=1 hctx_affinity=1 pin_threads=1 pin_base_cpu=3 pin_cpu_count=1 pin_stride=1
for _ in $(seq 1 100); do
	[ -b /dev/zcnblk0 ] && [ -c /dev/zcnblk-shmctl ] && break
	sleep 0.05
done
[ -b /dev/zcnblk0 ] && [ -c /dev/zcnblk-shmctl ]
sudo -n chgrp ubuntu /dev/zcnblk0
sudo -n chmod 0660 /dev/zcnblk0
REMOTE

log "starting one userspace WAL onramp to the target private address"
"${SSH_BASE[@]}" "ubuntu@$client_public" bash -s -- "$client_private" "$target_private" <<'REMOTE'
set -euo pipefail
client_private=$1
target_private=$2
rm -f /tmp/zcworkload-target.pid /tmp/zcworkload-target.log
nohup sudo -n env \
  URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE=/tmp/zcworkload-target.pid \
  URING_PLAY_ZCNBLK_SHM_LEAF_ADDR="$target_private:29000" \
  URING_PLAY_ZCNBLK_SHM_LEAF_SOURCE_ADDR="$client_private" \
  URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=2 \
  URING_PLAY_ROUTE_PROBE=1 URING_PLAY_EXPECT_ROUTE_DEV=ens34 URING_PLAY_EXPECT_ROUTE_SRC="$client_private" \
  /home/ubuntu/zcutils/target/release/zcnblk-shm-target \
  /dev/zcnblk-shmctl wal-tcp 128 2 1000 1000 10000 \
  >/tmp/zcworkload-target.log 2>&1 &
echo $! >/tmp/zcworkload-target-job.pid
for _ in $(seq 1 100); do
	[ -s /tmp/zcworkload-target.pid ] && break
	job=$(cat /tmp/zcworkload-target-job.pid)
	[ -e "/proc/$job" ] || { cat /tmp/zcworkload-target.log; exit 1; }
	sleep 0.05
done
[ -s /tmp/zcworkload-target.pid ]
REMOTE

"${SSH_BASE[@]}" "ubuntu@$client_public" bash -s <<'REMOTE' >"$OUTDIR/client-edge-topology.log" 2>&1
set -euo pipefail
pid=$(cat /tmp/zcworkload-target.pid)
printf 'target_pid=%s comm=%s affinity=' "$pid" "$(cat /proc/$pid/comm)"
taskset -pc "$pid"
printf 'module_parameters:\n'
for name in transport lanes queues queue_depth logical_block_size max_frame_bytes write_acks hctx_affinity pin_threads pin_base_cpu pin_cpu_count; do
  printf '%s=%s\n' "$name" "$(cat /sys/module/zcnblk_client_mod/parameters/$name)"
done
printf 'devices:\n'
ls -l /dev/zcnblk0 /dev/zcnblk-shmctl
printf 'hctx_maps:\n'
find /sys/block/zcnblk0/mq -maxdepth 2 -type f \( -name cpu_list -o -name cpu_list \) -print -exec cat {} \;
printf 'kthreads:\n'
ps -e -o pid=,comm=,psr= | awk '$2 ~ /^zcnblk-shm-/ {print}'
for pid in $(ps -e -o pid=,comm= | awk '$2 ~ /^zcnblk-shm-/ {print $1}'); do taskset -pc "$pid"; done
printf 'target_log:\n'
cat /tmp/zcworkload-target.log
printf 'dmesg_tail:\n'
sudo -n dmesg | tail -n 30
REMOTE
grep -q '^max_frame_bytes=4096$' "$OUTDIR/client-edge-topology.log"
grep -q '^logical_block_size=4096$' "$OUTDIR/client-edge-topology.log"
grep -q 'backend=WalTcp.*slot_bytes=4096' <("${SSH_BASE[@]}" "ubuntu@$client_public" 'cat /tmp/zcworkload-target.log')
check_deadline

run_workload() {
	local engine=$1 depth=$2 ops=$3 log_path=$4
	"${SSH_BASE[@]}" "ubuntu@$client_public" bash -s -- "$engine" "$depth" "$ops" "$rtt_ns" <<'REMOTE' >"$log_path" 2>&1
set -euo pipefail
engine=$1
depth=$2
ops=$3
rtt_ns=$4
cd /home/ubuntu/zcutils
env URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=1 URING_PLAY_ENTER_NO_IOWAIT=1 \
  target/release/zcworkload run --target /dev/zcnblk0 --capacity 128M \
  --engine "$engine" --workers 1 --depth "$depth" --ring-entries 64 \
  --ops-per-worker "$ops" --buffers small-pages --pin true --latency-sample-rate 1 \
  --completion-batch 4 --lane-map lane0:worker0:cpu1 --kthread-map kthread0:cpu3 \
  --completion remote-ack --transport-rtt-ns "$rtt_ns"
REMOTE
	grep -q '^zcworkload-result:' "$log_path"
	grep -q 'topology_preflight_passed=false' "$log_path"
	grep -q '^zcworkload-latency:' "$log_path"
	grep -q '^zcworkload-context:' "$log_path"
}

log "running short sync mixed workload"
run_workload sync 1 512 "$OUTDIR/sync-mixed.log"
check_deadline
log "running short io_uring mixed workload"
run_workload uring 4 2048 "$OUTDIR/uring-mixed.log"
check_deadline

"${SSH_BASE[@]}" "ubuntu@$client_public" 'cat /tmp/zcworkload-target.log' >"$OUTDIR/target.log" 2>&1
"${SSH_BASE[@]}" "ubuntu@$target_public" 'cat /tmp/zcworkload-leaf.log' >"$OUTDIR/leaf.log" 2>&1

log "workloads complete; beginning teardown"
terminate_cloud
trap - EXIT INT TERM
printf 'driver_exit_status=0\n' >>"$OUTDIR/teardown.env"
log "driver complete"
