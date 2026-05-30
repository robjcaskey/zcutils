# WAL Extent Framing

The WAL fast path should not frame every logical 4K record as a separate
network message and block write. It should frame a lane-local extent: one
coalesced payload that describes many fixed-size logical records and maps to
one physical WAL append.

This gives us a clean way to measure logical 4K IOPS while still using the IO
shape the hardware and kernel path want.

## Goals

- preserve `lane_id` from transport through WAL write and acknowledgement
- keep per-lane ordering without a global ordering point in the hot path
- write coalesced physical extents while counting contained logical records
- make storage placement explicit: lane, shard, byte range, and sequence range
- support a no-table fixed 4K record mode for the common hot path
- allow optional record tables for variable records, holes, and barriers

## Frame Shape

All integer fields are fixed little-endian. The fixed header is 128 bytes so it
can be copied, checksummed, and aligned without parsing a variable header on the
hot path.

```text
ZcWalExtentV1

0x00  magic[8]              "ZCWALX1\0"
0x08  version:u16           1
0x0a  header_len:u16        128
0x0c  flags:u32
0x10  lane_id:u32
0x14  lane_count:u32
0x18  shard_id:u32
0x1c  record_size:u32       normally 4096
0x20  record_count:u32
0x24  payload_len:u32
0x28  table_len:u32         0 for fixed contiguous records
0x2c  reserved0:u32
0x30  base_logical_index:u64
0x38  extent_sequence:u64   per-lane monotonic extent sequence
0x40  base_wal_offset:u64
0x48  wal_epoch:u64
0x50  descriptor_id:u64
0x58  payload_crc32c:u32
0x5c  table_crc32c:u32
0x60  header_crc32c:u32
0x64  reserved1[28]
```

Payload follows the header, then the optional record table. In fixed 4K mode,
`table_len = 0`, `record_size = 4096`, and:

```text
payload_len == record_count * record_size
logical_iops == extents_per_second * record_count
logical_iops == payload_bytes_per_second / 4096
```

That is not fake accounting as long as each record is addressable by
`base_logical_index + n`, covered by the extent checksum, and acknowledged by a
durable sequence range.

## Flags

```text
0x00000001  COMMIT_BARRIER       flush or commit boundary after this extent
0x00000002  HAS_RECORD_TABLE     payload is not fixed contiguous records
0x00000004  HAS_RECORD_CRC_TABLE per-record checksums follow the record table
0x00000008  REMAPPED_LANE        original lane metadata is in an extension
0x00000010  ZERO_COPY_DESCRIPTOR descriptor_id refers to a zc descriptor
```

The hot path should normally use no flags: fixed 4K records, contiguous logical
indexes, one payload checksum, no table.

## Optional Record Table

Use a table only when records are variable length, sparse, or carry individual
barrier state. The compact entry is 16 bytes:

```text
delta_index:u32
payload_offset:u32
record_len:u32
flags:u16
crc16_or_zero:u16
```

If full per-record CRC32C is required, set `HAS_RECORD_CRC_TABLE` and append a
parallel `u32` checksum array. Do not pay that cost on the default fixed 4K
path.

## Ack Shape

The receiver should ack extents by lane and sequence range, not by raw byte
count alone.

```text
ZcWalAckV1

magic[8]             "ZCWALA1\0"
version:u16          1
header_len:u16       64
flags:u32
lane_id:u32
shard_id:u32
status:u32
record_size:u32
first_logical_index:u64
last_logical_index:u64
extent_sequence:u64
durable_wal_offset:u64
durable_bytes:u64
header_crc32c:u32
reserved:u32
```

For Raft, the quorum layer can translate these durable extent acks into commit
indexes. The WAL path itself should stay lane-local.

## Snapshot Cuts

Point-in-time snapshots should be expressed as a cut over durable WAL extent
acks. A snapshot cut records `snapshot_id`, `wal_epoch`, ordering mode, and for
each lane the last durable `extent_sequence`, logical index range, and WAL byte
range included in the cut. That gives restore a precise cursor: replay the
manifest extents, then resume WAL replay after each lane watermark.

The required storage action is an extent pin or lease on the referenced WAL
regions so compaction and buffer recycling cannot discard bytes still named by
the snapshot. This is intentionally separate from block-device snapshots,
volume clones, RAID membership, `zcbrd`, `zcstripe`, `zcnblk`, and `zcraid-*`.
The current `zcsnap` command is the byte-compatible placeholder for emitting
that manifest shape.

## Extent Sizing Policy

The sender should maintain one coalescer per lane. Flush an extent when any of
these is true:

- payload reaches the throughput target
- queued age reaches the latency budget
- record count reaches the configured cap
- a commit barrier or fsync boundary arrives
- lane credits or WAL credits are exhausted

Based on the current EC2 c8gn tests, the first serious defaults should be:

- latency-biased: 64 KiB, 16 logical 4K records
- throughput-biased: 384 KiB, 96 logical 4K records
- never default to 1 MiB until a target proves it wins

The benchmark result that matters is not just extent size. On May 29, 2026,
the c8gn two-node RAM WAL tests showed:

- short single-`zcbrd` runs with port-lane sharding reached `188.8 Gbit/s` at
  32 lanes and 4K physical writes, but the hot window was only about 0.14 s
- longer 24 GiB `zcstripe0` runs reached `175.3 Gbit/s` at 64 lanes and 4K
  physical writes, `179.7 Gbit/s` at 64K extents, and `231.8 Gbit/s` at 384K
  extents
- pinned 64-lane/384K repeats were steadier at `197.0..214.7 Gbit/s`, or about
  `6.0..6.6M` logical 4K records/sec
- with the same 64-lane/384K shape, `port-lane` sharding beat `observed`
  sharding and `round-robin` sharding in a policy check

The frame therefore carries both `lane_id` and `shard_id`; receivers should not
guess placement from connection order.

## Local Segmentation Matrix

Use the local matrix before spending cluster time:

```bash
BYTES=2g \
SEGMENT_BYTES_LIST='64k 384k 1m' \
STRIPES_LIST='8' \
MIRRORS_LIST='2' \
FANIN_MODES='primary tree' \
INTEGRITY_MODES='none checksum' \
scripts/zcraidd-wal-segment-matrix.sh
```

The script runs `zcraidd wal-bench` against temporary local files, normally on
`/dev/shm`. It does not touch block devices, remote hosts, AWS, or
`/tmp/cluster.lock`. The matrix covers:

- lane count through `stripe(N,mirror(M))`
- extent size through `--segment-bytes`
- checksum generation and fanin verification through `--checksum`,
  `--no-checksum`, and `--verify`
- payload fanin versus descriptor-tree reaping through `--fanin-mode`
- wave reaping credits through `--wave-segments`

A local UTC 2026-05-30 smoke run on a 32-thread RAM-backed host used 2 GiB of
logical WAL, 8 lanes, 2 mirrors, and 4 KiB records:

| extent | fanin | integrity | fanout rec/s | fanin rec/s | effective rec/s |
| --- | --- | --- | ---: | ---: | ---: |
| 64 KiB | primary | none | 1.85M | 8.61M | 1.52M |
| 64 KiB | primary | checksum | 1.73M | 6.97M | 1.39M |
| 64 KiB | tree | none | 1.86M | 49.5M | 1.79M |
| 384 KiB | primary | none | 1.45M | 5.49M | 1.15M |
| 384 KiB | primary | checksum | 1.46M | 5.08M | 1.13M |
| 384 KiB | tree | none | 1.53M | 277M | 1.52M |
| 1 MiB | primary | none | 1.60M | 5.01M | 1.21M |
| 1 MiB | primary | checksum | 1.51M | 3.99M | 1.09M |
| 1 MiB | tree | none | 1.64M | 597M | 1.64M |

Treat these numbers as a functional and CPU/cache sanity check. The c8gn network
path can still prefer a larger extent, as the earlier two-node RAM WAL run did
with 384 KiB.

## Descriptor Mapping

When a zero-copy descriptor is available, the extent frame should be metadata
for the descriptor payload rather than a reason to copy the payload:

```text
descriptor.lane_id       -> frame.lane_id
descriptor.queue_id      -> preferred RX/TX or block queue
descriptor.object_id     -> frame.descriptor_id
descriptor.storage shard -> frame.shard_id
descriptor.byte range    -> frame.base_wal_offset + payload_len
```

The payload may already live in a registered buffer, ZCRX area, shared memory
window, or mapped WAL region. In that case the frame is the durable append
contract and the descriptor supplies the bytes.

## Current Prototype Mapping

`tcp-wal-mux-server` and `tcp-bench-uring-mux-send` approximate extent framing
today by treating `chunk_bytes` as the physical WAL extent. That measures the
right physical IO shape, but it does not yet carry:

- logical base index
- record count
- explicit extent sequence
- durable range ack
- descriptor id

The next code step should be a framed WAL mode rather than another block target
variant:

```text
tcp-wal-extent-send  -> lane-local ZcWalExtentV1 frames
tcp-wal-extent-target -> validate header, queue one WAL write, send ZcWalAckV1
```

An environment-gated mode on the existing TCP WAL commands is also acceptable
for a prototype, but the wire format should still be the extent frame above.
