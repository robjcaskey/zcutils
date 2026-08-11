# ConnectX-7 OFI queue design and benchmark gate

This note turns the queue behavior in the mlx5 kernel driver into a concrete
zcutils topology.  It is based on Linux 7.0.8 source commit
`cfeea921dd5092047179236c1969f2672637acc5` under the locally inspected tree
`/home/rob/dev-workspace/src/linux-7.0.8`, specifically
`drivers/infiniband/hw/mlx5` and
`drivers/net/ethernet/mellanox/mlx5/core`.  The kernel driver is only one layer;
the actual host run must also record the rdma-core and libfabric versions and
the capacities returned by the selected OFI provider.

## What the driver requires

### Receive queues

`qp.c:set_rq_size()` (lines 433-492) bounds `max_recv_wr` by the HCA's
`log_max_qp_sz`.  For a kernel-constructed RQ it rounds the data-segment WQE
size to a power of two, rounds the requested WR count to a power of two, and
derives `wqe_cnt`, `wqe_shift`, `max_gs`, and `max_post` from that allocation.
For a userspace QP it accepts the userspace WQE count/shift and exposes the
resulting maximum posts.  Queue depth is therefore a resource-allocation
contract, not just an application-side ring length.

zcutils now sets `fi_info.tx_attr.size` and `fi_info.rx_attr.size` before
`fi_getinfo`.  Every endpoint profile reports requested and returned TX/RX
capacity.  Strict mode fails before a representative benchmark if the provider
returns less capacity than requested.

### Send queues and WQEBBs

`qp.c:sq_overhead()` and `calc_send_wqe()` (lines 494-566) account for the RC
control and remote-address segments, SGE space, inline data, and integrity
segments.  The resulting WQE is aligned to `MLX5_SEND_WQE_BB`, a 64-byte
WQEBB.  `calc_sq_size()` (lines 591-633) rounds
`max_send_wr * wqe_size` to a power of two, expresses the allocation in
64-byte WQEBBs, checks `log_max_qp_sz`, and returns the actual `max_send_wr`.
The userspace path rejects a non-power-of-two `sq_wqe_count` at lines 650-653
and lays RQ then SQ memory out in one userspace buffer at lines 665-672.

Consequences:

- Do not assume requested QD equals allocated WQEBBs.  Record the provider's
  returned capacity and the measured maximum in flight.
- Keep the hot RMA shape to one SGE and one registered 4 KiB extent wherever
  possible.  Extra SGEs enlarge WQEs and can reduce the number of posts backed
  by a fixed SQ allocation.
- Sweep QD1/2/4/8/16 on one QP, then QD32/64/128 for saturation.  Do not jump
  directly to a giant SQ: power-of-two rounding and cache footprint can make a
  deeper queue slower.

### Pinned queue memory and page quantization

`qp.c:_create_user_qp()` (lines 941-1053) chooses a UAR/BFREG, places the RQ
before the 64-byte-WQEBB SQ, pins the userspace queue buffer with
`ib_umem_get()`, chooses the best quantized page size/offset, fills the PAS,
and maps the userspace doorbell record.  `cq.c:create_cq_user()` (lines
718-863) similarly pins the CQ buffer, quantizes page layout, maps its
doorbell, and associates the CQ with a UAR.

The application implication is stable registered arenas, not registration per
I/O.  zcutils treats a hot MR miss as a strict-topology failure, reports MR
registrations/hits, and computes memlock and HugeTLB headroom before a run.
Soft-RoCE can check lifetime correctness but not mlx5's DMA/PAS/page behavior.

### UAR and BlueFlame ownership

`qp.c` lines 688-695 reserve BFREG zero for ordinary 64-bit doorbells; the
comment explicitly says it is not used as BlueFlame and therefore does not need
the odd/even BlueFlame lock.  The allocation code at lines 697-817 divides
BFREGs into high- and medium-priority classes, falling back to the shared
ordinary-doorbell register.  `_create_user_qp()` maps the selected BFREG to a
UAR and returns its index to userspace.
`drivers/infiniband/hw/mlx5/main.c` lines 1881-1972 and 2184-2270 calculate
static/dynamic BFREG counts, account for 4 KiB UAR support, allocate UAR pages,
and advertise dynamic UAR capability. Separately,
`drivers/net/ethernet/mellanox/mlx5/core/main.c` line 2266 registers the
ConnectX-7 PCI device ID (`0x1021`) on this same mlx5 core path.

