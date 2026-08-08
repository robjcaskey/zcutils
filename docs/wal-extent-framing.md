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
volume clones, RAID membership, `zcbrd`, `zcnblk`, and `zcraid-*`. The current
`zcsnap` command is the byte-compatible placeholder for emitting that manifest
shape.

## Extent Sizing Policy

The sender should maintain one coalescer per lane. Flush an extent when any of
these is true:

- payload reaches the throughput target
- queued age reaches the latency budget
- record count reaches the configured cap
- a commit barrier or fsync boundary arrives
- lane credits or WAL credits are exhausted

For `/dev/zcnblk0` ingress, extent coalescing must not require a payload copy.
The shared transport negotiates individually transferable payload pages: the
kernel assigns a free physical slot and publishes its submit-sequence owner
token; the dirty cache and coalescer retain references to those slots until the
extent result HWM permits retirement. Request-ring order and payload-page reuse
are independent. A stale owner token is fatal, and sync seals partial extents
before waiting for every lane's remote HWM.

The payload pool is a byte-credit budget, not an excuse for unbounded buffering.
At low traffic, the age budget flushes a partial extent immediately enough for
latency-sensitive I/O. At high traffic, byte and record thresholds should form
large extents naturally. At hard pressure, admission waits or retires only
remotely committed dirty pages; it must not fall back to a hidden payload copy.

Based on the current EC2 c8gn tests, the first serious defaults should be:

- latency-biased: 64 KiB, 16 logical 4K records
- throughput-biased: 384 KiB, 96 logical 4K records
- never default to 1 MiB until a target proves it wins

The benchmark result that matters is not just extent size. On May 29, 2026,
the c8gn two-node RAM WAL tests showed:

- short single-`zcbrd` runs with port-lane sharding reached `188.8 Gbit/s` at
  32 lanes and 4K physical writes, but the hot window was only about 0.14 s
- longer 24 GiB pre-userspace-stripe block lab runs reached `175.3 Gbit/s` at
  64 lanes and 4K physical writes, `179.7 Gbit/s` at 64K extents, and
  `231.8 Gbit/s` at 384K extents
- pinned 64-lane/384K repeats were steadier at `197.0..214.7 Gbit/s`, or about
  `6.0..6.6M` logical 4K records/sec
- with the same 64-lane/384K shape, `port-lane` sharding beat `observed`
  sharding and `round-robin` sharding in a policy check

The frame therefore carries both `lane_id` and `shard_id`; receivers should not
guess placement from connection order.

On June 3, 2026, the two-node `c8gn.48xlarge` adhoc pair moved from one-card
smoke tests to a two-card WAL transport sweep. With two ENIs per node, card0
pinned to CPUs `0-95`, card1 pinned to CPUs `96-191`, 384 KiB extents, stream
uring framing, and explicit source-IP route checks, the best standard TCP/WAL
shape was 256 lanes per card:

- card0: 273.9 Gbit/s, 8.36M logical 4K records/s
- card1: 261.3 Gbit/s, 7.97M logical 4K records/s
- aggregate over the slower leg's wall time: 522.6 Gbit/s, 15.95M logical 4K
  records/s

The result did not use any block-device mirror or stripe primitive. It measured
WAL extent transport capacity only. See
[`ec2-c8gn-pair-wal-transport-benchmark.md`](ec2-c8gn-pair-wal-transport-benchmark.md)
for the full runbook, lane sweep, and libfabric/EFA status.

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

## OFI WAL Transport Prototype

`zcwal-ofi-send` and `zcwal-ofi-recv` carry the same `ZcWalExtentV1` and
`ZcWalAckV1` records over libfabric RDM endpoints. This is the RDMA/fabric
parallel to the TCP mux WAL path: lane id, shard id, extent sequence, logical
record count, and ack range stay in the WAL frame, while libfabric supplies the
per-lane endpoint, CQ, address vector, and registered-memory semantics.

The current prototype opens one libfabric endpoint per lane and uses a small
TCP control exchange on `service + URING_PLAY_OFI_CONTROL_PORT_OFFSET` to swap
provider endpoint names. Bind the data endpoint to the private fabric IP on
real hosts; keep public IPs for SSH/control only. On EFA, do not treat
`fi_info` as proof. Require a successful cross-host `fi_pingpong` or
`libfabric-smoke` for the exact provider, endpoint, domain, security group, and
private route before accepting results.

