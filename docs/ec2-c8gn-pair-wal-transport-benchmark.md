# c8gn Pair WAL Transport Benchmark

This note records the June 3, 2026 two-node `c8gn.48xlarge` adhoc run used to
validate the high-IOPS WAL transport shape before wiring it into RAID/fan
placement. It is a transport/WAL extent test, not a block-device stripe or
mirror test.

## Topology

- Run id: `zc-pair-adhoc-c8gn48-20260603T230056Z`
- Region/AZ: `us-east-2/us-east-2c`
- Placement group: `up-zc-ramtarget-us-east-2a`
- Instance type: `c8gn.48xlarge`, two nodes, two EFA ENIs per node
- Node0: public `3.151.209.179`, card0 `172.31.33.93`, card1 `172.31.40.131`
- Node1: public `18.119.177.17`, card0 `172.31.38.206`, card1 `172.31.40.23`
- Card0: `ens68`, NUMA 0, CPUs `0-95`
- Card1: `ens146`, NUMA 1, CPUs `96-191`

Bulk traffic must use private addresses only. Public addresses are for SSH and
rsync. The helper inventory and role map are saved under:

- `qemu-zcrx/zc-pair-adhoc-c8gn48-20260603T230056Z-inventory.json`
- `qemu-zcrx/zc-pair-adhoc-c8gn48-20260603T230056Z-role-map.tsv`

## Launch Shape

Use the adhoc helper, not hand-created instances:

```bash
/home/rob/spot-helper/ec2_perf_spot.py launch \
  --profile tf \
  --region us-east-2 \
  --availability-zone us-east-2c \
  --subnet-id subnet-c66ddd8b \
  --security-group-ids sg-025a50a35d3073a8a \
  --key-name adhocMasterKeypair \
  --instance-type c8gn.48xlarge \
  --nodes 2 \
  --max-spot-price 2.00 \
  --max-total-cost 10 \
  --root-gb 256 \
  --enable-efa \
  --network-card-count 2 \
  --associate-public-ip \
  --no-ena-express \
  --drop-dead-utc "$DROP_DEAD_UTC" \
  --run-id "$RUN_ID" \
  --inventory "qemu-zcrx/${RUN_ID}-inventory.json" \
  --yes
```

Validate that each node has two ENIs and exactly one public IPv4 on card0:

```bash
jq -e '
  (.instances | length) == 2 and
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

## Required Host Prep

The default TCP buffer cap on the Ubuntu image was too low for this benchmark.
Raise it on both nodes before trusting context-switch or throughput numbers:

```bash
sudo sysctl -w \
  net.core.rmem_max=134217728 \
  net.core.wmem_max=134217728 \
  net.ipv4.tcp_rmem='4096 87380 134217728' \
  net.ipv4.tcp_wmem='4096 65536 134217728' \
  net.core.netdev_max_backlog=250000 \
  net.ipv4.tcp_no_metrics_save=1