The resource allocation above motivates the central zcutils ownership rule for
ConnectX-7: one hot posting/polling thread should own each QP/CQ pair. This is
an application architecture inference, not a kernel API requirement. Sharing
one QP between workers introduces
software serialization and can also collapse independent doorbell/UAR
opportunities.  Conversely, creating many QPs on one thread just makes that
thread walk multiple CQs.  The hardware matrix must therefore test 1, 2, 4,
and 8 QP/CQ pairs with the same number of pinned owners, plus an explicitly
labelled fan-in control.

zcutils raw RMA already creates one endpoint/QP/CQ per lane.  Its summaries now
state `endpoint_count`, `cq_owner_count`, and `endpoint_to_owner_map`.  For the
ConnectX saturation points use `lanes == workers` so the map is one-to-one.  In
the block pipeline use userspace placement mode with one stable owner/endpoint
per selected lane.  `/dev/zcnblk0` remains only the block edge and never owns
placement, mirroring, striping, locality, or backpressure.

### CQ layout, compression, moderation, and IRQs

`cq.c:create_cq_user()` accepts only 64- or 128-byte CQEs (lines 746-750), pins
the CQ and maps its doorbell, and conditionally enables CQE compression only
when the HCA advertises the matching 64/128-byte capability (lines 808-833).
CQE padding has a separate capability check (lines 835-846).
`mlx5_ib_modify_cq()` at lines 1155-1172 programs the CQ period/count when
moderation is supported.

`pci_irq.c:mlx5_irq_alloc()` (lines 255-320) obtains or dynamically allocates
MSI-X vectors, names completion vectors `mlx5_compN`, requests the IRQ, and
applies the affinity mask/hint.  Consequently a hardware result is incomplete
without:

- CQ size, CQE size/compression mode, and any moderation period/count;
- the exact `mlx5_compN` IRQ numbers and their configured/effective CPU masks;
- CQ-owner CPUs, IRQ CPUs, HCA PCI BDF, and NUMA node;
- evidence that irqbalance will not silently rewrite the declared mapping.

The raw busy-poll path uses `URING_PLAY_OFI_CQ_SLEEP_NS=0`; that avoids adding
an application sleep to QD1, but does not excuse missing IRQ/NUMA evidence.
CQ batch statistics (`polls`, nonempty polls, CQEs, and average batch) are
reported for every endpoint.

## Resulting zcutils topology

For one HCA/rail, the preferred saturation shape is:

```text
/dev/zcnblk0 block edge
        |
        v
separate userspace placement stage
        |
        +-- lane 0 -> owner CPU A -> OFI endpoint/QP 0 -> CQ 0 -> leaf lane 0
        +-- lane 1 -> owner CPU B -> OFI endpoint/QP 1 -> CQ 1 -> leaf lane 1
        +-- lane 2 -> owner CPU C -> OFI endpoint/QP 2 -> CQ 2 -> leaf lane 2
        `-- lane 3 -> owner CPU D -> OFI endpoint/QP 3 -> CQ 3 -> leaf lane 3