Local functional smoke, receiver:

```bash
URING_PLAY_PIN_CPUS=1 \
URING_PLAY_PIN_CPU_LIST=4,5,6,7 \
URING_PLAY_OFI_TIMEOUT_MS=5000 \
target/release/zcutils zcwal-ofi-recv tcp rdm 127.0.0.1 30100 4 8M 4K 4 true
```

Local functional smoke, sender:

```bash
URING_PLAY_PIN_CPUS=1 \
URING_PLAY_PIN_CPU_LIST=20,21,22,23 \
URING_PLAY_OFI_TIMEOUT_MS=5000 \
target/release/zcutils zcwal-ofi-send tcp rdm 127.0.0.1 30100 4 8M 4K 4 true
```

`zcwal-ofi-relay` is the head/fan point for a userspace RAID1 WAL path. It
receives one upstream lane-local WAL extent stream, forwards the same in-memory
slot to every configured tail, and sends one upstream high-water-mark ACK only
after all tails have ACKed the same logical range. `tail-addr` and
`out-base-service` both accept comma-separated lists; a single value is
expanded across all tails. This keeps the client from duplicating mirror
traffic while still keeping mirror placement in userspace.

Local two-tail functional smoke:

```bash
# Tail 0
target/release/zcutils zcwal-ofi-recv tcp rdm 127.0.0.1 30600 2 8M 64K 2 true

# Tail 1
target/release/zcutils zcwal-ofi-recv tcp rdm 127.0.0.1 31600 2 8M 64K 2 true

# Head relay: one upstream, two downstream tails
target/release/zcutils zcwal-ofi-relay \
  tcp rdm 127.0.0.1 127.0.0.1 29600 30600,31600 2 8M 64K 2 true

# Upstream sender
target/release/zcutils zcwal-ofi-send tcp rdm 127.0.0.1 29600 2 8M 64K 2 true
```

For a real cross-host mirror, run one tail on each leaf host and pass
`tail0-private-ip,tail1-private-ip` to the relay. The relay prints
`ack_policy=all-tails-before-upstream-hwm`, `tail_count`, lane/worker placement,
context switches, migrations, logical IOPS, and branch wire throughput.
Set `URING_PLAY_OFI_ACK_WINDOW=N` to use one HWM/range ACK per contiguous batch
on the sender, relay, and terminal receiver. This reduces ACK/CQ churn without
changing the commit contract: upstream ACKs are sent only after all tails have
ACKed the same lane-local logical range. `URING_PLAY_OFI_RELAY_BRANCH_POST_NOWAIT=1`
is an opt-in experiment that posts a batch to every tail before draining
completions; benchmark it per provider because it did not improve the local
`tcp;rdm` mirror path. For local TCP-provider tests, compare direct and mirrored
results by total data movement: a two-tail relay performs one upstream receive
plus two downstream sends, so saturating the same copied-data budget can show up
as about one third of direct one-hop logical IOPS.

For ultra-low-latency ACK experiments, the shim can spin on empty CQs instead
of sleeping:

```bash
URING_PLAY_OFI_BUSY_POLL_ITERS=1000000
URING_PLAY_OFI_CQ_SLEEP_NS=0
```

That mode is intentionally not the everyday default. It burns CPU to avoid
scheduler handoff while a lane is hot. The command header prints
`busy_poll_iters` and `cq_sleep_ns`; benchmark notes must include those values.

### OFI RMA Direct Memory Prototype

`zcwal-ofi-rma-target` and `zcwal-ofi-rma-write` are the first direct remote
memory write smoke for the WAL/fabric path. The target opens one libfabric RDM
endpoint per lane, registers a lane-local arena with `FI_REMOTE_WRITE`, and
sends a 64-byte metadata message containing lane id, lane count, arena size,
extent size, remote address, and remote key. The writer receives that metadata
and issues `fi_write` calls directly into the remote arena, then sends one
64-byte commit doorbell message for the lane.

