# Zero-Copy Descriptor Model

`zcutils` treats a zero-copy record as metadata plus one or more leased byte
ranges. The hot path should reorder, split, join, and fan out descriptors
without normalizing payloads into new contiguous buffers.

The first command implementation still includes byte-stream compatibility tools
such as `zccat`, `zcgrep`, `zctee`, `zcsink`, and `zcstat`. Those are useful for
debugging and smoke tests, but a real cross-process zero-copy path needs the
descriptor protocol below over Unix sockets, shared memory, and fd passing.

Every descriptor stream starts with a fixed envelope header:

```rust
#[repr(C)]
pub struct ZcStreamHeader {
    pub magic: [u8; 8],       // b"ZCSTRM\0\1"
    pub protocol_version: u16,
    pub min_reader_version: u16,
    pub flags: u32,
    pub header_len: u32,
}
```

Version rules:

- `protocol_version`: framing/control protocol version.
- `min_reader_version`: oldest reader that can safely consume this stream.
- incompatible changes must bump `protocol_version`.
- compatible additions should use length-delimited records and feature flags.
- readers must reject unknown required flags.

## Descriptor Streams

A `ZcDescriptorStream` is the Unix-like primitive. It is an ordered control
stream that carries pool attachments, credits, descriptor records, releases,
checkpoints, and termination frames. Payload bytes stay in owned pools such as
ZCRX areas, registered buffers, mapped files, shared memory, or WAL regions.

A normal shell pipe can carry byte-compatible fallback data, but it is not
enough for true zero-copy because it cannot pass pool fds or enforce release
accounting by itself. Descriptor-native pipelines can be started by `zcflow`
or an equivalent supervisor that wires Unix sockets, fd passing, credits, and
release channels before producers begin emitting descriptors.

`zcflow` should resolve stages through the normal `PATH`. A descriptor-native
utility does not need to be compiled into this repository; it only needs to
speak the descriptor protocol on the inherited control fd or deliberately opt
into byte-compatible stdin/stdout mode. The expected launch contract is an
environment such as `ZC_DESCRIPTOR_FD`, `ZC_PROTOCOL_VERSION`, `ZC_STAGE_INDEX`,
and `ZC_STAGE_COUNT`, plus normal argv.

Normal shell composition should also work without an explicit manager command:

```bash
zcdemux ... | zcmap --preserve-lanes | zcmux --peer-addr C ...
```

In that form, the upstream producer creates or joins a session and writes the
session identity into the descriptor stream header. Each downstream `zc*`
command reads the header, connects to the same manager/session, and propagates a
fresh header for the next stage. Explicit `zcflow` is still useful when the user
wants one process to supervise the whole graph directly.

Required stream frame families:

- `STREAM_START`: protocol version, flags, topology, and limits.
- `POOL_ATTACH`: attach a shared memory, mapped file, ZCRX, registered-buffer,
  or device-backed pool.
- `CREDIT`: grant bounded descriptor/byte capacity to an upstream producer.
- `DESCRIPTOR`: one `ZcRecordDesc` plus its scatter/gather slices.
- `COLLECTION_START` and `COLLECTION_END`: bounded group with shared ordering
  and failure semantics.
- `RELEASE`: return a `release_token` to the owning pool authority.
- `CHECKPOINT`: durable or replayable progress marker.
- `EOF`: clean end of stream.
- `ABORT`: error end; outstanding leases are revoked by the pool authority.

## Collections and Lists

A `ZcDescriptorCollection` is a bounded group of descriptors with shared
lifetime, ordering, topology, and failure semantics. It is useful for batching a
Raft append slice, WAL commit group, scatter/gather file extent, or fanout unit.
Collections are stream frames, not separate ownership domains: every descriptor
inside still releases back to the original pool authority.

A `ZcDescriptorList` is the concrete serialized form of a collection. It is for
manifests, tests, checkpoints, and compatibility files. It can be replayed into
a descriptor stream, but replay must either attach the original pools safely or
materialize/copy bytes into a new owned pool. A stale list must not grant access
to recycled buffer generations.

## Soundness Contract

Every descriptor command must satisfy one of these roles:

- producer: owns a pool and emits descriptors only after downstream credit.
- transformer: passes, splits, joins, or rewrites descriptors without taking
  ownership of payload memory.
- fanout: increments branch references and releases each branch independently.
- terminal consumer: releases every descriptor after send, write, checksum,
  count, materialization, or drop completes.
- supervisor: creates the control channels and tears down outstanding leases on
  process exit, disconnect, or error.

No command should emit descriptors into an unbounded or unacknowledged stream.
If a downstream branch is not listening, disconnects, or exits, the upstream
owner must receive release or abort all leases held for that branch.

## Slice Descriptor

```rust
#[repr(C)]
pub struct ZcSliceDesc {
    pub pool_id: u32,
    pub queue_id: u32,
    pub buffer_id: u64,
    pub generation: u32,
    pub offset: u32,
    pub len: u32,
    pub flags: u32,
    pub numa_node: i16,
    pub preferred_cpu: i16,
}
```

Fields:

- `pool_id`: namespace for the memory pool or mapped region.
- `queue_id`: RX/TX queue or worker-local queue that owns the buffer.
- `buffer_id`: stable token while the buffer generation is live.
- `generation`: protects against stale descriptor reuse after recycling.
- `offset` and `len`: payload range inside the buffer.
- `flags`: checksum state, encryption state, forwarded state, and other hints.
- `numa_node`: preferred NUMA node for consumers; `-1` means unknown.
- `preferred_cpu`: producer/queue-local CPU hint; `-1` means unknown.