```

Each owner is the sole poster and poller for its QP/CQ.  Registered payload
leases stay immutable until the matching CQ completion.  Userspace placement
selects the lane before the leaf writer; block devices, if used, are terminal
leaf media only.

For low latency, use exactly one lane, one worker, one QP/CQ, and aggregate QD
equal to per-worker QD.  Report the measured raw RTT, the ceiling matching that
completion semantic, and actual/ceiling efficiency.  Keep these semantics
separate:

- RMA read: initiator CQ with data visible;
- RMA write with delivery completion: initiator CQ after remote visibility;
- remote application ACK: a distinct request/result round trip;
- ordinary block write: possibly early local acknowledgement;
- sync/FUA: remote drain and, with real media, durable completion.

## Doorbell batching

`FI_MORE` is now an opt-in, bounded hint.  zcutils inserts a non-`FI_MORE`
post at the configured burst limit, forces the next accepted post to be a
flush if provider backpressure rejects an immediate follow-up, and reports
more/flush/forced-flush/backpressure counts.
On EFA, raw QD64 improved about 9.6% at burst 16, but every tested full-block
burst regressed; the feature therefore remains off by default.  ConnectX-7 has
different UAR/BlueFlame behavior, so sweep off, 2, 4, 8, 16, 32, and 64 on raw
RMA first.  Promote a value to the block or PostgreSQL path only if repeated
end-to-end results improve without hurting the one-worker aggregate-QD1 point.

Selective completion is a separate experiment.  Never suppress the
completions that carry buffer-lifetime, delivery, read-data, sync, or error
semantics merely to reduce CQ traffic.

## Required matrices on the ConnectX-7 pair

Run in this order:

1. Preflight both hosts with `scripts/zcofi-rdma-topology-preflight.sh` under
   strict/fatal mode.  Save the output with the benchmark artifacts.
2. One endpoint/QP/CQ and one owner: read and remotely visible write at
   per-worker/aggregate QD1, 2, 4, 8, and 16, at least three repeats each.
3. Random saturation at QD32, 64, and 128, first with one QP and then with
   2, 4, and 8 one-owner QPs.  Record both per-worker QD and aggregate depth.
4. Sweep `FI_MORE` only after the off baseline is stable.
5. Run the full `/dev/zcnblk0` read, ordinary write, mixed, remote-ACK, and
   FUA/drain curves using the winning raw topology.  Recheck TCP between major
   topology changes.
6. Run PostgreSQL on identical topology/media.  Do not call a result durable
   unless the terminal leaf actually provides and acknowledges durability.
7. Test NVMe/TCP and NVMe/RDMA only with real terminal leaf media.  EFA cannot
   stand in for `nvmet-rdma`, and RAM/null/loop/dm/md/custom block substitutes
   are not valid mirror or stripe primitives.

For the 2/4/8-QP points, keep total work and registered bytes explicit.  Report
both fixed per-QP QD (aggregate depth grows) and fixed aggregate depth (per-QP
QD shrinks); they answer different questions.

The strict mlx5 preflight requires more than a device name. Set and verify:

- `ZCOFI_RDMA_DEVICE`, `ZCOFI_RDMA_NETDEV`, `ZCOFI_RDMA_PORT`,
  `ZCOFI_RMA_MATRIX_PROVIDER`, and `URING_PLAY_OFI_DOMAIN` for the exact path;
- `FI_VERBS_DEVICE_NAME`, `FI_VERBS_IFACE`, and `FI_VERBS_GID_IDX` for the
  selected port/GID (the preflight rejects an inactive port, zero GID, or a
  GID-to-netdev mismatch);
- ordered `URING_PLAY_PIN_CPU_LIST` and `ZCOFI_RDMA_IRQ_CPU_LIST` mappings;
- `ZCOFI_RDMA_CQE_SIZE`, `ZCOFI_RDMA_CQE_COMPRESSION`,
  `ZCOFI_RDMA_CQ_MODERATION_PERIOD`, and
  `ZCOFI_RDMA_CQ_MODERATION_COUNT`, followed by
  `ZCOFI_RDMA_CQ_CONFIGURATION_VERIFIED=1` only after they were inspected;
- `ZCOFI_RDMA_IRQ_AFFINITY_CONFIRMED=1` only after configured and effective
  `mlx5_compN` masks agree. Completion-vector CPUs default to disjoint from CQ
  owners; an intentional co-location control must say so explicitly.

The gate also rejects duplicate/offline/disallowed owners, owner/HCA NUMA
mismatches, physical-core sharing between owners, unpinned queue owners,
insufficient memlock or hugetlb headroom,
undersized returned provider queues, and an IRQ CPU declaration that does not
match the observed single-CPU completion-vector affinities. These checks are
derived from the driver's WQEBB, UAR, CQ and IRQ allocation paths; they do not
claim that a software-visible provider capacity is the exact physical WQEBB
count.

For full block runs, also set `ZCOFI_RDMA_BLOCK_MODE=1`, one explicit hctx CPU
per lane, `ZCOFI_RDMA_BLOCK_ENGINE=uring-fixed`, completion and wait batches,
and an explicit CQ spin/adaptive-spin/hot-poll policy. Missing block fast-path
knobs are fatal in strict mode before representative numbers.

### Provider and connection path on hardware

Use `verbs;ofi_rxm` for the first ConnectX-7 RDM qualification. Its RDM queues,
underlying verbs MSG queues, and application RMA rings are separate resources;
sweep them independently and record requested plus returned sizes. The v4 RMA
wire contract exchanges and validates profiles, including returned fabric
identity, then performs a synchronous 16-byte client-send/server-receive
warmup per lane before MR metadata or timing. Endpoint/control/warmup/MR setup
is admitted in ascending lane order, avoiding a connection-manager storm. A
failure-aware start gate holds every client owner until all owners have stable
endpoints and MRs; a later setup failure wakes the earlier waiters rather than
deadlocking them. This makes lazy RC connection establishment a setup cost,
catches a wrong GID/fabric early, and prevents staggered multi-QP timing.

If RxM is measurably the limiting layer on real mlx5 hardware, run a controlled
direct `FI_EP_MSG`/RC prototype before adopting a different architecture. Do
not infer that need from RXE's utility-provider behavior, and do not replace
the userspace placement stage with kernel block-device striping or mirroring.

## What Soft-RoCE proves

The local RXE run is a semantic rehearsal.  It can validate provider/domain
selection, endpoint creation/destruction, RC/RDM negotiation, MR lifetime,
RMA ordering, bounded `FI_MORE`, CQ dispatch, error handling, lane/QP ownership
reporting, and the QD matrix harness.  It is a shared-system measurement, so it
must be repeated and its spread reported.

It cannot validate ConnectX-7 WQEBB cache behavior, UAR/BFREG allocation,
BlueFlame versus ordinary doorbells, PCIe write-combining, DMA page
quantization, CQE compression/padding, CQ moderation, MSI-X steering, HCA NUMA
locality, link bandwidth, or hardware IOPS.  RXE numbers are never a performance
forecast for mlx5.

The local stack exposes an important provider distinction. Native
`ibv_rc_pingpong` over the two RXE devices completes, proving RC addressing and
GID selection. `verbs;ofi_rxm` advertises RDM, but on this RXE/libfabric build
it cannot complete the bounded runtime probe even after reducing both the
underlying MSG and utility-provider queues and selecting GID index 1. That is a
local utility-provider limitation, not evidence about ConnectX-7.

`verbs;ofi_rxd` can exercise the v4 wire contract and semantic RMA checks over
RXE UD, but it is emulated RMA. Repeated delivery-complete writes are stable
through per-worker QD4; QD8 stress can stall or fault, and a direct RXE
perftest QD8 control can also stall. The local harness therefore qualifies
RXD writes only through QD4, labels QD8+ blocked, retains the full read curve,
and never promotes either number to a hardware forecast. The ConnectX-7 run
must redo the full QD1/2/4/8/16 and saturation matrices on
`verbs;ofi_rxm` with real mlx5 queues.

The completed 2026-08-11 local artifact is
`bench-results/zcofi-softroce-local-20260811T183809Z`. Native 4 KiB RC
ping-pong measured 13.690-17.530 us (15.077 us mean, 25.47% spread). One-owner
RXD read QD1 measured 66,563 IOPS at 14.157 us completion RTT and 94.18% of its
matching ceiling; one-owner delivery-complete write QD1 measured 72,462 IOPS
at 13.773 us and 99.78%. Two owners at per-worker QD1 (aggregate depth 2)
measured 112,644 read IOPS and 123,234 delivery-complete write IOPS. These are
shared-host provider diagnostics, not mlx5 targets.

The ordered setup proved that this local RXD path supports two simultaneous
endpoints but returns `FI_ENOMEM` on the fourth endpoint's tiny setup SEND;
three repeated four-endpoint and three repeated eight-endpoint probes were all
blocked. Increasing each advertised provider/control queue to 64 did not
change that result. The failure-aware start gate unwound every probe and the
wrapper removed both RXE devices and its namespace. Real ConnectX hardware
must requalify 4/8 QPs; do not inherit RXD's endpoint cap.
