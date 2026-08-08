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

For `zcnblk-fan --engine wal` mirror/fan tests that send different branches out
different cards from the same fan process, use
`URING_PLAY_ZCNBLK_FAN_LEAF_SOURCE_IPS=card0_ip,card1_ip` instead of relying on
the global source bind. Confirm the fan logs show `leaf_source_ips=` and each
`zcnblk-fan-wal-leaf-stream` line has the expected `source_ip=`.

The planner-backed fan WAL runner can be split across a two-node pair without
hand-editing the topology contract. On the leaf host, start terminal userspace
RAM leaves:

```bash
LANES=64 \
LEAF0_BIND="$LEAF_CARD0_PRIV" \
LEAF1_BIND="$LEAF_CARD1_PRIV" \
OUTDIR="bench-results/${RUN_ID}-leaf" \
scripts/zcnblk-fanwal-plan-bench.sh leaf-node
```

On the client/fan host, connect both fan branches to those leaves and bind each
branch to the matching local card:

```bash
LANES=64 \
LEAF_ADDRS="$LEAF_CARD0_PRIV,$LEAF_CARD1_PRIV" \
FAN_LEAF_SOURCE_IPS="$FAN_CARD0_PRIV,$FAN_CARD1_PRIV" \
OUTDIR="bench-results/${RUN_ID}-fan" \
scripts/zcnblk-fanwal-plan-bench.sh fan-node
```

`fan-node` defaults leaf CPU lists to the `leaf-node` topology domain so strict
CPU checks do not compare local fan CPU numbers with remote leaf CPU numbers.
If both leaves share one host, keep one shared `LEAF_CPU_DOMAIN`; if the leaves
are on different hosts, use separate domains through
`FAN_LEAF_CPU_LISTS='leaf0@leaf-a=...;leaf1@leaf-b=...'`.

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
inserted, but no transfer completion arrived before timeout. For that run,
`libfabric_efa` stayed gated: a plan could name the provider and endpoint/CQ/MR
layout, but benchmarks could not mark it representative until a cross-host EFA
data smoke succeeded.

### Follow-up EFA Validation: June 5, 2026

Run id `zc-efa-direct-adhoc-c8gn16-20260605T104515Z` launched two
`c8gn.16xlarge` adhoc nodes in `us-east-2c` with one EFA ENI each:

- node0 private `172.31.47.142`, public `3.144.173.172`
- node1 private `172.31.42.237`, public `18.222.187.9`
- AWS libfabric `2.4.0amzn3.0`, EFA device `rdmap71s0`
- security group had self-referenced all-traffic ingress and egress

This run validated real EFA provider data movement:

```text
/opt/amazon/efa/bin/fi_pingpong -p efa 172.31.47.142
64B-4K messages: about 6.3-7.5 us/xfer
```

The WAL-over-OFI commands also completed over the private EFA path. Receiver
commands used `bind=auto`; sender commands used the peer private IP. The current
shim works with libfabric `provider=efa` and `fabric=efa`. Forcing
`efa-direct` still returns `fi_getinfo` `ENODATA` in this shim and should stay
gated until the address/fabric hints are corrected.

Representative WAL-over-OFI EFA smoke results:

| mode | shape | sender logical IOPS | receiver logical IOPS | sender ACK latency | context switches |
| --- | --- | ---: | ---: | --- | ---: |
| per-extent ACK | 4 lanes, 1 MiB/lane, 4 KiB records | 50k | 45k | p50 16 us, p95 26 us, p99 72 us, max 9.6 ms | 169 send, 185 recv |
| no ACK | 4 lanes, 8 MiB/lane, 4 KiB records | 207k | 193k | n/a | 155 send, 186 recv |

The same two hosts, same private path, same four CPUs, and TCP WAL extent
sync-ACK mode measured 50k sender logical IOPS, 51k receiver logical IOPS,
p50 70 us, p95 131 us, p99 245 us, and 322 us max ACK latency. The current
conclusion is that EFA busy-poll gives lower ACK latency, but the prototype is
not yet a high-throughput OFI data path because it posts one message at a time
and registers message buffers per transfer on EFA.

