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
Large upstream request batches are admission units, not mandatory leaf egress
units. The fan may split a mixed upstream batch by write payload before dirty
admission and leaf preparation so cached reads do not wait behind multi-MiB
receive and descriptor work in one handler turn. That split preserves the
original lane order and does not make placement decisions in the block client.
Async leaf writeback can further compact contiguous write runs into
`WRITE_EXTENT_BATCH`: a small extent table plus leased payload ranges. The leaf
expands those ranges back into ordered result ranges, so the upstream completion
contract stays per logical record while descriptor traffic stops scaling
linearly with every 4K write. For RAM-backed `zcleasemem` leaves,
`URING_PLAY_ZCNBLK_WAL_LEAF_ZCLEASEMEM_MEMFD=1` keeps the extent table in
userspace but splices the bulk TCP payload into a memfd-backed lease log, so
later cached reads can be returned by reference/sendfile rather than by
materializing another payload buffer.
`URING_PLAY_ZCNBLK_FAN_INGRESS_MEMFD_PAYLOAD=1` applies the same lease idea at
the fan ingress: upstream write payloads are spliced into the fan memfd dirty
log, dirty-cache entries point at that log, and leaf write payloads can be sent
as memfd ranges instead of fan heap slices.

## Local Copy Accounting

The local fan-WAL benchmark summaries now report the sender payload memory path:
`payload_generated_bytes`, `payload_heap_copy_bytes`,
`payload_buffer_grow_bytes`, `payload_write_bytes`, and
`response_payload_read_bytes`. Fan and leaf summaries already report
`async_copied_payload_bytes`, `async_leased_payload_bytes`,
`memfd_payload_bytes`, and `copy_submit_bytes`.

For a mirrored `write-read-same` workload, one logical payload byte still moves
over four local socket legs in the current TCP bridge:

1. client to fan write payload,
2. fan to leaf0 mirror payload,
3. fan to leaf1 mirror payload,
4. fan to client cached-read response.

The fan's hot-path goal is to avoid *additional* userspace payload copies while
those socket legs exist. The desired counters for this bridge are
`payload_heap_copy_bytes=0` at the sender, `async_copied_payload_bytes=0`,
`read_cache_materialized_bytes=0`, and `copy_submit_bytes=0` at the leaves. That
means placement, dirty-cache lookup, mirror fanout, terminal RAM admission, and
same-sector reads are exchanging leases/descriptors. It does not mean the local
TCP stack has stopped moving bytes through skb/socket buffers.

For same-host `local:zcleasemem` leaves, the terminal leaf socket legs disappear:
the client still writes payload through the first-hop socket, the fan reads that
payload into its upstream receive buffer, mirror leaves adopt leases from that
buffer/shared arena, and read responses are sent back over the fan-to-client
socket. In the 2026-07-08 local shared-leaf runs the desired zero-copy counters
held (`async_copied_payload_bytes=0`, `async_leased_payload_bytes` equal to the
mirrored payload, and `read_cache_materialized_bytes=0`), so the remaining bytes
to account for are sender payload generation, TCP write/read copies, fan
upstream receive, and sender response receive/verification.

The 2026-07-08 local bridge sweep repeated the 2-lane
`write-read-same` mirror shape on a busy host with CPUs `0-1` client, `2-3`
fan handlers, `4-5` fan async writeback, `6-7` leaf0, and `8-9` leaf1. The
preflight warned that the mapped CPUs were in `powersave`, had existing load,
`memlock=8192`, no huge pages, and capped socket buffers, so these are smoke
numbers rather than representative hardware limits.

| artifact | ingress memfd | fan Gbit/s | fan payload-read s | leaf payload-recv s | fan voluntary cs | leaf voluntary cs |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `local-bridge-ingress0-rep1-20260708T135223Z` | 0 | 58.8 | 0.036 | 0.113 | 157 | 21 |
| `local-bridge-ingress0-rep2-20260708T135257Z` | 0 | 61.2 | 0.028 | 0.100 | 173 | 30 |
| `local-bridge-ingress0-rep3-20260708T135259Z` | 0 | 59.0 | 0.030 | 0.105 | 169 | 24 |
| `local-bridge-ingress1-rep1-20260708T135224Z` | 1 | 59.5 | 0.031 | 0.110 | 173 | 18 |
| `local-bridge-ingress1-rep2-20260708T135258Z` | 1 | 57.5 | 0.030 | 0.109 | 134 | 20 |
| `local-bridge-ingress1-rep3-20260708T135300Z` | 1 | 59.7 | 0.030 | 0.113 | 178 | 13 |

Both modes leased 256 MiB of async mirror payload, copied 0 MiB in async
writeback, materialized 0 MiB of cached read payload, and submitted 0 MiB by
copy at the leaves. Splicing ingress writes into the fan memfd dirty log did
not materially change local throughput; the wall clock stayed around 58-61
Gbit/s because the paced work is socket receive/drain and the terminal TCP
payload path, not dirty-cache placement or branch payload copying inside the
fan.

The lower-level hot-cache control on the same host shows why sender-only numbers
must not be treated as end-to-end. In
`bench-results/local-readcache-hot-control-20260708T135502Z`, 8 pinned
memfd/sendfile fan workers queued 1 GiB at 526 Gbit/s with zero voluntary
context switches, but the local splice-to-`/dev/null` client drained only
109 Gbit/s and took 2746 voluntary context switches, with 5437 short receives
out of 5445 receive ops. In
`bench-results/local-readcache-hot-waitall-control-20260708T135534Z`, the
`waitall` client avoided short receives and used only 8 voluntary context
switches, but the userspace receive copy held the client to 94.8 Gbit/s. Local
same-host tests therefore need both the sender/fan number and the downstream
drain number before we claim a path is lightning fast.

Architectural conclusion: same-host fan-to-leaf RAM placement should use a
shared arena transport, not loopback TCP. The zcnblk client edge still speaks
the normal block-facing zcnblk protocol, and remote leaves still use TCP
send-zc/RDMA/libfabric transports, but a colocated `zcleasemem` leaf should
receive descriptors naming fan-owned memfd/page extents plus HWM ownership
transfers. That preserves the same ordered WAL/result-log contract while
removing the socket legs that dominate local runs. Inter-host transport should
keep the identical descriptor/placement/zipper layer and swap only the payload
lease implementation: local memfd/page ownership for same-host, NIC-readable
registered memory or RDMA RMA extents for remote.

The first integrated same-host version is `zcnblk-fan --engine wal --leaves
local:zcleasemem:SIZE,...`. It keeps the client edge on normal zcnblk/TCP but
replaces fan-to-leaf loopback sockets with local lease adoption. The local leaf
backend admits `Arc<Vec<u8>>` or memfd ranges into its dirty cache by reference;
the fan refuses copied local payload bytes for local writes. Full local
`zcleasemem` read misses now return leased leaf parts or reusable zero leases
instead of materializing a heap buffer; partial dirty-cache/leaf overlays still
must be treated as a copy-accounting hotspot until they also become spliceable
lease runs.

On 2026-07-08, the 2-lane shared-arena smoke used CPUs `0-1` for the client,
`2-3` for fan handlers, and `4-5` for fan async writeback, with
`URING_PLAY_ZCNBLK_FAN_MAX_REQUEST_BYTES=1M`, 1 MiB sender extents, async
writeback, disabled ordinary write ACKs, and `local:zcleasemem:1G` mirror
leaves. The local host was busy, so this table is a short consistency check, not
a hardware limit:

| artifact | workload | sender Gbit/s | fan Gbit/s | async copied | async leased | cached read materialized | fan voluntary cs |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `local-shmleaf-repeat-20260708T141737Z/wrsame-rep1` | write-read-same | 91.3 | 92.8 | 0 | 256 MiB | 0 | 233 |
| `local-shmleaf-repeat-20260708T141737Z/wrsame-rep2` | write-read-same | 100.0 | 102.6 | 0 | 256 MiB | 0 | 243 |
| `local-shmleaf-repeat-20260708T141737Z/wrsame-rep3` | write-read-same | 89.0 | 90.4 | 0 | 256 MiB | 0 | 195 |
| `local-shmleaf-repeat-20260708T141737Z/write-rep1` | write-only | 62.6 | 56.7 | 0 | 512 MiB | n/a | 439 |

The write-read-same path verified reads and removed the fan-to-leaf TCP payload
legs, improving the local end-to-end shape from the earlier 58-61 Gbit/s TCP
leaf bridge to roughly 89-100 Gbit/s in this short run. The write-only control
shows why copy accounting must include branch work: the printed `bytes=` value
counts client-visible write bytes, while mirror placement still leased two
branch copies into local leaves.

Later on 2026-07-08 the fan gained explicit logical-batch counters after a
misleading warning compared two physical extent frames to the configured 4 KiB
logical batch depth. With `write-read-same`, `BATCH_DEPTH=64` forms two 128 KiB
physical frames, not two 1 MiB frames. Raising `BATCH_DEPTH` to 512 lets the
same 4 KiB workload form two 1 MiB extent frames per batch when
`FAN_MAX_REQUEST_BYTES=1M`. On the same busy local host with `local:zcleasemem`
leaves and zero materialization/copy fallbacks:

| lanes | batch depth | fan Gbit/s reps | logical 4K IOPS reps | fan context switches | max logical records/batch |
| ---: | ---: | --- | --- | --- | ---: |
| 2 | 64 | 77.8, 78.2, 78.3 | 2.37M, 2.38M, 2.39M | 2882, 2758, 2841 | 64 |
| 2 | 512 | 87.0, 87.0, 85.9 | 2.65M, 2.65M, 2.62M | 504, 513, 499 | 512 |
| 2 | 1024 | 86.8, 86.5, 85.1 | 2.65M, 2.63M, 2.59M | 516, 520, 521 | 512 |
| 4 | 64 | 115.3, 109.5, 106.5 | 3.51M, 3.34M, 3.24M | 4031, 4012, 4050 | 64 |
| 4 | 512 | 120.0, 118.7, 119.3 | 3.66M, 3.62M, 3.63M | 571, 571, 577 | 512 |
| 8 | 512 | 103.4, 101.8, 102.0 | 3.15M, 3.11M, 3.10M | 2055, 1869, 1845 | 512 |

This keeps fan-to-leaf payload at lease/reference only
(`async_copied_payload_bytes=0`, `materialized_payload_copy_bytes=0`,
`read_cache_materialized_bytes=0`) and shows the remaining local ceiling is the
client-facing TCP ingress/egress and scheduler topology, not leaf placement.
`BATCH_DEPTH=1024` does not help while `FAN_MAX_REQUEST_BYTES=1M` caps each
paired write/read side at 256 logical records. On this shared machine, four
lanes with 512-record batches was the best local smoke; eight lanes spilled
further into SMT/contention and got slower.

The next local read-miss revision fixed an accidental materialization point:
when the dirty cache contained unrelated records, full leaf read misses were
allocating a zeroed heap response buffer before the local leaf filled it. The
fan now detects full misses, keeps `local_read=None`, and lets the local
`zcleasemem` backend return leased parts. Verified non-zero
`write-sync-read` runs on the same busy host showed:

| artifact | lanes / CPU map | mirror read policy | sender Gbit/s | fan Gbit/s | leaf records | read materialized | fan voluntary cs |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| `local-shmleaf-wsr-fill-repeat-20260708T143910Z/run-1..3` | 2 lanes, client `0-1`, fan `2-3`, async `4-5` | 4K stripe | 51.7-55.9 | 53.0-56.8 | 33,024 | 0 | 232-267 |
| `local-shmleaf-wsr-fill-extent-20260708T144450Z` | 2 lanes, client `0-1`, fan `2-3`, async `4-5` | extent, 1 MiB | 49.2 | 50.0 | 384 | 0 | 293 |
| `local-shmleaf-wsr-fill-extent-4lane-20260708T144533Z` | 4 lanes, client `0-3`, fan `4-7`, async `8-11` | extent, 1 MiB | 90.3 | 90.9 | 768 | 0 | 828 |
| `local-shmleaf-wsr-fill-extent-8lane-20260708T144602Z` | 8 lanes, client `0-7`, fan `8-15`, async `16-23` | extent, 1 MiB | 70.4 | 70.6 | 1,536 | 0 | 5,722 |
| `local-shmleaf-wsr-fill-extent-4lane-smt-20260708T144646Z` | 4 lanes, client `0-3`, fan SMT siblings `16-19`, async `4-7` | extent, 1 MiB | 74.1 | 75.0 | 768 | 0 | 961 |

These are not hardware-limit claims: the machine was shared, and the 8-lane and
SMT-paired runs show scheduler/context-switch sensitivity. The useful result is
copy accounting and topology direction: the local path can keep write and read
payloads leased (`async_copied_payload_bytes=0`,
`read_cache_materialized_bytes=0`), large mirror reads should not be fragmented
into 4K leaf records unless that policy is explicitly being tested, and on this
host four separate physical fan lanes outperformed both 2 lanes and the naive
8-lane/SMT layouts.

The 2026-07-08 read-extent response batching experiment added
`URING_PLAY_ZCNBLK_SEND_READ_EXTENT_BATCH_EXTENTS`. With 4 lanes,
`write-sync-read`, 256 MiB per lane, 1 MiB extents, client CPUs `0-3`, fan CPUs
`4-7`, and async CPUs `8-11`, `batch_extents=2` reduced fan response writev
calls from 512 to 256 and emitted 256 range responses, while `batch_extents=4`
reduced writev calls to 128 and saved 384 response headers. Neither was a clean
throughput win on the busy local host: the batch-2 A/B sample was 84.2 Gbit/s
sender-side versus an 80.5 Gbit/s immediate control, but earlier no-batch
repeats ranged from 82.2 to 93.8 Gbit/s. Treat this as a working framing knob
for remote/topology tests, not as proof that larger local loopback responses are
always faster.

A follow-up 2026-07-08 read-drain repeat added sender counters for
`response_payload_read_ops`, `response_payload_max_read_bytes`, and bulk fill
verification. With 4 lanes, 128 MiB/lane, 1 MiB extents, client CPUs `0-3`, fan
handlers `4-7`, async CPUs `8-11`, and shared-arena leaves, both `batch_extents=1`
and `batch_extents=2` drained read responses as 256 one-MiB reads. The verified
sender-side repeats were noisy on the shared host: batch 1 averaged 72.2 Gbit/s
with a 66.7-79.1 Gbit/s range, while batch 2 averaged 72.8 Gbit/s with a
68.7-75.8 Gbit/s range. That rules out a remaining 4K response-read loop as the
local bottleneck.

The copy-accounting matrix in
`bench-results/local-shmleaf-copyacct-matrix-20260708T152921Z` used the same
4-lane shared-arena topology with 256 MiB/lane. Write-only mirror traffic ran
72.4-77.8 Gbit/s sender-side. `write-sync-read` without read verification ran
96.0-102.0 Gbit/s sender-side, while enabling verification dropped that to
75.8-81.1 Gbit/s. The fan still reported zero async payload copies and zero
materialized read-cache bytes, so the production no-verify path is dominated by
TCP loopback user/kernel payload movement. The new summary fields make that
explicit: `socket_payload_copy_bytes_lower_bound` is the TCP payload copied at a
process boundary, `user_payload_touch_bytes_lower_bound` adds sender-side fill
and verification scans, `leased_payload_reference_bytes` counts leaf/cache
payload ownership passed by reference, and `materialized_payload_copy_bytes`
must stay zero for this path.

The 2026-07-08 copy-accounting refresh added
`observed_payload_copy_bytes_lower_bound` and per-second rates to
`zcnblk-fan-wal` handler and summary lines. That value is intentionally only the
fan's known socket payload movement plus materialized WAL-stage payload copies;
`leased_payload_reference_bytes` is not counted as a copy. Tiny 1-lane local
smokes in `bench-results/local-fanwal-copyacct-smoke-20260708T174921Z` and
`bench-results/local-fanwal-copyacct-smoke-repeat-20260708T174946Z` showed the
expected shape: `materialized_payload_copy_bytes=0`,
`leased_payload_reference_bytes=25165824`, and
`observed_payload_copy_bytes_lower_bound=16777216`. Both runs also warned about
powersave governors, busy mapped CPUs, low memlock, no hugepages, and undersized
socket buffers, so their 25-27 Gbit/s local TCP numbers are smoke evidence for
the counters, not representative performance.

The strict zero-copy follow-up in
`bench-results/local-zero-copy-strict-localleaf-long-20260708T163013Z` used
2 lanes, 512 MiB/lane, local `zcleasemem:1G` mirror leaves, memfd ingress
payloads, dirty-cache reads, and `URING_PLAY_ZCNBLK_WAL_ZERO_COPY_STRICT=1`.
It moved 1 GiB of logical write/read-same traffic at 79.0 Gbit/s fan-side with
`async_copied_payload_bytes=0`, `read_cache_materialized_bytes=0`,
`read_cache_fallback_materialized_bytes=0`, and
`materialized_payload_copy_bytes=0`. Fan-to-leaf placement was therefore
lease-only; the dominant phase remained `phase_upstream_payload_read_seconds`,
which is the client-to-fan TCP ingress copy into the fan-owned payload arena.
After the dirty-cache record-path fix, the same 2-lane shape was repeated in
`bench-results/local-zero-copy-strict-localleaf-overlay-20260708T171326Z`.
Fan-side throughput stayed in the same band, 77.1-79.0 Gbit/s across three
repeats, with `async_copied_payload_bytes=0`,
`materialized_payload_copy_bytes=0`, and `read_cache_materialized_bytes=0`.
The 4-lane repeat in
`bench-results/local-zero-copy-strict-localleaf-overlay-4lane-20260708T171404Z`
landed at 114.4 and 117.9 Gbit/s. Those runs confirm the dirty-cache fix did
not move the production loopback ceiling: the local production path is still
limited by TCP socket ingress/response movement and scheduling, while fan-to-leaf
payload ownership remains lease-only.

The local lane sweep in
`bench-results/local-zero-copy-strict-localleaf-lanes-20260708T163052Z` kept
the total logical payload at 1 GiB and varied upstream lanes against the same
local leaves:

| lanes | fan Gbit/s | copied payload | materialized reads | voluntary cs |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 43.8 | 0 | 0 | 2162 |
| 2 | 80.2 | 0 | 0 | 2471 |
| 4 | 116.1 | 0 | 0 | 3932 |
| 8 | 105.6 | 0 | 0 | 4436 |

This is a useful production-WAL smoke, but it is not the same-host design
ceiling. Adding handlers helps until about four lanes on this busy host, then
the TCP ingress/read path and scheduling churn dominate.

In the 2026-06-09 local 3-lane `write-read-same` mirror run with 1 MiB extents
and `zcleasemem` leaves, the client generated 805 MiB of write payload, handed
805 MiB to normal TCP sends, and read 805 MiB of responses. The fan admitted the
same 805 MiB into the memfd dirty cache, returned reads from leased memfd
extents without materializing read buffers, and sent 1.5 GiB of leased mirror
payload to the two leaves. Each leaf received 805 MiB into its memfd lease log
with `copy_submit_bytes=0`.

The current local ceiling is the TCP socket-to-memfd terminal path, not the
userspace RAID placement logic. Reusing a per-thread splice pipe removed
per-extent `pipe2`/`F_SETPIPE_SZ` churn and improved the 1-lane smoke from about
33 Gbit/s fan wall to about 40 Gbit/s, but the 3-lane run stayed near
46 Gbit/s because each terminal leaf worker was CPU-bound around 0.9 GiB/s.
Larger 4 MiB WAL extents reduced descriptor count but were slower on this path;
1 MiB extents remain the better local TCP/memfd point until the local fan-to-leaf
handoff becomes a shared-arena lease or the remote path becomes real NIC/RDMA
zero-copy.

The positive control is `zcfanout-logshm-bench`: with 10 million 4K-logical
records, one inline primary leg, one shared-memory secondary leg, and pinned
CPUs, the descriptor zipper ran at about 119 million logical 4K IOPS with zero
context switches. That benchmark moves result descriptors and payload references,
not payload bytes, so it does not replace end-to-end fan-WAL testing. It proves
the ordered fan-in/reassembly logic can run far faster than the current local
TCP socket-to-memfd terminal payload path.

`zcfanout-shmlease-bench` adds the missing same-host payload lease control. It
maps a memfd payload arena, gives the fan and leaf separate `MAP_SHARED` views,
and exchanges ordered descriptors plus high-water marks instead of copying 4K
payloads between processes. With pinned CPUs 4 and 5, 10 million 4K-logical
records, 2048-record batches, and an 8192-slot window:

- `touch=none`: 82.7 million logical 4K IOPS, no payload touches, no voluntary
  context switches.
- `touch=cacheline`: 64.9 million logical 4K IOPS, 1.28 GB write touch plus
  1.28 GB read touch, no context switches.
- `touch=full`: 10.8 million logical 4K IOPS, 40.96 GB write touch plus
  40.96 GB read touch, about 41 GiB/s per direction, no voluntary context
  switches.

The 2026-07-08 multi-lane run in
`bench-results/local-shmlease-lane-sweep-20260708T132909Z` uses one memfd arena,
one primary fan thread, and one secondary leaf thread per lane. With CPUs
`0-15`, 5 million records per lane for `none` and `cacheline`, and 1 million
records per lane for `full`, the measured logical 4K IOPS were:

| lanes | `touch=none` | `touch=cacheline` | `touch=full` |
| ---: | ---: | ---: | ---: |
| 1 | 79.7M | 60.7M | 9.3M |
| 2 | 180.0M | 134.4M | 17.9M |
| 4 | 356.9M | 255.2M | 6.7M |
| 8 | 713.5M | 301.6M | 5.8M |

Descriptor-only same-host leasing scales almost linearly through 8 lanes, and
cacheline inspection scales well through 4 lanes before flattening. Full 4K
cacheline sweeps stop scaling once the aggregate payload footprint exceeds the
cache-friendly regime; alternate 4-lane CPU maps changed the absolute result but
not the conclusion. That is the local copy stack we should preserve in the real
fan path: descriptors and ownership transfer can be lightning fast, while broad
payload touches must be avoided unless the work genuinely requires reading or
modifying the whole body.

`zcfanout-shmlease-bench ... write-read-same` adds the local dirty-cache
foreground behavior. The primary fan lane writes payload leases into its arena,
publishes mirror work to the secondary leaf lane, immediately serves a same-sector
read from the dirty lease by reference, and drains the leaf HWM only at arena
window pressure or an explicit sync interval. With `touch=cacheline`, 5 million
write/read-same pairs per lane, 2048-record batches, 8192-slot windows, and
`sync_records=0` (EOF sync only), the 2026-07-08 run in
`bench-results/local-shmlease-wrsame-sweep-20260708T133729Z` measured:

| lanes | logical read+write 4K IOPS | write branch Gbit/s | voluntary context switches |
| ---: | ---: | ---: | ---: |
| 1 | 155.2M | 5,085 | 0 |
| 2 | 329.6M | 10,801 | 0 |
| 4 | 619.9M | 20,315 | 0 |
| 8 | 732.4M | 23,998 | 0 |

On the same 4-lane shape, forcing sync every 2048 records reduced the result to
465.6M logical read+write IOPS; sync every 8192 records reached 537.7M; sync
every 65536 records reached 631.0M. The important architectural point is not the
synthetic headline rate, but the boundary: dirty-cache read visibility and
ordered mirror HWM tracking remain cheap when they exchange descriptors and
leases, while making every small batch a sync injects measurable foreground
waiting.

After adding explicit copy counters, the short
`bench-results/local-shmlease-current-repeat-20260708T153437Z` smoke repeated
the 4-lane `write-read-same` lease shape on CPUs `0-7`. Cacheline-touch repeats
landed at 494-514M logical 4K IOPS with zero voluntary context switches and
`payload_copy_bytes=0`. Full-payload-touch repeats landed at 15.1-15.8M logical
4K IOPS, about 493-516 Gbit/s of mirrored branch payload, still with
`payload_copy_bytes=0`. Use the cacheline number as the descriptor/ownership
ceiling and the full-touch number as the local memory-bandwidth ceiling.

The 2026-07-08 refresh in
`bench-results/local-shmlease-refresh-20260708T163147Z` and
`bench-results/local-shmlease-onramp-refresh-20260708T163213Z` reconfirmed the
gap between loopback WAL and same-host shared-memory transports on the busy
local box:

| control | lanes | logical 4K IOPS | first-hop Gbit/s | copy bytes | voluntary cs |
| --- | ---: | ---: | ---: | ---: | ---: |
| `zcnblk-shm-onramp` cacheline | 4 | 445.1M | 7292 | 0 | 0 |
| `zcnblk-shm-pipeline` cacheline | 4 | 263.9M | 4324 | 0 | 0 |
| `zcnblk-shm-mirror` no fan/leaf payload touch | 4 | 339.0M | 5555 | 0 | 0 |
| `zcnblk-shm-mirror` cacheline | 4 | 286.5M | 4694 | 0 | 0 |

The zcnblk shared-memory controls now also support random logical pages by
encoding the page in the zcnblk request token instead of recomputing placement on
every fan/drain step. With 4 lanes, 500K records per lane, 2048-record batches,
8192-slot windows, `sync_records=8192`, `touch=cacheline`, and a 65,536-page
working set, `bench-results/local-shmlease-random-zcnblk-decode-20260708T164221Z`
measured on the busy local box:

| control | logical 4K IOPS | first-hop Gbit/s | mirror ref Gbit/s | copied/materialized bytes | voluntary cs |
| --- | ---: | ---: | ---: | ---: | ---: |
| `zcnblk-shm-onramp` random | 249.3M | 4085 | n/a | 0 | 3 |
| `zcnblk-shm-pipeline` random | 188.8M | 3094 | n/a | 0 | 0 |
| `zcnblk-shm-mirror` random | 168.1M | 2755 | 5510 | 0 | 0 |

The dirty table was 2 MiB for 65,536 pages across 4 lanes, and both
`read_cache_materialized_bytes` and `materialized_payload_copy_bytes` stayed at
zero. Treat these as local control numbers, not remote throughput numbers; the
important result is that random dirty read visibility, fan/leaf HWM ownership,
and mirror branch descriptors still run without payload copies or context-switch
churn.
An immediate repeat saved in
`bench-results/local-shmlease-random-zcnblk-repeat-20260708T164800Z` moved the
absolute rates to 230.9M, 190.7M, and 172.6M logical 4K IOPS for onramp,
pipeline, and mirror respectively, while `payload_copy_bytes=0`, no migrations,
explicit `lane_cpu_map=...`, and 1-2 voluntary context switches stayed intact.
That is the expected interpretation on a shared local machine: use repeats to
separate host contention from architectural regressions.

The production fan WAL dirty cache had a separate 4K random-path bug. Before the
fix, exact 4K leased writes entered the general extent BTreeMap admission path.
Replacing an overlapping random 4K extent rebuilt the shard map, so even the
direct-lookaside experiment could not help: the local
`bench-results/local-zcnblk-dirty-cache-direct-small-20260708T170119Z` control
managed only about 8.9-9.0K logical 4K IOPS for 20K records/lane and incurred
33K-37K voluntary context switches.

The fix is to admit exact 4K copy/lease/memfd writes into the record cache path
and skip extent-map locks when no extents exist. Larger WAL extents still use the
extent map, and reads choose the highest-sequence record or extent when both are
present. The same small control in
`bench-results/local-zcnblk-dirty-cache-recordpath-20260708T170529Z` jumped to
47.7-48.4M logical 4K IOPS. Longer cacheline-touch repeats in
`bench-results/local-zcnblk-dirty-cache-recordpath-repeat-20260708T170615Z`
were tight at 60.1M and 60.5M logical 4K IOPS, with `payload_copy_bytes=0`,
`materialized_read_bytes=0`, no migrations, and about 529-551 voluntary context
switches across 8M logical operations. The optional direct table was slower
after this fix (about 52.7M vs 60.5M in the cacheline A/B), so it remains a
diagnostic knob rather than the recommended path.
The materialized fallback path now uses the same sequence-aware resolver as the
leased descriptor path, so an older covering extent cannot hide a newer exact 4K
record. The post-cleanup repeat in
`bench-results/local-zcnblk-dirty-cache-overlay-repeat-20260708T171136Z` held at
60.7-61.3M logical 4K IOPS with `payload_copy_bytes=0`,
`materialized_read_bytes=0`, no migrations, and 551-566 voluntary context
switches across 8M logical operations. Because this was on a shared local host,
the repeat band matters more than any single headline number.

That is the target shape for same-host client/fan and fan/leaf links: a
descriptor stream plus ownership/HWM transfer over shared arenas, not loopback
TCP. The production WAL path must retain the same ordering, dirty-cache, and
mirror-HWM contracts while swapping the local transport from socket payload
copies to shared arena leases.

The 2026-07-08 refresh in
`bench-results/local-shm-vs-tcp-refresh-20260708T173047Z` reran the same
four-lane shared-memory ladder after the TCP fan path was corrected to use
512-record/1 MiB extents. The local TCP fan smoke still topped out around
118.7-120.0 Gbit/s and 3.62-3.66M logical 4K IOPS on four lanes, while the
same zcnblk-shaped shared-memory controls stayed two orders of magnitude higher
with zero payload copies:

| control | logical 4K IOPS reps | first-hop Gbit/s reps | leaf/mirror ref Gbit/s reps | payload copies | voluntary cs |
| --- | --- | --- | --- | ---: | ---: |
| TCP fan WAL, `local:zcleasemem`, 4 lanes | 3.62M, 3.62M, 3.63M | 118.7, 119.3, 120.0 | mirror leases in-process | 0 | 571-577 |
| `zcnblk-shm-onramp` cacheline | 534M, 557M, 560M | 8756, 9128, 9183 | n/a | 0 | 0 |
| `zcnblk-shm-pipeline` cacheline | 240M, 240M, 249M | 3928, 3936, 4081 | 3928, 3936, 4081 | 0 | 0 |
| `zcnblk-shm-mirror` cacheline | 246M, 271M, 274M | 4029, 4443, 4486 | 8057, 8885, 8972 | 0 | 0 |
| `zcnblk-shm-mirror` descriptor-only fan/leaf | 346M, 347M, 352M | 5663, 5693, 5762 | 11326, 11386, 11525 | 0 | 0 |

This is the actionable split. Same-host colocated stages should use
shared-memory descriptor rings with HWM ownership transfer. Inter-host stages
should keep the same top-level descriptor/placement/dirty-cache contract and
swap only the payload lease implementation to TCP send-zc, registered-memory
NIC sends, or RDMA/libfabric RMA. Trying to make local loopback TCP behave like
the shared-memory ladder is the wrong optimization target.

`workload=zcnblk-shm-onramp` is the current first-hop same-host prototype. It
uses the same memfd arena but changes the roles to `client-onramp` and
`fan-ingress`: the client publishes zcnblk-shaped write/read records, the fan
consumes them in order and publishes read responses, and the client waits only
for arena pressure, `sync_records`, or EOF. It intentionally does not make RAID
placement decisions; it proves the local client/fan boundary can obey the WAL
ordering/HWM shape without loopback TCP payload copies.

The shared-memory zcnblk WAL record is now treated as a contract instead of an
ad hoc benchmark token. The request token carries the logical page, the
descriptor carries payload offset/length/checksum, every onramp/fan/pipeline
role validates the same decoded token and payload length, and response checksums
are tied back to that descriptor. Slot reuse remains controlled only by the
published HWM, so same-host payload ownership can move to a fan, leaf, or dirty
pool without materializing a 4K read buffer.

The same controls now report `shm_wal_contract_version=1` plus explicit avoided
TCP-copy counters. In
`bench-results/local-shm-contract-ledger-20260708T1754Z`, the two-lane onramp
run reported `client_fan_socket_payload_copy_bytes_lower_bound=0` and
`client_fan_tcp_socket_payload_copy_avoided_bytes=1638400000`; the two-lane
mirror run reported zero socket-copy lower bound on both the client/fan and
fan/leaf legs, with `fan_leaf_tcp_socket_payload_copy_avoided_bytes=819200000`.
The mirror run stayed at `payload_copy_bytes=0`, zero voluntary context switches,
and zero migrations. The onramp run had one involuntary context switch on this
busy local host, so it is counter-shape evidence rather than a representative
latency result.

The short wait-latency smoke in
`bench-results/local-shm-wait-latency-20260708T180242Z` adds explicit
`*-wait-latency-summary` lines to the same controls. The true low-sync onramp
case used `batch_records=1 sync_records=1` on CPUs `0-3` and repeated at
18.9M/18.3M logical 4K IOPS with 40K sync waits, zero context switches, and
`sync_wait_p50_ns=200`. The batched onramp case reached 121M logical 4K IOPS and
reported both sparse `sync_wait_*` and `dirty_window_wait_*` histograms. The
two-lane pipeline low-sync smoke on CPUs `0-5` reached 9.3M logical 4K IOPS with
separate `fan_leaf_sync_wait_*` counters. The two-lane mirror low-sync smoke on
CPUs `0-7` reached 8.9M logical 4K IOPS with zero context switches and the same
fan/leaf latency split. These are short same-host accounting smokes on a busy
development machine; use them to verify the topology, copy ledger, and HWM wait
shape, then repeat longer runs on quiet or remote hosts before treating absolute
IOPS as representative.

The short run in
`bench-results/local-zcnblk-shm-onramp-repeat-20260708T154104Z` used CPUs `0-7`
as `laneN:client_cpu(2N)->fan_cpu(2N+1)`, 4 lanes, 2048-record batches, and an
8192-record window:

| mode | records/lane | logical read+write 4K IOPS | first-hop logical Gbit/s | payload copies | voluntary cs |
| --- | ---: | ---: | ---: | ---: | ---: |
| metadata | 5M | 2.43-2.53B | 39,862-41,375 | 0 | 0 |
| cacheline | 5M | 845-895M | 13,848-14,659 | 0 | 0 |
| full payload | 1M | 14.4-15.7M | 236-257 | 0 | 0 |

With cacheline touches and 2M records/lane, sync every 2048 records reached
584M logical IOPS; sync every 8192 reached 610M; sync every 65536 reached 649M.
The gap versus the integrated TCP fan path is therefore architectural, not
inherent to zcnblk read/write/sync ordering: the first-hop local handoff needs
to become a shared/registered-memory descriptor transport when colocated.

`bench-results/local-shmlease-ladder-20260708T1815Z` reran the same-host
descriptor/HWM ladder with 1, 2, and 4 lanes and two touch modes. Descriptor-only
onramp averaged about 166M, 356M, and 652M logical 4K IOPS. Descriptor-only
pipeline averaged about 134M, 248M, and 486M logical 4K IOPS. Descriptor-only
mirror averaged about 115M, 222M, and 436M logical 4K IOPS, with zero payload
copy bytes and near-zero context switching. Cacheline-touch runs were lower but
still far above the integrated loopback TCP path. The ladder is intentionally a
repeatable local control on a busy host: accept it as evidence about descriptor
topology and HWM mechanics, not as a final inter-host throughput result.

`bench-results/local-shmlease-copyledger2-20260708T183851Z` reran the focused
4-lane same-host ledger after adding explicit descriptor/reference/touch fields.
The host load was nonzero, so these are consistency numbers, but the two repeats
were tight and used physical CPUs `0-15` without SMT overlap:

| workload | touch | logical 4K IOPS | first-hop Gbit/s | branch/ref Gbit/s | payload refs | descriptor bytes | payload touches | payload copies | voluntary cs |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| onramp | none | 715-721M | 11,726-11,808 | 11,726-11,808 | 16.4 GB | 256 MB | 0 | 0 | 0 |
| onramp | cacheline | 337-349M | 5,519-5,720 | 5,519-5,720 | 16.4 GB | 256 MB | 1.024 GB | 0 | 0 |
| pipeline | none | 517-518M | 8,480-8,485 | 8,480-8,485 | 16.4 GB | 512 MB | 0 | 0 | 0 |
| pipeline | cacheline | 218-221M | 3,568-3,613 | 3,568-3,613 | 16.4 GB | 512 MB | 1.536 GB | 0 | 0 |
| mirror | none | 458-468M | 7,509-7,667 | 15,019-15,334 | 32.8 GB | 768 MB | 0 | 0 | 0-1 |
| mirror | cacheline | 225-226M | 3,681-3,704 | 7,363-7,408 | 32.8 GB | 768 MB | 2.048 GB | 0 | 0-1 |

For the 4-lane mirror cacheline case, the memory stack per repeat was: client
writes one ingress payload stream; fan and both leaves intentionally touch only
cachelines for verification; branch payloads are represented as 32.8 GB of
descriptor references; descriptor/control metadata is 768 MB; and
`payload_copy_bytes=0`. The computed
`local_memory_traffic_bytes_lower_bound` was 2.816 GB, which is descriptor
metadata plus intentional payload touches, not the logical branch-reference
volume. That is the shape the colocated path should keep while the inter-host
path swaps memfd/shared-memory references for registered-memory, send-zc, or
RDMA extents.

The production dirty-cache isolate
`bench-results/local-dirtycache-copyledger-20260708T183605Z` held 60.9-61.6M
logical 4K IOPS across three repeats on CPUs `0-3`, with random write/read-same
coverage over a 65,536-page working set per lane. It reported
`payload_copy_bytes=0`, `materialized_read_bytes=0`, and read references around
998-1009 Gbit/s. The dirty-cache path is therefore much slower than raw
descriptor rings but still not copying or materializing read payloads; its cost
is lookup/admission machinery, not the memory handoff contract.

`bench-results/local-shmlease-dirtybridge-20260708T185030Z` then inserted the
general dirty cache into the 4-lane shared-memory mirror fan. It held only
94.6-96.4M logical 4K IOPS and 1.03-1.05 Tbit/s first-hop equivalent while the
synthetic mirror control was 426-429M IOPS. The copy ledger still showed
`payload_copy_bytes=0`; the loss came from per-record hash-map/lock/Arc churn
and per-record commit, not payload movement.

`bench-results/local-shmlease-dirtyhwm-20260708T185531Z` replaced that bridge
with the lane-local `arena-hwm` dirty cache. This cache indexes the owned arena
slot by the linear WAL/HWM contract, rejects slot reuse before commit, and keeps
one arena owner per lane instead of cloning an `Arc` per record. Two repeats on
physical CPUs `0-15` reached 384.8M and 395.2M logical 4K IOPS with
`payload_copy_bytes=0`, zero context switches, and 4.20-4.32 Tbit/s
first-hop-equivalent. The old `URING_PLAY_SHMLEASE_DIRTY_CACHE_MODE=general`
comparison in `bench-results/local-shmlease-dirty-general-20260708T185542Z`
landed at 74.0M IOPS. The lesson is sharp: the same-host fast path should use
HWM ownership and lane-local arenas for hot linear WAL traffic, while the
general associative dirty cache remains the correctness path for random
lookaside cases until it gets its own lane-local associative design.

The same day, the TCP fan-WAL read response path was corrected so segmented
leaf read results are returned as ordered lease parts instead of being copied
into a fan-owned materialized buffer. The strict local repeat artifacts
`bench-results/local-fanwal-vs-shm-3lane-leasefix-20260708T1830Z` and
`bench-results/local-fanwal-vs-shm-3lane-leasefix-repeat-20260708T1835Z` both
used 3 lanes because strict topology rejects the 4-lane local plan on this
machine: 4 TCP lanes require 20 stage CPUs and would pair some roles on SMT
siblings. Both verified write-sync-read over TCP with
`read_cache_leased_bytes=100663296`, `read_cache_materialized_bytes=0`, and
`materialized_payload_copy_bytes=0`. Client-facing throughput was 20.0-20.7
Gbit/s, or 0.61-0.63M logical 4K ops/s, while the fan still took about
6.1K voluntary context switches. That makes the current local TCP bottleneck the
blocking socket/result path and scheduler churn, not read materialization.

The next local sweep kept strict zero-copy and changed only scheduling,
placement, and framing knobs. `bench-results/local-fanwal-spin-*-
20260708T1900Z` showed that bounded fan result spinning cuts fan voluntary
context switches from about 6.6K to 316-374, but barely changes throughput, so
context-switching was no longer the first limiter. The
`bench-results/local-fanwal-readpolicy-*-20260708T1915Z` runs then switched
mirrored reads from 4K replica striping to extent routing. That reduced read
response source parts from 24,576 to 96 and raised the strict TCP-leaf path from
about 20 Gbit/s to 25-26 Gbit/s.
`bench-results/local-fanwal-ingress-memfd-coalesce1m-20260708T1925Z` added
ingress memfd payload leasing plus 1 MiB memfd-send coalescing; the short strict
run reached 31.1 Gbit/s and 0.90M logical 4K ops/s, with
`async_copied_payload_bytes=0`, `read_cache_materialized_bytes=0`, and leaf heap
payload bytes still zero. Longer 128 MiB-per-lane runs in
`bench-results/local-fanwal-extent-*-20260708T1935Z` held around 30-32 Gbit/s as
the external TCP leaf socket path became the main local limiter.

Removing that terminal loopback leg with in-process `local:zcleasemem` leaves
keeps the client-facing zcnblk/TCP edge but changes the fan-to-leaf transport to
a shared arena.
`bench-results/local-fanwal-localleaf-4lane-8m-cleanmap-20260708T2045Z`
passed strict zero-copy with 4 lanes, 8 MiB extents, extent mirror reads,
ingress memfd payloads, and no external leaf CPU stages
(`fan_leaf_cpu_lists=in-process-local-leaves`). The run reached 67.8 Gbit/s at
the fan and 1.98M logical 4K ops/s, with zero materialized/copy fallback bytes,
32 read response source parts, and zero response `writev` calls. The remaining
dominant phase is still client-to-fan TCP payload receive, so the next
architectural step is replacing same-host client/fan TCP with the shared-memory
descriptor onramp or using an inter-host registered-memory/RDMA transport.
The 2026-07-08 retake with 4 lanes, `write-sync-read`, `local:zcleasemem:4G`
mirror leaves, and strict zero-copy showed the same copy accounting:
fan-to-leaf writeback leased payloads with `async_copied_payload_bytes=0` and
`read_cache_materialized_bytes=0`, but the client/fan TCP edge still imposed a
2 GiB socket-copy lower bound for 1 GiB writes plus 1 GiB reads. The worker
writeback path reached about 77.8 Gib/s and 2.36M logical 4K ops/s with roughly
4.5K fan context switches. Enabling
`URING_PLAY_ZCNBLK_FAN_LOCAL_INLINE_WRITEBACK=1` removed the local writeback
worker wakeups, reduced fan context switches to roughly 3.5K, and reached about
80.1 Gib/s and 2.43M logical 4K ops/s. Disabling read verification raised that
to about 96.6 Gib/s and 2.95M logical 4K ops/s, proving sender-side verification
is a measurable memory-touch cost, while the remaining architectural gap is
still the TCP socket boundary rather than the local leaf lease path.

The split copy ledger added on July 8 makes that boundary explicit. Two longer
shared-host repeats in
`bench-results/local-fanwal-copyledger-split-long-20260708T194608Z-rep{1,2}`
used 4 lanes, 512 MiB per connection, `write-sync-read`,
`local:zcleasemem:4G` mirror leaves, extent reads, ingress memfd payloads, and
inline local writeback. They held 68.0-68.8 Gbit/s at the fan with
`materialized_payload_copy_bytes=0`,
`fan_leaf_socket_payload_copy_bytes_lower_bound=0`, and
`local_leaf_socket_payload_copy_avoided_bytes=2147483648`. The remaining
`all_socket_payload_copy_bytes_lower_bound=2147483648` split evenly:
about 34.0-34.4 Gbit/s of client-to-fan ingress socket payload copy and
34.0-34.4 Gbit/s of fan-to-client read-response socket payload copy. The
preflight still found powersave governors, busy mapped CPUs, low memlock, and
no hugepages, so use these as a copy-path diagnosis, not a final representative
ceiling.

The integrated random 4K `write-read-same` path now preserves large upstream
request batches and cached read responses through the same local shared-arena
leaf topology. The useful artifacts are
`bench-results/local-fanwal-random-dirtycache-4lane-64m-nosplit-bulkverify-20260708T210919Z`
and the no-verify repeats
`bench-results/local-fanwal-random-dirtycache-4lane-64m-nosplit-noverify-repeat-20260708T211009Z`
and
`bench-results/local-fanwal-random-dirtycache-4lane-64m-nosplit-noverify-repeat2-20260708T211415Z`.
Both used 4 lanes, `BATCH_DEPTH=512`, `SEND_WRITE_WINDOW=8192`,
`SEND_READ_WINDOW=8192`, `SEND_OP=write-read-same`, `SEND_ACCESS=random`,
`SEND_RANDOM_RANGE_BYTES=32M`, `WRITE_EXTENTS=0`, `READ_EXTENTS=0`,
`local:zcleasemem:512M` mirror leaves, memfd dirty cache, ingress memfd payloads,
inline local writeback, and strict zero-copy validation. The verified run reached
98.8 Gbit/s at the sender and 102.4 Gbit/s at the fan; the no-verify repeats
held 101.3-101.6 Gbit/s at the sender and 103.8-103.9 Gbit/s at the fan. The fan
saw 128 upstream request batches, 512 maximum logical records per upstream batch,
256 leaf submit batches, and 128 dirty admit/release events instead of per-4K
cache churn.

