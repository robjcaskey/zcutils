#!/usr/bin/env bash
# Persisted serial custody curves. RAM transport ceilings are a SEPARATE run.
set -euo pipefail
[[ $# = 2 ]] || { echo "usage: $0 INVENTORY.json OUTPUT_DIR" >&2; exit 2; }
inventory=$1 out=$2
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mkdir -p "$out"
key=${ADHOC_SSH_KEY:-/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519}
ssh=(ssh -i "$key" -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8 -o ServerAliveInterval=10)
scp=(scp -q -i "$key" -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8 -o ServerAliveInterval=5 -o ServerAliveCountMax=3)
client=$(jq -r '.instances[0].public_ip' "$inventory")
middle=$(jq -r '.instances[1].public_ip' "$inventory")
tail_node=$(jq -r '.instances[2].public_ip' "$inventory")
middle_ip=$(jq -r '.instances[1].private_ip' "$inventory")
tail_ip=$(jq -r '.instances[2].private_ip' "$inventory")
bin=${REMOTE_BIN:-/home/ubuntu/zcutils/zcutils}
lanes=${LANES:-1}
repeats=${REPEATS:-3}
qds=${QDS:-1 2 4 8 16 128 512}
transports=${TRANSPORTS:-tcp rdma}
extents=${EXTENT_BYTES:-4096}
records=${EXTENTS_PER_LANE:-4096}
domain=${RDMA_DOMAIN:-efa_0-rdm}
device=${RDMA_DEVICE:-efa_0}
cpus=${SOURCE_CPUS:-0-39}
middle_cpus=${MIDDLE_CPUS:-0-79}
tail_cpus=${TAIL_CPUS:-0-39}
for n in "$lanes" "$repeats" "$extents" "$records" $qds; do
    [[ "$n" =~ ^[1-9][0-9]*$ ]] || exit 2
done
bytes=$((records * extents))
logical=$((bytes * lanes))
journal=$((logical * 2 + 16777216))
run=$(date -u +%Y%m%dT%H%M%SZ)
children=() cleanup_hosts=() cleanup_paths=()
cleanup() {
    local i pidfile
    for i in "${!cleanup_hosts[@]}"; do
        pidfile=${cleanup_paths[$i]}
        "${ssh[@]}" "ubuntu@${cleanup_hosts[$i]}" "if [ -r '$pidfile' ]; then p=\$(cat '$pidfile'); case \$p in ''|*[!0-9]*) exit 1;; esac; if [ -r /proc/\$p/cmdline ] && tr '\\0' ' ' </proc/\$p/cmdline | grep -F -- '$bin' >/dev/null; then kill -TERM \$p; fi; fi" >/dev/null 2>&1 || true
    done
    for pid in "${children[@]}"; do wait "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
wait_listener() {
    local host=$1 port=$2 attempt
    for ((attempt=0; attempt<90; attempt++)); do
        if "${ssh[@]}" "ubuntu@$host" "ss -H -ltn 'sport = :$port'" | grep -q LISTEN; then return; fi
        sleep 0.1
    done
    echo "listener not ready host=$host port=$port" >&2
    return 1
}
for transport in $transports; do
    [[ $transport = tcp || $transport = rdma ]] || exit 2
    for qd in $qds; do
        for ((rep=1; rep<=repeats; rep++)); do
            tag="custody-$run-$transport-l$lanes-e$extents-q$qd-r$rep"
            local_dir="$out/$tag"
            remote_dir="/mnt/zc-terminal/$tag"
            mkdir -p "$local_dir"
            jq --argjson lanes "$lanes" --arg domain "$domain" '
                .compiled.lane_count=$lanes |
                .compiled.parallel_raid.branch_topology |= map(
                    .lanes=[range($lanes)] | .workers=[range($lanes)] |
                    .leader_cpus=[range($lanes)] | .fabric_domain=$domain)' \
                "$root/tests/fixtures/client-wal/plan.json" >"$local_dir/plan.json"
            custody_capacity=$((qd * extents * 4 + 67108864))
            jq --arg dir "$remote_dir/client" --arg transport "$transport" --arg domain "$domain" \
                --argjson capacity "$custody_capacity" '
                .journal_directory=$dir | .capacity_bytes_per_lane=$capacity |
                if $transport == "rdma" then .rdma={provider:"efa-direct",domain:$domain} else . end' \
                "$root/tests/fixtures/client-wal/client.json" >"$local_dir/client.json"
            jq --arg remote "$tail_ip" --arg dir "$remote_dir" --arg transport "$transport" \
                --arg domain "$domain" --argjson lanes "$lanes" --argjson qd "$qd" \
                --argjson records "$records" --argjson extent "$extents" \
                --argjson logical "$logical" --argjson journal "$journal" '
                .remote=$remote | .lanes=$lanes | .window=$qd | .extents_per_lane=$records |
                .extent_bytes=$extent | .queue_windows=8 |
                .terminal_target=("zcpwal:"+$dir+"/middle.wal,"+$dir+"/middle.base,"+($logical|tostring)+","+($journal|tostring)) |
                if $transport == "rdma" then .rdma={provider:"efa-direct",domain:$domain} else . end' \
                "$root/tests/fixtures/client-wal/middle.json" >"$local_dir/middle.json"
            jq -n --arg domain "$domain" '{provider:"efa-direct",domain:$domain}' >"$local_dir/rdma.json"
            for host in "$client" "$middle" "$tail_node"; do
                "${ssh[@]}" "ubuntu@$host" "mkdir '$remote_dir'"
                "${scp[@]}" "$local_dir"/*.json "ubuntu@$host:$remote_dir/"
            done
            envs="URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 URING_PLAY_HUGETLB=1 URING_PLAY_PIN_CPUS=1 URING_PLAY_RAID_ZLANE_COORD=lane-owner URING_PLAY_RAID_MIRROR_ACK_WINDOW=$qd URING_PLAY_OFI_ACK_WINDOW=$qd URING_PLAY_OFI_TX_QUEUE_DEPTH=$qd URING_PLAY_OFI_RX_QUEUE_DEPTH=$qd URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_RMA_WRITE_MORE=1 FI_EFA_IFACE=$device FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_ZCNBLK_PWAL_INTEGRITY=frame URING_PLAY_ZCNBLK_WAL_LEAF_SUBMIT_MODE=blocking URING_PLAY_SEND_FILL_BYTE=90"
            rdma_env=
            envs="$envs URING_PLAY_OFI_RMA_WRITE_QD=$qd"
            envs="$envs URING_PLAY_RAID_MIRROR_TERMINAL_BATCH=${TERMINAL_BATCH:-0}"
            [[ $transport = tcp ]] || rdma_env="URING_PLAY_RAID_MIRROR_RDMA_CONFIG=$remote_dir/rdma.json"
            cleanup_hosts=("$tail_node" "$middle")
            cleanup_paths=("$remote_dir/tail.pid" "$remote_dir/middle.pid")
            "${ssh[@]}" "ubuntu@$tail_node" "echo \$\$ >'$remote_dir/tail.pid'; exec timeout 180 env $envs $rdma_env URING_PLAY_PIN_CPU_LIST=$tail_cpus '$bin' zcraid-mirror-recv tcp 0.0.0.0 44000 1 '$bytes' '$extents' '$lanes' '$remote_dir/plan.json' efa-direct rdm true 'zcpwal:$remote_dir/tail.wal,$remote_dir/tail.base,$logical,$journal'" >"$local_dir/tail.log" 2>&1 &
            children=("$!")
            wait_listener "$tail_node" 44000
            "${ssh[@]}" "ubuntu@$middle" "echo \$\$ >'$remote_dir/middle.pid'; exec timeout 180 env $envs URING_PLAY_PIN_CPU_LIST=$middle_cpus '$bin' zcraid-mirror-hop '$remote_dir/middle.json'" >"$local_dir/middle.log" 2>&1 &
            children+=("$!")
            wait_listener "$middle" 43000
            printf 'topology=C->M->S frontend=userspace-custody-benchmark transport=%s lanes=%s per_worker_qd=%s aggregate_logical_depth=%s extent_bytes=%s access_pattern=sequential-wal-appends durability=local-persistent-plus-one-remote-or-both-remotes source_cpus=%s middle_cpus=%s tail_cpus=%s backing=dedicated-gp3-16000iops-1000MiBps block_raid=false raw_transport_rtt=separate-test-required\n' \
                "$transport" "$lanes" "$qd" "$((lanes*qd))" "$extents" "$cpus" "$middle_cpus" "$tail_cpus" >"$local_dir/topology.log"
            "${ssh[@]}" "ubuntu@$client" "echo \$\$ >'$remote_dir/client.pid'; exec timeout 180 env $envs URING_PLAY_PIN_CPU_LIST=$cpus URING_PLAY_RAID_MIRROR_CLIENT_WAL_CONFIG='$remote_dir/client.json' '$bin' zcraid-mirror-send '$transport' '$middle_ip' 42000,43000 '$bytes' '$extents' '$lanes' '$remote_dir/plan.json' efa-direct rdm true" >"$local_dir/client.log" 2>&1
            for pid in "${children[@]}"; do wait "$pid"; done
            children=() cleanup_hosts=() cleanup_paths=()
            for host in "$middle" "$tail_node"; do
                name=middle; [[ $host != "$tail_node" ]] || name=tail
                "${ssh[@]}" "ubuntu@$host" "sha256sum '$remote_dir/$name.base'" >"$local_dir/$name.sha256"
            done
            [[ $(cut -d' ' -f1 "$local_dir/middle.sha256") = $(cut -d' ' -f1 "$local_dir/tail.sha256") ]] || { echo "mirror payload mismatch" >&2; exit 1; }
            rg 'zcraid-mirror-(send-summary|send-latency-summary|client-wal-summary)' "$local_dir/client.log" | tee -a "$out/summary.log"
        done
    done
done
