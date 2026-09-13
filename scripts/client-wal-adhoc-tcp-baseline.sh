#!/usr/bin/env bash
# Raw TCP framed/ACK baseline, no persistent media and no mirror claim.
set -euo pipefail
[[ $# = 2 ]] || exit 2
inventory=$1 out=$2
mkdir -p "$out"
ssh=(ssh -i "${ADHOC_SSH_KEY:-/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519}" -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8 -o ServerAliveInterval=5 -o ServerAliveCountMax=3)
bin=/home/ubuntu/zcutils/zcutils
for leg in ${TCP_LEGS:-client-middle middle-tail}; do
    [[ $leg = client-middle || $leg = middle-tail ]] || exit 2
    source_index=0 target_index=1
    [[ $leg = client-middle ]] || { source_index=1; target_index=2; }
    source_host=$(jq -r ".instances[$source_index].public_ip" "$inventory")
    target_host=$(jq -r ".instances[$target_index].public_ip" "$inventory")
    target_ip=$(jq -r ".instances[$target_index].private_ip" "$inventory")
    for shape in latency saturation linear; do
        lanes=1 extent=4K bytes=16M cpus=0
        [[ $shape != saturation ]] || { lanes=40; bytes=256M; cpus=0-39; }
        [[ $shape != linear ]] || { lanes=40; extent=1M; bytes=1G; cpus=0-39; }
        for rep in 1 2 3; do
            tag="$leg-$shape-r$rep"
            envs="URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=$cpus URING_PLAY_HUGETLB=1 URING_PLAY_TCP_NODELAY=1 URING_PLAY_ZCWAL_SYNC_ACKS=1"
            "${ssh[@]}" "ubuntu@$target_host" "exec timeout 90 env $envs '$bin' zcwal-extent-recv '$target_ip' 57000 '$lanes' 1 '$bytes' '$extent' '$lanes' true extent blocking" >"$out/$tag-target.log" 2>&1 &
            pid=$!
            ready=0
            for ((n=0;n<60;n++)); do
                if "${ssh[@]}" "ubuntu@$target_host" "ss -H -ltn 'sport = :57000'" | grep -q LISTEN; then ready=1; break; fi
                sleep 0.1
            done
            ((ready)) || exit 1
            printf 'leg=%s lanes=%s workers=%s lane_to_worker_cpu=identity-0..%s per_worker_outstanding_extents=1 aggregate_outstanding_extents=%s extent_bytes=%s completion=remote-volatile-frame-ack disk_durability=false block_frontend=false data_path=blocking-vectored kernel_tcp_copies=true\n' "$leg" "$lanes" "$lanes" "$((lanes-1))" "$lanes" "$extent" >"$out/$tag-topology.log"
            "${ssh[@]}" "ubuntu@$source_host" "exec timeout 90 env $envs '$bin' zcwal-extent-send '$target_ip' 57000 '$lanes' 1 '$bytes' '$extent' '$lanes' true extent blocking" >"$out/$tag-client.log" 2>&1
            wait "$pid"
            rg '^zcwal-extent-send-(summary|latency-summary):' "$out/$tag-client.log" | sed "s/^/$tag /" | tee -a "$out/summary.log"
        done
    done
done
