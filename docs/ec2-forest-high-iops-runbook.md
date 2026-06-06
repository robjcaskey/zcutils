# EC2 Forest High-IOPS Runbook

This is the fast setup and teardown checklist for short forest/fan block IOPS
experiments on adhoc EC2 instances.

Hard rules:

- Launch only through `scripts/ec2_perf_spot.py` or
  `/home/rob/spot-helper/ec2_perf_spot.py`; this applies the adhoc run tags and
  drop-dead termination tags.
- Use one AZ and one subnet for the whole forest.
- Use two NICs per node from launch time: `--network-card-count 2`.
- Use exactly one public IPv4 per node, on card 0 only. Card 1 is private-only.
  With multiple launch-time ENIs, EC2 cannot auto-associate a public IPv4 in
  the `RunInstances` call; the helper therefore attaches one tagged temporary
  public address to card 0 after launch and releases it during helper teardown.
- Public IPs are for SSH, rsync, and small control traffic only. Bulk benchmark
  traffic uses inventory private IPs, preferably card 1 private IPs.
- Do not allocate public addresses or attach secondary ENIs by hand unless
  recovering a failed old run. The helper owns those resources and their tags.
- Do not use any block device as a mirror or stripe primitive. Block devices are
  terminal leaf media only after userspace placement.

## 1. Pick Capacity

Check support and prices first:

```bash
scripts/ec2_perf_spot.py list-adhoc-support --profile tf --regions us-east-2

scripts/ec2_perf_spot.py spot-prices \
  --profile tf \
  --regions us-east-2 \
  --instance-types c8gn.48xlarge,c8gn.metal \
  --nodes 4 \
  --limit 20
```

If the region/AZ support set is not ready, dry-run the support setup, then add
`--yes` only after the subnet, VPC, key, and security group are correct:

```bash
scripts/ec2_perf_spot.py prep-region-az \
  --profile tf \
  --region us-east-2 \
  --availability-zone us-east-2c \
  --subnet-id subnet-c66ddd8b
```

## 2. Launch The Forest

Use a run id that visibly says `adhoc` and keep the inventory under `qemu-zcrx/`.
Set a real absolute UTC drop-dead. The helper refuses very long leases unless
explicitly approved.

```bash
RUN_TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUN_ID="zc-forest-adhoc-c8gn48-${RUN_TS}"
REGION="us-east-2"
AZ="us-east-2c"
SUBNET_ID="subnet-c66ddd8b"
SG_IDS="sg-..."                  # replace with the ready adhoc SG id from prep-region-az
DROP_DEAD_UTC="$(date -u -d '+2 hours' +%Y-%m-%dT%H:%M:%SZ)"
INV="qemu-zcrx/${RUN_ID}-inventory.json"

scripts/ec2_perf_spot.py launch \
  --profile tf \
  --region "$REGION" \
  --availability-zone "$AZ" \
  --subnet-id "$SUBNET_ID" \
  --security-group-ids "$SG_IDS" \
  --key-name adhocMasterKeypair \
  --instance-type c8gn.48xlarge \
  --nodes 4 \
  --max-spot-price 2.00 \
  --max-total-cost 50 \
  --root-gb 256 \
  --enable-efa \
  --network-card-count 2 \
  --associate-public-ip \
  --no-ena-express \
  --drop-dead-utc "$DROP_DEAD_UTC" \
  --run-id "$RUN_ID" \
  --inventory "$INV"
```

Read the printed dry-run request. Confirm:

- `TagSpecifications` includes `Purpose=adhoc-performance-compute`,
  `uringPlayRunId=$RUN_ID`, `adhocKeepaliveModeAction=terminate`, and
  `adhocKeepalive=$DROP_DEAD_UTC`.
- `NetworkInterfaces` has two entries.
- For `network_card_count > 1`, `public_ip_mode` is
  `tagged-eip-post-launch`. The launch request has no
  `AssociatePublicIpAddress` field because AWS rejects that field with multiple
  network interfaces.
- Card 1 has no public IP request or public association.
- For EFA/RDMA runs, the security group allows all traffic both inbound from
  and outbound to itself; default `0.0.0.0/0` egress alone is not enough.

Then rerun the exact command with `--yes`.

## 3. Validate ENIs Before Any Benchmark

Fail fast if the shape is wrong:

```bash
jq -e '
  (.instances | length) == 4 and
  all(.instances[];
    (.network_interfaces | length) == 2 and
    ([.network_interfaces[].public_ip | select(. != null)] | length) == 1 and
    ([.network_interfaces[] | select(.network_card_index == 0) |
      .public_ip | select(. != null)] | length) == 1 and
    ([.network_interfaces[] | select(.network_card_index == 1) |
      .public_ip | select(. != null)] | length) == 0 and
    ([.network_interfaces[].network_card_index] | sort) == [0,1]
  )
' "$INV"
```

Also capture AWS' current view:

```bash
DESC="qemu-zcrx/${RUN_ID}-describe-instances.json"
aws ec2 describe-instances \
  --profile tf \
  --region "$REGION" \
  --filters Name=tag:uringPlayRunId,Values="$RUN_ID" \
  > "$DESC"
```

Write the role map. The benchmark data path should use `card1_private`:

