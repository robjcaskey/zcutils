#!/usr/bin/env bash
set -euo pipefail

key=/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519
sender=ubuntu@18.220.42.74
receiver=ubuntu@13.58.147.62
receiver_private=172.31.46.71
run=zc-mirror-real-c8gn48-20260815T155510Z
out=/home/rob/zcutils/bench-results/$run
ssh_opts=(-i "$key" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null)

transport=${1:?transport}
repeat=${2:?repeat}
case "$transport" in
  rdma) bind=auto ;;
  tcp) bind=0.0.0.0 ;;
  *) exit 2 ;;
esac
name=${transport}-r${repeat}
remote_dir=/home/ubuntu/$run/$name

common='PATH=/opt/amazon/efa/bin:/home/ubuntu/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin LD_LIBRARY_PATH=/opt/amazon/efa/lib64 URING_PLAY_TOPOLOGY_STRICT=1 URING_PLAY_PIN_CPUS=1 URING_PLAY_RAID_MIRROR_ACK_WINDOW=64 URING_PLAY_OFI_TX_QUEUE_DEPTH=128 URING_PLAY_OFI_RX_QUEUE_DEPTH=128 URING_PLAY_OFI_TX_CQ_SIZE=512 URING_PLAY_OFI_RX_CQ_SIZE=512 URING_PLAY_OFI_CQ_SLEEP_NS=0 URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 URING_PLAY_OFI_RMA_WRITE_MORE=1 URING_PLAY_OFI_RMA_WRITE_MORE_BURST=64 URING_PLAY_OFI_EFA_FABRIC=efa-direct FI_EFA_IFACE=efa_0 FI_EFA_USE_DEVICE_RDMA=1 FI_EFA_USE_HUGE_PAGE=1 URING_PLAY_RAID_MIRROR_TERMINAL_IO=blocking'

ssh "${ssh_opts[@]}" "$receiver" "set -e; d=$remote_dir; mkdir -p \"\$d\"; for f in j0 b0 j1 b1; do fallocate -l 256M \"\$d/\$f\"; done; cd /home/ubuntu/zcutils; env $common URING_PLAY_PIN_CPU_LIST=16-31 taskset -c 16-31 ./target/release/zcraid-mirror-recv $transport $bind 42000 0 8M 64K 16 plan.json efa rdm true \"zcpwal:\$d/j0,\$d/b0,256M,256M\" >\"\$d/recv0.log\" 2>&1 & echo \$! >\"\$d/recv0.pid\"; env $common URING_PLAY_PIN_CPU_LIST=32-47 taskset -c 32-47 ./target/release/zcraid-mirror-recv $transport $bind 44000 1 8M 64K 16 plan.json efa rdm true \"zcpwal:\$d/j1,\$d/b1,256M,256M\" >\"\$d/recv1.log\" 2>&1 & echo \$! >\"\$d/recv1.pid\""
sleep 1

ssh "${ssh_opts[@]}" "$sender" "cd /home/ubuntu/zcutils && env $common URING_PLAY_PIN_CPU_LIST=16-31 taskset -c 16-31 timeout 180 ./target/release/zcraid-mirror-send $transport $receiver_private 42000,44000 8M 64K 16 plan.json efa rdm true" >"$out/$name-send.log" 2>&1

for attempt in 1 2 3 4 5; do
  active=$(ssh "${ssh_opts[@]}" "$receiver" "a=0; for f in $remote_dir/*.pid; do p=\$(cat \"\$f\"); kill -0 \"\$p\" 2>/dev/null && a=1; done; echo \$a")
  [[ $active == 0 ]] && break
  sleep 1
done
ssh "${ssh_opts[@]}" "$receiver" "sha256sum $remote_dir/b0 $remote_dir/b1; grep -hE '^(zcraid-mirror-recv-summary|Error:)' $remote_dir/recv*.log" >"$out/$name-recv-summary.log"
grep -E '^(zcraid-mirror-send-summary|Error:)' "$out/$name-send.log"
cat "$out/$name-recv-summary.log"
