# EFA fan-in versus TCP benchmark report

Run ID: `zcutils-efa-fanin-adhoc-c8gn16-20260811T1554Z`

Date: 2026-08-11 UTC

## Scope and topology

The run used two Spot `c8gn.16xlarge` instances in one cluster placement
group in `us-east-2c`.  Bulk traffic used the private addresses
`172.31.35.59` (client) and `172.31.42.23` (leaf).  Each node reserved 8,192
2 MiB huge pages and had unlimited memlock.  Client workers, userspace target
threads, kernel queues, transport owners, leaf workers, and NICs are recorded
in each point's `topology.log`; the high-throughput EFA point used eight block
workers on CPUs 0,8,...,56, their target threads on CPUs 1,9,...,57, kernel
queues on CPUs 2,10,...,58, one userspace placement/transport owner on CPU 4,
and one leaf worker on CPU 3.

The block edge was `/dev/zcnblk0`.  Placement remained in the separate
userspace WAL/RAID stage.  Leaf media was volatile `zcmem`, so the measurements
isolate block edge, queueing, transport, and userspace placement rather than
terminal-media durability.

## Headline results

All values below are the mean of three steady repeats after a discarded warmup.

| Workload | EFA | TCP | Interpretation |
| --- | ---: | ---: | --- |
| PostgreSQL, scale 300, 128 clients, 16 jobs | 97,705.9 TPS, 1.305 ms | 96,145.9 TPS, 1.327 ms | EFA +1.62% TPS; EFA spread 2.58%, TCP spread 6.02% |
| 4 KiB random read, 4 workers x QD64, aggregate 256 | 1,206,772 IOPS | 1,044,455 IOPS | EFA +15.54% |
| 4 KiB random 50/50, 4 workers x QD64, aggregate 256 | 1,196,821 IOPS | 1,114,405 IOPS | EFA +7.40% |
| 4 KiB random ordinary write, scalable topology | 1,041,561 IOPS, 8 workers x QD64, one EFA owner | 1,152,586 IOPS, 4 workers x QD64, four TCP owners | Both demonstrate the million-IOPS regime; topology is intentionally capability-oriented, not transport-isolated |
| Same write pipeline with one transport owner | 1,041,561 IOPS | 290,650 IOPS | EFA fan-in is 3.58x the one-owner TCP result |

The EFA write saturation point repeated at 1,040,767, 1,041,882, and
1,042,035 IOPS (0.12% spread).  Increasing each block worker to QD128 and the
RMA payload queue to 128 reduced the mean to 956,733 IOPS, so QD64/RMA64 is the
better operating point on this pair.

The PostgreSQL EFA source buffer is currently `vmalloc_user` rather than
HugeTLB-backed.  Consequently those EFA numbers are controlled engineering
evidence but are not marked representative by the harness.  TCP and the EFA
leaf passed the topology gates.

## Single-worker low-depth curve

These are ordinary block writes with early local device acknowledgement.  They
are not remote durability completions, so no single network-RTT theoretical
ceiling applies and actual/theoretical network efficiency is `N/A`.  Each row
has one worker, one lane, and aggregate outstanding depth equal to the stated
per-worker QD.

| Per-worker QD / aggregate depth | EFA IOPS | TCP IOPS |
| ---: | ---: | ---: |
| 1 / 1 | 75,985 | 77,276 |
| 2 / 2 | 124,335 | 119,701 |
| 4 / 4 | 180,603 | 164,362 |
| 8 / 8 | 222,906 | 200,105 |
| 16 / 16 | 285,114 | 259,646 |

Preserving 256 progress-poll spins through QD16 fixed a polling-cadence
regression: EFA QD8 increased from 130,736 to 222,906 IOPS and QD16 from
207,579 to 285,114; TCP increased from 127,280 to 200,105 and from 187,549 to
259,646 respectively.

Per-write FUA/`dsync` (one worker, per-worker QD1, one lane, aggregate depth 1)
measured about 15,994 IOPS on EFA and 17,392 on TCP.  These are volatile-leaf
sync drains, not durable-media commits.  The EFA path currently performs an
RMA payload delivery completion and then a metadata doorbell/result exchange;
TCP carries payload and metadata in one application exchange.  A matching
two-stage EFA drain RTT was not separately measured, so a theoretical FUA
ceiling and efficiency would be misleading and are reported as `N/A`.

## Raw transport curve and completion semantics

One worker owned one lane/QP and one CQ.  The QD is both the per-worker and
aggregate outstanding depth.  Reads complete when data is visible at the
initiator CQ.  Writes use `FI_DELIVERY_COMPLETE` and complete at the initiator
CQ after remote visibility; neither means durable media.

