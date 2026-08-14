# Local Soft-RoCE and ConnectX-7 readiness report

Date: 2026-08-11 UTC

Artifact: `zcofi-softroce-local-20260811T183809Z`

## Classification

This is a shared-host, nonrepresentative semantic rehearsal. The host ran
Linux `7.2.0-rc1-io-slots-nvme`, libfabric 2.1.0, and RXE over a private veth
pair. It does not forecast mlx5/ConnectX-7 IOPS. The preflight correctly
reported no HugeTLB pages and only 8 MiB memlock, so every local number remains
diagnostic even where all semantic validations passed.

Native `ibv_rc_pingpong` validates RC addressing and GID index 1. The zcutils
RMA curves use `verbs;ofi_rxd`, which emulates RMA over RXE UD. The intended
ConnectX path, `verbs;ofi_rxm`, advertises RDM locally but its bounded runtime
probe failed at the first setup SEND with `FI_ENOMEM`; this is classified as a
local RXE/libfabric utility-provider limitation.

## Native RC reachability

The 4 KiB send/receive ping-pong repeated at 13.690, 17.530, and 14.010 us:
15.077 us mean with 25.47% spread. The spread reinforces the shared-system
classification.

## One owner, one endpoint/CQ

QD is both per-worker depth and aggregate outstanding depth. Reads complete at
the initiator CQ with data visible. Writes use delivery-CQ remote-visibility
semantics; neither operation implies terminal-media durability.

| Operation | Per-worker / aggregate QD | Mean IOPS | Spread | Completion RTT | Matching ceiling | Efficiency |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Read | 1 / 1 | 66,563 | 5.70% | 14.157 us | 70,681 | 94.18% |
| Read | 2 / 2 | 90,018 | 15.10% | 21.436 us | 93,723 | 96.07% |
| Read | 4 / 4 | 86,955 | 15.74% | 45.306 us | 88,690 | 98.05% |
| Read | 8 / 8 | 49,265 | 5.38% | 161.385 us | 49,605 | 99.32% |
| Read | 16 / 16 | 7,973 | 0.82% | 2,002.501 us | 7,990 | 99.79% |
| Read, random | 32 / 32 | 7,962 | 1.33% | 4,002.619 us | 7,995 | 99.58% |
| Read, random | 64 / 64 | 7,799 | 3.60% | 8,146.591 us | 7,858 | 99.26% |
| Remote-visible write | 1 / 1 | 72,462 | 3.28% | 13.773 us | 72,623 | 99.78% |
| Remote-visible write | 2 / 2 | 111,308 | 5.15% | 17.934 us | 111,570 | 99.77% |
| Remote-visible write | 4 / 4 | 113,995 | 7.43% | 35.006 us | 114,385 | 99.66% |

RXD reads collapse rather than saturate above QD4, which is a software-provider
behavior rather than a queue target. Repeated RXD delivery-complete write QD8
stress can stall or fault, and a direct RXE perftest QD8 control also stalled.
The harness therefore qualifies writes only through QD4 and records QD8+ as
blocked.

## Two owners and higher endpoint counts

Every endpoint has one posting/polling owner and one CQ. Endpoint/control/
warmup/MR setup is admitted in ascending lane order outside timing. A
failure-aware start gate holds traffic until all client owners are ready.

| Operation | Owners | Per-worker QD | Aggregate depth | Mean IOPS | Spread |
| --- | ---: | ---: | ---: | ---: | ---: |
| Read | 2 | 1 | 2 | 112,644 | 6.86% |
| Read | 2 | 16 | 32 | 16,021 | 1.37% |
| Read, random | 2 | 32 | 64 | 16,137 | 0.76% |
| Remote-visible write | 2 | 1 | 2 | 123,234 | 7.88% |
| Remote-visible write | 2 | 4 | 8 | 185,196 | 12.16% |

All three four-endpoint and all three eight-endpoint QD1 probes were blocked.
Ordered setup localized the failure to the fourth endpoint's 16-byte setup
SEND. Increasing advertised application/provider queues to 64 did not help,
so this is an RXD endpoint-capacity boundary rather than a connection storm or
data-QD result. Every failure released already-ready workers, terminated only
the exact target PID, and left no RXE namespace/device behind.

## ConnectX-7 gate

The hardware run must use `verbs;ofi_rxm`, explicit device/interface/GID,
stable registered arenas, and one owner per QP/CQ. Before representative
numbers, the strict preflight requires and verifies owner/HCA NUMA locality,
memlock and HugeTLB headroom, returned provider queue capacity, 64/128-byte CQE
mode, CQE compression, CQ moderation period/count, `mlx5_compN` configured and
effective affinity, irqbalance control, and disjoint owner/IRQ CPUs unless an
intentional co-location control is declared.

Run full one-QP QD1/2/4/8/16 read and remotely visible write curves, then
QD32/64/128 random saturation with 1/2/4/8 QPs under both fixed-per-QP and
fixed-aggregate depth. Sweep `FI_MORE` only after the off baseline, and retain
it only if raw, block, and PostgreSQL results improve without hurting the
single-owner aggregate-QD1 point. Compare TCP between topology changes. Real
NVMe/TCP versus NVMe/RDMA requires real terminal leaf media; EFA and local RXE
cannot stand in for `nvmet-rdma`.

The mlx5 rationale and exact Linux 7.0.8 source paths/functions are documented
in `docs/connectx7-ofi-queue-design.md`. The preceding EFA/TCP block and
PostgreSQL results are in
`bench-results/zcutils-efa-fanin-adhoc-c8gn16-20260811T1554Z/REPORT.md`.