### Dual-NIC WAL-over-OFI EFA: June 5, 2026

Run id `zc-dualnic-adhoc-c8gn48-20260605T135900Z` launched two
`c8gn.48xlarge` adhoc nodes with two EFA ENIs each and one public IPv4 on
card0 only. Bulk EFA traffic used the selected provider domain, not the public
interface:

- node0: card0 `172.31.38.204` / `efa_0-rdm`, card1 `172.31.44.250` /
  `efa_1-rdm`
- node1: card0 `172.31.47.214` / `efa_0-rdm`, card1 `172.31.41.234` /
  `efa_1-rdm`
- card0 workers pinned to CPUs `0-31`; card1 workers pinned to CPUs `96-127`
- compact 4 KiB WAL records, payload and ACK inject, CQ busy-poll, and ACK
  window as shown

| shape | sender IOPS | receiver IOPS | sender ACK latency | sender ctx switches | recv ctx switches |
| --- | ---: | ---: | --- | ---: | ---: |
| card0, 32 lanes, window 16 | 4.84M | 4.71M | p50 52 us, p99 140 us | 2.3k | 2.1k |
| card1, 32 lanes, window 16 | 5.09M | 5.08M | p50 52 us, p99 137 us | 2.2k | 2.1k |
| dual card, 32 lanes/card, window 16 | 8.45M aggregate | 8.45M aggregate | p50 48-51 us, p99 235-375 us | 4.5k aggregate | 4.2k aggregate |
| dual card, 64 lanes/card, window 16 | 7.99M aggregate | 10.98M aggregate | p50 55 us, p99 496-497 us | 9.3k aggregate | 8.4k aggregate |
| dual card, 64 lanes/card, window 32 | 8.26M aggregate | 11.55M aggregate | p50 95-107 us, p99 736-868 us | 9.6k aggregate | 8.9k aggregate |

The best client-side ACKed point in this pass was the 32-lane/card,
window-16 topology. Higher lane/window settings let receivers drain faster but
made the sender ACK loop and latency tails worse. Use the 32-lane/card shape as
the first parallel mirror/stripe baseline until the ACK provider message path is
batched further.

### Userspace Mirror Commit over EFA and TCP: June 5, 2026

The next pass used the same dual-NIC `c8gn.48xlarge` pair and the
`compiled.parallel_raid.branch_topology` plan as the benchmark contract. The
sender fanned each logical 4 KiB WAL record to two userspace mirror branches and
only counted a commit after both branch ACKs arrived. This is not block-device
mirroring or striping; the mirror primitive is entirely userspace.

Common topology:

- client/sender: node1, private control/data peer `172.31.38.204`
- mirror branches: node0 branch 0 on CPUs `0-31`, branch 1 on CPUs `96-127`
- lanes/workers: 32 logical lanes, 32 workers, 64 MiB per lane for steady runs
- EFA branch domains: `efa_0-rdm` and `efa_1-rdm`
- saved logs: `bench-results/zcraid-mirror-remote-20260605T1445/`

| transport | ACK window | sender logical IOPS | branch wire Gbit/s | sender ACK latency | sender ctx switches | receiver branch IOPS |
| --- | ---: | ---: | ---: | --- | ---: | --- |
| libfabric EFA RDM | 16 | 2.32M | 152.4 | p50 77 us, p99 734 us, p999 2.0 ms | 5.0k | 2.38M / 2.56M |
| libfabric EFA RDM | 32 | 1.80M | 117.7 | p50 136 us, p99 2.0 ms, p999 20 ms | 4.3k | 1.80M / 1.88M |
| TCP lane sockets | 16 | 1.16M | 76.3 | p50 191 us, p99 494 us, p999 2.0 ms | 120k | 1.17M / 1.19M |
| TCP lane sockets | 64 | 1.48M | 96.8 | p50 330 us, p99 2.0 ms, p999 12 ms | 43k | 1.48M / 1.52M |