That repeat accounts for the current same-host memory path. The sender generated
and wrote 128 MiB of request payload and drained 128 MiB of read responses. The
fan reported 128 MiB of client-to-fan ingress socket payload movement and
128 MiB of fan-to-client read-response socket movement, while avoiding 256 MiB
of fan-to-leaf socket payload movement with shared-arena leaves. WAL-stage copy
fallbacks stayed at zero: `async_copied_payload_bytes=0`,
`read_cache_materialized_bytes=0`, `read_cache_fallback_materialized_bytes=0`,
and `materialized_payload_copy_bytes=0`. `leased_payload_reference_bytes` was
384 MiB because mirror writeback and cached reads move ownership by descriptor.
Fan context switches were 35-52 voluntary and 4 involuntary in the no-verify
repeats; sender voluntary context switches were 127-130. The host preflight still
warned about powersave governors, busy mapped CPUs, low memlock, no hugepages,
and socket buffer caps, so the absolute throughput is not a representative
ceiling. The architectural conclusion is narrower and stronger: random
read-after-write semantics are now batched and zero-copy through the fan/leaf WAL
stages, and the remaining same-host production gap is the TCP client/fan edge.

The next control, `workload=zcnblk-shm-pipeline`, connects the local onramp to a
single leaf-stage through the fan. Each lane has three pinned roles:
`client-onramp -> fan-stage -> leaf-stage`. The client writes payload once into
the ingress arena, the fan publishes a leaf descriptor that references that same
ingress payload, and the leaf consumes it by reference. Client-visible response
progress and ingress slot reuse are released only after the leaf HWM has
consumed the referenced payload; otherwise the client could reuse an ingress
slot while a leaf descriptor still points at it. This is still not a RAID
primitive; it is the single-leaf transport/control proof needed before plugging
the same handoff into userspace mirror or stripe placement.

The first pipeline run,
`bench-results/local-zcnblk-shm-pipeline-repeat-20260708T154827Z`, published
client responses before leaf payload consumption and is superseded. The
HWM-safe run in
`bench-results/local-zcnblk-shm-pipeline-hwm-repeat-20260708T155208Z` used CPUs
`0-11` as `laneN:client_cpu(3N)->fan_cpu(3N+1)->leaf_cpu(3N+2)`, 4 lanes,
2048-record batches, an 8192-record window, and
`client_response_release=leaf-hwm`:

| mode | records/lane | logical read+write 4K IOPS | first-hop Gbit/s | leaf ref Gbit/s | payload copies | voluntary cs |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| metadata | 2M | 855-867M | 14,011-14,209 | 14,011-14,209 | 0 | 0 |
| cacheline | 2M | 307-319M | 5,032-5,225 | 5,032-5,225 | 0 | 0 |
| full payload | 500K | 13.2-13.8M | 216-225 | 216-225 | 0 | 0 |

With cacheline touches and 1M records/lane, sync every 2048 records reached
201M logical IOPS; sync every 8192 reached 275M; sync every 65536 reached 263M.
The combined local path is slower than the first-hop-only control because the
fan now also sequences leaf HWM results, the client response/reuse HWM waits on
leaf consumption, and the payload is touched at the client, fan, and leaf roles.
It is still orders of magnitude above the integrated loopback TCP path and keeps
`payload_copy_bytes=0`, so the next integration target is clear: replace
colocated zcnblk client/fan and fan/leaf socket legs with
registered/shared-memory descriptor rings while keeping the same WAL ordering
contract.

`workload=zcnblk-shm-mirror` is the HWM-safe two-leaf userspace mirror control.
Each lane runs four pinned roles:
`client-onramp -> fan-mirror -> leaf0-stage + leaf1-stage`. The client writes
the payload once into the ingress arena. The fan publishes two leaf descriptors
that reference that same ingress payload, one per mirror leg. Client-visible
response progress and ingress slot reuse advance only at the minimum leaf HWM,
so a descriptor cannot outlive the payload slot it references. No block device
participates in mirror placement, and no payload copy is allowed on the mirror
branch path.

The 2026-07-08 repeat in
`bench-results/local-zcnblk-shm-mirror-repeat-20260708T155937Z` used CPUs
`0-15` as `laneN:client_cpu(4N)->fan_cpu(4N+1)->leaf0_cpu(4N+2)+leaf1_cpu(4N+3)`,
4 lanes, 2048-record batches, and an 8192-record window:

| mode | records/lane | logical read+write 4K IOPS | first-hop Gbit/s | mirror ref Gbit/s | payload copies | voluntary cs |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| metadata | 2M | 659-740M | 10,803-12,129 | 21,606-24,259 | 0 | 0-1 |
| cacheline | 2M | 333-354M | 5,463-5,803 | 10,926-11,606 | 0 | 0-1 |
| full payload | 500K | 12.4-12.9M | 203-212 | 406-423 | 0 | 0 |

With cacheline touches and 1M records/lane, sync every 2048 records reached
222M logical IOPS; sync every 8192 and 65536 records both reached about 276M.
These are same-host shared-memory controls on a busy host, not inter-host NIC
numbers. The result says that mirror placement, dual branch descriptor
publication, minimum-leaf-HWM release, and local same-sector read visibility can
run with zero payload copies and almost no voluntary context switching when the
payload stays in an owned arena.

The July 8 HWM-only result experiment added
`URING_PLAY_SHMLEASE_HWM_ONLY_RESULTS=1` for the zcnblk shared-memory onramp,
pipeline, and mirror controls. In this mode the downstream stages still publish
ordered HWMs and still gate payload slot reuse on leaf consumption, but they do
not manufacture per-record result descriptors for write-heavy range completion.
Sequential local A/B runs in
`bench-results/local-shmlease-hwm0-seq-20260708T200001Z` and
`bench-results/local-shmlease-hwm1-seq-20260708T200009Z` used the same 4-lane
CPU map, 2048-record batches, and 8192-record sync cadence:

| case | HWM-only | logical 4K IOPS avg | first-hop Gbit/s avg | branch/ref Gbit/s avg | fan-leaf sync p50 |
| --- | --- | ---: | ---: | ---: | ---: |
| mirror metadata | no | 444M | 7,273 | 14,546 | 23 us |
| mirror metadata | yes | 740M | 12,127 | 24,255 | 3 us |
| mirror cacheline | no | 222M | 3,635 | 7,270 | 29 us |
| mirror cacheline | yes | 275M | 4,505 | 9,010 | 7 us |
| mirror dirty-cache metadata | no | 494M | 5,393 | 10,787 | 23 us |
| mirror dirty-cache metadata | yes | 667M | 7,280 | 14,560 | 3 us |
| mirror dirty-cache cacheline | no | 286M | 3,125 | 6,251 | 30 us |
| mirror dirty-cache cacheline | yes | 334M | 3,652 | 7,305 | 8 us |

This does not remove client-edge per-request completion work in a real block
front end; it proves that the remote userspace RAID/WAL stages should exchange
range HWMs for write-heavy paths and reserve per-record result descriptors for
actual read payloads or debug checksum validation.

A later July 8 clean-window ladder used 10M records/lane, 4 lanes,
`BATCH_RECORDS=2048`, `WINDOW=65536`, `PAYLOAD_SLOTS=65536`,
`SYNC_RECORDS=65536`, and `URING_PLAY_SHMLEASE_HWM_ONLY_RESULTS=1`:

| case | payload touch policy | logical 4K IOPS avg | first-hop Gbit/s avg | branch/ref Gbit/s avg | payload copy | observed payload touch/run | local mem lower bound/run | dirty drains |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pipeline | descriptor-only | 350M | 5,732 | 5,732 | 0 | 0 B | 5.12 GB | 0 |
| mirror | descriptor-only | 837M | 13,713 | 27,426 | 0 | 0 B | 7.68 GB | 0 |
| pipeline | client+fan+leaf cacheline | 130M | 2,129 | 2,129 | 0 | 15.36 GB | 20.48 GB | 0 |
| mirror | client+fan+leaves cacheline | 117M | 1,913 | 3,826 | 0 | 20.48 GB | 28.16 GB | 0 |
| pipeline | client cacheline only | 183M | 2,994 | 2,994 | 0 | 5.12 GB | 10.24 GB | 0 |
| mirror | client cacheline only | 180M | 2,951 | 5,902 | 0 | 5.12 GB | 12.80 GB | 0 |

Artifacts:
`bench-results/local-shmlease-clean-window-long-20260708T203639Z`,
`bench-results/local-shmlease-clean-window-cacheline-20260708T203706Z`, and
`bench-results/local-shmlease-clean-window-clienttouch-20260708T203748Z`.
These were busy-host, non-representative runs: powersave governors, busy mapped
CPUs, low memlock, and no hugepages were all flagged. The stable lesson is not
the absolute number; it is the copy ledger. Descriptor/HWM sequencing stays
fast with zero payload copies, while fan/leaf payload inspection adds enough
memory traffic to collapse the control path. The production mirror/stripe hot
path should therefore avoid payload reads in fan stages and leaf forwarding
stages unless an explicit verification or transform mode requested them. Use
`URING_PLAY_SHMLEASE_ZERO_COPY_FORWARDING_TARGET=1` for direct bench runs, or
`ZERO_COPY_FORWARDING_TARGET=1` with the ladder wrapper, so strict topology mode
fails if `FAN_TOUCH_PAYLOAD` or `LEAF_TOUCH_PAYLOAD` accidentally enables this
payload-inspection control path during a representative forwarding run.

The TCP fan WAL path has the same range-HWM completion option via
`URING_PLAY_ZCNBLK_FAN_HWM_ONLY_RESULTS=1`. It applies only to write-only
zero-payload range result batches from leaves: each leaf branch says "all
records in this submitted range are complete", then the fan validates the total
segment count and completes the ordered write batch without rebuilding per-4K
result counters. If the leaf returns descriptor results, the run fails instead
of silently falling back. Reads still use payload-bearing result descriptors or
leases because the fan must zip data references back into the upstream stream.

The copy/touch ledger for this local mirror shape is:

- Client onramp writes the payload into the ingress arena once.
- Fan reads the ingress descriptor and publishes branch descriptors; it does not
  allocate or copy branch payload buffers.
- Leaf0 and leaf1 consume the branch descriptors and read the original ingress
  payload by reference.
- The summary's `payload_copy_bytes=0` means there is no CPU memcpy-equivalent
  branch copy; `payload_write_touch_bytes` and `payload_read_touch_bytes` show
  intentional benchmark touches only.
- Full-payload mode is the local memory-bandwidth ceiling because every logical
  write causes one full client payload write plus full fan/leaf payload reads.

The follow-up run in
`bench-results/local-zcnblk-shm-mirror-touchpolicy-20260708T160726Z` made fan
and leaf payload inspection explicit with
`URING_PLAY_SHMLEASE_FAN_TOUCH_PAYLOAD` and
`URING_PLAY_SHMLEASE_LEAF_TOUCH_PAYLOAD`. Defaults preserve the older verifying
behavior. Setting either knob to `0` makes that stage derive result metadata
from the descriptor/checksum fields and leaves the payload in the arena without
rereading it. HWM release, mirror placement, and slot ownership are unchanged.

