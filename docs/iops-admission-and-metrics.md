# IOPS admission, guarantees, and metrics

The system has two deliberately separate layers.

The resource layer describes measured bottlenecks and the paths that consume
them. A path may include CPU ownership, a NUMA memory controller, PCIe lane
sets and switch uplinks, NIC queues and ports, storage queues, devices, and
network links. It is a graph rather than a storage tree: unrelated logical
volumes can collide at a shared PCIe uplink or memory controller. Capacity is a
measured envelope with a safety margin, never only a vendor nameplate value.

The policy layer exposes provisioned IOPS, a burst ceiling and duration,
foreground retention, and maintenance deadlines. It compiles policy into a
generation of per-lane grants. The userspace stage after `/dev/zcnblk0` owns
admission; the kernel client does not make placement or scheduling decisions.

## Capacity proof

For each resource, accepting a policy requires:

```text
sum(foreground guarantees * path cost)
+ sum(simultaneous maintenance reservations * path cost)
<= measured capacity * (1 - safety margin)
```

A snapshot reservation is derived from bytes, amplification, average I/O size,
and its deadline. A migration additionally needs modeled base-copy, dirty-tail,
network, read, and destination-write costs. Alternative operations may share a
scenario group, which reserves their maximum; operations in different groups
are assumed able to overlap and are summed. This makes assumptions explicit
instead of silently overprovisioning capacity needed to meet a snapshot or
migration deadline.

Provisioned foreground IOPS is a hard reservation. Burst capacity is
opportunistic and may use remaining capacity. An unprovisioned workload has no
admission guarantee; a runtime controller may target 90% of its recent clean
baseline, but must yield if that conflicts with provisioned or system work.
Changing policy builds and validates a complete new generation before mailbox
publication. Raft/change-log replication belongs on that publication path, not
on individual I/O admission.

## Hot path

Each lane owns a token state and receives a precomputed mailbox grant. It
accounts and admits a homogeneous descriptor or completion batch with ordinary
local arithmetic. There is no shared counter, mutex, allocation, resource-graph
walk, Raft operation, or subscriber notification per I/O. A lane reads its
cache-line-isolated mailbox only at a grant boundary. Live policy changes are
fenced by generation. Two lane-local buckets enforce both sustained and peak
rates: for example, a 1M sustained allocation can accumulate bounded credit and
run at a separately enforced 5M peak for the configured burst duration.

Device and lane totals remain exact at high rate: the lane adds the number of
operations retired by each CQ/descriptor batch. It publishes cumulative totals
to a cache-line-isolated seqlock every 50-250 ms. A collector computes deltas
and fans metric points out to subscribers away from the I/O threads.

Low-rate session metrics can be exact. Homogeneous high-rate session batches
also remain exact. Only high-cardinality attribution inside mixed batches needs
sampling; those points must carry sample probability and a 95% relative-error
estimate. The public metric schema always states whether a point is exact.

## Latency stream

Latency is stratified by lane and completion semantics. Local reads, remote
reads, early-local writes, remotely acknowledged writes, durable writes,
sync/FUA drains, snapshots, and migrations are never mixed into one percentile.
At low rates a stratum can use probability 1. At high rates it uses independent
geometric sampling with a configured inclusion probability and seed per lane.
An unsampled operation executes one lane-local decrement and branch; it performs
no clock read, random-number generation, histogram update, atomic operation,
allocation, or lock acquisition. Random gap generation and both timestamps
occur only for a selected operation.

Selected latencies enter a preallocated, three-significant-figure HDR histogram.
The interval stream contains the recorded bins, inclusion probability, estimated
population, clock source/resolution/read overhead, overflow count, and p50/p90/
p99/p99.9/p99.99. Quantile intervals use a distribution-free 95% DKW rank bound
over the raw random sample. This bound describes sampling uncertainty; HDR
quantization and clock error remain separately reported measurement errors.

For an open-loop workload with a known intended inter-arrival interval, the
recorder also emits a coordinated-omission-corrected histogram. Raw and corrected
distributions remain separate because synthetic correction entries are not
independent samples and therefore do not inherit the raw DKW confidence bound.
Closed-loop latency must be labeled as such and cannot claim offered-load tail
latency merely because the sampled histogram is precise.

Histogram materialization and subscriber fanout occur only after a recorder is
rotated out at a lane/grant boundary. Subscriber queues are bounded; a stalled
consumer is disconnected rather than propagating memory growth or backpressure
to the I/O path.

## Remaining integration

`iops_policy` implements the capacity proof, live lane mailboxes, exact
batch-level limiter, cache-line-isolated metric publication, and subscription
fanout. Next integrations are:

1. Discover and benchmark PCIe/NUMA/NIC/device resource envelopes and persist
   their provenance and confidence interval.
2. Compile workload paths from topology placement and split grants across
   lane/worker ownership without a shared atomic.
3. Connect migration and snapshot chunk admission to system-class grants.
4. Add adaptive, statistically annotated per-session attribution for batches
containing several sessions.
5. Publish accepted policy generations through the existing atomic change log.

## Arithmetic diagnostic

The ignored release-mode unit diagnostic is intentionally not a block-device,
transport, or durability benchmark. Pinned to CPU 0 on a shared host whose
allowed CPU set was 0-31, three runs performed 400.8M-416.8M admission batches/s
(2.40-2.49 ns per batch) with 256 logical operations charged per call. This
only establishes that the standalone lane arithmetic is far below the batch
frequency required for 12M IOPS. End-to-end overhead remains unproven until the
primitive is integrated into a topology-explicit mux/WAL benchmark and compared
as a repeated spread.

## QEMU migration and snapshot gate

`scripts/zciops-migration-qemu.sh` creates three KVM guests: a controller and
two userspace terminal-leaf writers. Placement and cutover stay in the
controller's userspace route. Each leaf writes 4 KiB operations with `O_DIRECT`
to its own terminal virtio block device; QEMU enforces independent media IOPS
ceilings. No block device is used as a mirror or stripe primitive.

The scenario measures fast and slow terminal media independently, live-copies
a versioned memory-volume image while forwarding foreground mutations to both
userspace leaves, then changes the userspace foreground rate. It starts a
competing terminal snapshot read, checks every exact 500 ms foreground interval
against the provisioned floor, waits for snapshot completion, verifies recovery
to the burst rate, migrates back, and validates every destination block's
sequence and value. The harness fails unless the media-rate step and snapshot
impact are visible and no snapshot interval falls below the provision.