| Operation | QD | IOPS | Measured raw RTT | Matching ceiling | Efficiency |
| --- | ---: | ---: | ---: | ---: | ---: |
| Read | 1 | 54,117 | 16.838 us | 59,390 | 91.12% |
| Read | 2 | 99,798 | 18.371 us | 108,866 | 91.67% |
| Read | 4 | 182,851 | 20.064 us | 199,376 | 91.71% |
| Read | 8 | 312,153 | 23.213 us | 344,641 | 90.57% |
| Read | 16 | 404,725 | 33.288 us | 480,876 | 84.14% |
| Read, random | 64 | 407,833 | 116.038 us | 552,726 | 73.78% |
| Remote-visible write | 1 | 64,962 | 15.340 us | 65,188 | 99.65% |
| Remote-visible write | 2 | 128,726 | 15.482 us | 129,185 | 99.64% |
| Remote-visible write | 4 | 253,580 | 15.706 us | 254,687 | 99.56% |
| Remote-visible write | 8 | 488,790 | 16.258 us | 492,057 | 99.34% |
| Remote-visible write | 16 | 877,366 | 17.987 us | 889,563 | 98.63% |
| Remote-visible write, random | 64 | 1,298,675 | 36.958 us | 1,731,682 | 75.00% |

The separate remote-application-ack QD1 measurement was 14.039 us on EFA
(71,230 one-outstanding IOPS ceiling) and 28.476 us on TCP (35,117 IOPS
ceiling).  This semantic is distinct from early local write acknowledgement,
RMA delivery CQ completion, and sync/FUA drain.

## Queue contract and `FI_MORE` experiment

The OFI endpoint now requests provider TX and RX work-request capacity before
`fi_getinfo`, records requested and returned capacities, and makes an
undersized return fatal under strict topology mode.  The EFA provider returned
the requested capacity at every tested point (for example TX 192/RX 64 at raw
write QD64 and TX 129/RX 64 in the block pipeline).

`FI_MORE` is opt-in, bounded by an explicit burst limit, and reports more,
flush, and forced-flush counts.  At raw random QD64 it improved remotely visible
writes from a 1,283,526 IOPS baseline to a 1,406,965 IOPS peak at burst 16
(+9.62%).  The full block pipeline moved in the opposite direction:

| `FI_MORE` setting | 8 workers x QD64 block IOPS |
| --- | ---: |
| Disabled, fresh post-sweep retest | 1,035,782 |
| Enabled, burst 2 | 968,899 |
| Enabled, burst 4 | 979,274 |
| Enabled, burst 8 | 975,715 |
| Enabled, burst 16 | 953,436 |
| Enabled, burst 32 | 959,664 |
| Enabled, burst 64 | 959,668 |

At one worker, per-worker QD1, one lane, aggregate depth 1, the fresh means
were 75,991 IOPS disabled and 75,488 with burst 4.  The feature therefore
remains disabled by default.  It is retained as a provider-specific tuning
knob for future ConnectX testing, where BlueFlame/doorbell behavior differs
from EFA.

## Conclusions and remaining hardware gate

The refactored path demonstrates both north stars: a stable roughly 76k IOPS
single-worker aggregate-QD1 ordinary-write edge and more than one million 4 KiB
random IOPS for EFA reads, mixed I/O, and writes.  EFA's clearest wins are
one-owner fan-in and read/mixed throughput.  TCP remains slightly better at
ordinary-write QD1 and per-write volatile FUA, and its independently scaled
four-owner write path is about 11% above the one-owner EFA write point.

Kernel NVMe/RDMA was not reported on these EFA instances: EFA is a libfabric
device, not a verbs/RDMA-CM device usable by `nvmet-rdma`.  Substituting a RAM,
null, loop, dm, md, or custom block backend would also violate the benchmark
topology contract.  The next valid NVMe/RDMA comparison requires the planned
ConnectX-7 pair and real terminal leaf media.  Local Soft-RoCE can rehearse
verbs semantics and lifecycle, but cannot validate mlx5 BlueFlame, UAR,
PCIe-doorbell, CQ-compression/moderation, MSI-X affinity, or hardware IOPS.

## Cloud teardown

At `2026-08-11T17:42:38Z`, after the requested 1 hour 45 minute checkpoint,
the two exact run instances (`i-00f9819477a7cb017` and
`i-07f4b01792ca1a268`) were terminated. AWS reported both instances in the
`terminated` state at `2026-08-11T17:43:36Z`. Their delete-on-termination root
volumes (`vol-013fa041989e0c63e` and `vol-0018a8efc2a6b86cf`) and primary EFA
interfaces (`eni-0c7ff682602db9641` and `eni-0a9aa4e5bc65dbb94`) no longer
exist. A final run-tag query found no pending, running, stopping, stopped, or
shutting-down instances.

The follow-on local RXE semantic rehearsal and ConnectX-7 hardware gate are in
`bench-results/zcofi-softroce-local-20260811T183809Z/REPORT.md`.
