# zcnblk WAL Fan Topology

This is the consistency contract for a four-host high-IOPS ordered fan:

```text
client /dev/zcnblk0
  -> zcnblk-fan
  -> zcnblk-wal-leaf on host A -> terminal /dev/zcbrd0
  -> zcnblk-wal-leaf on host B -> terminal /dev/zcbrd0
```

The fan target is a userspace placement point. It must not use a block device,
dm, md, loop, `/dev/zcbrdN`, `/dev/nullbN`, `/dev/ramN`, or a custom block module
as a stripe or mirror primitive. Edge block devices are terminal leaf media only.

The current implementation is:

```bash
zcnblk-wal-leaf /dev/zcbrd0 10.0.1.31 24600 64 1 4K 64 true mixed
zcnblk-wal-leaf /dev/zcbrd0 10.0.1.32 24600 64 1 4K 64 true mixed

URING_PLAY_ZCNBLK_WRITE_ACKS=1 \
URING_PLAY_ZCNBLK_BATCH_DEPTH=64 \
URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW=16 \
zcnblk-fan --engine wal --leaves 10.0.1.31,10.0.1.32 --bind 10.0.1.20 \
  --base-port 23600 --ports 64 --connections-per-port 1 \
  --bytes-per-connection 64G --chunk-bytes 4K --stripe-bytes 4K \
  --leaf-base-port 24600 --pin-handlers true --mode stripe
```

This is real TCP descriptor/result fanout. `mixed` leaf submit mode alternates
terminal frames between blocking and io_uring while preserving identical
result-log semantics. The write path uses explicit `WRITE_BATCH`,
`RESULT_BATCH`, and upstream `BATCH_RESP` framing when the client sends
`URING_PLAY_ZCNBLK_BATCH_DEPTH=N`. Send-zc payload leases remain next-step
performance work.

`URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW` is the write-side pipeline depth between
the fan and leaf result logs. A value of `1` is useful for exposing the old
serialized context-switch-per-batch behavior, but representative write IOPS
tests should state the window and lane-to-CPU map. Terminal io_uring leaves can
override their local ring with `URING_PLAY_ZCNBLK_WAL_LEAF_RING_ENTRIES` and
`URING_PLAY_ZCNBLK_WAL_LEAF_CQ_ENTRIES`; this only changes leaf submission
mechanics, not userspace placement.

`URING_PLAY_ZCNBLK_FAN_RESULT_ARENA=1` is an experimental fan-local result
interleave path. Result receiver threads read leaf range-result batch headers
and publish them into lane-local `memfd`/`MAP_SHARED` rings, letting the handler
zip compact leaf result streams without parsing descriptor payloads. It is valid
only with range result batches and pipelined write batches. The optional
`URING_PLAY_ZCNBLK_FAN_RESULT_ARENA_INLINE_PRIMARY=1` mode keeps the primary
leaf result stream on the handler lane and maps only secondary result streams;
`URING_PLAY_ZCNBLK_FAN_RESULT_ARENA_SPIN=1` polls secondary result headers with
`recv(MSG_DONTWAIT)` for dedicated-core experiments. These modes are userspace
RAID/fan mechanisms and never use a block device as a mirror or stripe
primitive.

## Write Path Contract

1. The fan receives zcnblk write frames from the client block edge.
2. It assigns each logical record to an edge by userspace placement policy.
3. It emits a fan WAL `WRITE_DESC` with `lane_id`, `sequence`, `branch_id`,
   `segment_index`, `logical_offset`, `leaf_offset`, and `payload_len`.
4. It sends the descriptor and payload to the selected edge lane and consumes a
   compact `RESULT` record.
5. It advances the fan's committed range map only after the WAL ACK is valid.
6. It may ACK the zcnblk write once that write's extent is committed.

Write ACK means the fan can serve later reads for that logical range from either
the committed WAL image or a leaf media image that is at least as new as the
record's committed sequence. A write ACK is not a promise that every leaf has
already been compacted.

## Read Path Contract

Reads resolve through a version check before any leaf read:

1. Locate the logical 4 KiB record range.
2. Check the fan's committed range map for the newest committed WAL sequence.
3. If the range is still in pinned WAL memory, serve bytes from the WAL image.
4. If the leaf compaction watermark covers the committed sequence, forward the
   read to the selected edge leaf.
5. If neither is true, wait, replay, or return a retryable error; do not return
   stale leaf bytes.

For the hot path, fixed 4 KiB records should avoid a per-record table. A compact
range map can track `(logical_record, edge_id, lane_id, extent_sequence,
payload_offset, compacted_sequence)`. Larger WAL extents are still counted as
contained logical 4 KiB IOPS, but visibility is per logical record range.

## Ordered Fan

`zcnblk-fan` is a userspace placement target for both reads and writes. It does
not sort completed IOs globally. It computes the stripe switch points for each
logical request up front, maps each segment to a leaf stream and payload offset,
and reassembles the original zcnblk response from those known segment positions.

Overlapping logical 4 KiB records are serialized by header arrival order before
the fan forwards work to leaves. Batch frames reserve all per-record order slots
before any payload is forwarded, so a later read cannot pass an earlier write in
the same batch. Unrelated sectors may still run concurrently through independent
fan handlers and leaf lanes.

Leaf write ACKs are required. The fan releases same-sector ordering only after
the selected leaf has acknowledged every write segment. If
`URING_PLAY_ZCNBLK_WRITE_ACKS=1` is set on the fan, it then returns a normal
zcnblk write ACK to the upstream client.

WAL ordering is per lane in the data path and global only at explicit
high-water marks. A sync or barrier records a vector watermark: the last
committed `extent_sequence` for every lane that must be visible. Reads after
that barrier must not observe an older version for any covered logical range.

## Benchmark Shape

The first fan benchmark can run directly against terminal `zcbrd` leaves to
measure topology and extra-hop cost. It is a block-media fan benchmark, not a
full WAL visibility benchmark, until the WAL range map and read-your-writes
overlay are active.

Representative numbers require:

- explicit client, fan, and edge host roles
- data-NIC host routes for every fan edge
- `URING_PLAY_EXPECT_ROUTE_DEV=IFACE` and `URING_PLAY_EXPECT_ROUTE_SRC=IP` on
  each process so the tools source-probe established sockets and warn when Linux
  routes a peer through the wrong NIC
- `URING_PLAY_ZCNBLK_FAN_LEAF_SOURCE_IPS=card0_ip,card1_ip` on the fan when
  different mirror/stripe branches must leave through different local NICs
- lane-to-worker and lane-to-CPU mapping at every hop
- hugetlb and memlock headroom
- pinned zcnblk kthreads, fan workers, and edge workers

For strict CPU checks, use per-host topology domains in the leaf CPU map:
`URING_PLAY_ZCNBLK_FAN_LEAF_CPU_LISTS='leaf0@host-a=0-31;leaf1@host-b=0-31'`.
Stages with different domains are different machines, so CPU numbers are not
compared across them. Leaves on the same host should use the same domain so
exact CPU and SMT sibling conflicts remain fatal under strict mode.
- stated placement policy, stripe size, and read fallback mode

## Local KVM Walk

Use the local KVM harness before burning time on remote machines:

```bash
FAN_MODE=stripe FAN_BYTES=128m FAN_CHUNK=1m \
  qemu-zcrx/fan-topology-qemu-kvm.sh
```

It launches four KVM guests with socket-backed point-to-point virtio NICs:

```text
client 10.71.0.1 -> fan eth0 10.71.0.2
fan eth1 10.71.1.1 -> edge1 10.71.1.2
fan eth2 10.71.2.1 -> edge2 10.71.2.2
```

The fan runs `zc-tcpmux-receive | zcraid-split`, and each branch is a
`zc-tcpmux-send` to a terminal edge `zcsink`. This is a topology and TCP
composition smoke test, not a block-device RAID test. It proves that placement
is in userspace and that each edge receives through its own NIC link before the
same shape is moved to multi-host, multi-NIC hardware.