This is a transport primitive, not RAID placement. Userspace RAID still owns
mirror, stripe, spill, placement, lane selection, locality, and backpressure.
The RMA command only proves that a selected lane can place bytes directly into a
selected remote registered memory slice.

Local functional smoke, target:

```bash
URING_PLAY_OFI_TIMEOUT_MS=20000 \
URING_PLAY_OFI_CQ_SLEEP_NS=0 \
URING_PLAY_OFI_BUSY_POLL_ITERS=100000 \
URING_PLAY_PIN_CPUS=1 \
URING_PLAY_PIN_CPU_LIST=0-3 \
target/release/zcutils zcwal-ofi-rma-target tcp rdm 127.0.0.1 31700 4 64M 1M 4
```

Local functional smoke, writer:

```bash
URING_PLAY_OFI_TIMEOUT_MS=20000 \
URING_PLAY_OFI_CQ_SLEEP_NS=0 \
URING_PLAY_OFI_BUSY_POLL_ITERS=100000 \
URING_PLAY_PIN_CPUS=1 \
URING_PLAY_PIN_CPU_LIST=4-7 \
target/release/zcutils zcwal-ofi-rma-write tcp rdm 127.0.0.1 31700 4 64M 1M 4
```

On June 5, 2026, local release-mode loopback with libfabric `tcp` RDM showed:

| mode | lanes | lane CPU map | payload | extent | writer payload Gbit/s | writer logical IOPS | target payload Gbit/s | target logical IOPS |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| RMA write + one doorbell | 1 | target `0`, writer `1` | 16 MiB/lane | 1 MiB | 59.3 | 1.81M | 48.1 | 1.47M |
| RMA write + one doorbell | 4 | target `0-3`, writer `4-7` | 64 MiB/lane | 1 MiB | 98.0 | 2.99M | 90.4 | 2.76M |
| RMA write + one doorbell | 4 | target `0-3`, writer `4-7` | 64 MiB/lane | 64 MiB | 53.4 | 1.63M | 37.5 | 1.14M |

For local `tcp` RDM, `fi_info` reports `mr_mode=[]`, so peers target registered
memory with an offset from the start of the region; `remote_addr=0` is expected.
For providers with `FI_MR_VIRT_ADDR`, peers target the registered virtual
address. Do not compare these local TCP values to EFA/RDMA line-rate claims; use
them only to validate the API contract, lane pinning, and RMA visibility story.

The next step is a descriptor-ring/HWM doorbell so the target can poll committed
remote WAL progress without receiving one control message per lane. If the
initiator completion level does not imply target visibility for a provider, use
a fenced delivery-complete doorbell or provider-supported remote CQ data before
advancing the high-water mark.

### RDMA Parallel WAL Path

The RDMA equivalent of the high-rate TCP WAL target-drain path is not a
block-device mirror or stripe. It is the same userspace RAID placement contract
mapped onto lane-local registered memory:

```text
client/fan lane N
  -> local WAL extent lease in a registered lane arena
  -> RDMA write into remote lane arena slot N,S
  -> small descriptor/doorbell record {lane, seq, slot, len, hwm_epoch}
  -> leaf consumes committed slots in lane order
  -> leaf result/HWM log returns one range ACK per contiguous batch
```

The hot data path is one-sided `fi_write`/provider RMA into pre-registered,
lane-owned slots. The control path is small: initial arena metadata exchange,
credit return, descriptor doorbells, and high-water-mark ACKs. A lane leader owns
its source arena, remote slot credits, transmit CQ, and ACK/result CQ. It should
post many RDMA writes from stable per-slot buffers, drain completions in batches,
and advance remote visibility with one doorbell per slot batch, not one message
per 4 KiB logical record.

For mirror fanout, userspace RAID writes the same payload lease to each selected
remote arena according to placement. Commit is the minimum contiguous HWM across
the required branches. For stripe, placement emits segment descriptors that
target distinct remote arena slots; the zipper emits upstream completion only
after the segment mask for that `(lane, sequence)` is complete. Both policies
use the same per-lane descriptor/result-log zipper as TCP mux.

The first production-shaped implementation should add:

- persistent registered source and target arena pools, preferably hugetlb-backed;
- per-lane remote slot tables with credits and wrap-safe sequence numbers;
- batched `fi_write` posting and batched CQ draining;
- a descriptor doorbell format that carries lane, sequence, slot, byte range,
  checksum/encryption state, placement epoch, and sync epoch;
- range/HWM ACKs, never per-4K ACKs, with sync/flush/FUA using explicit epochs;
- topology warnings for missing fabric domain, MR registration, memlock,
  hugetlb, lane CPU pinning, CQ busy-poll policy, and provider smoke proof.

This path can share the `ZcWalExtentV1` logical header, ACK/range semantics,
branch placement, and result-log zipper with TCP mux. It should not share the
TCP socket framing, per-message receive loop, or block-leaf placement decisions.
If encryption is enabled, encrypt once at the WAL extent lease boundary and send
ciphertext descriptors to every branch that is allowed to hold that ciphertext;
do not decrypt and re-encrypt at each fan hop unless the key domain explicitly
requires it.

On June 5, 2026, local release-mode loopback with libfabric `tcp` RDM, receiver
CPUs `4,5,6,7`, sender CPUs `20,21,22,23`, 4 lanes, and 4 KiB logical records
showed:

| mode | payload | sender logical IOPS | receiver logical IOPS | context switches | migrations |
| --- | ---: | ---: | ---: | ---: | ---: |
| no ACK | 64 MiB/lane | 2.50M | 2.46M | 442/0 send, 358/0 recv | 0 |
| per-extent ACK, busy poll | 8 MiB/lane | 454k | 452k | 19/0 send, 25/1 recv | 0 |

These are OFI API and lane-contract smokes, not EFA performance claims. The hot
path still posts one message at a time and needs registered per-lane buffers,
batched extents, CQ draining, and provider-specific max-message handling before
it can be judged against the TCP mux on 400G+ hosts.

On June 5, 2026, the same commands were validated cross-host on two
`c8gn.16xlarge` adhoc instances with one EFA ENI each. The receiver used
`bind=auto`, the sender used the peer private IP, and the security group allowed
self-referenced all traffic in both directions. AWS `fi_pingpong -p efa`
reported roughly 6.3-7.5 us per transfer for 64B-4K messages, proving provider
data movement before running the WAL transport.

With `URING_PLAY_OFI_BUSY_POLL_ITERS=1000000` and
`URING_PLAY_OFI_CQ_SLEEP_NS=0`, the WAL-over-OFI EFA provider path showed:

| mode | shape | sender logical IOPS | receiver logical IOPS | sender ACK latency | context switches |
| --- | --- | ---: | ---: | --- | ---: |
| per-extent ACK | 4 lanes, 1 MiB/lane, 4 KiB records | 50k | 45k | p50 16 us, p95 26 us, p99 72 us, max 9.6 ms | 169 send, 185 recv |
| no ACK | 4 lanes, 8 MiB/lane, 4 KiB records | 207k | 193k | n/a | 155 send, 186 recv |

The matching TCP WAL extent sync-ACK run over the same private EC2 path, same
4 lanes, same CPUs, and same 1 MiB/lane payload showed 50k sender logical IOPS,
51k receiver logical IOPS, p50 70 us, p95 131 us, p99 245 us, and 322 us max
ACK latency. EFA is already materially better on median/tail ACK latency in
busy-poll mode, but this shim is still throughput-limited by one-message-at-a-
time send/recv and EFA per-message memory registration. The next throughput
step is persistent registered lane buffers, posted receive rings, batched send
submission, and CQ draining across batches.

## Current Prototype Mapping

`tcp-wal-mux-server` and `tcp-bench-uring-mux-send` approximate extent framing
today by treating `chunk_bytes` as the physical WAL extent. That measures the
right physical IO shape, but it does not yet carry:

- logical base index
- record count
- explicit extent sequence
- durable range ack
- descriptor id

The next code step should be a framed WAL mode rather than another target-side
block variant:

```text
tcp-wal-extent-send  -> lane-local ZcWalExtentV1 frames
tcp-wal-extent-target -> validate header, queue one WAL write, send ZcWalAckV1
```

An environment-gated mode on the existing TCP WAL commands is also acceptable
for a prototype, but the wire format should still be the extent frame above.
