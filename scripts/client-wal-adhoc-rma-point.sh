#!/usr/bin/env bash
# Adapter for the existing strict raw-RMA queue matrix. No local storage I/O.
set -euo pipefail
[[ $# = 4 ]] || exit 2
mode=$1 qd=$2 rep=$3 out=$4
[[ $mode = read || $mode = write ]] || exit 2
inventory=${ADHOC_INVENTORY:?set ADHOC_INVENTORY}
source_host=$(jq -r '.instances[0].public_ip' "$inventory")
target_host=$(jq -r '.instances[2].public_ip' "$inventory")
target_ip=$(jq -r '.instances[2].private_ip' "$inventory")
ssh=(ssh -i "${ADHOC_SSH_KEY:-/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519}" -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8 -o ServerAliveInterval=10)
bin=${REMOTE_BIN:-/home/ubuntu/zcutils/zcutils}
lanes=1 payload=64M cpus=0
if ((qd>16)); then lanes=32; payload=512M; cpus=0-31; fi
envs="URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_TOPOLOGY_FATAL=1 URING_PLAY_HUGETLB=1 URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=$cpus URING_PLAY_OFI_DOMAIN=efa_0-rdm FI_EFA_IFACE=efa_0 FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_OFI_TIMEOUT_MS=60000 URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_BUSY_POLL_ITERS=100000 URING_PLAY_OFI_RMA_READ_QD=$qd URING_PLAY_OFI_RMA_WRITE_QD=$qd URING_PLAY_OFI_TX_QUEUE_DEPTH=$qd URING_PLAY_OFI_RX_QUEUE_DEPTH=$qd URING_PLAY_OFI_RMA_WRITE_MORE=1 URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 URING_PLAY_OFI_RMA_ACCESS_PATTERN=${URING_PLAY_OFI_RMA_ACCESS_PATTERN:-sequential}"
pidfile="/tmp/zc-raw-rma-$mode-q$qd-r$rep.pid"
target_pid=
cleanup() {
    "${ssh[@]}" "ubuntu@$target_host" "if [ -f '$pidfile' ]; then p=\$(cat '$pidfile'); case \$p in ''|*[!0-9]*) exit 1;; esac; if [ -r /proc/\$p/cmdline ] && tr '\\0' ' ' </proc/\$p/cmdline | grep -F '$bin' >/dev/null; then kill -TERM \$p; fi; fi" >/dev/null 2>&1 || true
    [[ -z $target_pid ]] || wait "$target_pid" 2>/dev/null || true
}
trap cleanup EXIT
"${ssh[@]}" "ubuntu@$target_host" "echo \$\$ >'$pidfile'; exec timeout 150 env $envs '$bin' zcwal-ofi-rma-target efa-direct rdm '$target_ip' 55000 '$lanes' '$payload' 4K '$lanes'" >"$out/target-r$rep.log" 2>&1 &
target_pid=$!
ready=0
for ((n=0;n<90;n++)); do
    if "${ssh[@]}" "ubuntu@$target_host" "ss -H -ltn 'sport = :56000'" | grep -q LISTEN; then ready=1; break; fi
    sleep 0.1
done
((ready)) || exit 1
"${ssh[@]}" "ubuntu@$source_host" "exec timeout 150 env $envs '$bin' zcwal-ofi-rma-$mode efa-direct rdm '$target_ip' 55000 '$lanes' '$payload' 4K '$lanes'"
wait "$target_pid"
target_pid=
trap - EXIT
