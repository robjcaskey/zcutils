#!/usr/bin/env bash
set -euo pipefail

more="${1:?usage: run-fi-more-block-ab.sh 0|1 [lanes [qd [ops-per-worker]]]}"
[[ "$more" =~ ^[01]$ ]] || exit 2
burst="${URING_PLAY_OFI_RMA_WRITE_MORE_BURST:-64}"
[[ "$burst" =~ ^[0-9]+$ ]] && [ "$burst" -ge 1 ] && [ "$burst" -le 65536 ] || exit 2
lanes="${2:-8}"
qd="${3:-64}"
ops="${4:-250000}"
wait_min=1
[ "$qd" -lt 16 ] || wait_min=16

key=/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519
client=ubuntu@52.15.70.216
leaf=ubuntu@3.17.27.233
run_id=zcutils-efa-fanin-adhoc-c8gn16-20260811T1554Z
private_ips=172.31.35.59,172.31.42.23
tag="${TAG_OVERRIDE:-efa-fi-more${more}-b${burst}-w${lanes}-q${qd}}"
ssh_base=(ssh -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 -i "$key")
common="URING_NODE_INDEX=2 URING_RUN_ID=$run_id URING_PRIVATE_IPS=$private_ips URING_PLAY_OFI_RMA_WRITE_MORE=$more URING_PLAY_OFI_RMA_WRITE_MORE_BURST=$burst"

stop_leaf() {
	"${ssh_base[@]}" "$leaf" \
		"$common /home/ubuntu/fanin-node.sh leaf-stop '$tag'" >/dev/null 2>&1 || true
}
trap stop_leaf EXIT INT TERM

"${ssh_base[@]}" "$client" \
	"URING_NODE_INDEX=1 URING_RUN_ID=$run_id URING_PRIVATE_IPS=$private_ips URING_PLAY_OFI_RMA_WRITE_MORE=$more URING_PLAY_OFI_RMA_WRITE_MORE_BURST=$burst /home/ubuntu/fanin-node.sh leaf-start '$tag' efa write 1 64"
"${ssh_base[@]}" "$leaf" \
	"$common /home/ubuntu/fanin-node.sh leaf-start '$tag' efa write 1 64"
"${ssh_base[@]}" "$client" \
	"URING_NODE_INDEX=1 URING_RUN_ID=$run_id URING_PRIVATE_IPS=$private_ips URING_PLAY_OFI_RMA_WRITE_MORE=$more URING_PLAY_OFI_RMA_WRITE_MORE_BURST=$burst /home/ubuntu/fanin-node.sh zcnblk-run '$tag' efa write '$lanes' '$qd' 1 single-domain-fan-in 64 '$ops' 1 '$wait_min' 0"
stop_leaf
trap - EXIT INT TERM