| mirror case | fan touch | leaf touch | logical 4K IOPS | first-hop Gbit/s | payload read touch |
| --- | --- | --- | ---: | ---: | ---: |
| full inspect | yes | yes | 11.3-11.4M | 185-187 | 24.6 GiB |
| full fan-only | yes | no | 15.4M | 253 | 8.2 GiB |
| full leaf-only | no | yes | 13.2M | 217 | 16.4 GiB |
| full descriptor | no | no | 27.5-27.6M | 451-452 | 0 |
| cacheline inspect | yes | yes | 402-412M | 6,581-6,756 | 3.1 GiB |
| cacheline fan-only | yes | no | 423M | 6,936 | 1.0 GiB |
| cacheline leaf-only | no | yes | 419M | 6,862 | 2.0 GiB |
| cacheline descriptor | no | no | 525-554M | 8,595-9,073 | 0 |

The first-hop-only A/B in
`bench-results/local-zcnblk-shm-onramp-touchpolicy-20260708T160801Z` showed the
same pattern before mirror fanout: full-touch onramp measured 15.2-15.9M logical
IOPS when fan ingress reread payloads and 30.4-34.8M when it stayed
descriptor-only; cacheline onramp measured 634-650M with fan inspection and
818-830M without it. The production rule is therefore strict: ordinary write
admission and mirror forwarding must not inspect payload bytes unless a
configured verifier, transform, checksum, encryption stage, or terminal media
operation genuinely needs the body. Descriptor/lease forwarding is the baseline
path; payload inspection is an explicit paid feature.

For inter-host work the same logical ledger must be preserved with a different
transport substrate: receive into registered NIC-owned or ZCRX-owned memory,
publish descriptors over the WAL stream, avoid CPU payload copies on fanout, and
send from the registered receive/lease buffer whenever the NIC/provider supports
it. A same-host memfd reference is not portable across hosts; the inter-host
proof must therefore account separately for NIC DMA receive, any CPU payload
inspection, and NIC DMA send.

The 2026-07-08 local follow-up separated three concepts that were previously
conflated in the shared-memory control:

- `sync_records` is an explicit sync/HWM durability cadence.
- `ack_records` is write acknowledgement and ingress payload-slot release; it
  can advance without an explicit sync.
- `payload_slots_per_lane` is the hot payload ring size and does not have to
  equal the descriptor/HWM window.

This matters for same-host performance. With four physical-core lanes, mirror
dirty-cache, no fan/leaf payload inspection, `ack_records=8192`, and no periodic
sync, the compact hot-ring result was:

| descriptor window | payload slots | logical 4K IOPS | first-hop Gbit/s | mirror ref Gbit/s | notes |
| ---: | ---: | ---: | ---: | ---: | --- |
| 8192 | 8192 | 37.8-38.7M | 413-423 | 826-846 | best local repeat |
| 16384 | 8192 | 34.9-38.1M | 382-417 | 763-833 | noisy but plausible |
| 32768 | 8192 | 31.6-34.2M | 345-373 | 690-746 | descriptor footprint hurts |
| 65536 | 8192 | 31.1-33.3M | 339-364 | 679-728 | compact payload ring recovers most loss |
| 65536 | 65536 | 11.7-13.4M | 128-146 | 256-293 | huge hot payload ring collapses locality |

Pre-faulting the large 65,536-payload-slot arena did not recover performance,
which points at hot-ring footprint, TLB/cache locality, and descriptor working
set rather than first-touch page faults. The architectural rule for local
fan/mirror is therefore: keep descriptor rings and hot payload slots as small
and topology-local as the ack/sync contract permits, and put large dirty
capacity behind that as colder extents rather than making the hot ring huge.
Testing 8 mirror lanes across CPUs `0-31` required SMT sibling overlap and was
slower, around 22.8-24.7M logical IOPS and 249-269 Gbit/s first-hop, with
thousands of involuntary context switches; those results are explicitly
SMT-topology-specific and should not replace the four-physical-lane baseline.

The arena-HWM dirty-cache bridge now supports deterministic random working-set
pages for the fast same-host path. The cache metadata table is sized to at least
the working set so random logical pages do not collide before the global HWM can
release older payload ownership. Newer writes to the same page can replace older
uncommitted dirty entries; reads observe the latest dirty token while the global
HWM still controls when the ingress payload slot can be reused. The `general`
dirty-cache backend remains linear-only for this benchmark because its commit
path still marks linear offsets.

The smoke in
`bench-results/local-shmlease-ladder-working-set-smoke-20260708T204851Z`
validated the helper path with `WORKING_SET_RECORDS=256`. A longer busy-host
repeat in
`bench-results/local-shmlease-random-dirty-forwarding-20260708T204916Z` used
4 lanes, `WORKING_SET_RECORDS=65536`, `WINDOW=65536`,
`PAYLOAD_SLOTS=65536`, `SYNC_RECORDS=65536`,
`ZERO_COPY_FORWARDING_TARGET=1`, `FAN_TOUCH_PAYLOAD=0`,
`LEAF_TOUCH_PAYLOAD=0`, and HWM-only results:

| mode | logical 4K IOPS avg | first-hop Gbit/s avg | mirror ref Gbit/s avg | payload copies | observed payload touch/run | dirty drains | context switches avg |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| random dirty descriptor-only | 344M | 3,763 | 7,525 | 0 | 0 B | 0 | 9.3 |
| random dirty client cacheline | 188M | 2,052 | 4,104 | 0 | 2.56 GB | 0 | 76.3 |

The preflight marked every row non-representative because this was a shared
local host with powersave governors, busy mapped CPUs, low memlock, and no
hugepages. The useful result is functional and architectural: random
read-after-write dirty-cache semantics now run through the same zero-copy
forwarding contract, with no fan/leaf payload reads and no block-device RAID
primitive.

The real `/dev/zcnblk0` shared onramp now uses the same arena-HWM idea instead
of the generic hash/lock dirty cache. Shared ABI v2 separates a 128-entry hot
descriptor ring from a larger payload lease pool. The single-lane
`wal-memory` userspace stage keeps one preallocated metadata slot per logical
4K page, ACKs writes after reference admission, selects the latest dirty slot
before reduced RAM on reads, and materializes up to 2,048 ordered writes per
writeback batch. A fixed release bitmap advances payload ownership without a
tree allocation or lock on every I/O. Explicit `fsync(2)` becomes
`REQ_OP_FLUSH`, drains prior writes, and only then returns the sync completion.

The correctness artifact
`bench-results/local-zcnblk-shm-walmem-correctness-20260711T102640Z` verified
two writes to the same sector, dirty read-after-write after each overwrite, one
observed sync, and a matching post-sync read. The first generic-cache version
ran three 50/50 random 4K repeats at 2.248-2.346M IOPS. Replacing per-I/O
`Arc`/trait/hash/lock work with the direct HWM table reached 2.385-2.640M, and
replacing the release `BTreeSet` plus read-then-overlay copy reached 2.739M,
2.894M, and 2.897M IOPS. The same immediate-copy RAM control was
2.955-2.972M. These are noisy-host small-page controls, not representative
ceilings; the useful comparison is that the warm writeback/HWM path is within
about 2.4% of immediate copy while preserving dirty visibility and sync.

The latest run mapped client, target, and kernel roles to CPUs 0, 1, and 2.
Target and kernel context switching stayed at 0.001-0.004 switches per 1K I/O;
the client measured 0.560-0.934 per 1K. Multi-lane `wal-memory` remains fatal
at startup: per-sector locks alone do not prove global submit order or a
cross-lane sync HWM for overlapping requests. The next fan integration must
retain the fixed metadata/HWM fast path while moving placement and remote leaf
commit into the separate userspace RAID stage.

The first real TCP leaf integration keeps that split intact. `/dev/zcnblk0`
hands descriptors and leased payload slots to `zcnblk-shm-target`; the
co-located userspace stage owns the WAL protocol and sends to
`zcnblk-wal-leaf`. The kernel edge does not select a leaf and does not implement
mirror or stripe placement. Writes are coalesced into 2,048-record WAL batches
and sent from shared slots with vectored I/O. Cold reads use mixed FIFO request
windows: writes update the dirty-reference table, dirty reads snapshot the
correct preceding write, and cold reads across interleaved writes are gathered
into one leaf request batch. Leaf result payloads land directly in the shared
block output slots before FIFO completion publication.

This ordering contract is intentionally precise. Completion order is global
FIFO for the current one-channel edge. Data order is per-sector plus a global
sync HWM. A cold read collected before a later same-sector write observes the
old leaf value because that later write has not been applied remotely; a read
after an earlier same-sector write is answered from the dirty lease. Sync drains
all prior WAL writes, asks the terminal leaf to sync, and only then completes.
The 32-pair io_uring ordering artifact
`local-zcnblk-shm-tcp-leaf-concurrent-order-20260711T1110Z` verifies both
directions and post-sync state through the actual block edge.

Payload ownership is a second independent backpressure axis. The target flushes
when the write batch fills and when outstanding leases approach
`payload_entries - ring_entries`. The latter is required even when the write
count is below its threshold; a 50/50 workload otherwise can exhaust all payload
slots and stop the kernel producer. With that invariant restored, the noisy
local mixed QD128 control reached 1.211-1.293M 4K IOPS with 18-20 target context
switches per 1K I/O and about 0.3 kernel switches per 1K. This is a local
small-page control, not a network or hardware ceiling.

Dirty-cache mirror summaries after
`bench-results/local-shmlease-mirror-dirty-corrected-20260708T213027Z` separate
client-visible read I/O from the fan's internal dirty-cache probe. Use
`client_visible_read_records` for logical IOPS; `internal_dirty_cache_read_records`
is local cache bookkeeping and must not be counted as an extra block read. With
4 lanes, 500K records/lane, `WINDOW=8192`, `PAYLOAD_SLOTS=8192`,
`ACK_RECORDS=8192`, HWM-only results, zero fan/leaf payload touches, and
`client_response_release=local-dirty-cache`, the corrected busy-host repeats
were 305-337M client-visible logical 4K IOPS, 5.0-5.5 Tbit/s first-hop
descriptor-reference rate, 10.0-11.0 Tbit/s mirror-reference rate, and
`payload_copy_bytes=0`. The same preflight warnings applied, so this is a
same-host control and not a representative hardware limit.

`URING_PLAY_SHMLEASE_DIRTY_EARLY_RESPONSES=1` splits the mirror dirty-cache
clock: the client can observe unsynced write/read-same responses after local
dirty-cache admission, but ingress payload slot reuse remains gated by the
minimum mirror leaf HWM. A benchmark-only
`URING_PLAY_SHMLEASE_LEAF_BATCH_DELAY_NS` busy-spin delay can expose this
contract. In
`bench-results/local-shmlease-early-delay-20260708T213014Z`, 1 lane with 128
leaf batches delayed by 100 us let the client thread finish in 2.2-2.7 ms while
the leaf/fan HWM path finished in about 17 ms. With early responses disabled in
`bench-results/local-shmlease-hwmresp-delay-20260708T213014Z`, the client
thread waited on the leaf HWM and took 13-26 ms. That proves the intended
unsynced local-ACK behavior without weakening the payload ownership rule.

