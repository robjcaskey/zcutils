# zcnblk Log Fanout Architecture

This is the high-IOPS fanout target for `/dev/zcnblk0` traffic. It keeps RAID
policy in userspace and treats block devices only as terminal leaf media after
placement has already been decided.

## TLDR

The fan node is a log switch, not a request router. Ingress lanes append
request-log records with monotonic lane sequence numbers. Placement emits
branch append descriptors into per-branch output logs. Leaves append compact
result records into per-branch result logs. The fan-in side zips those monotonic
result logs by `(lane, sequence)` and emits ordered upstream completions.

The hot path must not deep-inspect payloads for mirror writes. It should parse
only the fixed zcnblk frame header, allocate a sequence number, and fan out
descriptor references to the same payload lease. On hardware/kernel paths that
can receive into ZCRX memory and transmit that memory with send-zc, mirror
branches should reuse the same received payload pages. Literal "same skb"
forwarding is a kernel/driver fast path; the userspace contract is same payload
lease, not copied branch buffers.

## Logs

Every lane owns independent append-only logs:

```text
client lane N
  -> request log N
  -> placement log N
  -> branch output log N,B
  -> leaf result log N,B
  -> zipper result log N
  -> upstream completion lane N
```

The request log record is small and fixed:

```text
lane_id
sequence
op
logical_offset
logical_len
request_id
sync_epoch
payload_descriptor
placement_epoch
```

The payload descriptor is a reference to ZCRX memory, a registered send buffer,
a WAL extent, or a shared pool slice. Mirror placement duplicates descriptor
references, not payload bytes. Stripe placement emits segment descriptors with
known `(branch, segment_index, logical_offset, len)` fields.

The current local `zcraid-split --descriptor-wal-dir DIR` prototype materializes
that shape as `payload.wal` plus per-branch `branch-NNNN.zcraid-desc` logs. Each
descriptor carries branch/lane id, copy id, logical offset, payload WAL offset,
payload length, checksum metadata, and EOF/commit metadata in a fixed 128-byte
record. This is a placement metadata prototype, not the hot streaming path, not
a terminal media backend, and not a block-device stripe.

The hot tcpmux prototype is `zcraid-split --to-tcpmux HOST:PORT --encryption
none --zero-copy-send required`. It streams zcraid frame headers and sends the
shared payload chunk from the fan process to branch sockets with io_uring
`SEND_ZC`. The fan holds the payload lease until each branch send completion,
and a kernel "copied" zero-copy notification is a failed run when zero copy is
required.

The first implemented zcnblk WAL fan path is
`zcnblk-fan --engine wal --leaves A,B --bind FAN --mode stripe|mirror` paired
with `zcnblk-wal-leaf TARGET BIND ...` on each edge. It uses explicit fixed
128-byte fan WAL frames:

```text
HELLO
WRITE_DESC { lane, sequence, branch, segment, logical_offset, leaf_offset, len } + payload
READ_DESC  { lane, sequence, branch, segment, logical_offset, leaf_offset, len }
RESULT     { lane, sequence, branch, segment, status } [+ read payload]
SYNC
EOF
```

That path is currently plaintext TCP. `zcnblk-wal-leaf` supports blocking,
io_uring, and mixed terminal submit adapters for block-device reads/writes. It
proves the descriptor/result-log contract over real TCP and keeps placement in
userspace. It is not yet the final zero-copy payload lease implementation.

Write batching is part of the contract. An explicit upstream zcnblk write batch
is forwarded to each leaf as one coalesced `WRITE_BATCH` WAL chunk: a descriptor
array followed by the payload area for those descriptors. The leaf returns one
coalesced `RESULT_BATCH` payload containing the result descriptor array. The fan
then zips the leaf result streams in the lane-local handler and returns a zcnblk
`BATCH_RESP` containing the individual write ACK headers. This keeps logical 4K
completion semantics while removing the per-record request/response ping-pong.

## Write Path

1. Ingress reads only the fixed zcnblk header and assigns `(lane, sequence)`.
2. The payload becomes a descriptor lease. For mirror, all branch output log
   entries point to the same lease. For stripe, segment descriptors point to
   slices of the lease.
3. Branch senders drain output logs independently and keep lane affinity.
4. Leaves append write result records after their local durability contract is
   met.
5. The zipper ACKs a write when the placement policy is satisfied:
   all replicas for mirror, all stripe segments for stripe, quorum for future
   erasure or consensus modes.

No per-request thread should block waiting for a leaf ACK. Backpressure is log
credit: if a branch output log or result log falls behind, ingress stops taking
new descriptors for that lane before payload pools are exhausted.