```bash
jq -r '
  (["role","public","card0_private","card1_private"]),
  (.instances | to_entries[] | [
    (["client","fan","leaf0","leaf1"][.key]),
    .value.public_ip,
    (.value.network_interfaces[] | select(.network_card_index == 0) |
      .private_ip),
    (.value.network_interfaces[] | select(.network_card_index == 1) |
      .private_ip)
  ]) | @tsv
' "$INV" | tee "qemu-zcrx/${RUN_ID}-role-map.tsv"
```

## 4. Sync Source Everywhere

Use the helper for the broad source sync:

```bash
scripts/ec2_perf_spot.py ssh-commands --inventory "$INV"

scripts/ec2_perf_spot.py sync \
  --inventory "$INV" \
  --repo . \
  --remote-dir ~/uring-play
```

Install common runtime/build packages on all nodes:

```bash
scripts/ec2_perf_spot.py exec \
  --inventory "$INV" \
  'sudo apt-get update &&
   sudo DEBIAN_FRONTEND=noninteractive apt-get install -y
     build-essential pkg-config liburing-dev clang make ethtool jq sysstat
     rsync curl ca-certificates numactl fio linux-tools-common'
```

## 5. Build Once, Copy Binaries To The Forest

Build on node 1 only. Pull a tarball back locally, then push it to every node.
This avoids four expensive Rust builds.

```bash
KEY="/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519"
SSH="ssh -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 -i $KEY"
SCP="scp -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 -i $KEY"
NODE1_PUBLIC="$(jq -r '.instances[0].public_ip' "$INV")"
ART_DIR="qemu-zcrx/${RUN_ID}-artifacts"
mkdir -p "$ART_DIR"

$SSH "ubuntu@${NODE1_PUBLIC}" '
  set -euo pipefail
  cd ~/uring-play
  if ! command -v cargo >/dev/null 2>&1; then
    curl https://sh.rustup.rs -sSf | sh -s -- -y
  fi
  . ~/.cargo/env
  cargo build --release \
    --bin zcutils \
    --bin zcnblk-fan \
    --bin zcnblk-wal-leaf \
    --bin zcfanout-logshm-bench
  tar -C ~/uring-play -czf /tmp/zcutils-release-bins.tgz \
    target/release/zcutils \
    target/release/zcnblk-fan \
    target/release/zcnblk-wal-leaf \
    target/release/zcfanout-logshm-bench
'

$SCP "ubuntu@${NODE1_PUBLIC}:/tmp/zcutils-release-bins.tgz" \
  "${ART_DIR}/zcutils-release-bins.tgz"

jq -r '.instances[].public_ip' "$INV" | while read -r ip; do
  $SCP "${ART_DIR}/zcutils-release-bins.tgz" "ubuntu@${ip}:/tmp/"
  $SSH "ubuntu@${ip}" '
    set -euo pipefail
    mkdir -p ~/uring-play
    tar -C ~/uring-play -xzf /tmp/zcutils-release-bins.tgz
    ls -lh ~/uring-play/target/release/{zcutils,zcnblk-fan,zcnblk-wal-leaf}
  '
done
```

## 6. Run Benchmarks

Use the role map's `card1_private` addresses for listeners and peers. Public IPs
must not appear in fan, leaf, or client bulk-traffic commands.

Minimum benchmark metadata to record in every run directory:

- `RUN_ID`, inventory path, role map path, and drop-dead UTC.
- Lane count, lane-to-worker map, lane-to-CPU map, and source private IP.
- Which NIC/card private IP was used for client, fan, and leaves.
- Batch depth, WAL batch window, write window, and result range mode.
- Whether the leaf backend is `zcdevnull`, terminal `/dev/zcbrdN`, or another
  explicitly allowed terminal leaf.

Keep raw logs under `qemu-zcrx/${RUN_ID}-.../` or `bench-results/${RUN_ID}-.../`
so teardown does not lose the result trail.

For a concrete two-node `c8gn.48xlarge` WAL transport run, including the exact
two-card topology, TCP buffer tuning, libfabric/EFA validation result, and
measured lane sweep, see
[`ec2-c8gn-pair-wal-transport-benchmark.md`](ec2-c8gn-pair-wal-transport-benchmark.md).
The best standard TCP/WAL result in that run was 522.6 Gbit/s and 15.95M
logical 4K records/s with 256 lanes per card across both network cards.

## 7. Teardown

Terminate with the helper by run id:

```bash
scripts/ec2_perf_spot.py terminate \
  --profile tf \
  --region "$REGION" \
  --run-id "$RUN_ID" \
  --yes
```

Then verify nothing billable from the run remains:

```bash
aws ec2 describe-instances \
  --profile tf \
  --region "$REGION" \
  --filters Name=tag:uringPlayRunId,Values="$RUN_ID" \
  --query 'Reservations[].Instances[].{Id:InstanceId,State:State.Name,PublicIp:PublicIpAddress}'

aws ec2 describe-volumes \
  --profile tf \
  --region "$REGION" \
  --filters Name=tag:uringPlayRunId,Values="$RUN_ID" \
  --query 'Volumes[].{Id:VolumeId,State:State,Size:Size}'
```

The expected post-teardown state is no pending/running/stopped instances and no
available unattached volumes for `RUN_ID`.