`workload=random-write-read-same` keeps the same fast lease path but chooses a
deterministic random logical page for each write/read pair and verifies that the
dirty table returns the latest descriptor token for that page. This validates
random placement lookup and latest-write semantics without materializing 4K read
payloads. On a busy local system, repeated `touch=cacheline` runs with a 65,536
record working set in
`bench-results/local-shmlease-random-dirty-sweep-20260708T134546Z` produced:

| lanes | records/lane | min IOPS | median IOPS | max IOPS |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 1M | 96.9M | 97.2M | 101.6M |
| 2 | 1M | 189.5M | 190.2M | 197.2M |
| 4 | 5M | 517.0M | 518.0M | 521.4M |
| 8 | 5M | 589.5M | 606.8M | 615.4M |

Treat local microbench numbers as contention-sensitive unless repeat runs show a
tight spread. The 8-lane case still varies more than the 4-lane case, which is
consistent with shared CPU/cache/memory pressure on a busy host.

That stacks the local memory path clearly: descriptor ordering is hundreds of
millions of records/sec scale, lease metadata plus small payload inspection is
tens of millions of IOPS scale, and full-payload ownership is bounded by memory
touch bandwidth rather than by the zipper. The same descriptor contract must be
used for inter-host transfers, but the payload lease changes shape: local peers
exchange memfd/page ownership, while remote peers exchange descriptors pointing
at NIC-readable registered memory, `send_zc` ranges, or RDMA RMA extents. A
remote fallback that materializes a heap copy is a benchmark failure unless it
is explicitly the experiment under test.
Production `zcnblk-fan --engine wal` and `zcnblk-wal-leaf` expose this as
`URING_PLAY_ZCNBLK_WAL_ZERO_COPY_STRICT=1`: strict runs may still report TCP
socket-copy lower bounds, but fan `materialized_payload_copy_bytes` and leaf
heap payload-data/copy-submit counters must stay at zero or the run fails
before the final throughput result is printed. Heap descriptor/control bytes are
reported separately and do not count as user-payload fallback.

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

For RAM-backed fan development, use `zcnblk-wal-leaf zcmem:SIZE`. It is a
NUMA-placeable userspace mmap arena with real read-after-write behavior and no
terminal block subsystem hop. `/dev/zcbrdN` remains useful as a terminal
block-media debug/control leaf, but it is not the fast RAM design point.

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

Current fan-WAL code maps the zcnblk client `REQ_OP_FLUSH` path to
`ZCNBLK_OP_SYNC`/`ZCNBLK_OP_SYNC_ACK`. In admission-ACK mode, ordinary write ACKs
mean the write was accepted into the bounded local writeback/WAL pipeline and
submitted to the userspace leaf streams. A sync drains the handler's local
outstanding queue, waits for the shared dirty budget to reach zero across all
handlers, sends a WAL sync to every leaf, and only then ACKs the flush upstream.
That makes sync the global high-water mark for the volatile window.

When async writeback is enabled, the fast path may defer unsynced leaf
submission until sync, EOF, or dirty-pressure drain. That does not weaken the
ordinary write contract: the write is visible after local dirty-cache admission,
and a sync still forces the deferred queue into the leaf streams, waits for
result HWMs, sends leaf sync records, and only then returns `SYNC_ACK`.

## Write-Back Buffer Budget

The high-performance mirror path assumes a bounded volatile write-back buffer
between ordinary writes and explicit sync/flush/FUA barriers. Ordinary write
ACKs mean accepted, ordered, and visible to later reads. They do not mean
durable media commit unless the request carries sync/FUA semantics or crosses an
explicit protocol sync.

Performance benchmarks should include at least one large-cache profile with
2 GiB or more of dirty payload capacity, and preferably larger pools on 400G+
hosts. At 600 Gbit/s, the logical ingress rate is about 75 GB/s, so 2 GiB covers
only about 27 ms of dirty data before backpressure must engage. This is enough
to prove batching, lane-local arenas, mirror fanout, and result zipping without
turning every 4K write into a remote durability round trip.

The budget is also the safety valve for deferred unsynced writeback. Below the
soft limit, foreground lanes can keep ACKing writes and serving same-sector
reads from dirty leases while the backend forms larger WAL extents. At pressure,
the fan must enqueue deferred writeback before draining outstanding leaf
results; at the hard limit, admission stops until replicated HWMs release dirty
bytes. A fast run that never reports dirty peak, wait events, and sync drain time
is missing the information needed to judge the architecture.

### Transferred Shared-Arena Pages

Lane-batched `wal-tcp` negotiates
`ZCNBLK_SHM_CAP_TRANSFER_PAYLOAD_SLOTS` at daemon attach. Each physical payload
page has an atomic owner token in the shared mapping. The kernel claims any free
page, fills it, changes the token to the request's global `submit_sequence`, and
publishes the descriptor with the physical `payload_slot`. The userspace dirty
cache and WAL sender retain that page by reference. After remote commit and
dirty-cache retirement, userspace returns a write page with a compare-and-swap
from the exact submit token to zero. Reads and syncs are returned by the kernel
only after the read payload has reached the bio or the completion has been
consumed.

This separates request sequence order from page lifetime. A current dirty value
no longer pins every later request across a circular HWM, stale releases cannot
free a page owned by a newer request, and independently committed pages return
to the ingress free pool without a payload copy. The legacy contiguous
`payload_lease_hwm` path remains available when ownership transfer is not
negotiated; it is a compatibility path, not the target WAL architecture.

`URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS=1` selects transfer mode for lane-batched
WAL. `URING_PLAY_ZCNBLK_SHM_DIRTY_PRESSURE_RESERVE` still reserves enough free
pages to admit reads and sync. Pressure now retires only the required number of
oldest remotely committed dirty pages instead of bulk-clearing a generation.
Summaries report `payload_ownership`, `payload_slots_free`,
`dirty_pressure_events`, `dirty_pressure_evictions`, and
`max_payload_slots_outstanding`. A clean shutdown must report every negotiated
slot free; a stale-token mismatch is fatal.

When both sides negotiate `ZCNBLK_SHM_CAP_READ_PAYLOAD_REF`, a dirty read CQE
identifies the retained write slot and its source channel instead of copying the
4 KiB value into the read request's slot. Userspace reader-pins that source
slot until the kernel advances `comp_cons`; retirement cannot return a pinned
slot to the ingress pool. The kernel then copies directly from the referenced
dirty slot into the read bio. Capability absence keeps the materialized-copy
fallback, while an invalid channel, slot, owner token, or zero-length reference
is fatal.

Logical and remote completion trackers retain one global ordering contract, but
advance their high-water marks by contiguous runs. A lane publishes all slot
markers for a ready batch, one short-lived scanner clears the contiguous run,
and one release-store publishes the new HWM. This replaces one shared HWM
compare-exchange per 4 KiB operation without weakening cross-lane predecessor
or sync ordering.

The first local controls on July 11, 2026 used a busy shared host without
hugetlb and therefore are regression signals, not representative ceilings:

- linked io_uring ordering/read-after-write/sync passed, followed by 30,000
  mixed QD16 operations across the old 8,192-slot wrap at 934k IOPS;
- a 65,536-page (256 MiB) pool completed two 50,000-write bursts at
  1.20-1.29M IOPS with zero pressure events and `65536/65536` pages returned;
- a 200,000-write sustained run completed at 1.18M IOPS, targeted 31,609
  committed dirty-page evictions, and returned `65536/65536` pages;
- conventional synchronous mixed QD1 I/O passed at 118.9k IOPS with
  `8192/8192` pages returned.

The remaining transport problem is framing, not ingress ownership: these
controls averaged only about 16 records and 64 KiB per outbound write batch.
The lane coalescer still needs an adaptive age/byte policy to form larger WAL
extents without delaying low-QD traffic.

For latency measurement, `zcblockbench --latency-sample-rate N` records actual
submission-to-completion latency for every Nth operation. The block harness
passes the same setting through
`URING_PLAY_BLOCKBENCH_LATENCY_SAMPLE_RATE`. Use every operation at low queue
depth and a sparse rate such as 64 for throughput runs. A local A/B/A control at
aggregate QD512 measured about 2.5% sparse-sampling cost, within that shared
host's run-to-run spread.

The shared target also reports source `write_payload_iovecs`, coalesced
`write_payload_tx_iovecs`, `write_payload_runs`, both per-batch averages,
`avg_write_run_bytes`, and `max_write_payload_run_bytes`. The blocking TCP path
now emits one `IoSlice` per physically adjacent shared-arena run rather than one
per 4 KiB write; a byte-equivalence test guards the wire order. These counters
decide which TCP zero-copy primitive is appropriate. A four-lane 50/50 mixed control in
`bench-results/local-zcnblk-wal-payload-layout-20260711T151830Z` averaged 15.9
write iovecs per batch but only 7.8 KiB per contiguous payload run; the largest
run was 64 KiB. Issuing one `SEND_ZC` SQE per run would therefore trade the copy
for excessive SQE/completion traffic.

Linux 7.0 exposes the more direct fast path through `IORING_OP_SEND_ZC` plus
`IORING_SEND_VECTORIZED`: `sqe.addr` points to `struct iovec[]` and `sqe.len` is
the vector count. The shared zcnblk payload arena is an unregistered mapping, so
this path can send the copied frame header/descriptors plus all referenced
payload slots with one SQE and no `msghdr`. Vectored registered-buffer send-zc
is not supported by the 7.0 implementation and must be rejected rather than
silently downgraded; it is not required for this arena path. `SENDMSG_ZC` is the
portable io_uring fallback when vectorized send is unavailable.

`RawRing` now has checked normal-fd and fixed-file vectorized `SEND_ZC` SQE
builders, with construction tests that verify the Linux 7.0 encoding never sets
registered-buffer mode. Its lifetime-bound iovec cursor owns stable metadata and
advances across partial completions without copying payload bytes or resending a
completed prefix. Its per-batch attempt tracker gives every resubmission a
separate initial/notification lifetime, tolerates notification-before-initial
CQE observation, and releases the payload only after the cursor is complete and
all attempts retire. This follows Linux 7.0 `io_send_zc_prep()`, which allocates
one notification request per SQE and copies that SQE's `user_data` into the
notification CQE. This is foundation only: the shared target does not use the
builders until its send slots own these states through their respective
completion boundaries, and the path has passed isolated runtime validation.
The synthetic `tcp-bench-uring-mux-send` path exposes the same SQE construction
as `send-zc-vectorized`, with a strictly parsed vector width, so isolated tests
can measure its CQE and copied-notification behavior before block-edge wiring.

For either primitive, descriptor/iovec storage must remain alive through the
initial completion and shared payload leases must remain alive through the
notification CQE. A short initial completion must advance an iovec cursor and
resubmit only the unsent suffix. Registered-memory RMA remains the parallel
fabric implementation.

Do not enable the existing host `SEND_ZC` prototype on Linux 7.0.8 as a local
shortcut: prior host tests produced bad-page/slab crashes and the repository
gates that mode outside QEMU unless explicitly overridden. Validate the new
vectored path first on an isolated adhoc host, count copied notifications, and
make strict zero-copy mode fatal when any payload notification reports copied
fallback.

### Lane-Owned SEND_ZC Validation

The 2026-07-11 arm64 correctness run used this single-placement topology:

```text
/dev/zcnblk0 client edge
  -> shared-memory request/payload rings
  -> one CPU-pinned userspace WAL lane owner
  -> vectorized io_uring SEND_ZC over private TCP
  -> userspace zcmem leaf
```