## Record Descriptor

```rust
#[repr(C)]
pub struct ZcRecordDesc {
    pub desc_version: u16,
    pub desc_len: u16,
    pub record_id: u64,
    pub stream_id: u64,
    pub group_id: u64,
    pub sequence: u64,
    pub lane_id: u32,
    pub preferred_worker: u32,
    pub total_len: u32,
    pub slice_count: u16,
    pub flags: u16,
    pub release_token: u64,
}
```

The record header is followed by `slice_count` `ZcSliceDesc` entries. Consumers
seek by walking the scatter/gather list and translating a logical record offset
to a `(buffer_id, offset, len)` view. Reordering should hold descriptors, not
copy bytes.

`desc_len` lets newer writers append fields while older readers skip the tail
when no required feature bit is set. `desc_version` names the schema used by the
fixed prefix.

## Topology Hints

## Encryption Hint

Transport descriptors should distinguish encrypted and plaintext payloads.
`aes-256` means AES-256-GCM framed chunks and is the default encrypted zc
stream format produced by `zcencrypt` and tcpmux. `none` is an explicit
plaintext hint and should only be produced when the user passed
`--encryption none`.

Descriptors should carry locality hints so downstream commands can keep work
near the source queue and memory when possible:

- `numa_node`: memory locality for mapped files, registered buffers, or RX pools.
- `preferred_cpu`: CPU that received, produced, or should next consume the slice.
- `queue_id`: RX/TX/NVMe queue that owns or best matches the buffer.
- `lane_id`: mux lane or 5-tuple lane associated with the record.
- `preferred_worker`: stable worker/shard hint for `zcmux`, `zcdemux`, `zctee`, and app consumers.

These fields are hints, not correctness requirements. A consumer may ignore them
if its topology is different, if the CPU is unavailable, or if applying the hint
would break ordering/backpressure. When preserved, they let a pipeline avoid
cross-NUMA copies, L3 cache disruption, queue bouncing, and avoidable worker
migrations.

## TCP Mux Topology Header

`zc-tcpmux` parallel lanes now use a versioned V2 lane header. It carries the
same placement intent as descriptor records so a receiver can keep each lane on
the matching local worker:

- `lane_id` and `lane_count`: stable mux lane identity for the transfer.
- `preferred_worker`: worker/shard that should own the lane.
- `queue_id`: queue-local owner; currently the lane id for TCP mux traffic.
- `preferred_cpu` and `numa_node`: sender-side CPU and NUMA hints after
  optional affinity is applied.
- `flags`: whether affinity was applied and whether CPU/NUMA were known.
- `chunk_bytes`: planned per-lane transfer chunk size.

The header is advisory. `zc-tcpmux-receive` logs both the sender hint and the
receiver's actual pinned CPU/NUMA placement. Use `--pin-cpus --cpu-list LIST`
on `zc-tcpmux-xfer` to apply the same lane-to-CPU map to both the local send
workers and the remote receive workers. `--send-cpu-list` and
`--receive-cpu-list` can diverge the two sides when the machines have different
topologies.

## Lifetime

`zcutils` owns the authoritative lease table. Consumers receive descriptors and
must release by token.

Normal flow:

```text
NIC/ZCRX produces buffer
  -> zcdemux creates a buffer lease
  -> zcflow or stream-header discovery has wired downstream credit/release channels
  -> zcdemux emits a ZcRecordDesc + sglist when credit is available
  -> zcmap/zctee/zcmaptee/zcmux/app/WAL pass or refcount references
  -> each async completion or app release decrements references
  -> refs hit zero
  -> buffer returns to the RX refill path or userspace pool
```

Required release events:

- app consumer release
- send-zc completion
- WAL write completion
- demux reorder-window eviction
- fanout branch disconnect

The app must never directly free RX memory. It sends `release_token` back to the
owner, and the owner validates `(pool_id, buffer_id, generation)` before
recycling.

## Ordering

`zcdemux` should default to per-stream ordering, not global ordering.

```text
global: unordered
peer pair: optional ordering
stream/group: ordered
fragment: ordered within record
```

The reorder window stores descriptor references. A late or duplicate record can
be dropped by metadata without touching payload bytes.

## Zero-Copy Defaults

The network-facing commands default toward zero-copy with portable fallback:

- `zcmux` defaults to `--zero-copy-send auto`, which uses send-zc when available.
- `zcdemux` defaults to `--zero-copy-receive auto`, which tries ZCRX and falls back if unavailable.
- `zcnc connect` defaults to automatic send-zc detection.
- `zcnc listen` defaults to automatic ZCRX detection.
- `zcflow` is the intended descriptor supervisor; the current implementation
  uses byte-compatible pipes until fd-passing descriptor transport is added.
- `zcmap` and `zcmaptee` are the descriptor transform/fanout names; their current
  implementation is a byte-compatible passthrough/fanout.
- `--zero-copy-receive auto` tries ZCRX but falls back to copied receive.
- `--zero-copy-send auto` tries send-zc but falls back to copied send when setup is not allowed.

Byte-stream tools are intentionally marked as compatibility/debug paths until
the descriptor socket protocol is implemented.