```

When using card1, bind the source address and enable route checks. Without a
source bind, Linux routed `172.31.40.23` out `ens68` because both ENIs are in
the same subnet:

```bash
export URING_PLAY_SOURCE_IP=172.31.40.131
export URING_PLAY_EXPECT_ROUTE_DEV=ens146
export URING_PLAY_EXPECT_LOCAL_ADDR=172.31.40.131
```

## Standard TCP/WAL Results

All runs used:

- `zcwal-extent-send` / `zcwal-extent-recv`
- `stream uring` framing
- 384 KiB WAL extents, 96 logical 4 KiB records per extent
- one TCP stream per lane
- `port-lane` lane-to-worker identity
- explicit source IP and route probes

| shape | card0 Gbit/s | card1 Gbit/s | aggregate wall Gbit/s | aggregate wall logical 4K IOPS | recv ctx switches |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 card, 64 lanes | n/a | 218.4 | 218.4 | 6.66M | 283k |
| 2 cards, 32 lanes/card | 218.2 | 145.0 | 289.9 | 8.85M | 404k |
| 2 cards, 64 lanes/card | 198.6 | 241.4 | 397.2 | 12.12M | 521k |
| 2 cards, 96 lanes/card | 236.9 | 226.7 | 453.3 | 13.83M | 686k |
| 2 cards, 128 lanes/card | 239.1 | 268.8 | 478.2 | 14.59M | 636k |
| 2 cards, 192 lanes/card | 270.8 | 252.8 | 505.6 | 15.43M | 992k |
| 2 cards, 256 lanes/card | 273.9 | 261.3 | 522.6 | 15.95M | 1.41M |

The best run was `256` lanes per card. It used 512 total TCP flows, 48 GiB per
card, and pinned card0 workers to CPUs `0-95` and card1 workers to CPUs
`96-191`. The result directory is:

```text
bench-results/zc-pair-adhoc-c8gn48-20260603T230056Z-standard-tcp-wal-dualcard-256lane-20260603T232532Z
```

The instance advertises 600 Gbit/s aggregate networking split across two
300 Gbit/s network cards. The live ENA driver exposed only 32 combined queues
per interface and rejected `ethtool -L IFACE combined 128`. High lane counts
therefore work by creating enough TCP 5-tuples to feed the available queues and
cloud paths, not by one lane per hardware queue.

## Libfabric/EFA Status

The AWS EFA userspace stack installed cleanly with:

```bash
curl -fsSLO https://efa-installer.amazonaws.com/aws-efa-installer-1.48.0.tar.gz
tar -xf aws-efa-installer-1.48.0.tar.gz
cd aws-efa-installer
sudo ./efa_installer.sh -y --skip-kmod --skip-mpi --skip-plugin --no-verify
```

After reboot, `fi_info` reported AWS libfabric `2.4.0` EFA RDM domains named
`rdmap83s0-rdm` and `rdmap166s0-rdm`. The `sockets` provider completed a
cross-host `fi_pingpong` smoke. The `efa` provider exchanged control traffic and
EFA addresses but hung in the data phase, even for one 1-byte RDM message.

Treat libfabric/EFA as not validated for this run. The current production-grade
path is TCP/WAL with many explicit lanes. The next EFA pass should start with a
known-good EFA AMI or AWS support recipe, then require a one-message
`fi_pingpong` success before any WAL transport work.

### Follow-up EFA Smoke: June 5, 2026

Run id `zc-efa-adhoc-c8gn16-20260605T022316Z` launched two `c8gn.16xlarge`
adhoc nodes in `us-east-2c` with one EFA ENI each:

- node0 private `172.31.44.136`, public `52.14.22.210`
- node1 private `172.31.47.102`, public `18.219.57.103`
- AWS libfabric `2.4.0amzn3.0`, EFA device `rdmap71s0`
- security group had self-referenced all-traffic ingress and open egress

`fi_info -p efa -t FI_EP_RDM` succeeded on both nodes and reported
`domain=rdmap71s0-rdm` with both `fabric=efa-direct` and `fabric=efa`.
The sockets provider moved data over the same private path:

```text
fi_pingpong -p sockets -e rdm 172.31.44.136
4k, 1000 iterations: about 107 MB/s, 38 us/xfer
```

EFA provider data did not complete:

- AWS example shape, `fi_pingpong -p efa` / `fi_pingpong -p efa 172.31.44.136`:
  timed out before printing a transfer summary.
- Forced `-f efa -e rdm`: timed out.
- `FI_EFA_ENABLE_SHM_TRANSFER=0`: timed out.
- `-e dgram -d rdmap71s0-dgrm`: timed out.

The debug trace showed EFA endpoints were created and peer EFA addresses were
inserted, but no transfer completion arrived before timeout. Therefore
`libfabric_efa` remains gated: a plan may name the provider and endpoint/CQ/MR
layout, but benchmarks must not mark it representative until a cross-host EFA
data smoke succeeds.

## Transport Abstraction

The WAL transport boundary should be:

```text
WalExtentTransport
  open_lane(path_id, peer, lane_id, local_addr, remote_addr, cpu_hint)
  send_extent(lane_id, descriptor, payload)
  recv_extent(lane_id) -> descriptor + payload
  ack_extent(lane_id, extent_seq, status)
```

Implementations:

- `tcp_mux`: validated here; one lane is a TCP 5-tuple, port-lane worker
  identity is the stable mapping, route checks are mandatory.
- `libfabric_sockets`: functional fallback for OFI API shape only, not a
  high-throughput result.
- `libfabric_efa`: planned; do not enable as a benchmark path until EFA RDM data
  messages complete independently.

Userspace RAID, fanout, mirror, spill, placement, and backpressure sit above
this transport. Block devices are terminal leaf media only after userspace
placement has already been decided.

## Teardown

When done, terminate by run id:

```bash
/home/rob/spot-helper/ec2_perf_spot.py terminate \
  --profile tf \
  --region us-east-2 \
  --run-id zc-pair-adhoc-c8gn48-20260603T230056Z \
  --yes
```
