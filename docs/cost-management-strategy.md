# Cost Management Strategy

This project uses adhoc EC2 Spot instances for disposable performance tests.
The cost target is a hard maximum of $50/day, with each experiment bounded by
an explicit UTC drop-dead time and automatic termination tags.

## Budget Rules

- Default daily ceiling: $50 total.
- Default selector ceiling: $12.50/hour for a three-node run, which keeps a
  four-hour experiment under $50 before small control-plane costs.
- Never launch without `adhocKeepaliveModeAction=terminate`.
- Never launch a run longer than 15 minutes without an absolute
  `adhocKeepalive` UTC timestamp.
- Do not extend adhoc leases beyond 3 hours unless Rob explicitly approves it.
- Use one-time Spot with interruption behavior `terminate`.
- Set `--max-spot-price` on launch so the worst-case instance-hour ceiling is
  bounded before the request is submitted.

The launcher estimates a conservative ceiling from:

- `nodes * max_spot_price * hours`;
- public IPv4 address hours when SSH public IPs are attached;
- root EBS volume GB-hours.

It refuses launches above `--max-total-cost`, which defaults to `$50`.

## Spot Helper

The reusable helper lives at:

```text
/home/rob/spot-helper/ec2_perf_spot.py
```

Use it to efficiently find low-cost Spot instances for performance compute:
it ranks candidate region/AZ/type combinations by cost, bandwidth, local NVMe,
architecture, and optional placement score. The repo path
`scripts/ec2_perf_spot.py` is only a compatibility wrapper.

Spot searches are intentionally cached. The helper stores AWS discovery data in
`/home/rob/spot-helper/.cache/ec2_perf_spot_cache.json` with a default
15-minute TTL. The first broad search across many regions and instance types
may be slow because it has to query AWS for Spot history and instance metadata;
repeated searches should be much faster while the cache is warm.

Cached data includes:

- enabled regions;
- default public subnets;
- instance type metadata and offerings;
- Spot price history by region and instance type;
- Spot placement score results when requested.

For high-bandwidth metal searches, prefer:

```bash
/home/rob/spot-helper/ec2_perf_spot.py recommend \
  --all-regions \
  --no-local-nvme \
  --no-x86-only \
  --metal-only \
  --min-network-gbps 300 \
  --score-mode bandwidth \
  --max-hourly-total 50
```

## Allowed Network Shape

Control path:

- SSH from Rob's current public IP to each worker's public IPv4 address.
- `rsync`/small package setup traffic over SSH is acceptable.
- Public IPv4 is used only for control and has an hourly charge. This is the
  default for these short experiments because it avoids a jumpbox and keeps
  copy/build/debug loops simple.

Bulk Raft path:

- All benchmark nodes must be in one Availability Zone and one subnet.
- Raft replication, TCP mux, libfabric/EFA, RDMA-style experiments, and WAL
  benchmark traffic must use private IPv4 addresses from the EC2 inventory.
- No public IP, Elastic IP, NAT Gateway, load balancer, cross-AZ path, VPC
  peering, Transit Gateway, or cross-region path is allowed for bulk traffic.
- For instance types advertised above 200 Gbit/s, usually launch the run with
  at least two private NICs from the start. The ad hoc helper supports this
  with `launch --network-card-count 2`, which avoids stop/start disruption and
  gives topology tests a second private address/card to bind lanes against.

Same-AZ private EC2-to-EC2 traffic is the intended no-cost bulk path. Public IP
traffic between instances can be billed even when the instances are physically
near each other, so benchmark config must never use the public IPs for Raft.

## Placement Groups

Cluster placement groups are useful for lower latency and tighter node
placement. Creating a placement group has no direct AWS charge, and EFA itself
has no additional feature charge. The hidden cost risk is operational:

- placement groups can reduce Spot capacity and cause launch failures;
- using stop/hibernate Spot interruption behavior is not allowed in placement
  groups, so this project uses terminate;
- this repo's local policy currently says not to create adhoc AWS resources
  other than EC2 instances.

Therefore the launcher may use an existing placement group with
`--placement-group`, but it must not create one unless the local policy is
changed or Rob explicitly approves that extra resource.

## Existing us-east-1 Control Pattern

The currently suitable existing pieces in `us-east-1` are:

- Key pair: `adhocMasterKeypair`.
- Local private key:
  `/home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519`.
- SSH SG: `sg-06a6264f49bd2329d` (`mudbox...`) allows SSH from Rob's public IP.
- Bulk/private SG: `sg-e0dfdb9d` (`default`) allows all traffic among members
  of the same SG, which is suitable for same-SG private Raft traffic and EFA
  security-group self traffic.

The launch command should attach both SGs:

```bash
/home/rob/spot-helper/ec2_perf_spot.py launch \
  --region us-east-1 \
  --availability-zone us-east-1a \
  --subnet-id subnet-9cf16dc7 \
  --security-group-ids sg-06a6264f49bd2329d,sg-e0dfdb9d \
  --key-name adhocMasterKeypair \
  --instance-type m6idn.metal \
  --nodes 3 \
  --max-spot-price 1.50 \
  --drop-dead-utc 2026-05-23T23:30:00Z
```

Omit `--yes` to inspect the request first. Add `--yes` only after the cost
ceiling, AZ, subnet, and tags are correct.

For cheaper regions that only have the default self-referencing SG, prepare or
reuse a small no-hourly-cost support set in the selected region/AZ:

```bash
/home/rob/spot-helper/ec2_perf_spot.py prep-region-az \
  --region eu-north-1 \
  --availability-zone eu-north-1a
```

That dry run resolves the default public subnet, VPC, and current public IPv4.
Add `--yes` to create or repair:

- the approved `adhocMasterKeypair` import;
- SSH ingress from the current public IPv4 `/32`;
- all private traffic among instances attached to the same SG;
- optional cluster placement group if `--placement-group NAME` is passed;
- default egress.

Support resource names are intentionally short, such as `up-adhoc-ctl`, but
their tags are explicit:

- `Project=zcutils`;
- `Purpose=adhoc-performance-compute-support`;
- `AdhocSupport=true`;
- `AdhocSupportKind=...`;
- `AdhocSupportRegion=...`;
- cleanup scope tags for the region and, where applicable, AZ.

These support resources have no hourly cost, but they are still AWS resources.
They are meant to be shared reusable regional adhoc infrastructure, not
per-experiment disposable objects. It is reasonable to keep one adhoc VPC/subnet
layout, control SG, private self-traffic SG, key import, and optional placement
group per useful region/AZ. Per-experiment cleanup should terminate instances
and delete volumes; regional adhoc support resources can remain unless we are
intentionally cleaning up a region.

The helper may create or tag these no-hourly-cost resource types:

- EC2 key pair import named `adhocMasterKeypair`.
- Security group named `up-adhoc-ctl`, allowing public SSH from the current
  operator `/32` and private all-traffic self-reference for cluster traffic.
- Optional cluster placement group, only when `--placement-group NAME` is
  explicitly passed.

List shared adhoc support resources with:

```bash
/home/rob/spot-helper/ec2_perf_spot.py list-adhoc-support --all-regions
```

## SSH

After launch, the script writes an inventory JSON containing public and private
IPs. Print the SSH commands with:

```bash
/home/rob/spot-helper/ec2_perf_spot.py ssh-commands \
  --inventory qemu-zcrx/ec2-adhoc-inventory.json
```

That prints commands like:

```bash
ssh -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30 \
  -i /home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519 \
  ubuntu@PUBLIC_IP
```

Use public IPs only for SSH. Inside the benchmark scripts, use the inventory's
private IP list for Raft peers.

## Jumpbox Fallback

The default is direct public SSH to each worker. If an AZ/subnet/security-group
combination leaves the workers without public SSH line of sight, create one
tiny adhoc Spot jumpbox in the same subnet/AZ and attach the public SSH SG to
only that jumpbox. Workers can then run with private IPs only.

The jumpbox is only for SSH, rsync forwarding, and command fanout. It is not
large enough to build or benchmark. Use SSH proxying through it to reach a real
worker:

```bash
ssh -i /home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519 \
  -J ubuntu@JUMPBOX_PUBLIC_IP ubuntu@WORKER_PRIVATE_IP

rsync -az \
  -e 'ssh -i /home/rob/robsSecretStore/aws/adhocMasterKeypair-20260523-ed25519 -J ubuntu@JUMPBOX_PUBLIC_IP' \
  ./ ubuntu@WORKER_PRIVATE_IP:~/zcutils/
```

The jumpbox must carry the same `adhocKeepaliveModeAction=terminate`,
`adhocKeepalive`, and `uringPlayRunId` tags as the worker set so cleanup reaps
it with the run.

## Things That Can Accidentally Cost Money

- Public IPv4 addresses: billed hourly while attached.
- Elastic IPs: billed whether in use or idle; do not allocate them for this.
- NAT Gateways: hourly charge plus per-GB processing; do not use them.
- Cross-AZ traffic: avoid by keeping all nodes in one subnet/AZ.
- Public-IP instance-to-instance traffic: can be billed as regional/internet
  data transfer; use private IPs.
- Load balancers, Transit Gateway, PrivateLink, VPC peering, cross-region links:
  not allowed for these benchmarks.
- EBS root volumes: delete on termination must be true.
- Unattached volumes and snapshots: do not create; verify cleanup after runs.
- Marketplace AMIs or licensed OS images: use Ubuntu public AMIs from SSM, not
  paid marketplace images.
- Stopped instances: not allowed for disposable benchmarks; terminate instead.

## Cleanup Checklist

At the end of every run:

```bash
/home/rob/spot-helper/ec2_perf_spot.py terminate \
  --region REGION \
  --run-id RUN_ID \
  --yes
```

Then verify:

```bash
aws ec2 describe-instances --profile tf --region REGION \
  --filters Name=tag:uringPlayRunId,Values=RUN_ID \
  --query 'Reservations[].Instances[].{Id:InstanceId,State:State.Name}'

aws ec2 describe-volumes --profile tf --region REGION \
  --filters Name=tag:uringPlayRunId,Values=RUN_ID \
  --query 'Volumes[].{Id:VolumeId,State:State,Size:Size}'
```

There should be no running/stopped instances and no available unattached
volumes for the run id.

## Sources

- EC2 placement groups: https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/placement-groups.html
- EC2 same-AZ data transfer pricing: https://aws.amazon.com/ec2/pricing/on-demand/
- Public IPv4 hourly charge: https://aws.amazon.com/blogs/aws/new-aws-public-ipv4-address-charge-public-ip-insights/
- NAT Gateway pricing: https://docs.aws.amazon.com/vpc/latest/userguide/nat-gateway-pricing.html
- EFA pricing: https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/efa.html
