#!/usr/bin/env bash
# Serial transport ceiling, NOT a persistent-mirror benchmark.
set -euo pipefail
[[ $# = 2 ]] || { echo "usage: $0 INVENTORY.json OUTPUT_DIR" >&2; exit 2; }
inventory=$1 out=$2
mkdir -p "$out"
ssh=(ssh -i "${ADHOC_SSH_KEY:-/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519}" -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8 -o ServerAliveInterval=10)
hosts=() ip0=() ip1=()
for n in 0 1 2; do
    hosts+=("$(jq -r ".instances[$n].public_ip" "$inventory")")
    ip0+=("$(jq -r ".instances[$n].network_interfaces[0].private_ip" "$inventory")")
    ip1+=("$(jq -r ".instances[$n].network_interfaces[1].private_ip" "$inventory")")
done
bin=${REMOTE_BIN:-/home/ubuntu/zcutils/zcutils}
lanes=${LANES_PER_CARD:-40} qd=${QD_PER_WORKER:-256} payload=${PAYLOAD_PER_LANE:-4G} reps=${REPEATS:-3}
for n in "$lanes" "$qd" "$reps"; do [[ $n =~ ^[1-9][0-9]*$ ]] || exit 2; done
[[ $lanes -le 80 ]] || exit 2
children=() owners=() pidfiles=()
cleanup() {
    local i
    for i in "${!owners[@]}"; do
        "${ssh[@]}" "ubuntu@${owners[$i]}" "if [ -f '${pidfiles[$i]}' ]; then p=\$(cat '${pidfiles[$i]}'); case \$p in ''|*[!0-9]*) exit 1;; esac; if [ -r /proc/\$p/cmdline ] && tr '\\0' ' ' </proc/\$p/cmdline | grep -F '$bin' >/dev/null; then kill -TERM \$p; fi; fi" >/dev/null 2>&1 || true
    done
    for p in "${children[@]}"; do wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
wait_listener() {
    local host=$1 port=$2 n
    for ((n=0; n<90; n++)); do
        if "${ssh[@]}" "ubuntu@$host" "ss -H -ltn 'sport = :$port'" | grep -q LISTEN; then return; fi
        sleep 0.1
    done
    return 1
}
run_id=$(date -u +%Y%m%dT%H%M%SZ)
for ((rep=1;rep<=reps;rep++)); do
    dir="/tmp/zc-serial-efa-$run_id-r$rep"
    for host in "${hosts[@]}"; do "${ssh[@]}" "ubuntu@$host" "mkdir '$dir'"; done
    owners=() pidfiles=() children=()
    for rail in 0 1; do
        m=${ip0[1]} s=${ip0[2]} start=0
        if ((rail)); then m=${ip1[1]} s=${ip1[2]} start=96; fi
        cpus="$start-$((start+lanes-1))"
        inport=$((46000 + rail * 4000)) outport=$((48000 + rail * 4000))
        envs="URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 URING_PLAY_OFI_DOMAIN=efa_$rail-rdm FI_EFA_IFACE=efa_$rail FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_OFI_TIMEOUT_MS=60000 URING_PLAY_OFI_BUSY_POLL_ITERS=100000 URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_ACK_WINDOW=$qd URING_PLAY_OFI_RELAY_WINDOW=$qd URING_PLAY_OFI_TX_QUEUE_DEPTH=$qd URING_PLAY_OFI_RX_QUEUE_DEPTH=$qd URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=$cpus"
        "${ssh[@]}" "ubuntu@${hosts[2]}" "echo \$\$ >'$dir/tail$rail.pid'; exec timeout 180 env $envs '$bin' zcwal-ofi-recv efa-direct rdm '$s' '$outport' '$lanes' '$payload' 4K '$lanes' true" >"$out/r$rep-tail$rail.log" 2>&1 &
        children+=("$!") owners+=("${hosts[2]}") pidfiles+=("$dir/tail$rail.pid")
        wait_listener "${hosts[2]}" "$((outport+1000))"
        "${ssh[@]}" "ubuntu@${hosts[1]}" "echo \$\$ >'$dir/middle$rail.pid'; exec timeout 180 env $envs '$bin' zcwal-ofi-relay efa-direct rdm '$m' '$s' '$inport' '$outport' '$lanes' '$payload' 4K '$lanes' true" >"$out/r$rep-middle$rail.log" 2>&1 &
        children+=("$!") owners+=("${hosts[1]}") pidfiles+=("$dir/middle$rail.pid")
        wait_listener "${hosts[1]}" "$((inport+1000))"
    done
    for rail in 0 1; do
        m=${ip0[1]} start=0
        if ((rail)); then m=${ip1[1]} start=96; fi
        cpus="$start-$((start+lanes-1))" inport=$((46000 + rail * 4000))
        envs="URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 URING_PLAY_OFI_DOMAIN=efa_$rail-rdm FI_EFA_IFACE=efa_$rail FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_OFI_TIMEOUT_MS=60000 URING_PLAY_OFI_BUSY_POLL_ITERS=100000 URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_ACK_WINDOW=$qd URING_PLAY_OFI_TX_QUEUE_DEPTH=$qd URING_PLAY_OFI_RX_QUEUE_DEPTH=$qd URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=$cpus"
        "${ssh[@]}" "ubuntu@${hosts[0]}" "echo \$\$ >'$dir/client$rail.pid'; exec timeout 180 env $envs '$bin' zcwal-ofi-send efa-direct rdm '$m' '$inport' '$lanes' '$payload' 4K '$lanes' true" >"$out/r$rep-client$rail.log" 2>&1 &
        children+=("$!") owners+=("${hosts[0]}") pidfiles+=("$dir/client$rail.pid")
    done
    for p in "${children[@]}"; do wait "$p"; done
    children=() owners=() pidfiles=()
    printf 'rep=%s topology=C->M->S rails=2 lanes_per_rail=%s workers_per_rail=%s per_worker_qd=%s aggregate_logical_depth=%s completion=tail-volatile-memory-ack block_frontend=false persistent_mirror=false copy=registered-receive-slot-forwarded kernel=record-in-node-manifests\n' \
        "$rep" "$lanes" "$lanes" "$qd" "$((lanes*2*qd))" >"$out/r$rep-topology.log"
    rg '^zcofi-wal-send-summary:' "$out/r$rep-client0.log" "$out/r$rep-client1.log" |
        awk -v rep="$rep" '{for(i=1;i<=NF;i++){split($i,a,"="); if(a[1]=="logical_records") ops+=a[2]; if(a[1]=="seconds" && a[2]>sec)sec=a[2]}} END{if(sec<=0)exit 1; printf "rep=%d completed_ops=%.0f conservative_seconds=%.6f iops=%.0f payload_Gbitps=%.3f\n",rep,ops,sec,ops/sec,ops*4096*8/sec/1e9}' | tee -a "$out/summary.log"
done