The contract is independent of the local I/O API. A conventional `write`/`pwrite`
leaf and an `io_uring` leaf both work if they append the same ordered result-log
record after the same durability/admission point. The fan/zipper must consume
`(lane, sequence, placement_epoch, result_status)` records, not infer ordering
from whether the leaf used blocking syscalls, async `io_uring`, SQPOLL, or fixed
buffers. The fast implementation should share framing, placement, branch logs,
and zipper code across both modes; only the leaf submit/completion adapter
changes.

`zcnblk-wal-leaf` selects that adapter with the final CLI argument:
`blocking`, `uring`, or `mixed`. Mixed mode intentionally alternates terminal
frames between blocking and io_uring so one terminal block device can prove that
both paths coexist under the same ordered result-log contract. The adapter
replaces only the leaf submit/completion internals and keeps emitting identical
`RESULT` records.

## Read Path

Reads also use logs. A read request log record is placed to the branches that
own the requested ranges. Leaves append result records with payload descriptors
for the returned bytes. The zipper reassembles by request sequence and segment
index.

For mirror reads, the default policy can be "first successful result wins" while
the duplicate result is consumed and released later. For strict validation, the
zipper can wait for both and compare checksums, but that is not the high-IOPS
default.

For striped reads, the zipper knows the segment count from the placement log. It
emits the upstream read response only after all segment result bits are present.
Payload assembly should be scatter/gather descriptors. Materializing a single
contiguous read buffer is a compatibility fallback, not the fast path.

## Result-Log Zipper

The result logs are monotonic per `(branch, lane)`, so fan-in does not sort. It
interleaves leaf streams in the lane-local fan handler aligned to the next-hop
output lane, using large result-log chunks and a bounded per-lane reorder window
indexed by `sequence % window`.

Each window slot contains:

```text
sequence
required_mask
complete_mask
status
segment_count
segment_complete_count
payload_descriptors
```

Processing a result record is O(1):

1. Validate `lane` and `sequence` are inside the active window.
2. OR the branch or segment bit into `complete_mask`.
3. Release duplicate mirror payload descriptors that lost the race.
4. Advance `next_emit` while the head slot satisfies its required mask.
5. Emit upstream ACK/read response descriptors in order.

The zipper can run one worker per lane group. It should never share a global
priority queue for hot-path results. Cross-lane sync uses a global high-water
mark only for explicit flush/FUA/sync epochs.

## Sync Contract

Normal writes can be ACKed individually once their placement policy is
committed. Sync/FUA is a log barrier:

```text
sync_epoch E is complete when every lane has emitted all writes <= E
according to the placement policy.
```

That preserves conventional block-device expectations without forcing total
ordering of unrelated writes in the hot path. Multiple application threads can
issue writes concurrently; sync observes the whole device up to the global
epoch, while ordinary write ACKs remain per request.

## Fast-Path Rules

- No block device may perform mirror, stripe, tier, spill, or placement.
- No payload copy per mirror branch.
- No deep payload inspection on mirror writes.
- No per-request wait in the fan node.
- No global sorting of result records.
- Lane identity must survive ingress, placement, branch send, leaf result, and
  zipper completion.
- Backpressure is credit on logs, descriptors, and payload pool leases.
- Benchmarks must state lane-to-worker and lane-to-CPU mapping.

## RAID And Spill Shape

Mirroring, striping, and spill are log primitives:

```text
request log -> placement log -> branch output logs -> leaf result logs
            -> lane-local zipper -> upstream completion log
```

Mirror writes publish multiple branch descriptors for the same payload lease.
Stripe writes publish segment descriptors into dedicated shard lanes. Spill
publishes a hot-leaf descriptor and a bounded cold-copy descriptor; the write ACK
point is the hot write plus cold-spill admission, while explicit durability
policies can wait for cold result-log records. The spill queue owns backpressure:
when it is full, placement stops admitting new work before payload pools churn.

Reads should prefer the current hot/placement log. If a leaf must read from a
cold spill copy after restart, that rehydration result is still appended to the
leaf result log so the upstream zipper preserves order.

## Kernel Interface Shape

Userspace can implement the control contract with ZCRX and send-zc:

```text
TCP RX -> ZCRX buffer lease -> request descriptor -> branch output descriptors
       -> send-zc from the same payload lease -> result-log zipper
```

If the kernel and NIC cannot transmit from the received pool directly, the
fallback is registered bounce buffers. That fallback is useful for correctness
and local testing but is not the 400G design point.

Literal same-SKB TCP forwarding belongs in a kernel or driver fast path because
userspace sees a stream after TCP processing, not the original skb ownership.
The userspace architecture should still be compatible with such a fast path by
keeping the branch decision in the fixed header and by making payload forwarding
descriptor-only.
