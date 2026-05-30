# zcutils FAQ

## What are lanes? Are they PCI lanes?

No. In zcutils, a lane is a software data-plane stripe, not a PCIe lane.

A lane is the stable identity for one parallel flow of work through the system.
Depending on the transport or storage path, that identity can map to a TCP
destination port, TCP 5-tuple, RDMA queue pair or endpoint, io_uring worker,
WAL shard, block target shard, or CPU/NUMA placement hint.

PCIe lanes are physical hardware links. zcutils lanes are logical scheduling
and ordering lanes. A zcutils lane may eventually be placed on hardware that is
behind a PCIe device, such as a NIC or NVMe controller, but the lane itself is
not a PCIe concept.

## Why do lanes matter?

Lanes let the hot path avoid rediscovering placement for every chunk. If lane 7
is assigned to a TCP port, RX queue, worker, WAL region, and block shard, then
each descriptor carrying `lane_id = 7` should keep that placement unless a
stage explicitly remaps it.

That makes the system easier to reason about:

- per-lane ordering can be preserved without globally ordering all traffic
- queue, CPU, NUMA, and shard affinity can remain stable
- WAL offsets can be partitioned by lane or shard
- acknowledgements and completions can be accounted against the same lane that
  issued the work
- a slow lane can be diagnosed without blaming unrelated lanes

## How should descriptors keep lanes aligned?

The descriptor should carry the lane identity and placement hints all the way
through the pipeline:

- `lane_id`: stable logical lane for ordering and ownership
- `lane_count`: expected number of lanes in the current flow
- `queue_id`: receive/send queue or local queue owner when known
- `preferred_worker`: worker that should process the lane
- `preferred_cpu` and `numa_node`: locality hints, not correctness rules
- storage placement: WAL shard, block shard, and byte-range region when known

The default rule is preserve-lane: a stage should pass `lane_id` through
unchanged when it splits, joins, maps, encrypts, tees, sends, receives, or
writes descriptors. If a stage must rebalance, it should write a new descriptor
mapping rather than silently changing the meaning of the old lane.

## What does lane alignment mean for WAL traffic?

For WAL traffic, lane alignment means the same logical lane should choose the
same WAL writer, shard, offset region, and ack path. A useful shape is:

```text
lane_id -> mux port/flow -> RX queue -> worker -> WAL shard -> byte range -> ack lane
```

With that shape, a WAL writer can coalesce many small logical records from the
same lane into a larger append extent without losing per-lane ordering. The
acknowledgement can report the durable byte range and the lane/shard it belongs
to, rather than making the rest of the system infer placement after the fact.

## Should every stage preserve global ordering?

No. Global ordering is expensive and usually unnecessary. The descriptor model
prefers per-lane ordering plus explicit barriers or sequence numbers for the
few operations that require a cross-lane decision.

For example, Raft may need a global log index decision, but the network, WAL
append, checksum, encryption, and block write stages should avoid forcing all
lanes through a single global reorder point.

## What point-in-time snapshot primitives would fit zcutils?

Keep them as small descriptor/WAL metadata primitives. The current `zcsnap`
command is only a byte-compatible cut/manifest marker, but it reserves the
right shape for descriptor-native snapshots:

- snapshot checkpoint: a named cut with `snapshot_id`, `wal_epoch`, and
  per-lane durable sequence/index watermarks.
- extent pin: a lease/fence that keeps referenced WAL extents or descriptor
  pool generations from being recycled until the snapshot is released.
- snapshot manifest: a descriptor-list-like record of the extents, lanes,
  shards, byte ranges, and generation tokens that make up the cut.
- restore cursor: a reader position that replays the manifest, then resumes WAL
  replay after the recorded per-lane watermarks.

Those primitives describe a logical descriptor/WAL cut. They should not freeze
block devices, grow volume-clone semantics, manage RAID membership, or turn
`zcbrd`, `zcstripe`, `zcnblk`, or `zcraid-*` into a snapshot subsystem. If
snapshots come up in the block-device or RAID context, the answer is: not here.

## When is it OK to remap lanes?

Remapping is OK when it is explicit and recorded. Examples include:

- changing lane count between two machines with different queue counts
- moving a hot lane away from a saturated CPU or shard
- compacting many input lanes into fewer output WAL shards
- expanding one input lane into several output storage shards

The remapping stage should emit descriptors that preserve the original lane as
metadata and add the new local placement. That keeps observability and
completion accounting intact.

## How does this relate to zcnblk and zcbrd?

`zcnblk` uses the same idea: the sender maps mux lanes to block shards, and the
target uses the frame shard plus offset to write or read the selected target.
For future descriptor-native paths, the frame should not need to rediscover
that mapping. It should receive descriptors whose lane, shard, queue, and byte
range are already explicit.

`zcbrd` advertises descriptor support through configfs, but its current block
path still pays normal block request overhead. Descriptor-native WAL or block
targets should use the lane and shard metadata directly so we can compare the
cost of old block semantics against a purpose-built data path.

## What is the proton pack principle?

Don't cross the ~~streams~~ lanes.