The EFA path is now meaningfully faster for this mirror commit benchmark when
throughput is weighted, mostly because it avoids the TCP sender context-switch
rate. These corrected sender numbers exclude endpoint/control setup from the
timed region. The current bottleneck is still sender-side mirror fanout and
ACK joining: one lane worker serially sends the same record to both branches and
then waits for the all-branch commit condition. Window 16 is the current
throughput and latency point for this implementation.

Do not compare this table directly to the 522.6 Gbit/s TCP/WAL bulk transport
result above. That run used large ordered WAL extents and measured data-plane
capacity, while this table used 4 KiB mirror commit records. The mirror tools
now also accept large ordered extents, for example `384K` or `1M`, and count
the logical 4 KiB records inside each extent; use that mode when validating the
dual-card EFA path against the 600 Gbit/s instance envelope.

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
- `libfabric_efa`: validated for EFA RDM data and ACKed WAL messages. Treat a
  plan as representative only when it names the selected domain, pins the
  domain-local CPU slice, and records the lane-to-worker/lane-to-CPU map.

Userspace RAID, fanout, mirror, spill, placement, and backpressure sit above
this transport. Block devices are terminal leaf media only after userspace
placement has already been decided.

## Block-Edge Revalidation Gate: July 11, 2026

Keep bulk WAL transport records separate from block-edge IOPS. The 522.6 Gbit/s
TCP/WAL result is 15.95M normalized 4 KiB records/s through large ordered
extents; it is not `/dev/zcnblk0` random IOPS. The latest saved single-target
cloud block result before the bounded-arena changes was 832k IOPS, with an 874k
hot repeat, for 16 workers at QD128 each. That is aggregate QD2048. Its implied
queue residence is about 2.46-2.34 ms by Little's law, not a measured latency
percentile.

Current local code completed a 20M-operation, four-lane, random 50/50 read/write
control at 2.30M IOPS. It used `/dev/zcnblk0 -> userspace WAL target -> TCP
loopback -> zcmem leaf`, retained writes by shared-slot reference, and evicted
remote-completed cache generations before arena wrap. The run was on a shared
host without huge pages and its soft-exclusive CPU/memory claim was not
honored, so it is a regression gate only. Artifact:
`bench-results/local-zcnblk-wal-target-profile-batched-20260711T150826Z`.

The next `c8gn.48xlarge` single-target run must measure the current binary before
adding mirror or stripe placement:

- QD1, QD2, QD4, QD8, and QD16 per lane, with lane count and aggregate depth
  stated explicitly;
- raw TCP and EFA 4 KiB RTT on the same NIC/CPU topology, plus theoretical
  `aggregate_depth / RTT` IOPS and actual/theoretical efficiency at every
  low-QD point;
- random read, random write, and 50/50 mixed traffic over private data NICs;
- real sampled p50/p95/p99/p999 latency, not queue-depth/IOPS inference;
- client, kernel hctx, target lane, leaf lane, NIC, NUMA, and IRQ/RPS mapping;
- context switches per 1,000 logical I/O for every process and kernel lane;
- dirty pressure events, evictions, peak outstanding slots, and sync drain time;
- a separate large-extent bulk run on both NICs to prove the transport ceiling.

Use latency sample rate 1 for low-QD points and 64 for throughput points after
an unsampled control. Do not call a cloud result representative if hugetlb,
memlock, socket buffers, CPU/hctx pinning, private-NIC routing, or the lane map
is missing. First validate one non-RAID userspace leaf; only then insert the
userspace mirror/fan stage and attribute the delta hop by hop.

## Teardown

When done, terminate by run id:

```bash
/home/rob/spot-helper/ec2_perf_spot.py terminate \
  --profile tf \
  --region us-east-2 \
  --run-id zc-pair-adhoc-c8gn48-20260603T230056Z \
  --yes
```