No block device performed placement, mirroring, or striping. The leaf was a
userspace memory target; `/dev/zcnblk0` remained only the client block edge.
Descriptor-only read/control batches used an explicitly counted blocking
`writev`, while every payload-bearing batch used SEND_ZC and retained its
shared-arena lease through the notification CQE.

Strict mode exposed an important batching precondition. With
`URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_FILL_US=0`, the lane flushed the first 4 KiB
write immediately and Linux reported a copied SEND_ZC notification. The target
failed the request instead of silently accepting that fallback. With a bounded
100 microsecond fill window and 128-record extent limit, the same ordering
smoke passed and a 10,000-operation, 50/50 random 4 KiB, QD16 run reported:

- 31,622 IOPS and 506 microseconds average submission-to-completion latency;
- 598 payload SEND_ZC notifications, zero copied notifications;
- 38 descriptor-only control `writev` batches;
- 5,010 writes and 4,999 remote read misses in 636 remote batches;
- same-sector old-before-write/new-after-write behavior and terminal sync
  correctness in the preceding ordering smoke.

Artifacts are under
`bench-results/arm64-t4g-sendzc-lane-owned-20260711T1720Z` and
`bench-results/arm64-t4g-sendzc-lane-owned-long-20260711T1725Z`.

These are functional, not representative, numbers. The `t4g.small` has only
two vCPUs, so the client submitter and zcnblk kernel kthread shared CPU 0 while
the WAL lane owner used CPU 1. During the longer run, the lane owner incurred
68 context switches per 1,000 I/O, the remote leaf about 70 per 1,000, and the
kernel kthread about 957 involuntary switches per 1,000. The next performance
run must provide separate physical cores for client submission, kernel
handoff, WAL lane ownership, and leaf work before comparing SEND_ZC against
blocking TCP.

The lane-owned result receiver now has three explicit wait profiles:

- `URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_POLICY=fixed` with
  `URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_SPINS=0` blocks immediately. This is the
  control and lowest-idle-CPU profile.
- The default `adaptive` policy starts with a zero minimum, blocks while
  traffic is sparse, and grows its spin budget only when observed waits return
  within `URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_WAIT_NS`.
- An adaptive nonzero minimum, such as 256 spins on both target and leaf, is
  the explicit dedicated-core ultra-low-latency profile.

Every remote summary reports receive policy, current lane budget, spin hits,
blocking fallbacks, would-block polls, and growth/shrink events. This makes a
greedy CPU trade visible instead of attributing the result only to IOPS.

A noisy local A/B/A control on 2026-07-11 used one lane, separate physical
cores for client, target, kernel kthread, and userspace zcmem leaf, QD16, 50/50
random 4 KiB I/O, and three 20,000-operation repeats. The soft-exclusive claim
was not honored, huge pages were unavailable, and the CPU governor was not
changed, so these are comparative controls rather than representative ceilings:

| wait policy | mean IOPS | mean latency | target ctx/1k | leaf ctx/1k |
| --- | ---: | ---: | ---: | ---: |
| fixed target, fixed leaf, confirmation | 135.3k | 117.9 us | 58.7 | 68.1 |
| adaptive target, fixed leaf | 139.0k | 114.7 us | 0.13 | 66.7 |
| fixed target, adaptive leaf | 137.4k | 116.1 us | 58.8 | 0.10 |
| adaptive target and leaf, 256 minimum | 139.6k | 114.2 us | 0.12 | 0.07 |
| zero-start adaptive target, fixed leaf | 138.2k | 115.4 us | 0.23 | 66.5 |

The final zero-start target blocked nine times while learning, recorded 22
growth and nine shrink events from total observed wait time, then handled 2,370
waits while runnable. This is the normal automatic profile: it does not require
a permanently greedy thread to remove steady-state result-wait switches. The
client still showed about 100 context switches per 1,000 I/O from its QD16
completion cadence; that is a separate block/io_uring front-edge problem.

Artifacts are under `bench-results/local-zcnblk-wal-recv-*20260711T173*Z`.

### Ready-Order Block Completions

The client edge must not force unrelated operations to complete in submission
order. A local dirty-lease write can complete before an older remote read miss,
provided that same-sector predecessors, payload lease ownership, and sync HWM
fences remain intact. The shared target therefore publishes completions in
ready order, and the kernel client resolves each completion through an O(1)
request-sequence table. Remote placement and remote commit ordering remain in
userspace.

Dependent io_uring operations must express their dependency. The ordering smoke
uses `IOSQE_IO_LINK` within each read/write or write/read pair while leaving
different sectors concurrent. Unlinked SQEs are independent and are not a valid
same-sector ordering assertion.

A noisy one-lane local control on 2026-07-11 used separate physical cores for
the client, target, kernel kthread, and userspace zcmem leaf. The run used QD16,
50/50 random 4 KiB I/O, fixed buffers, and a linked eight-sector ordering and
sync smoke. The soft-exclusive claim was not honored, huge pages were
unavailable, and the governor was unchanged, so these results are comparative:

| completion/fill policy | mixed IOPS | read mean | write mean |
| --- | ---: | ---: | ---: |
| submission-order completion, 100 us fill | 137.5k | 122.6 us | 109.2 us |
| ready-order completion, 100 us fill | 243.3k-248.9k | 121-124 us | 6.2-6.5 us |
| ready-order completion, zero fill | 783.7k-816.3k | 31.9-33.2 us | 7.0-7.3 us |

At QD1, zero fill reached 112.9k mixed IOPS with a 14.0 us remote-read
mean and a 3.6 us local-write mean. The old 100 us fill control reached only
18.1k mixed IOPS, with 106-107 us reads and 3.35 us writes. The write ACK was
already local and fast; removing the artificial wait repaired the read side.

The zero-fill write-only control reached 1.56-1.57M IOPS and 47.1 Gibit/s of
logical payload while still averaging 15.6 records per remote batch. Zero fill
is now the normal WAL lane default: the lane continues coalescing while the
channel has backlog and sends immediately once that backlog drains. A nonzero
fill window remains an explicit strict-SEND_ZC tuning option when tiny sends
would report copied notifications; that throughput profile must be measured
separately and must not silently replace the low-latency default.

Client CQ polling did not help this path. A bounded adaptive spin reduced
client context switches from about 182 to 6 per 1,000 I/O, but consumed 99% of
the client core, cut mixed throughput to about 517k IOPS, and increased both
read and write latency. The default remains blocking CQ wait with a minimum of
one completion. Per-worker context-switch accounting is emitted as
`zcblockbench-context`; process-wide `perf stat` counts include coordinator and
setup noise and must not be substituted for it.

Artifacts are under `bench-results/local-zcnblk-ooo-*20260711T204*Z`.

## WAL Combine And Reduce Blockstore

The simple local problem is:

```text
4K logical ops -> lane-local WAL combiner -> dirty latest-value map
               -> separate topology-aware reduce blockstore
```

The WAL combiner owns hot admission. It appends payload into large lane-local
WAL extents, updates a compact `logical_record -> WAL slot` dirty map, and can
ACK ordinary writes once local admission and placement policy are satisfied.
It must not make RAID placement decisions inside the block client. Placement,
mirror, stripe, spill, lane choice, and backpressure still belong to userspace
RAID stages.

Those userspace stages may run on the same host as the client and may be called
client-side. The boundary is ownership, not machine location: `/dev/zcnblk0`
and its kernel client emit the ordered block/WAL stream, then a separate
userspace stage chooses stripe or mirror placement. A co-located
`zcraid0-userspace:` target stage is valid when member block devices are only
terminal leaves after that userspace decision.

The reduce blockstore is a separate userspace layer. It is a NUMA/topology-aware
memory image that represents the compact latest reduced view. Reads first check
the dirty map and copy from the WAL slot if the record has not been reduced yet;
otherwise they read from the reduced blockstore. Sync/FUA waits for the reducer
high-water mark, not for every ordinary write to make a remote round trip.

Descriptor-native reads should not have to copy that 4 KiB payload. The dirty
map can return a descriptor reference to the retained WAL slot, and the normal
release path should retire the slot by lane HWM once replica, reducer, and active
read/snapshot pins have all advanced. `zcwal-reduce-bench --read-access ref`
models that descriptor-return path separately from forced materialization.

This is the intended fast mirror shape: the frontend is optimized for random
IOPS and freshness, while the backend reducer and transport are optimized for
large WAL extents and ordered high-water marks.

`zcwal-reduce-bench` isolates that shape without `/dev/zcnblk0`, TCP, RDMA, or a
terminal block device:

```bash
target/release/zcwal-reduce-bench \
  --mode mixed --pattern random \
  --lanes 8 --workers 8 \
  --records-per-lane 262144 \
  --block-records-per-lane 32768 \
  --extent-records 256 \
  --read-pct 50 \
  --pin --cpu-list 0-7
```

Local 2026-06-06 serial runs on one NUMA node, 8 lanes, 8 pinned workers,
4 KiB records, and 256-record/1 MiB extents showed:

| mode | logical IOPS | WAL Gbit/s | reduce Gbit/s | read Gbit/s | total touched Gbit/s | context switches |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| combine, sequential | 5.13M | 168.0 | 0 | 0 | 168.0 | 2 vol / 4 invol |
| reduce, sequential | 2.41M | 79.1 | 79.1 | 0 | 158.2 | 1 vol / 13 invol |
| mixed random, no forced reduce | 6.85M | 112.2 | 0 | 112.4 | 224.6 | 4 vol / 1 invol |
| mixed random, reduce every extent | 3.88M | 63.4 | 63.4 | 63.8 | 190.5 | 1 vol / 1 invol |

The result is the current architectural target for the zcnblk fan path. If the
end-to-end block/network run is far below this, look for tiny batch/result ACK
churn, misplaced context switches, or leaf/network serialization before blaming
the dirty WAL model itself.

Restricted-memory profiles are also required. The implementation must continue
to behave correctly with small dirty budgets such as 64 MiB, 256 MiB, and
512 MiB. Those runs are allowed to throttle sooner and produce lower IOPS, but
they must preserve:

- read-after-write from the local dirty range map;
- same-sector ordering;
- global sync/FUA high-water fencing;
- explicit hard-limit backpressure before payload leases are exhausted;
- accurate dirty-bytes, dirty-age, throttle, and sync-wait counters.

Use separate soft and hard limits. Below the soft limit, coalesce writes into
large lane-local WAL extents and ACK after local visibility plus mirror queue
admission. Between soft and hard limits, prefer draining and batching over new
admission. At the hard limit, stop ACKing new ordinary writes until durable or
replicated high-water marks advance enough to release buffer space.

Representative cache benchmark points:

```text
64 MiB    restricted/container smoke; should throttle quickly but stay correct
256 MiB   small-host profile; proves bounded dirty maps and eviction
512 MiB   moderate profile; enough for short low-QD/mixed tests
2 GiB     minimum high-performance profile for local fan/mirror tuning
8-128 GiB 400G/600G host profile for long bulk-streaming write tests
```

## Fast-Path Rules

- No block device may perform mirror, stripe, tier, spill, or placement.
- No payload copy per mirror branch.
- No deep payload inspection on mirror writes.
- Strict zero-copy validation must fail on materialized fan payloads, heap leaf
  payload-data receives, or copied leaf submits; TCP socket-copy lower bounds
  are measured separately.
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
