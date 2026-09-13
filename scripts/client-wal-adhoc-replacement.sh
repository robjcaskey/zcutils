#!/usr/bin/env bash
# Destructive, scoped EC2 correctness test. NOT seamless client failover or a
# Raft quorum test. The existing committed topology controller stages/activates
# custody; this harness supplies the EC2 provisioning worker for its decision.
set -euo pipefail
[[ $# = 3 ]] || { echo "usage: $0 INVENTORY OUTPUT_DIR tcp|rdma" >&2; exit 2; }
inventory=$1 out=$2 transport=$3
[[ $transport = tcp || $transport = rdma ]] || exit 2
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mkdir -p "$out"
out=$(realpath "$out")
key=${ADHOC_SSH_KEY:-/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519}
ssh=(ssh -i "$key" -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8 -o ServerAliveInterval=5 -o ServerAliveCountMax=3)
scp=(scp -q -i "$key" -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8 -o ServerAliveInterval=5 -o ServerAliveCountMax=3)
aws=(aws --profile "${ADHOC_AWS_PROFILE:-tf}" --region us-east-2)
client=$(jq -r '.instances[0].public_ip' "$inventory")
middle=$(jq -r '.instances[1].public_ip' "$inventory")
tail_node=$(jq -r '.instances[2].public_ip' "$inventory")
middle_ip=$(jq -r '.instances[1].private_ip' "$inventory")
tail_ip=$(jq -r '.instances[2].private_ip' "$inventory")
middle_id=$(jq -r '.instances[1].instance_id' "$inventory")
expiry=$(jq -r '.drop_dead_utc' "$inventory")
[[ $(jq -r '.region' "$inventory") = us-east-2 ]] || exit 2
[[ $middle_id =~ ^i-[0-9a-f]+$ ]] || exit 2
(( $(date -d "$expiry" +%s) - $(date +%s) > 600 )) || { echo "insufficient bounded lease for replacement" >&2; exit 1; }
run_id="zc-client-wal-replacement-$transport-$(date -u +%Y%m%dT%H%M%SZ)"
dir="/mnt/zc-terminal/$run_id"
replacement_ip=10.73.1.250
bin=/home/ubuntu/zcutils/zcutils
logical=4194304
journal=16777216
test_volume=$(date +%s)
"${aws[@]}" ec2 describe-instances --instance-ids "$middle_id" >"$out/lost-instance-before.json"
jq -e '.Reservations[0].Instances[0] | .State.Name=="running" and .Placement.AvailabilityZone=="us-east-2c" and .Placement.GroupName=="zc-client-wal-trio-20260906"' "$out/lost-instance-before.json" >/dev/null
cp "$root/tests/fixtures/client-wal/plan.json" "$out/plan.json"
jq --arg dir "$dir" --arg tail "$tail_ip" --arg replacement "$replacement_ip" --arg transport "$transport" --argjson volume "$test_volume" '
  .policy.scope.volume=$volume |
  .journal_directory=($dir+"/client") | .repair.controller_directory=($dir+"/controller") |
  .repair.survivor_control=($tail+":45000") | .repair.replacement_control=($replacement+":45000") |
  .repair.logical_bytes=4194304 | .repair.replacement_wait_seconds=900 |
  if $transport=="rdma" then .rdma={provider:"efa-direct",domain:"efa_0-rdm"} else . end' \
  "$root/tests/fixtures/client-wal/failure-client.json" >"$out/client.json"
jq --arg dir "$dir" --arg tail "$tail_ip" --arg transport "$transport" '
  .remote=$tail | .extents_per_lane=1024 |
  .terminal_target=("zcpwal:"+$dir+"/middle.wal,"+$dir+"/middle.base,4M,16M") |
  if $transport=="rdma" then .rdma={provider:"efa-direct",domain:"efa_0-rdm"} else . end' \
  "$root/tests/fixtures/client-wal/failure-middle.json" >"$out/middle.json"
for role in tail replacement; do
    bind=$tail_ip; [[ $role = tail ]] || bind=$replacement_ip
    jq --arg dir "$dir" --arg tail "$tail_ip" --arg bind "$bind" --arg role "$role" --argjson volume "$test_volume" '
      .scope.volume=$volume |
      .bind=($bind+":45000") | .copy_endpoint=($tail+":45001") | .logical_bytes=4194304 |
      .target=("zcpwal:"+$dir+"/"+$role+".wal,"+$dir+"/"+$role+".base,4M,16M")' \
      "$root/tests/fixtures/client-wal/repair-$role.json" >"$out/repair-$role.json"
done
printf '{"provider":"efa-direct","domain":"efa_0-rdm"}\n' >"$out/rdma.json"
for host in "$client" "$middle" "$tail_node"; do
    "${ssh[@]}" "ubuntu@$host" "mkdir '$dir'"
    "${scp[@]}" "$out"/*.json "ubuntu@$host:$dir/"
done
envs="URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0,1 URING_PLAY_HUGETLB=1 URING_PLAY_RAID_MIRROR_ACK_WINDOW=1 URING_PLAY_RAID_MIRROR_IDLE_TIMEOUT_MS=5000 URING_PLAY_ZCNBLK_PWAL_INTEGRITY=frame URING_PLAY_ZCNBLK_WAL_LEAF_SUBMIT_MODE=blocking URING_PLAY_SEND_PATTERN=fill URING_PLAY_SEND_FILL_BYTE=90 URING_PLAY_OFI_TX_QUEUE_DEPTH=1 URING_PLAY_OFI_RX_QUEUE_DEPTH=1 URING_PLAY_OFI_RMA_WRITE_QD=1 URING_PLAY_OFI_CQ_SLEEP_NS=0 FI_EFA_IFACE=efa_0 FI_EFA_USE_DEVICE_RDMA=1"
rdma_env=
[[ $transport = tcp ]] || rdma_env="URING_PLAY_RAID_MIRROR_RDMA_CONFIG=$dir/rdma.json"
wait_listener() {
    local host=$1 port=$2 n
    for ((n=0;n<90;n++)); do
        if "${ssh[@]}" "ubuntu@$host" "ss -H -ltn 'sport = :$port'" | grep -q LISTEN; then return; fi
        sleep 0.1
    done
    return 1
}
# S automatically transitions from failed old ingress to a sealed repair server.
"${ssh[@]}" "ubuntu@$tail_node" "env $envs $rdma_env URING_PLAY_ZCNBLK_WAL_LEAF_WRITE_DELAY_US=20000 '$bin' zcraid-mirror-recv tcp 0.0.0.0 44000 1 4M 4K 1 '$dir/plan.json' efa-direct rdm true 'zcpwal:$dir/tail.wal,$dir/tail.base,4M,16M'; status=\$?; test \$status -ne 0 || exit 90; exec timeout 1100 env $envs '$bin' zcraid-repair-terminal '$dir/repair-tail.json'" >"$out/tail.log" 2>&1 &
tail_pid=$!
wait_listener "$tail_node" 44000
"${ssh[@]}" "ubuntu@$middle" "exec timeout 180 env $envs '$bin' zcraid-mirror-hop '$dir/middle.json'" >"$out/middle.log" 2>&1 &
middle_pid=$!
wait_listener "$middle" 43000
"${ssh[@]}" "ubuntu@$client" "exec timeout 1100 env $envs URING_PLAY_RAID_MIRROR_CLIENT_WAL_CONFIG='$dir/client.json' '$bin' zcraid-mirror-send '$transport' '$middle_ip' 42000,43000 4M 4K 1 '$dir/plan.json' efa-direct rdm true" >"$out/client.log" 2>&1 &
client_pid=$!
deadline=$((SECONDS+60))
until rg -q 'client-wal-custody-progress:.*released_hwm=[1-9][0-9]*' "$out/client.log"; do
    ((SECONDS<deadline)) && kill -0 "$client_pid" || { echo "no reclaimed prefix before loss" >&2; exit 1; }
    sleep 0.05
done
date -u +%FT%TZ >"$out/fault-requested-at"
# Actual hard loss, not a stopped process with its old disk available.
"${aws[@]}" ec2 terminate-instances --instance-ids "$middle_id" --force --skip-os-shutdown >"$out/terminate-middle.json"
deadline=$((SECONDS+90))
until rg -q 'client-wal-repair-staged:' "$out/client.log"; do
    ((SECONDS<deadline)) && kill -0 "$client_pid" || { echo "controller did not stage replacement" >&2; exit 1; }
    sleep 0.1
done
date -u +%FT%TZ >"$out/controller-staged-at"
# Provision only after observing the controller's staged decision. Its new
# replica incarnation cannot count for durability until replay and sync finish.
/home/rob/spot-helper/ec2_perf_spot.py launch --profile "${ADHOC_AWS_PROFILE:-tf}" \
    --region us-east-2 --availability-zone us-east-2c --subnet-id subnet-0a18b0a152bdb2039 \
    --security-group-id sg-0497f0ce885599e5f --instance-type c8gn.48xlarge --nodes 1 \
    --ami-id ami-03e774c3214166a53 --drop-dead-utc "$expiry" --max-spot-price 3 \
    --max-total-cost 5 --root-gb 64 --enable-efa --network-card-count 2 \
    --placement-group zc-client-wal-trio-20260906 --run-id "$run_id" \
    --inventory "$out/replacement-inventory.json" --yes >"$out/provision.log" 2>&1
new_id=$(jq -r '.instances[0].instance_id' "$out/replacement-inventory.json")
new_host=$(jq -r '.instances[0].public_ip' "$out/replacement-inventory.json")
new_eni=$(jq -r '.instances[0].network_interfaces[0].network_interface_id' "$out/replacement-inventory.json")
"${aws[@]}" ec2 assign-private-ip-addresses --network-interface-id "$new_eni" --private-ip-addresses "$replacement_ip" >"$out/secondary-address.json"
deadline=$((SECONDS+180))
until "${ssh[@]}" "ubuntu@$new_host" true >/dev/null 2>&1; do ((SECONDS<deadline)) || exit 1; sleep 1; done
"${ssh[@]}" "ubuntu@$new_host" "mkdir -p /home/ubuntu/zcutils/scripts"
"${scp[@]}" "$root/scripts/welcome-to-the-team.sh" "ubuntu@$new_host:/home/ubuntu/zcutils/scripts/"
"${ssh[@]}" "ubuntu@$new_host" 'bash /home/ubuntu/zcutils/scripts/welcome-to-the-team.sh --no-build --hugepages 32768 --install-efa' >"$out/replacement-bootstrap.log" 2>&1
"${scp[@]}" "${ADHOC_RELEASE_BINARY:?set ADHOC_RELEASE_BINARY to tested native binary}" "ubuntu@$new_host:$bin"
"${aws[@]}" ec2 create-volume --availability-zone us-east-2c --size 32 --volume-type gp3 --iops 16000 --throughput 1000 \
    --tag-specifications "ResourceType=volume,Tags=[{Key=uringPlayRunId,Value=$run_id},{Key=Name,Value=$run_id-terminal}]" >"$out/replacement-volume.json"
volume=$(jq -r .VolumeId "$out/replacement-volume.json")
"${aws[@]}" ec2 wait volume-available --volume-ids "$volume"
"${aws[@]}" ec2 attach-volume --volume-id "$volume" --instance-id "$new_id" --device /dev/sdf >"$out/attach-volume.json"
"${aws[@]}" ec2 wait volume-in-use --volume-ids "$volume"
"${aws[@]}" ec2 modify-instance-attribute --instance-id "$new_id" --block-device-mappings '[{"DeviceName":"/dev/sdf","Ebs":{"DeleteOnTermination":true}}]'
# The serial must identify the newly created, empty task volume before mkfs.
serial=${volume//-/}
"${ssh[@]}" "ubuntu@$new_host" "set -eu; sudo udevadm settle; test \$(cat /sys/block/nvme1n1/device/serial) = '$serial'; test -z \$(sudo blkid -s TYPE -o value /dev/nvme1n1 || true); sudo mkfs.ext4 -q /dev/nvme1n1; sudo mkdir -p /mnt/zc-terminal; sudo mount -o noatime /dev/nvme1n1 /mnt/zc-terminal; sudo chown ubuntu:ubuntu /mnt/zc-terminal; mkdir '$dir'; iface=\$(ip -o -4 route show default | awk '{print \$5; exit}'); sudo ip addr add '$replacement_ip/24' dev \$iface; chmod +x '$bin'"
"${scp[@]}" "$out"/repair-replacement.json "ubuntu@$new_host:$dir/"
date -u +%FT%TZ >"$out/replacement-ready-at"
"${ssh[@]}" "ubuntu@$new_host" "exec timeout 180 env $envs '$bin' zcraid-repair-terminal '$dir/repair-replacement.json'" >"$out/replacement.log" 2>&1 &
replacement_pid=$!
set +e
wait "$client_pid"; source_status=$?
wait "$middle_pid"; lost_status=$?
wait "$tail_pid"; tail_status=$?
wait "$replacement_pid"; replacement_status=$?
set -e
[[ $source_status != 0 && $lost_status != 0 && $tail_status = 0 && $replacement_status = 0 ]]
rg -q 'client-wal-repair-complete:.*replacement_active=true' "$out/client.log"
through=$(sed -n 's/.*client-wal-repair-terminal-complete:.*through=\([0-9][0-9]*\).*/\1/p' "$out/replacement.log")
[[ $through =~ ^[1-9][0-9]*$ && $through -le 1024 ]]
expected=$( { head -c "$((through*4096))" /dev/zero | tr '\000' Z; head -c "$(((1024-through)*4096))" /dev/zero; } | sha256sum | cut -d' ' -f1)
for role in tail replacement; do
    host=$tail_node; [[ $role = tail ]] || host=$new_host
    "${ssh[@]}" "ubuntu@$host" "sha256sum '$dir/$role.base'" >"$out/$role.sha256"
    [[ $(cut -d' ' -f1 "$out/$role.sha256") = "$expected" ]] || { echo "reconstructed content mismatch $role" >&2; exit 1; }
done
"${scp[@]}" "ubuntu@$client:$dir/controller/topology.ndjson" "$out/topology.ndjson"
jq --slurpfile replacement "$out/replacement-inventory.json" '.instances[1]=$replacement[0].instances[0]' "$inventory" >"$out/updated-trio.json"
date -u +%FT%TZ >"$out/recovery-completed-at"
printf 'PASS transport=%s destroyed_middle=%s replacement=%s fenced_hwm=%s expected_sha256=%s new_disk=true reclaimed_prefix_tested=true controller_activation_after_actual_sync=true continuous_client=false raft_quorum_test=false\n' "$transport" "$middle_id" "$new_id" "$through" "$expected" | tee "$out/result.log"
echo "Replacement remains running under the original expiry; task teardown must include $out/replacement-inventory.json"
