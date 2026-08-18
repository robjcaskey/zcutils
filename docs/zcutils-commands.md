# zcutils Commands

`zcutils` builds both an umbrella binary and separate command binaries from the
same implementation.

```bash
cargo build --release --bins
```

Both forms work:

```bash
zcutils zcmux --peer-addr 10.0.1.12 --lanes 128
zcmux --peer-addr 10.0.1.12 --lanes 128
```

## Topology characteristics

`zctopology-detect` discovers permissionless local EC2 facts through IMDSv2
and accepts arbitrary typed overrides:

```bash
zctopology-detect --set durability.role=hop --set failure.rack='"rack-7"'
ZC_TOPOLOGY_CHARACTERISTICS='durability.role=leaf,failure.foo="bar"' \
  zctopology-detect
```

Use the resulting map in `UpsertEntity` or
`PatchEntityCharacteristics` commands committed to `TopologyStore`. See
[dynamic-topology.md](dynamic-topology.md) for ownership and handoff semantics.

Persistent metadata quorum benchmarking uses a new file per voter. The leader
counts its own WAL and a follower only after the prefix has completed
`fdatasync`:

```bash
zcutils raft-durable-follower voter-b.wal 0.0.0.0 9100 100000 64 64
zcutils raft-durable-follower voter-c.wal 0.0.0.0 9100 100000 64 64
zcutils raft-durable-leader voter-a.wal B:9100,C:9100 100000 64 64
zcutils raft-durable-inspect voter-a.wal 64
```

The last argument is records per physical flush. Use `1` to measure one flush
per record and a larger value to measure group commit.

For external-WAL QD1-QD16 measurements on dedicated EC2 ad-hoc nodes, run
`scripts/adhoc-nic-low-latency.sh apply OUTDIR` on every client and leaf through
`scripts/ec2_perf_spot.py exec`. It disables ENA adaptive RX moderation and
sets RX/TX coalescing to zero, verifies the resulting state, and preserves
before/after evidence. Only after every endpoint succeeds should the client
harness receive `URING_PLAY_EXTERNAL_NIC_LOW_LATENCY_CONFIRMED=1`. The helper
refuses shared hosts, and the benchmark harness does not silently mutate NICs.

`zcwal-ofi-pipe` is an experimental bidirectional libfabric `FI_EP_RDM` bridge
for carrying the existing userspace WAL TCP byte stream between hosts without
changing its request, ordering, acknowledgment, or placement protocol. The
client role accepts a local TCP connection from the shared-memory target; the
server role connects to a local userspace WAL leaf. Two persistent RDM
endpoints carry request and response directions independently:

```bash
# Leaf host: 32 local leaf lanes listen on ports 29000 through 29031.
URING_PLAY_OFI_DOMAIN=efa_0-rdm \
zcwal-ofi-pipe server 127.0.0.1:29000 LEAF_PRIVATE_IP 37000 efa rdm 32

# Client host: point 32 target lanes at ports 28900 through 28931.
URING_PLAY_OFI_DOMAIN=efa_0-rdm \
zcwal-ofi-pipe client 127.0.0.1:28900 LEAF_PRIVATE_IP 37000 efa rdm 32
```

The RDM pipe owns transport only. It makes no placement, mirror, stripe, tier,
or spill decision, and `/dev/zcnblk0` remains the client block edge. Each lane
consumes two consecutive services (request then response), so the
32-lane example uses services 37000 through 37063. Set
`URING_PLAY_PIN_CPU_LIST` to NIC-local CPUs; lane N uses affinity indexes 2N
and 2N+1 for its request/response workers, and startup logs record that mapping.
`URING_PLAY_OFI_PIPE_FRAME_BYTES` controls the bounded RDM message size
(default 64 KiB); it must not exceed the provider's maximum.

The shared-memory WAL target and WAL leaf can also use one direct,
bidirectional `FI_EP_RDM` endpoint per lane, removing both local TCP bridge
processes. The leaf is still a separate userspace stage after `/dev/zcnblk0`;
this transport does not make placement, mirror, stripe, tier, or spill
decisions. For a 32-lane EFA run:

```bash
# Leaf host. Services 29000..29031 carry WAL messages; the TCP control-plane
# address exchange uses ports 30000..30031 by default.
FI_EFA_USE_DEVICE_RDMA=1 URING_PLAY_OFI_DOMAIN=efa_0-rdm \
URING_PLAY_OFI_CQ_SLEEP_NS=0 \
URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 \
URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT=ofi \
URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER=efa \
URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1 \
URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB=1 \
URING_PLAY_PIN_CPU_LIST=LEAF_NIC_LOCAL_CPUS \
zcnblk-wal-leaf zcmem:16G LEAF_PRIVATE_IP 29000 32 1 4K 32 true blocking

# Client host. wal-tcp names the protocol/backend contract; this selects OFI
# instead of TCP for its downstream userspace leaf connection.
FI_EFA_USE_DEVICE_RDMA=1 URING_PLAY_OFI_DOMAIN=efa_0-rdm \
URING_PLAY_OFI_CQ_SLEEP_NS=0 \
URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 \
URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=ofi \
URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER=efa \
URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS=1 \
URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD=8 \
URING_PLAY_ZCNBLK_SHM_LEAF_ADDR=LEAF_PRIVATE_IP:29000 \
URING_PLAY_ZCNBLK_SHM_TARGET_CPU_LIST=CLIENT_NIC_LOCAL_CPUS \
zcnblk-shm-target /dev/zcnblk-shmctl wal-tcp 64
```

`URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES` bounds one complete WAL protocol
emission (default 1 MiB). Oversize frames fail instead of being split across
unordered RDM messages. The leaf requires exactly one connection and one
worker per lane and prints the identity lane-to-worker map plus the explicit
lane-to-CPU map. Direct low-latency runs should raise `RLIMIT_MEMLOCK`, reserve
huge pages, pin client kthreads and userspace workers, set hctx/NAPI affinity,
and use `URING_PLAY_TOPOLOGY_STRICT=1`; the strict preflight rejects sleeping
OFI completions or an incomplete declared topology before benchmark numbers
are printed.

When both RMA-read switches are enabled, HELLO negotiation advertises the
userspace memory leaf's remote-read window. Each client lane registers a ring
of fixed 4 KiB buffers, one stable buffer per outstanding operation. Set
`URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD` to the per-lane ring depth (1 by
default, maximum 1024). Reads are posted independently, CQ entries are drained
in batches, and out-of-order slot completions are copied into their owned
shared block slots before request batches retire in FIFO order. The RMA-read
path deliberately does not register the full shared mapping: EFA provider behavior must be measured
for MR size and subrange access, and the kernel client still owns no placement
decision. Startup and summary logs state the lane QD, registered ring bytes,
peak in-flight reads, batched-CQ yield, local-CQ completion semantics, and copy
time. Reads from a leaf that does not advertise the feature retain the framed
result-payload path; sync/FUA and RMA-to-framed transitions drain outstanding
reads before using the explicit message/HWM contract.

The shared-block WAL path also has an OFI RMA write-payload mode. Enable it on
the initiator with `URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES=1` and on the terminal
`zcmem` leaf with `URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_WRITES=1`. The hello
exchange advertises one final-memory window per owner lane. Before the hot path,
each initiator endpoint registers the complete shared mapping once as an
`FI_WRITE` source. A write batch then performs RMA directly from leased shared
slots to the chosen leaf offsets and sends only its descriptor table as a
metadata doorbell; there is no payload gather into the OFI message buffer.

This mode has stricter ordering rules than framed payloads:

- `URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS=1` is mandatory so logical extents
  have stable userspace owners. Placement remains outside the kernel block
  client.
- `URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_BATCHES=1` is mandatory. A lane cannot
  expose a second direct-to-final-memory batch until the first doorbell result
  retires. Disjoint runs inside one batch may use
  `URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_QD`; overlapping runs are separated by a
  delivery-completion barrier so input order is preserved. This queue depth is
  independent of block per-worker QD: the block harness defaults it to 16 even
  for an aggregate-QD1 workload because one userspace batch can contain several
  random destination runs. Strict/representative harness runs reject values
  below `URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_MIN_QD` (16 by default); lower the
  floor explicitly only for diagnostic serialization A/B runs.
- `URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1` is the default and is required
  by the WAL path. The shim posts `FI_DELIVERY_COMPLETE`; only after every CQE
  is reaped may it send the doorbell. The leaf validates metadata, performs any
  requested sync/FUA action, and returns a result HWM. A local transmit CQE
  without delivery semantics is not sufficient.
- Set `URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES_REQUIRED=1` for attributable
  benchmarks. Otherwise an unnegotiated window or a busy lane may retain the
  compatible framed-message fallback.
- `URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_OWNER_MODE=single-domain-fan-in`
  routes every block-ingress lane through one stable userspace owner and one
  long-lived OFI endpoint. This is the single-leaf/single-rail capability
  topology: the kernel still owns no placement decision, and the userspace
  owner count defaults to one. Its payload-operation QD defaults to 64 because
  the endpoint must expose enough delivery-complete operations to reach the
  EFA high-PPS path. The harness records the block-lane-to-owner fan-in,
  endpoint count, per-endpoint QD, and aggregate RMA depth separately.
- The default owner mode is `placement`, where every stable owner remains a
  distinct placement/transport stream. Same-domain EFA RMA-write runs with
  more than one such endpoint warn because endpoint contention can erase
  scaling. Representative or strict runs must either use the explicit fan-in
  topology or set
  `URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED=1` to confirm
  that the multi-endpoint placement topology is intentional. Confirmation
  records the topology; it does not assert that the endpoints scale.

Startup preflight accounts for the full shared mapping once per initiator OFI
domain and the full leaf window once per leaf endpoint. Logs report RMA bytes,
doorbells, queue occupancy, CQ batching, completion semantics, and the copy
ledger. This removes the userspace message-payload gather and leaf receive copy;
it is not automatically end-to-end PostgreSQL zero copy because ordinary
filesystem/block bios still copy into the shared slot. The client module has a
strictly opt-in exception: with an imported external HugeTLB arena,
transferred payload leases, `shm_bio_arena_zero_copy=1`, and an O_DIRECT bio
whose pages exactly alias the selected lane-local payload slot, it leases those
same pages and skips the write/read copy.

For application-originated buffers, set
`URING_PLAY_ZCNBLK_SHM_APP_ARENA_SOCKET=/run/zcnblk/arena.sock` on
`zcnblk-shm-target`. The target retains the imported HugeTLB memfd and exports
that fd plus the validated lane/slot layout over the mode-0660 Unix socket.
`ZcnblkAppArena::connect` maps it, and `allocate(lane)` atomically reserves a
lane-local slot. The returned buffer hands its application owner token to the
kernel on an O_DIRECT `read_at` or `write_at`; it cannot be accessed again until
`wait_reacquire` (or `sync_and_reacquire`) observes target release and reserves
it again. Pin the submitting thread to a CPU whose blk-mq hctx maps to that lane.
The blocking helpers are a correctness interface; an io_uring application can
register the arena mapping, call `handoff_to_kernel`, submit the returned exact
pointer, and call `wait_reacquire` after CQ completion. This adds no allocator
copy and no per-I/O heap allocation.
This interface maps the complete transport arena and is therefore a trusted
local application interface; restrict socket ownership and permissions as
needed.
Arena export currently requires the independently releasable per-slot WAL path:
`URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1` and
`URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS=1`. Startup fails if export is requested
with a legacy contiguous-HWM mode that cannot return an individual application
buffer.

Set `shm_bio_arena_zero_copy_required=1` on the kernel client to make the
contract fail closed: an arena-backed bio with the wrong alignment, extent, or
lane fails instead of copying, and a busy exact slot is requeued rather than
bounced. Ordinary non-arena bios retain the compatible copy path.
`/sys/kernel/debug/zcnblk/state` reports `bio_alias_writes`,
`bio_alias_reads`, `bio_alias_busy_fallbacks`,
`bio_alias_required_retries`, and `bio_alias_required_rejects`; do not claim
this operation was exercised unless the relevant alias counter advances and
the fallback/reject counters remain zero. `zcnblk-arena-io` performs a
write/fsync/read data check and enforces that counter contract.
This is buffer ownership only: placement remains in the userspace target.
`scripts/zcnblk-shm-block-bench.sh` exposes
the same variables for correctness and QD curves. `scripts/zcnblk-pgbench.sh`
uses `WAL_TRANSPORT=tcp|ofi`; keep owner count, owner/ingress/leaf CPU maps,
pipeline depth, database settings, and workload parameters identical for a
matched TCP-versus-EFA comparison. That harness remains a fixed two-owner
placement comparison. Set `OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED=1` only to
record that its same-domain EFA topology is intentional; without that explicit
confirmation the run is non-representative, and strict mode stops before
printing benchmark results.

The client begins with `vmalloc_user()` backing but can import a sealed HugeTLB
memfd before attach; the target startup line reports `import_active` and
`bio_arena_alias_supported`. Both block and PostgreSQL harnesses still warn and
mark RMA-write measurements non-representative unless
`URING_PLAY_ZCNBLK_SHM_RMA_SOURCE_HUGETLB_CONFIRMED=1` (block) or
`OFI_RMA_SOURCE_HUGETLB_CONFIRMED=1` (PostgreSQL) is supplied after the
external-HugeTLB import is active and verified. The terminal `zcmem`
leaf can independently use its existing explicit HugeTLB mapping.

### PostgreSQL TCP versus EFA RMA proof, 2026-08-11

Two independent fresh-database runs per transport completed on the same pair
of `c8gn.16xlarge` hosts in `us-east-2c`. Each run used PostgreSQL 16.14,
scale 300, 128 clients, 16 jobs, two lanes, three 20-second samples, the same
lane/owner/application CPU map, a 32 GiB volatile-memory leaf, and synchronous
commit with `fsync` and full-page writes enabled.

| Transport | Fresh runs | Samples | Mean TPS | Mean latency | Sample spread |
|---|---:|---:|---:|---:|---:|
| TCP message payload | 2 | 6 | 94,961.2 | 1.343 ms | 4.530% |
| EFA RMA payload | 2 | 6 | 96,661.6 | 1.319 ms | 3.683% |

EFA was 1.791% faster in aggregate and reduced mean transaction latency by
about 1.8%. The two fresh-run TPS deltas were 1.37% and 2.21%. Both EFA runs
had zero provider/CQ errors, no message-payload fallback, and exact equality
between target write bytes and leaf RMA payload bytes (29,436,796,928 and
29,538,689,024 bytes). The leaf reported zero payload receive-copy bytes.

The TCP rows passed strict topology preflight. The EFA rows are intentionally
classified non-representative because the client RMA source remains the
`vmalloc_user()` shared arena described above; a captured strict EFA attempt
failed before printing numbers. The remote leaf was explicit HugeTLB, and RMA
payload completion was delivery CQ before the metadata doorbell and result
HWM. Sync acknowledged remote volatile memory rather than power-loss-durable
media. Full topology, repeat, correctness, copy-ledger, and provider evidence
is in `bench-results/zcutils-pgrma-adhoc-c8gn16-20260811T1055Z/`.

`scripts/zcnblk-shm-rma-qd-ladder.sh` drives QD1/2/4/8/16 with at least three
repeats per point. A representative run requires an executable
`RMA_QD_RAW_RUNNER` for matched raw RMA RTT samples and either
`RMA_QD_BEFORE_BLOCK_HOOK` to start a fresh external leaf for each point or an
explicit `RMA_QD_EXTERNAL_LEAF_SUPERVISED=1`. The consolidated report includes
per-worker QD, worker/lane count, aggregate outstanding depth, raw transport
RTT, the matching ceiling, actual/theoretical efficiency, topology artifacts,
and spread. The hooks own external process lifecycle; the block edge remains
only `/dev/zcnblk0` and placement remains in userspace.

### OFI queueing and EFA profiles

The libfabric shim now uses one type-aware batched TX CQ dispatcher for SEND,
RMA READ, and RMA WRITE, plus a batched RX dispatcher. Every operation owns a
stable `fi_context2` ring slot until its CQE is reaped; completions may arrive
out of order. `send_nowait`, fixed-batch sends, preposted receives, queued RMA
reads, and queued RMA writes therefore maintain real outstanding windows.
Endpoint shutdown prints configured and peak ring occupancy, CQ polls and CQE
batch yield, `FI_EAGAIN` retries, provider/CQ errors, injection counts, and MR
registration activity. A CQ error poisons the endpoint and leaves the failed
slot owned; it cannot be recycled into another request.

The main controls are:

- `URING_PLAY_OFI_TX_QUEUE_DEPTH` and `URING_PLAY_OFI_RX_QUEUE_DEPTH` for MSG
  slot rings. The TX value is also the fixed-batch window for headered no-ACK
  WAL sends, so that transport-ceiling path does not silently collapse to QD1;
- `URING_PLAY_OFI_RMA_READ_QD` and `URING_PLAY_OFI_RMA_WRITE_QD` for one-sided
  operation rings;
- `URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1` to require remote-delivery
  CQ semantics for write operations that precede a WAL doorbell;
- `URING_PLAY_OFI_RMA_WRITE_MORE=1` to mark every non-final write in a
  software-admitted burst with `FI_MORE`. This is an opt-in doorbell-batching
  A/B path; each final post omits `FI_MORE`, and CQ/buffer-lifetime semantics
  are unchanged. `URING_PLAY_OFI_RMA_WRITE_MORE_BURST` (default 64) inserts a
  non-`FI_MORE` flush at least once per configured number of accepted posts.
  If provider backpressure rejects the immediate follow-up to an accepted
  `FI_MORE` post, the next accepted post is forced to be a flush;
- `URING_PLAY_OFI_CQ_SIZE` for both CQs, or
  `URING_PLAY_OFI_TX_CQ_SIZE`/`URING_PLAY_OFI_RX_CQ_SIZE` separately;
- `URING_PLAY_OFI_CQ_BATCH` and `URING_PLAY_OFI_CQ_HEADROOM` for CQ progress
  and safety capacity;
- `URING_PLAY_OFI_MR_ARENA_COUNT` for the explicit stable-arena table; and
- `URING_PLAY_OFI_SELECTIVE_COMPLETION=1` binds selective completion.
  `URING_PLAY_OFI_RMA_READ_COMPLETION_STRIDE` (default 1) controls fenced RMA
  read CQ markers. Values above one retire every prior operation through the
  next marker in posting order. The userspace read queue retains its newest
  real request so a blocking drain can make that request the fence marker;
  this avoids a synthetic network operation on the saturated path. A private
  one-byte fenced read remains only as a liveness fallback when no real tail
  request or outstanding marker exists. Endpoint stats distinguish periodic,
  full-window, forced-real-tail, and synthetic-fallback markers.
  `URING_PLAY_OFI_RMA_DEFER_TAIL_COMPLETION=0` disables the real-tail policy
  for controlled A/B diagnosis without disabling selective completion.
  The moderated-read implementation retires the provider-proven posting FIFO
  directly. The block target registers its whole shared payload mapping once;
  posts inside that stable arena use the endpoint's bounds-checked descriptor
  directly, without an MR-table scan or per-I/O lookup-counter store. Disjoint
  diagnostic buffers retain the generic MR-table fallback. Live read batches
  use direct FIFO indexing by contiguous monotonic identifier instead of keyed
  hashing, queued/active slots retain only the fields needed through CQ
  retirement, and attached-mapping CQ progress borrows the stable mapping
  directly instead of cloning its shared reference count on every poll.
  Disabled latency telemetry allocates no timestamp state in active slots;
  those slots use a zero-token sentinel and retain only batch/token words.
  A fenced marker validates each retired slot but advances posting,
  completion, provider-inflight, and completion-total counters once for the
  proven group rather than once per 4 KiB read. The Rust lane queue likewise
  subtracts in-flight ownership once per returned group and coalesces
  consecutive completions for the same request batch into one remaining-count
  update. Returned free slots are initialized in the preallocated QD array and
  its length is published once per group; active-slot retirement clears only
  the token ownership sentinel. Optional latency timestamps are retired in a
  separate diagnostics-only pass selected once per group. Independently
  wrapped posting and completion FIFOs are divided into contiguous spans,
  keeping ring-wrap arithmetic outside the per-slot loop.
  Submission records each known fence boundary in a preallocated group-length
  FIFO, so CQ retirement checks the oldest marker directly instead of rescanning
  every operation in its prefix. Without doorbell batching, ordinary unsignaled
  reads use the provider's flag-less `fi_read` entry point and only the real
  group marker constructs `fi_readmsg` with `FI_FENCE | FI_COMPLETION`.
  `URING_PLAY_OFI_RMA_READ_MORE=1` instead posts the unsignaled prefix with
  `fi_readmsg(FI_MORE)` and leaves `FI_MORE` off the marker. This is the default
  for EFA-direct: its data-path source stages each `FI_MORE` WQE and rings the
  submission doorbell on the final non-`FI_MORE` marker (or at the provider's
  maximum batch boundary), instead of ringing once per 4 KiB read. An explicit
  zero restores the unbatched A/B path. Libfabric
  still reports asynchronous errors for unsignaled operations and requires a
  provider to flush delayed operations when a subsequent post returns an error.
  The endpoint summary reports the unique `rma_read_marker_posts` count and derives
  `rma_read_unsignaled_fast_posts` from total accepted read posts, so a hardware
  artifact proves which entry point carried the workload; `rma_read_more_posts`
  proves how many accepted reads used doorbell staging.
  Latency telemetry selects a compile-time-specialized posting loop once per
  group; the production variant contains no timestamp call or per-post telemetry
  branch. Interior descriptors bypass the real-tail liveness policy until either
  the pending queue or free-slot stack reaches its last entry. Payload ranges are
  validated once at batch admission, after which the stable registered mapping
  requires only a pointer addition at provider submission.
  Blocking ring polls accumulate provider-poll calls in a local register and
  commit the total once when the API returns; `tx_cq_empty`/`rx_cq_empty` are
  derived exactly from total, nonempty, and error polls, avoiding redundant
  endpoint-counter stores on the dominant empty-CQ path. The internal CQ
  dispatcher returns empty/progress/error directly, so that loop also avoids
  spilling an output-count pointer on every empty provider poll.
  The post, poll, and group-retirement routines are
  release-assembly checked to contain no runtime integer division.

Before `fi_getinfo`, the shim now requests a provider TX work-request capacity
equal to the sum of the SEND, RMA READ, and RMA WRITE rings, and an RX capacity
equal to the receive ring. Endpoint profiles print both requested and returned
capacities. Strict topology mode rejects a provider result below the request.
This is separate from CQ sizing: mlx5, for example, converts requested WR and
SGE geometry into power-of-two hardware WQEBB rings, while CQ capacity controls
how many generated completions can wait to be reaped.

Every control-plane address exchange also exchanges the selected endpoint
profile and a versioned wire contract. Provider class, endpoint type,
`efa`/`efa-direct` selection, message shape, ACK window, compact/header mode,
and mirror branch/zlane fields must agree before a peer address is installed or
the timed data phase begins.

The raw RMA window contract initializes the remote arena with a nonzero
sentinel. Reads verify every returned extent against that sentinel. Queued
writes send an operation-tagged payload digest in the v4 done exchange, and
the target verifies the complete remote arena after the initiator's timed
local-CQ interval. Before metadata or timing, every v4 client synchronously
sends a 16-byte lane-tagged connection-warmup record and the server receives
and validates it. This moves RxM/verbs connection establishment out of the
timed phase and rejects a mismatched lane before MR metadata is consumed. That
post-timing arena check catches missing or mis-placed writes; it is validation
evidence, not a remote-admission or durability latency.

For `verbs;ofi_rxm`, set `FI_VERBS_DEVICE_NAME`, `FI_VERBS_IFACE`, and
`FI_VERBS_GID_IDX` explicitly on both hosts. The control exchange rejects
different returned fabric identities before the connection warmup. The
underlying verbs MSG queues (`FI_OFI_RXM_MSG_TX_SIZE` and
`FI_OFI_RXM_MSG_RX_SIZE`) are distinct from RxM's RDM queues and from the
zcutils operation rings; record all three layers when tuning ConnectX.

Use provider `efa` for the normal EFA RDM profile. Use provider `efa-direct`
(or `URING_PLAY_OFI_EFA_FABRIC=efa-direct`) only after `fi_getinfo` returns the
direct fabric with `FI_CONTEXT2` and the full direct MR contract:
`FI_MR_LOCAL | FI_MR_VIRT_ADDR | FI_MR_ALLOCATED | FI_MR_PROV_KEY`. The
endpoint log records the requested/query/returned API versions, compatibility
fallback to 1.11 when needed, returned MR mode, device, maximum MSG/RMA sizes,
and queried EFA emulated READ/WRITE state. The direct profile keeps the WAL
sequence header even if `URING_PLAY_OFI_COMPACT_4K=1`, because `efa-direct`
lacks the normal EFA provider's SAS guarantee. Its queried MSG limit gates
compact 4 KiB first; large extents use the separate RMA path rather than silent
segmentation.

`URING_PLAY_OFI_EFA_WRITE_HIGH_PPS=1` opts RMA writes into the EFA
`FI_EFA_WR_HIGH_PPS` `fi_writemsg` variant only when the build-time EFA header
advertises that flag. The shim never manufactures a provider-reserved bit.
Endpoint profiles and final statistics report
`efa_write_high_pps_available`, `requested`, `effective`, and `verified`;
strict mode rejects a requested-but-unavailable variant before timing, while a
non-strict run records its fallback. `scripts/zcofi-rma-queue-matrix.sh` drives
matched read, write, and explicitly selected high-PPS write curves at
QD1/2/4/8/16 and a separate random-permutation saturation curve. It requires
at least three repeats, an external fresh-target runner, explicit topology
evidence, and reports spread and completion-matched theoretical efficiency.
Read and write are the default modes; add `write-high-pps` through
`ZCOFI_RMA_MATRIX_MODES` only on a build/provider pair that advertises the
flag, because a representative unavailable request is intentionally fatal.
Raw random access is enabled with `URING_PLAY_OFI_RMA_ACCESS_PATTERN=random`
and a reproducible optional `URING_PLAY_OFI_RMA_RANDOM_SEED`.
RMA writes now default to `URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1`, so
new write matrices label and derive their ceiling from post-to-delivery-CQ
latency. Set it explicitly to `0` only for a source-reusable local-CQ transport
control; the WAL RMA payload path rejects that weaker completion semantic.
`ZCOFI_RMA_MATRIX_SATURATION_QDS=none` is accepted only for an explicitly
nonrepresentative partial qualification, such as the bounded RXE write smoke;
a representative matrix still requires read and write QD1/2/4/8/16 plus at
least one saturation depth of 32 or greater.

In `URING_PLAY_TOPOLOGY_STRICT=1` or `URING_PLAY_TOPOLOGY_FATAL=1`, sleeping
CQ progress, insufficient rings/CQs, hot MR churn, missing worker/CPU or
EFA domain/NIC mapping, and inadequate hugetlb or memlock headroom fail before
representative summaries. `FI_EFA_IFACE`, `URING_PLAY_OFI_DOMAIN`,
`URING_PLAY_PIN_CPUS=1`, and `URING_PLAY_PIN_CPU_LIST` are part of that
contract. See the upstream [`fi_efa(7)` documentation](https://ofiwg.github.io/libfabric/main/man/fi_efa.7.html)
for the provider-specific options and limits.

For a local semantic rehearsal, run:

```bash
scripts/zcofi-softroce-local.sh
```

The wrapper creates a private veth/netns RXE pair, repeats native verbs RC
ping-pong, records a bounded `verbs;ofi_rxm` runtime probe, and uses
`verbs;ofi_rxd` only as an explicitly labelled RXE-UD emulated-RMA fallback.
It fully repeats the stable one- and two-endpoint curves, then runs bounded
four- and eight-endpoint capability probes. Endpoint/control/warmup/MR setup is
admitted in lane order outside timing; a failure-aware all-owner gate starts
traffic only after every client owner is ready and releases waiting workers if
a later endpoint fails. The wrapper always tears down the named RXE topology.
Local results are shared-system semantic evidence, never a ConnectX performance
forecast. See
`docs/connectx7-ofi-queue-design.md` for the hardware gate and provider split.

## Command Idiom

The descriptor-native model is:

```text
source -> transform/map -> fanout/join/split -> sink
```

Payload bytes live in owned pools. Commands pass descriptor leases with bounded
credits and explicit release. The target Unix UX does not require an explicit
manager command:

```bash
zcdemux ... | zcmap --preserve-lanes | zcmux --peer-addr C ...
```

The first stage creates or joins a session and writes the session identity in
the descriptor stream header. Downstream `zc*` tools read that header, connect
to the same manager/session, and propagate a fresh header. `zcflow` is still
available when one process should supervise the whole graph directly. Plain
shell pipes are byte-compatible today until descriptor fd passing is implemented.

No command should hide forwarding inside `zcdemux`. The demuxer receives network
traffic and emits a descriptor stream. Relay and fanout are explicit:

```text
zcdemux -> zcmap -> zcmux
zcdemux -> zcmaptee -> zcmux + zcsink + zcstat
```

## Block Boundary

`/dev/zcnblk0` is the client-side block onramp to the SAN fabric. Existing
block-speaking applications, fio, filesystems, and databases should see one
client block device family there. It uses the `zcnblk` wire protocol and the
userspace `zcnblk-target` service.

Target hosts should not be assembled from custom zc block targets. A target is
a userspace service. It may finally land bytes on `zcdevnullN`, ordinary files,
real allowlisted block devices, or optional `/dev/zcbrdN` RAM media, but those
are last-hop backing media, not the topology. Custom stripe kernel block
devices are not SAN target backends.

A userspace RAID stage may be co-located on the client host or embedded in the
userspace target process. `zcraid0-userspace:...` is that explicit target-stage
form; the userspace stage chooses the member and offset before issuing I/O to
terminal leaf media. The older `zcraid0:...` spelling is intentionally rejected.
It does not make `/dev/zcnblk0` or the zcnblk kernel client a stripe primitive.
Startup must report `placement_owner=zcnblk-target-userspace-raid-stage` and
`block_client_placement=no` for this form.

Keep the fan tree in userspace: `fanplan`, mux/demux routing, forwarding,
RAID0/RAID1 policy, tiering, tier spill decisions, backpressure, descriptor
lane scheduling, and fanin assembly belong in the userspace pipeline. A
userspace tier may choose to write its hot or spill leg to a block device, but
that block device is only the last hop. That boundary is why recipes may use
`/dev/zcbrdN` as a convenient edge source or sink during tests while the
topology itself is still expressed with `zcraid-*`, `zcfanplan`, `zcforward`,
`zctier`, `zcmux`, and `zcdemux`.

### zcnblk-fan

`zcnblk-fan` is the userspace ordered fan target for the block fabric. It sits
between `/dev/zcnblk0` or `zcnblk-send` and userspace leaf processes. It owns
stripe/mirror placement, splits large logical requests at placement switch
points, forwards each descriptor to the selected leaf stream, and reassembles
read responses with the original zcnblk header metadata. `zcnblk-read-fan`
remains a compatibility alias for the older synchronous stripe path.

Same-sector ordering is part of the contract. The fan reserves per-4K order
slots when request headers arrive; batch headers are all reserved before any
payload is forwarded. With the WAL dirty budget enabled, writes become locally
visible after bounded dirty-cache admission; same-sector reads that arrive before
leaf commit are answered from that dirty cache. Explicit `ZCNBLK_OP_SYNC`
requests are the durability/high-water-mark boundary and wait for leaf commit
and sync results before returning. Set `URING_PLAY_ZCNBLK_WAL_WRITE_ACK_MODE=remote`
only when ordinary write ACKs must wait for leaf result batches too.

The WAL engine is selected with `--engine wal`. It speaks fixed 128-byte fan WAL
descriptor/result frames to `zcnblk-wal-leaf`, supports `--mode stripe|mirror`,
and keeps block devices only as terminal leaf media. Blocking and io_uring
callers share the same ordered descriptor/result contract; the selected leaf
submit adapter must not change placement, ack, sync, or freshness semantics.

```bash
zcnblk-wal-leaf zcmem:1G 127.0.0.1 24600 1 1 4K 1 false blocking
zcnblk-wal-leaf zcmem:1G 127.0.0.2 24600 1 1 4K 1 false blocking

URING_PLAY_ZCNBLK_WRITE_ACKS=1 \
URING_PLAY_ZCNBLK_BATCH_DEPTH=512 \
URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW=16 \
zcnblk-fan --engine wal --leaves 127.0.0.1,127.0.0.2 --bind 127.0.0.1 \
  --base-port 23600 --ports 1 --connections-per-port 1 \
  --bytes-per-connection 4M --chunk-bytes 4K --stripe-bytes 4K \
  --leaf-base-port 24600 --pin-handlers false --mode stripe
```

For dual-NIC fan edges, bind each leaf branch to the intended local source
address with `URING_PLAY_ZCNBLK_FAN_LEAF_SOURCE_IPS=card0_ip,card1_ip`. A
single `URING_PLAY_SOURCE_IP` still applies to every leaf for one-NIC runs.
The fan startup line and `zcnblk-fan-wal-leaf-stream` logs print
`leaf_source_ips=` and per-stream `source_ip=`; do not treat a multi-NIC
benchmark as representative unless those match the declared lane-to-NIC plan.

`zcmem:SIZE` is the preferred local correctness/performance leaf for early
fan work. It is a userspace mmap-backed block image, so it preserves
read-after-write data without adding a terminal kernel block-device hop.
It logs NUMA placement and supports `URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_NUMA_NODE`,
`URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_HUGETLB`,
`URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_THP`, and
`URING_PLAY_ZCNBLK_WAL_LEAF_ZCMEM_FIRST_TOUCH`. Use `/dev/zcbrdN` only as
a terminal block-media control after userspace placement has already happened.
For local conformance, run `zcnblk-send` with
`URING_PLAY_ZCNBLK_OP=write-sync-read`,
`URING_PLAY_ZCNBLK_VERIFY_READS=1`, and
`URING_PLAY_ZCNBLK_WAIT_WRITE_ACKS=1`; this writes a range, waits for ACKs,
sends `ZCNBLK_OP_SYNC`, then reads the same range back from the still-live
userspace RAM leaves. `URING_PLAY_ZCNBLK_OP=write-read-same` is the faster
read-after-write smoke and can be combined with `URING_PLAY_ZCNBLK_BATCH_DEPTH`.
For random 4K read-after-write smokes, set `URING_PLAY_ZCNBLK_ACCESS=random`
and `URING_PLAY_ZCNBLK_RANDOM_RANGE_BYTES=SIZE`. Random placement must keep
`URING_PLAY_ZCNBLK_SEND_WRITE_EXTENTS=0` and
`URING_PLAY_ZCNBLK_SEND_READ_EXTENTS=0`; otherwise the sender would hide the
per-sector ordering and dirty-cache lookup problem inside synthetic linear
extents.
For high-throughput linear WAL runs, also set
`URING_PLAY_ZCNBLK_SEND_WRITE_EXTENTS=1` and
`URING_PLAY_ZCNBLK_SEND_READ_EXTENTS=1`; otherwise the first hop is still being
fed batched 4K descriptors instead of large logical WAL extents. `zcnblk-send`
prints `write_extents=` and `read_extents=` in its startup line and emits a perf
warning when a linear batched run omits those extent onramp knobs. For
`write-read-same` extent tests, batch depth is split between the write and read
extent in each request. A 1 MiB extent with 4 KiB records therefore needs
`URING_PLAY_ZCNBLK_BATCH_DEPTH>=512` and
`URING_PLAY_ZCNBLK_READ_WINDOW>=256`; the sender warns, and strict topology mode
fails, if smaller knobs silently cap the real WAL extent size.
`URING_PLAY_ZCNBLK_SEND_READ_EXTENT_BATCH_EXTENTS=N` can then group adjacent
read extent headers into one upstream batch. This lets the fan emit
`READ_RANGE_RESP` records and reduces response `writev` calls, but it is not a
free default: on local loopback, larger range responses can block longer on the
client socket. Always report `read_extent_batch_extents=`,
`read_cache_range_response_*`, `read_cache_response_writev_calls`, and context
switches with those runs.
The fan startup line prints `client_fan_transport=` and
`fan_leaf_transport=`. A `local:zcleasemem` leaf should currently report
`client_fan_transport=tcp fan_leaf_transport=shared-arena`: that removes the
fan-to-leaf socket payload leg, but the client-to-fan edge is still TCP unless a
shared-memory onramp control is explicitly being tested.
For copy accounting, sender summaries now also print
`socket_payload_copy_bytes_lower_bound` and
`user_payload_touch_bytes_lower_bound`. The socket value is the first-hop TCP
payload copied into/out of the client process; the touch value also includes
payload generation and optional read verification scans. Fan summaries print a
split ledger: `client_fan_ingress_socket_payload_copy_bytes_lower_bound`,
`fan_client_response_socket_payload_copy_bytes_lower_bound`,
`fan_leaf_socket_payload_copy_bytes_lower_bound`,
`local_leaf_socket_payload_copy_avoided_bytes`,
`all_socket_payload_copy_bytes_lower_bound`, `leased_payload_reference_bytes`,
`materialized_payload_copy_bytes`, and
`observed_total_payload_copy_bytes_lower_bound`. The observed total lower bound
is known socket payload movement plus materialized WAL-stage payload copies; it
does not count lease references as copies. A same-host shared-arena run can
still be limited by TCP loopback copies even when `async_copied_payload_bytes=0`
and `read_cache_materialized_bytes=0`.
Set `URING_PLAY_ZCNBLK_WAL_ZERO_COPY_STRICT=1` (or
`URING_PLAY_ZCNBLK_ZERO_COPY_STRICT=1`) for zero-copy validation runs. In that
mode the fan prints its counters and then fails before the final throughput line
if `materialized_payload_copy_bytes` is non-zero. The WAL leaf similarly fails
if it used heap payload-data receive or copy submits. Heap descriptor/control
bytes are printed separately as `heap_payload_descriptor_bytes` and are not a
user-payload copy fallback. This strict guard does not fail on
`socket_payload_copy_bytes_lower_bound`; that counter is the TCP transport cost
and must be interpreted separately from WAL-stage ownership fallbacks.

Current same-host copy ledger:

- `zcnblk-send -> zcnblk-fan` over TCP still copies write payloads across the
  socket boundary and later copies/drains read responses at the client.
- `zcnblk-fan --engine wal --leaves local:zcleasemem:...` can keep
  fan-to-leaf mirror payloads as memfd/shared-arena leases. Strict runs should
  show `async_copied_payload_bytes=0`, `read_cache_materialized_bytes=0`, and
  `materialized_payload_copy_bytes=0`, with
  `fan_leaf_socket_payload_copy_bytes_lower_bound=0`.
- Same-host production WAL numbers are therefore a TCP-onramp measurement, not
  the shared-memory architectural ceiling. Use `zcfanout-shmlease-bench
  ... zcnblk-shm-onramp`, `zcnblk-shm-pipeline`, and `zcnblk-shm-mirror` as the
  no-loopback controls; those paths must report `payload_copy_bytes=0`,
  `observed_payload_touch_bytes`, `voluntary_ctxt_switches=0`, and an explicit
  `lane_cpu_map=`.
- Set `URING_PLAY_ZCNBLK_REQUIRE_LOCAL_SHM_ONRAMP=1` when a same-host ceiling
  run must fail instead of silently using the TCP client/fan edge with local
  `zcleasemem` leaves.
- Inter-host runs should keep the same descriptor/HWM contract but replace
  memfd leases with NIC-readable registered memory, TCP send-zc, or RDMA/libfabric
  extents. Do not compare them to local loopback without accounting for each
  socket or NIC DMA boundary.

The final optional `zcnblk-wal-leaf` argument is leaf submit mode:
`blocking` uses `pread`/`pwrite`, `uring` queues terminal block-device
reads/writes through io_uring, and `mixed` intentionally sends some frames
through each adapter while still emitting the same fan WAL `RESULT` records.
That mixed mode exists because a real `/dev/zcnblk0` client can receive
conventional and io_uring requests over its lifetime. `zcdevnull` has no kernel
block I/O, so the `uring` side of `mixed` is only a control-path check there; use
a real terminal leaf such as `/dev/zcbrdN` or an allowlisted raw `PARTUUID` for
actual io_uring block submission.

For write IOPS tests, `URING_PLAY_ZCNBLK_BATCH_DEPTH` is required. The fan
preserves per-4K ordering, but explicit upstream write batches are forwarded to
each leaf as `WRITE_BATCH` WAL chunks with a descriptor array followed by one
payload area. Leaves return coalesced `RESULT_BATCH` descriptor arrays, and the
fan interleaves those leaf result logs in the lane-local handler before
answering the client with a `BATCH_RESP` containing the write ACK headers. Treat
unbatched 4K WAL fan numbers as a topology smoke only. Bigger is not always
better on the TCP path: record both `max_upstream_batch_frames` and
`max_leaf_submit_records`, and do not cross the local socket/leaf framing knee
without a benchmark proving that throughput still rises.
`URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW` controls how many disjoint leaf write
batches the fan can keep outstanding before draining result batches; `1`
intentionally exposes the serialized wakeup-per-batch path. For terminal
io_uring leaves, `URING_PLAY_ZCNBLK_WAL_LEAF_RING_ENTRIES` and
`URING_PLAY_ZCNBLK_WAL_LEAF_CQ_ENTRIES` override the leaf ring size and are
printed by `zcnblk-wal-leaf`.
`URING_PLAY_ZCNBLK_FAN_LEAF_MAX_BATCH_RECORDS=N` caps direct fan-to-leaf
`WRITE_BATCH` frames while preserving the larger upstream batch as a single
ordered WAL admission unit. It is an egress-shaping diagnostic for TCP socket
locality and should be reported with the run; it is not a userspace RAID
placement primitive.
`URING_PLAY_ZCNBLK_FAN_REQUEST_CHUNKING=1` is enabled by default and splits
large mixed upstream request batches inside the fan by write payload before
dirty admission and leaf preparation. `URING_PLAY_ZCNBLK_FAN_REQUEST_CHUNK_BYTES`
defaults to `1M` and must be a multiple of 128-byte WAL records. This is
lane-local WAL admission shaping, not RAID placement. Alternating write/read
batches keep same-sector cached reads adjacent to their writes, but avoid
forcing a single multi-MiB upstream receive/descriptor bubble through one fan
handler turn.

On the local June 7, 2026 4-lane strict mirror control with terminal `zcmem`
userspace RAM leaves, batch depth `512` is the current useful TCP foreground
zone after leasing upstream dirty payload buffers, compacting async writeback
descriptors, and chunking large request batches. With sender lanes pinned to
CPUs `0-3`, fan handlers to `4-7`, fan async writeback to `8-11`, leaf 0 to
`12-15`, and leaf 1 to `28-31`, a verified `write-read-same` run with
`512M/lane`, `URING_PLAY_ZCNBLK_BATCH_DEPTH=512`, `URING_PLAY_ZCNBLK_FAN_REQUEST_CHUNK_BYTES=1M`,
and deferred unsynced writeback measured about 2.24M foreground 4K frames/s at
the sender. The fan summary includes the deferred EOF flush to the leaves, so
use the sender foreground rate for client-visible unsynced IOPS and the fan/leaf
rates for writeback drain capacity. The same 64M/lane profile measured about
1.67M frames/s with immediate async leaf writeback and about 1.88M frames/s with
deferred unsynced writeback. An 8-lane SMT-sharing local map was slower because
the fan receive phase ballooned, so do not assume more lanes without isolated
cores and a benchmark proving that the lane-to-CPU map still fits the host.
Report `max_upstream_batch_frames`, `max_leaf_submit_records`,
`request_chunk_bytes`, context switches, and the fan phase timing fields before
treating a larger batch or lane count as better.
For full fan-stage topology checks, pass the client and leaf maps to the fan:
`URING_PLAY_ZCNBLK_FAN_CLIENT_CPU_LIST=0-3` and
`URING_PLAY_ZCNBLK_FAN_LEAF_CPU_LISTS='leaf0=8-11;leaf1=12-15'`. The fan already
uses `URING_PLAY_PIN_CPU_LIST` for its own handler map. With those variables set
it prints `zcnblk-fan-wal-stage-cpus` and warns, or fails under
`URING_PLAY_TOPOLOGY_STRICT=1`, when the client, fan, or leaf stages reuse exact
CPUs or SMT siblings. A run that places two mirror leaves on sibling threads is
an SMT-paired experiment, not a physically separated topology result.
When a CPU list describes a remote host, add a topology domain to the stage
label, for example
`URING_PLAY_ZCNBLK_FAN_LEAF_CPU_LISTS='leaf0@leaf-a=0-31;leaf1@leaf-b=0-31'`.
CPU and SMT conflicts are checked only within the same domain, so local fan CPU
IDs are not compared with remote leaf CPU IDs. If two leaves share one remote
host, give them the same domain so strict mode can still catch sibling or exact
CPU overlap on that host.

Experimental fan result zipping knobs are available for locality tests.
`URING_PLAY_ZCNBLK_FAN_RESULT_ARENA=1` starts per-leaf result receiver threads
that publish range result-batch headers into `memfd`/`MAP_SHARED` rings for the
lane handler to consume. It requires `URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1` and
only supports the pipelined batched write path. `URING_PLAY_ZCNBLK_FAN_RESULT_ARENA_SLOTS`
sizes each ring, defaulting to `max(2 * URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW, 64)`.
`URING_PLAY_ZCNBLK_FAN_RESULT_ARENA_INLINE_PRIMARY=1` keeps leaf 0 result reads
on the handler lane and uses the arena only for secondary leaves.
`URING_PLAY_ZCNBLK_FAN_RESULT_ARENA_SPIN=1` makes arena receivers poll result
headers with `recv(MSG_DONTWAIT)` for experiments on dedicated cores. These are
userspace fan/interleave mechanisms; they are not block-device RAID primitives.
`URING_PLAY_ZCNBLK_FAN_HWM_ONLY_RESULTS=1` makes the fan treat write-only
zero-payload range result batches as branch high-water marks and complete the
submitted write batch after a batch-level segment-count check, rather than
expanding every 4K result back into per-request counters. It requires
`URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1`; if a leaf returns descriptor results
instead, the run fails instead of quietly falling back to a different completion
path.

Fan handler result waits default to ordinary blocking reads for everyday
traffic. `URING_PLAY_ZCNBLK_FAN_RESULT_WAIT_POLICY=adaptive` makes the handler
spin with `recv(MSG_DONTWAIT)` only once the lane has enough queued result work;
`URING_PLAY_ZCNBLK_FAN_RESULT_SPIN_MIN_OUTSTANDING` overrides that threshold,
defaulting to roughly one quarter of `URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW`.
`URING_PLAY_ZCNBLK_FAN_RESULT_WAIT_POLICY=greedy` or
`URING_PLAY_ZCNBLK_FAN_ULTRA_LOW_LATENCY=1` spins before every result wait for
dedicated-core, ultra-low-latency deployments. Bound it with
`URING_PLAY_ZCNBLK_FAN_RESULT_SPIN_BUDGET` unless the fan lanes have isolated
CPUs. Non-blocking wait policies print warnings when handler pinning or an
explicit lane-to-CPU map is missing.

Mixed read/write upstream batches stay on the request-batch path by default so
the fan and leaves use one result framing contract for writes and dirty-cache
reads. `URING_PLAY_ZCNBLK_FAN_SPLIT_MIXED_WRITES=1` is an experimental scheduler
knob for locality work and should be treated as opt-in until its ordering and
completion behavior has separate conformance coverage. For `mixed` and
`write-read-same`, the sender bounds in-flight reads with
`URING_PLAY_ZCNBLK_MIXED_READ_DRAIN_WATERMARK`; the default is a conservative
derived value from the read window and batch depth. This prevents a high-window
sender from filling the TCP path with writes while the paired read responses are
waiting behind socket backpressure.

For high-IOPS runs, size `URING_PLAY_ZCNBLK_BATCH_DEPTH` so descriptor chatter is
amortized without forcing per-leaf socket bursts that stall the lane. Record the
lane-to-worker and lane-to-CPU mapping with the result, plus the requested and
observed socket buffer sizing. On a busy shared local host, repeat the run before
treating the number as anything stronger than a smoke result; CPU contention,
powersave governors, socket buffer caps, memlock, and memory-bandwidth pressure
can move the score even when the copy and ordering counters are stable.

The WAL fan also has a bounded volatile write-back dirty budget. It is enabled
by default and accounts logical payload bytes admitted into the outstanding
write-batch pipeline until the corresponding leaf result batch advances the
replicated high-water mark. Configure it with
`URING_PLAY_ZCNBLK_FAN_WAL_MAX_DIRTY_BYTES`,
`URING_PLAY_ZCNBLK_WAL_MAX_DIRTY_BYTES`, or
`URING_PLAY_ZCNBLK_WAL_DIRTY_BYTES`; the soft limit defaults to 70% of the hard
limit and can be set with `URING_PLAY_ZCNBLK_FAN_WAL_SOFT_DIRTY_BYTES` or
`URING_PLAY_ZCNBLK_WAL_SOFT_DIRTY_BYTES`. Use `2G` or more for high-performance
fan/mirror benchmarks. Restricted runs such as `64M`, `256M`, or `512M` are
valid correctness/backpressure profiles, but they should be expected to
throttle. The fan summary prints dirty admit/release, current, max-observed,
pressure, and wait counters so benchmark logs show whether the run was
cache-sized correctly.
Dirty-cache entries lease the upstream batch payload buffer instead of copying
each 4K record into a separate cache allocation. Read-after-write conformance
should include a `zcmem` terminal-leaf `write-read-same` smoke with
`URING_PLAY_ZCNBLK_VERIFY_READS=1`; representative logs should show verified
read bytes at the client and `read_bytes=0` on the leaves for fully cached
same-sector reads.
Mixed request batches keep same-batch write leases in a lane-local run list and
only publish those runs to the shared dirty cache at a batch boundary or on a
local read miss. This avoids flushing every alternating 4K write/read pair
through the global dirty map while preserving strict read-after-write order.
The shared dirty extent map is range-sharded with
`URING_PLAY_ZCNBLK_FAN_DIRTY_EXTENT_SHARDS` and
`URING_PLAY_ZCNBLK_FAN_DIRTY_EXTENT_SHARD_RECORDS`; the default span is 16K
4K records, so 64 MiB-spaced lanes land on separate extent locks. Benchmark
logs must report `dirty_extent_shards`, `dirty_extent_shard_records`,
`phase_dirty_admit_seconds`, and context switches when comparing lane counts.
Fully cached fan reads should stay in leased extent form until the upstream
response write. Adjacent leased read parts are coalesced into large response
iovecs; a result that falls back to materialized 4K buffers or one iovec per
record is a control path, not the target 600 Gbit/s fan-cache architecture.

When `URING_PLAY_ZCNBLK_WRITE_ACKS=1`, `zcnblk-fan --engine wal` defaults to
`URING_PLAY_ZCNBLK_WAL_WRITE_ACK_MODE=admit`: pure write batches ACK after local
dirty-budget admission and leaf-stream submission, while explicit block flushes
travel as `ZCNBLK_OP_SYNC` and return `ZCNBLK_OP_SYNC_ACK` only after the global
fan-WAL dirty HWM drains and leaf sync results return. Set
`URING_PLAY_ZCNBLK_WAL_WRITE_ACK_MODE=remote` to make ordinary write ACKs wait
for leaf result batches too.

For sync-bound writeback runs, `URING_PLAY_ZCNBLK_FAN_ASYNC_WRITEBACK=1` lets
the fan complete writes after bounded dirty-cache admission and moves leaf
submission to a userspace writeback thread. The writeback thread can coalesce
adjacent queued leaf work with
`URING_PLAY_ZCNBLK_FAN_ASYNC_MERGE_LEAF_BATCHES=1`,
`URING_PLAY_ZCNBLK_FAN_ASYNC_MERGE_MAX_BATCHES`,
`URING_PLAY_ZCNBLK_FAN_ASYNC_MERGE_MAX_RECORDS`,
`URING_PLAY_ZCNBLK_FAN_ASYNC_MERGE_MAX_PAYLOAD_BYTES`, and the bounded busy
wait `URING_PLAY_ZCNBLK_FAN_ASYNC_MERGE_SPIN_USEC`. This path requires range
result batches; disabling `URING_PLAY_ZCNBLK_WAL_RESULT_RANGES` is a startup
error when async leaf merging is enabled. Async writeback logs
`async_copied_payload_bytes` and `async_leased_payload_bytes`; representative
mirror runs should show branch payload leaving through leased ranges, not
pre-egress fan-side payload copies.
`URING_PLAY_ZCNBLK_FAN_ASYNC_RESULT_WINDOW` defaults to `4096` result batches so
immediate async writeback can keep a lane's leaf streams moving until a sync,
EOF, dirty-budget pressure, or the explicit window is reached. Smaller values
are useful diagnostics for result-ack churn, but representative high-throughput
runs should report the window and the `result_window_drains` counters.
`URING_PLAY_ZCNBLK_FAN_ASYNC_WRITE_EXTENTS=1` is enabled by default and emits
compact `WRITE_EXTENT_BATCH` records for contiguous write runs instead of one
128-byte descriptor per 4K record on the async leaf path. The fan summary prints
`async_write_extent_batches`, `async_write_extents`, and
`async_saved_descriptor_bytes`. Per-batch extent tracing is intentionally
separate and opt-in with `URING_PLAY_ZCNBLK_FAN_ASYNC_WRITE_EXTENT_TRACE=1`
because trace logging is not part of a representative IOPS run.
When fan handlers are pinned, the fan now reports CPU topology by runtime slot:
`fan-handler` uses affinity slots `0..total_connections`, async writeback uses
`fan-async-writeback` slots starting at `total_connections`, and result-arena
receiver threads use `fan-result-leafN` slots after the handler range. With four
fan lanes, for example, `URING_PLAY_PIN_CPU_LIST=4,5,6,7,16,17,18,19` maps
handlers to `4-7` and async writeback to `16-19`; under
`URING_PLAY_TOPOLOGY_STRICT=1`, the fan refuses representative numbers when any
active fan/client/leaf stage wraps, overlaps, or SMT-pairs without being stated.
For `zcleasemem` leaves, `URING_PLAY_ZCNBLK_WAL_LEAF_ZCLEASEMEM_MEMFD=1`
receives the compact extent table in userspace but splices the bulk extent
payload from the leaf TCP socket into a memfd lease log. Later cached reads can
then be served by reference/sendfile from the leaf lease cache instead of
materializing the payload into a fresh heap buffer. This is an opt-in WAL leaf
mode and does not make placement, mirror, stripe, tier, or spill decisions.
For same-host fan-to-leaf RAM tests, `zcnblk-fan --engine wal` can use
`--leaves local:zcleasemem:SIZE,local:zcleasemem:SIZE` instead of starting
separate `zcnblk-wal-leaf` TCP processes. This path requires
`URING_PLAY_ZCNBLK_FAN_ASYNC_WRITEBACK=1` and range results, cannot currently
mix local and TCP leaves, and is intentionally limited to async writes plus
dirty-cache or `zcleasemem` read hits. Full uncached local reads now return
leased local leaf parts, with unwritten holes backed by a reusable zero lease
controlled by `URING_PLAY_ZCNBLK_ZERO_LEASE_BYTES`; representative zero-copy
runs should show `read_cache_materialized_bytes=0`. The local leaf adopts
fan-owned payload leases; it is not a block mirror/stripe primitive, and
`/dev/zcbrdN` or other block devices remain terminal media only after userspace
placement has already happened.
`URING_PLAY_ZCNBLK_FAN_LOCAL_INLINE_WRITEBACK=1` keeps that same
`local:zcleasemem` lease-publish path on the fan handler thread instead of
waking a per-lane local writeback worker. Use it only for in-process local
leaves; the fan prints `zcnblk-fan-wal-local-inline-writeback` lines and the
summary still reports the work in the `async_*` counters for A/B comparison.
Recent local tests showed this removes the worker handoff context switches but
does not remove the client/fan TCP socket payload copies.
For mirror mode, `URING_PLAY_ZCNBLK_FAN_MIRROR_READ_POLICY=extent` selects one
mirror leg for an entire read extent instead of splitting a large read into 4K
mirror stripes. `URING_PLAY_ZCNBLK_FAN_MIRROR_READ_EXTENT_BYTES=N` controls the
load-balance unit and accepts the same `K/M/G` suffixes as CLI size arguments;
the fan startup line prints `mirror_read_policy=...` so benchmark artifacts
state the read placement policy.
`URING_PLAY_ZCNBLK_FAN_MEMFD_SEND_COALESCE_BYTES=N` coalesces adjacent memfd
payload ranges before the fan sends them onward. For cached read responses this
is required to avoid one sendfile per 4 KiB dirty-cache hit: range response
headers can collapse descriptor chatter while the payload path is still
fragmented. The default `0` preserves one sendfile per existing range; benchmark
runs that rely on the memfd dirty cache should report this value and the
`read_cache_response_*` counters.
`URING_PLAY_ZCNBLK_FAN_INGRESS_MEMFD_PAYLOAD=1` is an opt-in fan ingress
experiment that splices upstream write payload bytes into the fan memfd dirty
log and forwards leaf write payloads as memfd ranges. Use it with
`URING_PLAY_ZCNBLK_FAN_MEMFD_DIRTY_CACHE=1` when measuring whether the fan can
avoid heap materialization on the write/mirror path.
`scripts/zcnblk-fanwal-plan-bench.sh` exposes the corresponding high-IOPS
topology knobs as stable environment variables: `MIRROR_READ_POLICY`,
`MIRROR_READ_EXTENT_BYTES`, `FAN_RESULT_WAIT_POLICY`,
`FAN_RESULT_SPIN_BUDGET`, `FAN_RESULT_SPIN_MIN_OUTSTANDING`,
`FAN_UPSTREAM_SPIN_READS`, `SEND_WRITE_WINDOW`, `SEND_READ_WINDOW`, and
`LEAF_SPIN_BUDGET`. `FAN_LOCAL_INLINE_WRITEBACK=1` forwards the inline local
writeback knob to the fan and records it in `topology.env`. For in-process
`local:zcleasemem` leaves the runner no longer passes external leaf CPU groups
to the fan; representative local-leaf artifacts must show
`fan_leaf_cpu_lists=in-process-local-leaves` and the fan startup line must show
`fan_leaf_transport=shared-arena`.
For strict zero-copy validation, set `URING_PLAY_ZCNBLK_WAL_ZERO_COPY_STRICT=1`.
The fan rejects non-zero `materialized_payload_copy_bytes`; the WAL leaf rejects
non-zero heap payload-data receive or copy-submit counters. Heap descriptor
bytes are still reported because compact extent tables are control metadata, not
the data payload. Use this for representative lease/memfd runs so copy fallbacks
cannot masquerade as zero-copy results.
The WAL leaf summary prints `payload_read_bytes`, heap-vs-memfd receive bytes,
lease-vs-copy submit bytes, and `phase_payload_read_seconds`,
`phase_decode_seconds`, `phase_submit_seconds`, and
`phase_result_write_seconds`. Treat those phase counters as required evidence:
recent local loopback runs showed leaf lease adoption was effectively free while
TCP payload receive dominated, and the leaf memfd splice path was slower than
heap receive plus lease adoption. Do not assume a "zero-copy" knob is a win
unless those counters and end-to-end throughput prove it on the target topology.
For dedicated-core WAL leaf receive tests,
`URING_PLAY_ZCNBLK_WAL_LEAF_SPIN_READS=1` with
`URING_PLAY_ZCNBLK_WAL_LEAF_SPIN_BUDGET=N` enables bounded
`recv(MSG_DONTWAIT)` spinning before blocking. `URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN=1`
or `URING_PLAY_ZCNBLK_WAL_LEAF_SPIN_POLICY=adaptive` makes that budget adaptive;
use `URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MIN`,
`URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_SPIN_MAX`, and
`URING_PLAY_ZCNBLK_WAL_LEAF_ADAPTIVE_WAIT_NS` to bound it. The WAL leaf startup
line prints `leaf_spin_policy`, `leaf_spin_budget`, and the adaptive min/max/wait
values. On June 7, 2026, a topology-explicit local 4-lane mirror run moved from
about 36.5 Gbit/s with blocking leaf receives to about 40.4 Gbit/s with bounded
leaf receive spin; spinning sender/fan result reads did not improve that run.
`URING_PLAY_ZCNBLK_FAN_DEFER_UNSYNCED_WRITEBACK=1` is the default when async
writeback is enabled. It queues unsynced leaf writeback until explicit sync,
EOF, or dirty-budget pressure. Ordinary writes are ACKed after bounded local
admission and are visible to later reads through the dirty cache; sync remains
the replicated high-water-mark fence. Set it to `0` when deliberately measuring
immediate background leaf submission. `URING_PLAY_ZCNBLK_UNINIT_READ_BUFFERS=1`
is an experimental receive-buffer knob; the default is false because local TCP
tests were faster and more stable with prefaulted zeroed receive buffers.

`zcfanout-logzip-bench` isolates that zipper. It consumes materialized monotonic
branch result logs in memory and reports descriptor-equivalent IOPS without
claiming TCP or block-device throughput:

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-31 \
  zcfanout-logzip-bench mirror-write 32 2 1000000 4K 8192 32 64 true
```

`zcha-readview-bench` isolates the HA read-authority snapshot. It validates a
cached leader hash, term, configuration epoch, lease expiry, and committed HWM
using a cache-line-aligned atomic view. It does not perform block I/O and must
not be reported as `/dev/zcnblk0` IOPS:

```bash
URING_PLAY_PIN_CPU_LIST=0 URING_PLAY_TOPOLOGY_STRICT=1 \
  target/release/zcha-readview-bench 1000000000 1
```

The first positional argument is total checks and the second is worker count.
Supply at least one pinned CPU per worker. Strict/fatal topology mode rejects an
unpinned run before printing a benchmark result.

`zcwal-reduce-bench` isolates the local WAL-combination and reduce-blockstore
problem without using a block device, TCP, RDMA, or terminal media. It measures
lane-local WAL append, dirty read freshness, and a separate userspace reduced
blockstore view:

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

Use `--mode combine` for WAL append only, `--mode reduce` for WAL append plus
reduction into the userspace blockstore, `--mode read` for reduced-view reads,
and `--mode mixed` for dirty-map read/write freshness. `--reduce-every-extents`
adds a sync-like reducer high-water-mark profile. The output always prints
lane-to-worker and worker-to-CPU mapping plus context switches.

Use `--read-access copy` to force 4 KiB payload materialization on reads,
`--read-access ref` to model descriptor/native dirty-cache reads where the read
returns a WAL/blockstore slot reference instead of copying the payload, or
`--read-access forward-ref` to hand those references to a bounded downstream
NIC/RDMA-style descriptor queue. In ref and forward-ref modes, copied read
traffic stays at `read_Gbitps=0`, logical referenced read traffic is reported as
`read_ref_Gbitps`, and downstream reference forwarding is reported as
`forward_ref_Gbitps` with event/completion counts.

Use `--read-access forward-extent` for the ordered read-cache case where the fan
returns one descriptor for a contiguous WAL/result-log extent instead of one
descriptor per 4 KiB logical record. This mode requires `--mode read` or
`--mode hot-read` with `--pattern seq`; random read coalescing must use an
explicit range builder rather than pretending unrelated sectors are contiguous.
The output reports both logical 4 KiB IOPS and `forward_events_per_sec`; use the
event rate to judge zipper/descriptor cost and the logical rate to size the
covered block workload.

Example leased fan-cache ceiling runs:

```bash
target/release/zcwal-reduce-bench \
  --mode hot-read --lanes 8 --workers 8 \
  --records-per-lane 1048576 --block-records-per-lane 1048576 \
  --record-bytes 4096 --extent-records 256 --read-repeats 1000 \
  --read-access forward-extent --write-access lease \
  --forward-window 4096 --pin --cpu-list 0-7 --thp
```

On the local June 7, 2026 bare-metal shared-extent run
`bench-results/local-zcwal-shared-extent-iovmerge-20260607T165507Z/`, 8 workers
were pinned one-to-one to CPUs `0-7`. The forced-copy read control reached about
138.7M logical 4K reads/s and touched about 4.54 Tbit/s of memory. The per-4K
`forward-ref` path reached about 1.25B descriptor events/s. The
`forward-extent` path resolved real dirty extent-index entries and reached about
433.9M extent descriptor events/s for 2048-record extents, or about 888.6B
logical 4K reads/s, with zero payload copies and near-zero context switching.
Those are local descriptor/cache ceilings, not TCP or RDMA wire claims.

The same result directory also includes a local TCP hot-cache control using
`zcfan-readcache-bench`: memfd/sendfile fan egress reached about 440-446 Gbit/s
over 8 loopback lanes, while a userspace `read_exact` client sink consumed about
97 Gbit/s and a splice-to-`/dev/null` sink consumed about 114 Gbit/s but caused
about 78K voluntary context switches. Treat that as evidence that pipe/splice
or materialized client sinks can dominate the local test; it is not a block or
RAID primitive and it is not a substitute for remote multi-NIC validation.
Current `zcfan-readcache-bench` output includes `recv_ops`, `send_ops`,
`short_recv_ops`, `max_recv_op_bytes`, `pipe_capacity_bytes`,
`thread_cpu_seconds`, and `cpu_wall_ratio` for splice, `read`, and `waitall`
client drains, so short receive churn and CPU-copy limits are visible. For bulk
hot-cache or cold-cache drain tests, set and report `--rcvlowat-bytes`/
`URING_PLAY_ZCFAN_READCACHE_RCVLOWAT_BYTES` near 256 KiB to the splice chunk
size; leave it unset for low-QD latency tests where waiting for a large receive
low-water mark would be misleading.

On the local June 8/9, 2026 counted receive runs, a hot-cache `read`/`waitall`
client with `SO_RCVLOWAT=1M` received full 1 MiB chunks (`short_recv_ops=0`) but
stalled around 167-171 Gbit/s while burning 5-10 CPU-wall cores. The splice
client avoided that user copy and reached about 290 Gbit/s on 8 lanes and
381 Gbit/s on 16 lanes, but still reported short socket-splice wakeups. A clean
8-lane cold fan run with corrected `leaf-send --bytes-per-lane` semantics showed
`stream-direct` at about 150 Gbit/s downstream plus 150 Gbit/s leaf ingress
(`total_io_Gbitps` about 299), while the leaf sources alone could produce
roughly 577-780 Gbit/s. Interpret cold fan results by both downstream Gbit/s and
`total_io_Gbitps`: on a one-host loopback test, each cold byte is counted once
from leaf to fan and once from fan to client, so the downstream leg is expected
to be roughly half of the local aggregate socket/pipe movement budget.

Writes are lease-backed by default: inbound ZCRX/RDMA/provided-buffer payload
memory is modeled as adopted by the dirty pool by descriptor, with no
`--write-access` flag required. `--write-access copy` is currently fatal because
copy has not been accepted for this dirty-pool path; if we need copy later, it
may belong in another layer of the code. Do not silently fall back to payload
copies while validating the zero-copy design. In lease mode, copied WAL traffic
stays at `wal_Gbitps=0`, logical admitted write traffic is reported as
`wal_ref_Gbitps`, and `touched_Gbitps` counts only payload bytes actually copied
by materialized reads or reduction.

For large ordered read streams, benchmark the range-HWM zipper instead of
per-4K result descriptors. Each branch publishes contiguous result-log HWMs, and
the zipper emits the minimum contiguous range across branches:

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-31 \
  zcfanout-logzip-bench stripe-read-hwm 32 8 10000000 4K 32768 32 0 true
```

For the lowest-overhead descriptor hot-loop, build the standalone C variant.
It generates branch result descriptors analytically, avoids pre-materialized
branch vectors, and still reports explicit lane-to-worker and worker-to-CPU
mapping:

```bash
cc -O3 -march=native -Wall -Wextra -pthread \
  -o /tmp/zcfanout_logzip_fast_bench tools/zcfanout_logzip_fast_bench.c

URING_PLAY_PIN_CPU_LIST=0-31 \
  /tmp/zcfanout_logzip_fast_bench \
    --mode mirror-write \
    --lanes 32 \
    --branches 2 \
    --records-per-lane 1000000 \
    --payload-bytes 4K \
    --window 8192 \
    --workers 32 \
    --skew 64 \
    --pin true
```

`zcfanout-logtcp-bench` puts the same result-log zipper behind real TCP streams
on each lane and branch. It still sends compact result descriptors, not payload
blocks, and it does not use block devices as mirror or stripe primitives. Its
summary includes worker CPU time, voluntary context switches, involuntary
context switches, and migrations so local oversubscription is visible:

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-31 \
  zcfanout-logtcp-bench mirror-write 127.0.0.1 24000 16 2 250000 4K 1024 16 true
```

`zcfanout-logshm-bench` isolates the primary/secondary shared-memory handoff.
It allocates a `memfd` arena, maps it with `MAP_SHARED` into separate primary
and secondary views, and stores secondary result descriptors in cache-line
aligned ring slots. The primary leg is processed inline by the zipper thread,
the secondary leg publishes descriptor progress through mapped atomics, and the
primary interleaves the two ordered result streams. Use `spin` to prove the
handoff can avoid voluntary context switches and `condvar` to model a blocking
batch handoff.

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=4,5 \
  zcfanout-logshm-bench 2000000 4K 2048 2048 spin true
```

`zcfanout-shmlease-bench` extends that control path with a memfd payload arena.
It measures same-host fan-to-leaf ownership transfer where payload bodies stay
in shared mapped storage and the result stream carries descriptors, not copied
4K buffers. `touch=none` measures descriptor lease overhead, `touch=cacheline`
measures descriptor plus first/last cacheline inspection, and `touch=full`
forces a full payload cacheline sweep so memory bandwidth shows up explicitly.

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-15 \
  zcfanout-shmlease-bench 5000000 4K 2048 8192 cacheline spin true 8
```

The optional tail is `lanes workload sync_records working_set_records`. Each
lane gets a primary fan thread and a secondary leaf thread, using affinity
indexes `lane*2` and `lane*2+1`. `workload=write-read-same` serves same-sector
reads from the local dirty lease by reference and waits for mirrored leaf HWMs
only at arena pressure or `sync_records` boundaries; `sync_records=0` means EOF
sync only. `workload=random-write-read-same` additionally uses a deterministic
random working set and verifies that dirty-table reads return the latest
descriptor token for each logical page.
`workload=zcnblk-shm-onramp` flips the same arena into a first-hop
client-to-fan control: the client-onramp thread publishes zcnblk-shaped
write/read requests into shared memory, the fan-ingress thread consumes them in
order and publishes read responses, and the client drains responses on window
pressure, `sync_records`, or EOF. It is a same-host transport prototype only;
it does not make mirror, stripe, tier, spill, or leaf-placement decisions.
`workload=zcnblk-shm-pipeline` extends that into a three-role local
client-onramp -> fan-stage -> leaf-stage pipeline. The fan bridges an ingress
ring to a leaf ring and forwards payload ownership by descriptor reference back
to the ingress arena, so the leaf reads the original payload without a fan-side
copy. Client-visible response progress and payload slot reuse are released at
the leaf HWM (`client_response_release=leaf-hwm`), so descriptors cannot point
at a client-reused slot. This is a single-leaf transport/control proof, not a
RAID placement primitive.
`workload=zcnblk-shm-mirror` extends the shape to four roles per lane:
client-onramp -> fan-mirror -> leaf0-stage + leaf1-stage. The fan is the
userspace mirror primitive: it publishes two branch descriptors that both lease
the same ingress payload, and client-visible completion plus ingress slot reuse
advance only at the minimum leaf HWM
(`client_response_release=mirror-leaf-hwm`). The branch payload is not copied;
copy fallback is treated as fatal in this control path.
By default the synthetic fan and leaf stages also inspect the payload according
to `touch=...` so full-touch runs expose memory bandwidth. Set
`URING_PLAY_SHMLEASE_FAN_TOUCH_PAYLOAD=0` or
`URING_PLAY_SHMLEASE_LEAF_TOUCH_PAYLOAD=0` to measure descriptor/lease
forwarding without that role rereading payload bytes. That mode is a forwarding
control, not a data-verification run.
Set `URING_PLAY_SHMLEASE_HWM_ONLY_RESULTS=1` to make zcnblk shared-memory
onramp, pipeline, and mirror stages publish ordered range HWMs instead of
per-record result descriptors. This models the fast write-heavy WAL contract:
payload ownership and slot reuse are still gated by leaf HWMs, while the client
edge can complete local requests from the HWM range without remote per-record
result churn. Leave it off when validating per-record response checksum logic.
For the `zcnblk-shm-*` workloads, a nonzero `working_set_records` tail switches
the synthetic request stream from linear sectors to deterministic random logical
pages. The logical page is carried in the zcnblk request token, the fan updates a
per-lane dirty-token table, and response drains verify that reads see the latest
dirty token for that page without materializing a 4K read buffer.
These workloads validate a shared-WAL descriptor contract: token-decoded
logical page, payload offset/length/checksum, response checksum, and HWM release
are checked through the same code path for onramp, pipeline, and mirror stages.
The zcnblk shared-memory runs print `shm_wal_contract_version=1`. Their
`client_fan_socket_payload_copy_bytes_lower_bound=0` and
`client_fan_tcp_socket_payload_copy_avoided_bytes=...` fields line up with the
TCP fan-WAL `socket_payload_copy_bytes_lower_bound` field, making the avoided
same-host TCP onramp cost explicit. Pipeline and mirror runs also print
`fan_leaf_socket_payload_copy_bytes_lower_bound=0` and
`fan_leaf_tcp_socket_payload_copy_avoided_bytes=...` for the leaf handoff.
The same controls print a `*-wait-latency-summary` line with `sync_wait_*` and
`dirty_window_wait_*` histograms. Pipeline and mirror controls also include
`fan_leaf_sync_wait_*` and `fan_leaf_dirty_window_wait_*`, which separate
client/fan waiting from fan/leaf HWM waiting. For a true low-queue-depth sync
smoke, use `batch_records=1 sync_records=1`; otherwise syncs are observed at
batch boundaries.
Representative output must include the `lane_cpu_map=` summary before the number
is accepted as a topology-aligned same-host result. On a busy local host, repeat
runs before trusting absolute IOPS; CPU and memory-bandwidth contention can move
the score while copy bytes and context-switch counts remain the better signal.
The shared-memory summaries print `first_hop_write_bytes` and
`first_hop_Gibitps` for client-to-fan traffic. Pipeline and mirror summaries
also print `leaf_reference_Gibitps` or `mirror_reference_Gibitps`; those count
descriptor-referenced branch payload ownership, not a block-device RAID path.
They also print `descriptor_slot_bytes`, `control_descriptor_bytes`,
`payload_reference_bytes`, `observed_payload_touch_bytes`, and
`local_memory_traffic_bytes_lower_bound`. Read these together:
`payload_reference_bytes` is ownership passed by descriptor, not a copy;
`observed_payload_touch_bytes` is intentional benchmark payload write/read
traffic; `control_descriptor_bytes` is the descriptor-ring metadata footprint;
and `payload_copy_bytes` must remain zero for these zero-copy controls.
Mirror dirty-cache summaries also print `client_visible_read_records` and
`internal_dirty_cache_read_records`. Only `client_visible_read_records` belongs
in logical block IOPS; the internal value is the fan's local dirty-cache lookup
probe and is reported so it cannot accidentally inflate benchmark claims.

`scripts/zcfanout-shmlease-ladder.sh` wraps those controls into a repeatable
same-host ladder. It builds `zcfanout-shmlease-bench` when needed, pins each
lane role to an explicit CPU, skips lane counts that cannot be pinned without
SMT overlap unless `ALLOW_SMT_OVERLAP=1`, and writes `summary.tsv` with
workload, lanes, touch mode, topology preflight representative/warning fields,
IOPS, Gbit/s, copy bytes, reference bytes, descriptor bytes, payload slots,
client-visible/internal dirty-cache read counts, ack/sync wait, context
switches, wait-latency p50s, `leaf_batch_delay_ns`, and `lane_cpu_map`.
Use it before changing the integrated TCP/RDMA fan path so descriptor/HWM
changes can be separated from socket or NIC movement. On a shared local system,
keep `REPEATS` greater than one and compare the copy/context-switch counters as
well as the absolute IOPS.

```bash
RUN_ID=local-shmlease-ladder-$(date -u +%Y%m%dT%H%M%SZ) \
LANES_LIST="1 2 4" TOUCH_MODES="none cacheline" \
RECORDS=300000 BATCH_RECORDS=2048 WINDOW=8192 SYNC_RECORDS=8192 REPEATS=2 \
  scripts/zcfanout-shmlease-ladder.sh
```

`ACK_RECORDS=N` models write acknowledgement and payload-slot release without
an explicit sync. `SYNC_RECORDS=N` remains the sync/HWM durability cadence.
`WORKING_SET_RECORDS=N` passes the final `working-set-records` argument through
the ladder wrapper for `zcnblk-shm-*` random logical page tests; `summary.tsv`
records `working_set_records` and `dirty_table_bytes`.
`PAYLOAD_SLOTS=N` decouples the hot payload ring from the descriptor window:
for example `WINDOW=65536 PAYLOAD_SLOTS=8192 ACK_RECORDS=8192` keeps a larger
descriptor/HWM window while reusing a smaller topology-local payload ring after
ack release. The runner records `payload_slots_per_lane` and fails under strict
topology if payload slots are smaller than the ack/sync release cadence.
`ARENA_HUGEPAGE=1` requests `MADV_HUGEPAGE`; `ARENA_PREFAULT=1` faults arena
pages before timing. Use `ALLOW_SMT_OVERLAP=1` only for an explicit SMT study;
the runner logs a warning and those numbers are topology-specific.
For the zero-copy forwarding target, set `FAN_TOUCH_PAYLOAD=0` and
`LEAF_TOUCH_PAYLOAD=0`; leaving them enabled is a deliberate payload-inspection
control and should be reported as such. Set
`ZERO_COPY_FORWARDING_TARGET=1` in the ladder wrapper, or
`URING_PLAY_SHMLEASE_ZERO_COPY_FORWARDING_TARGET=1` for direct runs, when a
representative forwarding run must warn or fail under
`URING_PLAY_TOPOLOGY_STRICT=1` instead of silently rereading payload bytes in
fan or leaf stages.

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-7 \
  zcfanout-shmlease-bench 5000000 4K 2048 8192 cacheline spin true 4 write-read-same 8192
```

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-3 \
  zcfanout-shmlease-bench 20000 4K 1 256 cacheline spin true 2 zcnblk-shm-onramp 1
```

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-7 \
  zcfanout-shmlease-bench 5000000 4K 2048 8192 cacheline spin true 4 random-write-read-same 0 65536
```

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-7 \
  zcfanout-shmlease-bench 5000000 4K 2048 8192 cacheline spin true 4 zcnblk-shm-onramp 8192
```

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-11 \
  zcfanout-shmlease-bench 2000000 4K 2048 8192 cacheline spin true 4 zcnblk-shm-pipeline 8192
```

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-15 \
URING_PLAY_SHMLEASE_HWM_ONLY_RESULTS=1 \
  zcfanout-shmlease-bench 2000000 4K 2048 8192 cacheline spin true 4 zcnblk-shm-mirror 8192
```

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-15 \
URING_PLAY_SHMLEASE_FAN_TOUCH_PAYLOAD=0 \
URING_PLAY_SHMLEASE_LEAF_TOUCH_PAYLOAD=0 \
URING_PLAY_SHMLEASE_ZERO_COPY_FORWARDING_TARGET=1 \
  zcfanout-shmlease-bench 2000000 4K 2048 8192 cacheline spin true 4 zcnblk-shm-mirror 8192
```

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-15 \
  zcfanout-shmlease-bench 500000 4K 2048 8192 cacheline spin true 4 zcnblk-shm-mirror 8192 65536
```

`workload=zcnblk-shm-mirror-dirty-cache` adds a same-host dirty-cache read
probe into the mirror fan path. The default backend is
`URING_PLAY_SHMLEASE_DIRTY_CACHE_MODE=arena-hwm`: a lane-local arena lease ring
that holds shared-WAL payload ownership until the mirror leaf HWM allows commit
and slot reuse. It is the fast topology-local path for linear WAL streams and
for deterministic random working sets when the metadata table is at least the
working-set size. Use `URING_PLAY_SHMLEASE_DIRTY_CACHE_MODE=general` only as a
regression control for the hash-map-backed production dirty cache; it is much
slower on this topology and remains linear-only in this benchmark.
By default `URING_PLAY_SHMLEASE_DIRTY_EARLY_RESPONSES=1` reports unsynced
client responses after local dirty-cache admission while keeping payload slot
reuse on the mirror leaf HWM. Set it to `0` to force client-visible responses
to wait for mirror leaf HWM. `URING_PLAY_SHMLEASE_LEAF_BATCH_DELAY_NS=N` is a
benchmark-only busy-spin delay before each leaf HWM publish; use it to prove
that local dirty responses decouple client latency from a lagging mirror leaf
without introducing sleep-driven context switches.

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-15 \
URING_PLAY_SHMLEASE_FAN_TOUCH_PAYLOAD=0 \
URING_PLAY_SHMLEASE_LEAF_TOUCH_PAYLOAD=0 \
URING_PLAY_SHMLEASE_ZERO_COPY_FORWARDING_TARGET=1 \
  zcfanout-shmlease-bench 500000 4K 2048 8192 none spin true 4 zcnblk-shm-mirror-dirty-cache 8192
```

```bash
WORKLOADS="zcnblk-shm-mirror-dirty-cache" LANES_LIST="4" TOUCH_MODES="none cacheline" \
WINDOW=65536 PAYLOAD_SLOTS=65536 SYNC_RECORDS=65536 WORKING_SET_RECORDS=65536 \
HWM_ONLY_RESULTS=1 FAN_TOUCH_PAYLOAD=0 LEAF_TOUCH_PAYLOAD=0 \
ZERO_COPY_FORWARDING_TARGET=1 REPEATS=3 \
  scripts/zcfanout-shmlease-ladder.sh
```

`workload=zcnblk-fan-dirty-cache` isolates the production fan WAL dirty cache
without TCP, block devices, or mirror/stripe placement. It writes exact 4K
leased dirty records, reads them back by descriptor, and prints copy,
materialization, context-switch, and lane-map counters. Use it to validate that
random dirty read/write freshness stays on the record path; exact 4K writes must
not fall into the extent BTreeMap path. Materialized fallback reads must use the
same sequence-aware record/extent resolver as leased descriptor reads; otherwise
an older covering extent can hide a newer exact 4K dirty record.

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-3 \
  zcfanout-shmlease-bench 1000000 4K 2048 8192 cacheline spin true 4 zcnblk-fan-dirty-cache 0 65536
```

`URING_PLAY_ZCNBLK_FAN_DIRTY_DIRECT_RECORDS=N` enables an experimental bounded
direct lookaside in front of the production dirty cache. It is not recommended
by default: after exact 4K writes were moved to the record cache path, repeated
local runs were faster with the normal record map than with the extra direct
admission work. Keep it as a diagnostic knob for collision/cache experiments.
On a busy shared local host, repeat the command before treating the absolute
number as meaningful. The July 8, 2026 repeat artifact
`bench-results/local-zcnblk-dirty-cache-overlay-repeat-20260708T171136Z` held at
60.7-61.3M logical 4K IOPS, with zero payload copies, zero materialized reads,
no migrations, and explicit `lane_cpu_map=lane0:worker_cpu0,...`.

```bash
URING_PLAY_ZCNBLK_WRITE_ACKS=1 \
URING_PLAY_EXPECT_ROUTE_DEV=ens146 \
URING_PLAY_EXPECT_ROUTE_SRC=10.0.1.20 \
zcnblk-fan --engine wal --leaves 10.0.1.31,10.0.1.32 --bind 10.0.1.20 \
  --base-port 23600 --ports 64 --connections-per-port 1 \
  --bytes-per-connection 64G --chunk-bytes 4K --stripe-bytes 4K \
  --leaf-base-port 24600 --pin-handlers true --mode stripe
```

## Descriptor Commands

### zcplan

`zcplan` emits the topology-aware descriptor contract used by high-IOPS mux,
WAL, and userspace RAID planning. `zcplan caps` reports local `ZC_CAPS_V1`
state: CPUs, NUMA grouping, SMT sibling groups, physical NIC queues, hugetlb,
memlock, and transport capability placeholders. `zcplan plan` compiles a
`ZC_PLAN_V1` from workload intent into lane, worker, CPU, NIC queue, WAL shard,
branch, zipper, coalescing, and backpressure maps.

The command plans only userspace fabric topology. It never implements mirror,
stripe, tier, or spill inside a block device. Terminal leaves can still be
reported as capabilities or used behind a userspace leaf writer after placement
has already been decided.

```bash
zcplan caps --role fan --node-id fan-a --cpu-list 0-31 --nics ens34

zcplan plan \
  --mode mirror \
  --operation-mix write \
  --objective max-iops \
  --lanes 32 \
  --workers 32 \
  --branches 2 \
  --cpu-list 0-31 \
  --nics ens34 \
  --extent-bytes 384K \
  --batch-window 16 \
  --zero-copy required
```

For a dual-card EFA mirror plan, pass the provider domains explicitly so each
mirror leg owns a separate userspace branch topology:

```bash
zcplan plan \
  --mode mirror \
  --transport libfabric-efa \
  --libfabric-domains efa_0-rdm,efa_1-rdm \
  --libfabric-smoke-ok \
  --lanes 32 \
  --workers 32 \
  --branches 2 \
  --cpu-list 0-31,96-127 \
  --extent-bytes 384K \
  --batch-window 16 \
  --zero-copy required
```

The emitted `compiled.parallel_raid.branch_topology` maps each mirror leg or
stripe shard to a fabric domain, CPU slice, lane set, ACK policy, result-log
contract, and `block_device_raid_primitive=false`.

### zcraid-mirror-send / zcraid-mirror-recv

Run a branch-topology-aware mirror commit benchmark. With an extent size of
`4K`, the sender writes each logical 4 KiB WAL record to every userspace mirror
branch and counts the record as committed only after all branch ACKs arrive.
With larger ordered extents such as `384K` or `1M`, each extent is still
accounted as logical 4 KiB records, but data and ACKs move as WAL ranges. This
benchmark does not use a block device as a mirror primitive; block devices may
only sit behind a terminal leaf writer after userspace placement has already
happened.

For the TCP path, every branch/lane extent is sent as a `ZcWalExtentHeader`
followed by its payload. Receivers validate branch, lane, sequence, logical
range, payload length, and WAL offset before sending an ACK. The sender counts a
mirror extent as committed only when every branch ACK matches that ordered
tail-accepted range.

Receiver, one process per mirror branch:

```bash
URING_PLAY_PIN_CPUS=1 \
URING_PLAY_PIN_CPU_LIST=0-31 \
URING_PLAY_RAID_MIRROR_ACK_WINDOW=32 \
URING_PLAY_OFI_COMPACT_4K=1 \
URING_PLAY_OFI_PAYLOAD_INJECT=1 \
URING_PLAY_OFI_ACK_INJECT=1 \
URING_PLAY_OFI_CQ_SLEEP_NS=0 \
FI_EFA_USE_DEVICE_RDMA=1 \
zcraid-mirror-recv ofi auto 42000 0 64M 4K 32 plan.json efa rdm true
```

Sender:

```bash
URING_PLAY_PIN_CPUS=1 \
URING_PLAY_PIN_CPU_LIST=0-31,96-127 \
URING_PLAY_RAID_MIRROR_ACK_WINDOW=32 \
URING_PLAY_OFI_COMPACT_4K=1 \
URING_PLAY_OFI_PAYLOAD_INJECT=1 \
URING_PLAY_OFI_ACK_INJECT=1 \
URING_PLAY_OFI_CQ_SLEEP_NS=0 \
FI_EFA_USE_DEVICE_RDMA=1 \
zcraid-mirror-send ofi 172.31.38.204 42000,44000 64M 4K 32 plan.json efa rdm true
```

`zcraid-mirror-send` accepts either one peer address for same-host smoke tests
or one address per branch for real userspace RAID1 leaves. For a two-leaf,
dual-NIC EFA run, use `addr0,addr1` with matching branch base ports, for
example:

```bash
zcraid-mirror-send ofi 172.31.40.44,172.31.37.202 \
  62100,62200 64M 4K 32 plan.json efa rdm true
```

That form keeps placement in userspace RAID: branch 0 and branch 1 are separate
userspace mirror legs, not block-device RAID primitives.

`zcraid-mirror-send` supports three ACK policies. The default
`URING_PLAY_RAID_MIRROR_ACK_POLICY=remote` waits for every mirror branch HWM in
each ACK window. An ACK is durable only when each receiver has a persistent
terminal target; without one, startup says `remote-userspace-receipt-only` and
strict topology mode rejects the run. `sync` changes the periodic drain window.
Use `disabled` only as a transport ceiling; it does not measure committed or
sync-safe writes.

A receiver terminal can be a presized-file WAL or an allowlisted terminal block
leaf. Placement has already happened in the userspace mirror stage before this
writer is selected. For example:

```bash
zcraid-mirror-recv tcp 0.0.0.0 42000 0 64M 64K 8 plan.json \
  efa rdm true 'zcpwal:/wal/branch0.journal,/wal/branch0.base,128M,16M'
```

The receiver writes every extent in the window, performs one terminal sync,
and only then returns the extent HWM ACKs. The terminal startup and worker lines
report the target, I/O mode, write count, sync count, and durability semantic.

The OFI sender allocates stable per-branch TX and ACK arenas, posts an entire
window to every userspace branch, progresses branch TX CQs fairly, and polls
the branch-by-window ACK matrix without assuming CQ order. A sequence commits
only when its required branch mask is complete. Receivers prepost the full RX
window and derive the sequence from the WAL header when the wire profile has
one. `URING_PLAY_OFI_BRANCH_POST_NOWAIT=1` and
`URING_PLAY_OFI_PREPOST_ACK=1` are enabled by default; strict mode rejects a
multi-branch/windowed run that disables either fast path. The queue summary
reports branch-post skew, branch-mask completion time, out-of-order mask
completions, configured and peak ACK outstanding depth, and the measured HOL
residence time for masks that completed before the contiguous commit HWM could
retire them.

`rdma` is a separate transport, not an alias for OFI messages. It uses
`FI_RMA_WRITE` from one registered source window shared by every mirror branch
into a registered staging window at each receiver. After all one-sided writes
in a window reach delivery-complete CQ state, the sender transmits a small
doorbell. Each receiver drains the staging window to its persistent terminal,
syncs it, and returns HWM ACKs; source and remote staging slots are not reused
before those ACKs. RDMA mode refuses to start without ACKs, a terminal, or
delivery-complete ordering. For example:

```bash
URING_PLAY_RAID_MIRROR_ACK_WINDOW=64 \
URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 \
URING_PLAY_OFI_RMA_WRITE_MORE=1 \
zcraid-mirror-recv rdma auto 42000 0 64M 64K 8 plan.json efa rdm true \
  'zcpwal:/wal/branch0.journal,/wal/branch0.base,128M,16M'

URING_PLAY_RAID_MIRROR_ACK_WINDOW=64 \
URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 \
URING_PLAY_OFI_RMA_WRITE_MORE=1 \
zcraid-mirror-send rdma 172.31.40.44,172.31.37.202 \
  42000,44000 64M 64K 8 plan.json efa rdm true
```

Use `ofi-msg` (or the shorter `ofi`) for the registered `FI_MSG` path. Benchmark
output labels these as `libfabric-rdm-message` and `libfabric-fi-rma-write`, so
results cannot silently conflate the two.

Zlane coordination is part of the mirror contract. By default
`URING_PLAY_RAID_ZLANE_COORD=lane-owner` maps each `(lane, sequence)` to a
disjoint logical range, so same-sector ordering is preserved by lane ownership
without a hot global lock. To stress overlapping logical ranges, use
`URING_PLAY_RAID_ZLANE_COORD=range-lock`; the sender then uses a shared logical
sequence namespace and holds sorted batch range locks until all branch ACKs for
the batch have arrived. `URING_PLAY_RAID_ZLANE_COORD=none` is diagnostic only
and prints a warning because overlapping zlanes may reorder same-sector writes.

For high-IOPS local TCP mirror runs, `URING_PLAY_RAID_MIRROR_TCP_RECV_SPIN=1`
enables bounded `MSG_DONTWAIT` receive spinning before the receiver falls back
to blocking reads. Tune the bounded spin with
`URING_PLAY_RAID_MIRROR_TCP_RECV_SPIN_BUDGET`; the receiver startup line prints
the active spin setting so context-switch numbers are topology-explicit.
Use `URING_PLAY_SOCKET_BUFFER_BYTES` for the TCP socket buffer request, and
raise `net.core.wmem_max`, `net.core.rmem_max`, `tcp_wmem`, and `tcp_rmem` if
the benchmark warns that the kernel clamped the requested buffers.

Bulk ordered WAL mirror shape for throughput-oriented testing:

```bash
URING_PLAY_PIN_CPUS=1 \
URING_PLAY_PIN_CPU_LIST=0-31,96-127 \
URING_PLAY_RAID_MIRROR_ACK_WINDOW=1 \
URING_PLAY_OFI_CQ_SLEEP_NS=0 \
FI_EFA_USE_DEVICE_RDMA=1 \
zcraid-mirror-send ofi 172.31.38.204 42000,44000 24G 384K 64 plan.json efa rdm false
```

Use `tcp` to run the same userspace mirror contract over lane-aware TCP sockets.
The TCP sender creates the payload once and uses a batched header/payload
`writev` window per branch, avoiding per-branch userspace payload copies and
per-extent syscalls. Every run prints branch domains, lanes, leader CPUs,
workers, logical commit IOPS, branch wire Gbit/s, ACK latency, context switches,
and migrations.

For dual-ENI TCP mirror runs where both private addresses are in the same subnet,
bind the sender source address per branch with
`URING_PLAY_RAID_MIRROR_TCP_SOURCE_IPS=addr0,addr1`. Without that, Linux may
route both branch destinations through one interface and the run is not
topologically aligned.

`zcplan plan --caps client.json,fan.json,leaf0.json,leaf1.json` uses previously
advertised `ZC_CAPS_V1` documents instead of guessing from the local host. The
emitted `descriptor_projection` shows how the plan maps to `ZcRecordDesc`,
`ZcSliceDesc`, tcpmux topology headers, WAL extent fields, and zcnblk topology
headers.

`zcplan validate` is the strict form. It prints the same plan JSON and exits
nonzero when the result is not representative because pinning, hugetlb, memlock,
route/NIC selection, batching, or zero-copy requirements are missing.

### zcflow

Run a descriptor-aware command chain. The current implementation uses ordinary
byte pipes and prints a notice in `auto` mode; it is the reserved place for the
supervised descriptor transport. `zcrun` is kept as a compatibility alias.

Stages are resolved like normal shell commands, so third-party utilities can
join the ecosystem without being compiled into `zcutils`. Descriptor-native
stages will receive the control channel through environment/fd handoff; byte
compatibility stages can continue to read stdin and write stdout.

```bash
zcflow \
  'zccat --generate --bytes 128g --chunk-bytes 1m' \
  'zcmap --preserve-lanes' \
  'zcmux --peer-addr 10.0.1.12 --base-port 9000 --lanes 240'
```

The same chain can be written as a heredoc spec:

```bash
zcflow --spec - <<'EOF'
zccat --generate --bytes 128g --chunk-bytes 1m
zcmap --preserve-lanes
zcmux --peer-addr 10.0.1.12 --base-port 9000 --lanes 240
EOF
```

### zc-tcpmux-send, zc-tcpmux-receive, and zc-tcpmux-xfer

TCP mux transfer primitives. `zc-tcpmux-xfer` uses SSH only as the control
plane to launch `zc-tcpmux-receive`, pass a one-use token, wait for readiness,
and clean up failures. It does not use SCP as the payload transport and payload
bytes are not sent over SSH.

AES-256-GCM is the default and only encrypted transfer mode. Use
`--encryption none` only for explicit plaintext tests. The one-use token seeds
both token authentication and the AES-256 key.

```bash
cat foo | zc-tcpmux-xfer nodeB:/tmp/foo
zc-tcpmux-xfer ./foo nodeB:/tmp/foo \
  --receive-listen-address 10.0.1.12 \
  --receive-listen-port-range 42000-42100
```

Topology-aligned xfer runs can pin both sides:

```bash
zc-tcpmux-xfer ./foo nodeB:/dev/null \
  --lanes 128 \
  --pin-cpus \
  --cpu-list 0-95
```

Use `--send-cpu-list` and `--receive-cpu-list` when the two machines have
different CPU numbering. Each parallel lane sends a versioned topology header
with lane, queue, preferred worker, preferred CPU, NUMA node, flags, and chunk
size. `zc-tcpmux-receive` logs those sender hints beside its local receiver
placement so benchmark logs can prove whether a run stayed on the intended
NUMA node.

The direct send/receive tools are useful for tests and controlled pipelines:

```bash
TOKEN=$(zc-tcpmux-receive --generate-token)
zc-tcpmux-receive \
  --listen-address 0.0.0.0 \
  --listen-port-range 42000-42100 \
  --token "$TOKEN" \
  --output /tmp/out

zc-tcpmux-send \
  --peer-address 10.0.1.12 \
  --port 42000 \
  --token "$TOKEN" < foo
```

`zc-tcpmux-receive` accepts `--listen-address`, `--listen-port-range`,
`--buffer-bytes`, `--token`, `--generate-token`, `--disable-authentication`,
`--encryption aes-256|none`, and `--already-encrypted`. In receive mode,
`--already-encrypted` preserves the AES-256 frame stream instead of decrypting
it. `zc-tcpmux-send --already-encrypted` forwards that framed stream without
encrypting it again. `zcencrypt` and `zcdecrypt` are generic pipeline elements
for AES-256-GCM zc descriptor/frame streams; they are not tied to tcpmux.
`zcdecrypt` accepts topology hints such as `--lane-id`, `--queue-id`,
`--preferred-cpu`, `--numa-node`, and `--ordered global|per-lane`.

`zc-tcpmux-xfer` exposes receive-side knobs as `--receive-listen-address`,
`--receive-listen-port-range`, `--receive-buffer-bytes`, `--receive-token`,
`--receive-disable-authentication`, `--receive-encryption`, and
`--receive-already-encrypted`.

The expansion is conceptually:

```text
local zc-tcpmux-xfer
  -> ssh nodeB 'zcutils zc-tcpmux-receive --token ... --encryption aes-256 ...'
  -> wait for remote READY
  -> local source -> AES-256-GCM framed TCP data socket -> remote output
  -> wait for receive completion
```

A B-node forward-and-local-consume shape keeps ciphertext as the shared branch
format. READY goes to stderr when receive writes payload bytes to stdout, so the
stdout pipeline is not contaminated by control text:

```bash
TOKEN=one-use-token-from-control-plane
zc-tcpmux-receive \
  --listen-address 0.0.0.0 \
  --listen-port-range 42000-42100 \
  --token "$TOKEN" \
  --already-encrypted \
  --output - |
zcforward \
  --to "zc-tcpmux-send --peer-address nodeC --port 43000 --token '$TOKEN' --already-encrypted" \
  --to "zcdecrypt --token '$TOKEN' --topology nodeB-forward --ordered global | zcdemux --ordered global"
```

In descriptor-native mode the tee branches should share encrypted-buffer leases:
the forwarding branch releases after the send completes, and the local branch
releases after decrypt/demux consumes the ordered plaintext view.

### zcrepl

Replication-facing command surface for direct stream tests and CSI/control
orchestration. `zcrepl send/recv` is a one-shot encrypted byte stream using the
same Rust tcpmux/AES functions as the controller. `zcrepl csi-*` can talk to the
OpenAPI REST sidecar with `--control-url` or to the legacy Unix socket with
`--socket`, then asks the local control plane to open its own volume or snapshot
backing path.

```bash
TOKEN="$(zcrepl token)"
zcrepl recv --output /dev/target --listen 0.0.0.0 --port 42000 --token "$TOKEN"
zcrepl send --input /dev/source --peer node-b --port 42000 --token "$TOKEN"

zcrepl csi-recv --socket /var/lib/zcblock-csi-b/control.sock --volume "$TARGET_VOL" --token auto
zcrepl csi-send --socket /var/lib/zcblock-csi-a/control.sock --snapshot "$SNAP" --peer "$IP" --port "$PORT" --token "$TOKEN"
zcrepl csi-status --socket /var/lib/zcblock-csi-b/control.sock --repl-id "$REPL"

zcrepl csi-recv --control-url http://127.0.0.1:9788 --volume "$TARGET_VOL" --token auto
zcrepl csi-send --control-url http://127.0.0.1:9788 --snapshot "$SNAP" --peer "$IP" --port "$PORT" --token "$TOKEN"
```

This is not the final zero-copy topology path. It exposes the operational
surface now, while the descriptor-native implementation remains responsible for
cross-process buffer leases, lane metadata, NUMA/queue alignment, and target
durable ACK accounting.

### zcpit

Create a point-in-time file snapshot. The strict zero-copy mode uses Linux
`FICLONE`, so the source and snapshot must live on a reflink-capable filesystem
such as XFS with reflink enabled or btrfs. `--mode auto` falls back to a full
copy when reflink is unavailable; `--mode reflink` fails instead.

```bash
zcpit snapshot --source /var/lib/zcblock-csi/files/vol.img \
  --snapshot /var/lib/zcblock-csi/snapshots/images/vol-snap.img \
  --mode reflink
```

For a filesystem mounted through a loop-backed file, use the freeze/barrier
path before creating the PIT snapshot when application consistency matters.

### zccat

Source bytes, files, block ranges, or generated payloads as descriptors. Today
it is byte-compatible stdin-to-stdout copying.

```bash
zccat --max-bytes 1g | zcout > /tmp/out
```

### zcout

Materialize descriptors to stdout bytes. This is an explicit copy/materialize
boundary.

```bash
zcflow 'zccat --max-bytes 1g' 'zcout' > /tmp/out
```

### zcmap

Transform descriptor metadata or views without copying payloads when descriptor
transport is available. Today it is byte passthrough and accepts the intended
shape flags so examples remain stable.

```bash
zcflow \
  'zcdemux --bind 0.0.0.0 --base-port 9000 --lanes 240' \
  'zcmap --preserve-lanes --ordered per-lane' \
  'zcmux --peer-addr 10.0.1.13 --base-port 9000 --lanes 240'
```

### zctee and zcmaptee

`zctee` fans out a descriptor stream. `zcmaptee` is the hot-path fused form for
map plus fanout. In descriptor mode each branch gets its own lease reference and
release path. Today they provide byte-compatible fanout.

```bash
zcflow \
  'zcdemux --bind 0.0.0.0 --base-port 9000 --lanes 240' \
  'zcmaptee --preserve-lanes \
     --to "zcmux --peer-addr 10.0.1.13 --base-port 9000 --lanes 240" \
     --to "zcsink --consume checksum"'
```

### zcsnap

`zcsnap` marks a descriptor/WAL snapshot cut without owning block-device or RAID
semantics. In descriptor-native mode it should become a checkpoint frame plus
extent pins and a manifest. Today it is byte-compatible: it passes stdin to
stdout by default, records the selected byte-stream cut, and writes a
`zcsnap-manifest-v1` JSON manifest.

```bash
zcdemux --bind 0.0.0.0 --base-port 9000 --lanes 64 |
zcsnap \
  --id snap-a \
  --at-bytes 16g \
  --ordered per-lane \
  --lane-count 64 \
  --wal-epoch 7 \
  --manifest /tmp/snap-a.json |
zcsink --consume checksum
```

Useful flags:

- `--id ID`: snapshot identifier; generated when omitted.
- `--manifest PATH`: write the manifest to a file; stderr when omitted.
- `--at-bytes N|eof`: record the cut at a byte offset or at EOF.
- `--ordered global|per-lane`: declare the ordering contract for the cut.
- `--lane-id N --lane-count N`: annotate a lane-local cut.
- `--wal-epoch N --base-logical-index N --logical-record-bytes N`: WAL replay
  coordinates for translating bytes into logical records.
- `--require-record-aligned`: reject cuts that are not aligned to the logical
  record size.

This command is intentionally not a volume clone, block freeze, RAID member
operation, `zcbrd` feature, or `zcnblk` mode. It only describes a logical
stream/WAL cut that future descriptor-aware stages can pin, release, and replay.

### zcforward

`zcforward` is the fused B-node primitive for A -> B -> C replication. It can
read one stream from stdin, or accept a single tcpmux stream with
`--from-tcpmux [HOST:]PORT`. It shares each chunk with bounded branch queues,
then writes to command, file, stdout, or direct tcpmux branches in parallel.
That keeps the forwarding branch from serializing behind the local consume
branch and avoids shell pipes on the hot B-node path when tcpmux ingress and
egress are both fused.

```bash
zc-tcpmux-receive --output - --token "$TOKEN" --already-encrypted |
zcforward \
  --queue-depth 8 \
  --to "zc-tcpmux-send --peer-address nodeC --port 43000 --token '$TOKEN' --already-encrypted" \
  --to "zcdecrypt --token '$TOKEN' | zcsink --consume checksum"
```

The lower-overhead A -> B -> C form lets `zcforward` own both the B-node
receive socket and the outbound tcpmux connection:

```bash
zcforward \
  --from-tcpmux 0.0.0.0:42000 \
  --ready-stderr \
  --token "$TOKEN" \
  --already-encrypted \
  --to-tcpmux nodeC:43000 \
  --local-data-address nodeB-private-ip \
  --to "zcdecrypt --token '$TOKEN' | zcsink --consume checksum"
```

Use `--encryption none --disable-authentication` for plaintext test paths.
With AES paths, `--already-encrypted` means the input is forwarded as the
existing AES frame stream; omit it when B should decrypt and re-encrypt.

### zcraid-split, zcraid-merge, and Daemon Aliases

`zcraid-split` frames ordered input chunks with global offsets and stripes or
mirrors those frames across branches. `zcraid-merge` reads branch streams back,
deduplicates mirrored chunks, verifies optional checksums, and writes the
ordered byte stream. `zcraid-fanoutd` is an alias for `zcraid-split`, and
`zcraid-fanind` is an alias for `zcraid-merge`; use those names when the process
is acting as a long-running fanout or fanin daemon around tcpmux receive/send
commands. These commands are userspace RAID stand-ins: block devices may appear
behind a branch command as an edge target, but branch selection and reassembly
stay in userspace.

Both conventional syscall leaves and `io_uring` leaves fit this contract. The
RAID primitive cares about the ordered frame/result-log record; the leaf adapter
decides whether local writes are blocking `write`/`pwrite` calls or `io_uring`
submissions. Mixed devices are expected: do not fork placement, ordering, ack,
sync, freshness, or backpressure logic by I/O API.

For the hot streaming path, prefer direct tcpmux branches:
`zcraid-split --to-tcpmux HOST:PORT --encryption none --zero-copy-send auto`.
This keeps the shared input chunk alive until each branch send completes and can
send payload bytes with io_uring `SEND_ZC`; the zcraid frame header is tiny and
is still written normally. Use `--zero-copy-send required` when benchmark
results must fail instead of falling back to copied TCP sends.

`zcraid-split --descriptor-wal-dir DIR` is only the materialized local
descriptor prototype. It writes payload once to `DIR/payload.wal`, then writes
fixed 128-byte little-endian append descriptors to
`DIR/branch-NNNN.zcraid-desc`. It is useful for validating placement metadata,
but it is not the fast tcpmux fanout path.

```bash
zccat --generate --bytes 8g |
zcraid-split --mode raid10 --replicas 2 --chunk-bytes 1m \
  --to "zc-tcpmux-send --peer-address nodeB --port 44000 --encryption none --disable-authentication" \
  --to "zc-tcpmux-send --peer-address nodeC --port 44000 --encryption none --disable-authentication"

zcraid-merge \
  --from "zc-tcpmux-receive --output - --port 44000 --encryption none --disable-authentication" \
  --from "zc-tcpmux-receive --output - --port 44001 --encryption none --disable-authentication" \
  --output /tmp/reassembled

zccat --generate --bytes 8g --chunk-bytes 1m |
zcraid-split --mode mirror --branches 4 --chunk-bytes 1m \
  --descriptor-wal-dir /dev/shm/zcraid-desc-mirror

zcraid-split --mode mirror --chunk-bytes 1m \
  --to-tcpmux nodeB:44000 \
  --to-tcpmux nodeC:44000 \
  --encryption none --disable-authentication \
  --zero-copy-send required
```

For performance work, use megabyte-class chunks or WAL segments, state the
lane-to-worker mapping, and inspect `zcraid-*-result`/`zcraidd-wal-result`.
`zcraid-split` and `zcraid-merge` expose `--io-buffer-bytes` for file/pipe/WAL
buffering and now print branch CPU/context-switch counters where byte branches
are used so scheduler churn is visible.

For a local multi-machine topology smoke, run
`qemu-zcrx/fan-topology-qemu-kvm.sh`. It starts four KVM guests with dedicated
socket-backed virtio NIC links and runs `client -> tcpmux -> fan
zcraid-split -> tcpmux -> edge zcsink`. It is intentionally a userspace RAID
composition test; terminal edge media can be swapped later without moving
placement into a block device.

### zctier

`zctier` is the userspace hot-tier plus spill endpoint. It writes each input
chunk to the hot path synchronously, then queues the same bytes to an optional
cold spill path or spill command. `--memory-bytes` bounds queued spill data, so
the upstream pipeline gets backpressure when the cold tier falls behind.
File and pipe writes are chunk-buffered, and the result line reports main/spill
CPU time plus context-switch counters. It reports active `hot_admit_seconds`
and `hot_admit_MiBps` at the point where all bytes have reached the hot
materialization, separately from `spill_drain_seconds` and whole-pipeline
`seconds`. `input_wait_seconds` isolates listener/upstream idle time, while
`hot_admit_wall_seconds` retains the wall-clock boundary from process start.
`hot_admit_queued_bytes` records how much bounded cold backlog
remained at that acknowledgement boundary. This distinction is required when
comparing an early hot acknowledgement with a synchronous cold-tier drain.

This composes with `zcraid-split` for RAID1-style fanout without putting the
tier policy into every fanout command. Spill remains userspace work:

```bash
zccat --generate --bytes 8g --chunk-bytes 1m |
zcraid-split --mode mirror --chunk-bytes 1m \
  --to "zctier --hot /dev/shm/mirror-a.hot --spill /mnt/cold/mirror-a --memory-bytes 2g" \
  --to "zctier --hot /dev/shm/mirror-b.hot --spill /mnt/cold/mirror-b --memory-bytes 2g"
```

The same policy is available as a `zcnblk-target` backend for the block path.
That keeps spill admission and cold-tier writes inside the userspace fabric
target instead of the kernel block client:

```bash
URING_PLAY_TCP_WAL_WRITE_MODE=write \
URING_PLAY_ZCNBLK_READ_MODE=write \
zcutils zcnblk-target \
  zctier:/dev/shm/zcnblk.hot:/mnt/cold/zcnblk.spill:64g:4k:2g \
  0.0.0.0 23600 64 1 64g 4k 256 64 4096 small-pages true
```

The block backend uses sparse/random-access files or block-like paths: writes
are `pwrite`d to the hot path, copied into a bounded spill queue, and ACKed
after the hot write plus spill admission. Reads are served from the hot path;
if a restarted target finds the hot path missing and the spill path present, it
uses the spill path as a cold-start read fallback and rehydrates the hot path as
reads arrive. The target logs `zcnblk-target-tier-spill` lines with spill
bytes, chunks, and queued-byte high-water marks.

The block backend is an ingress/edge adapter for block-speaking clients. It
does not move the tier policy into the kernel block layer; the hot/write/spill
decision, queued spill pressure, and rehydration behavior remain userspace
policy.

### zcsink

Terminal consumer. It must release every lease after count, drop, checksum, WAL
write, or other terminal work completes. Today it consumes stdin bytes.

```bash
zcsink --consume checksum
```

### zcstat, zcmeter, and zcgrep

Inspection and filtering commands. Descriptor-native filtering should pass
descriptor slices/views when possible and release filtered-out records.
`zcmeter` is the live meter form: it passes bytes through by default and prints
one stderr line per interval with cumulative received bytes and bytes per
second.

```bash
zcflow 'zccat --max-bytes 1g' 'zcstat --pass-through' 'zcsink --consume count'
zccat --generate --bytes 8g | zcmeter | zcsink --consume count
printf 'abc\n' | zcgrep --pattern b
```

## Network Primitives

### zcprobe

Inspect kernel and userspace capabilities.

```bash
zcprobe
```

### zcdemux

Receive lane-multiplexed TCP traffic. It is a source; it should not grow hidden
forwarding modes. Defaults to automatic receive zero-copy detection.

```bash
zcdemux \
  --bind 0.0.0.0 \
  --base-port 9000 \
  --lanes 240 \
  --connections-per-lane 1 \
  --expected-bytes 512m \
  --workers 40 \
  --zero-copy-receive auto
```

Important flags:

- `--zero-copy-receive auto`: default; try ZCRX, but fall back to io_uring recv.
- `--zero-copy-receive required`: fail if ZCRX cannot be used.
- `--zero-copy-receive off`: force copied io_uring recv.
- `--ifname IFACE`: select NIC for ZCRX.
- `--rxq N`, `--rxq-count N`: select ZCRX queue range.
- `--zcrx-consume checksum`: checksum payload in-place.

### zcmux

Send a descriptor stream over lane-multiplexed TCP. It is a terminal consumer:
input leases are released after send completion.

```bash
zcmux \
  --peer-addr 10.0.1.12 \
  --base-port 9000 \
  --lanes 240 \
  --connections-per-lane 1 \
  --bytes-per-connection 512m \
  --chunk-bytes 1m \
  --pipeline 8 \
  --workers 40 \
  --zero-copy-send auto
```

Important flags:

- `--send-mode send-zc`: default.
- `--send-mode send-zc-fixed`: use send-zc with registered buffers.
- `--send-mode send`: explicit copied fallback.
- `--zero-copy-send auto`: default; try send-zc, but fall back to copied send.
- `--zero-copy-send required`: fail if send-zc cannot be used.
- `--zero-copy-send off`: force copied send.
- `--source-port-base N`: pin generated flow source ports.
- `--source-port-stride N`: stride source ports for 5-tuple shaping.
- `--pin-cpus true`: enable worker CPU pinning.

### zcnc

Small netcat-like frontend. It is useful for smoke tests, but descriptor-native
pipelines should prefer `zcdemux`, `zcmux`, and `zcflow`.

```bash
zcnc listen --bind 0.0.0.0 --port 9000 --connections 1 --expected-bytes 1g
zcnc connect --peer-addr 10.0.1.12 --port 9000 --connections 1 --bytes-per-connection 1g
```

## Relay Examples

A to B:

```bash
# B
zcflow \
  'zcdemux --bind 0.0.0.0 --base-port 9000 --lanes 240 --zero-copy-receive required' \
  'zcsink --consume checksum'

# A
zcflow \
  'zccat --generate --bytes 128g --chunk-bytes 1m' \
  'zcmux --peer-addr B --base-port 9000 --lanes 240 --zero-copy-send required'
```

A to B to C:

```bash
# C
zcflow \
  'zcdemux --bind 0.0.0.0 --base-port 9000 --lanes 240 --zero-copy-receive required' \
  'zcsink --consume count'

# B
zcflow \
  'zcdemux --bind 0.0.0.0 --base-port 9000 --lanes 240 --zero-copy-receive required' \
  'zcmap --preserve-lanes --ordered per-lane' \
  'zcmux --peer-addr C --base-port 9000 --lanes 240 --zero-copy-send required'
```

Fanout at B:

```bash
zcflow \
  'zcdemux --bind 0.0.0.0 --base-port 9000 --lanes 240' \
  'zcmaptee --preserve-lanes \
     --to "zcmux --peer-addr C --base-port 9000 --lanes 240" \
     --to "zcstat" \
     --to "zcsink --consume checksum"'
```

## Byte Compatibility

These commands currently work with normal stdin/stdout bytes for local smoke
tests:

```bash
printf 'abc\n' | zccat | zcmap | zcout
printf 'abc\n' | zcmaptee --to 'zcsink --consume count' --stdout false
printf 'abc\n' | zctee --output /tmp/out --stdout false
printf 'abc\n' | zcsnap --id smoke --manifest /tmp/smoke-snap.json | zcsink --consume count
printf 'abc\n' | zcgrep --pattern b
printf 'abc\n' | zcstat
```

### zcnblk-target and zcnblk-send

`zcnblk-target` receives mux-aligned block read/write frames for userspace
targets such as `zcdevnullN` and the userspace tier backend
`zctier:HOT[:SPILL[:BYTES[:ALIGN[:MEMORY]]]]`. It may also write directly to
optional last-hop backing media such as `/dev/zcbrdN`, Linux test block
devices, or allowlisted real devices by `PARTUUID=...`. It deliberately rejects
custom zc stripe block backends so forwarding, RAID, fanout/fanin, tier spill,
and backpressure stay in userspace instead of becoming target-side custom block
topology.

`zcnblk-target zcwal:ADDR[:WAL_BASE[:EXTENT[:ACK[:BYTES[:ALIGN]]]]] ...`
turns the existing target into a zcnblk-to-WAL userspace onramp. It accepts
normal zcnblk write frames, keeps the incoming lane/port topology, coalesces
4 KiB records into fixed WAL extents, sends those extents to a matching
`zcwal-extent-recv ... extent blocking` receiver, and can return zcnblk write
ACKs after WAL ACKs when `URING_PLAY_ZCNBLK_WRITE_ACKS=1`. This is still not a
block stripe or mirror path; the block device is only the edge protocol into a
userspace WAL socket pipeline. Plaintext zcwal mode uses socket-to-socket
`splice(2)` payload forwarding by default to avoid copying payloads through the
middle process; set `URING_PLAY_ZCNBLK_WAL_SPLICE=0` to force the copy path.
When the client sends `ZCNBLK_OP_BATCH` frames, the target validates each inner
4 KiB request but moves the contiguous batch payload into the WAL as one
splice/copy group, so device-edge write coalescing happens before the
userspace WAL write without doing placement in the kernel.
The onramp is write-only for now.

`zcnblk-send` is the user-space generator for write, read, and mixed 4K block
traffic. Set `URING_PLAY_ZCNBLK_WAIT_WRITE_ACKS=1` when testing a target that
returns write ACKs. Set `URING_PLAY_ZCNBLK_BATCH_DEPTH=N` on write-only
generator runs to emit kernel-client-style `ZCNBLK_OP_BATCH` frames and test
the same coalesced WAL path without loading `/dev/zcnblk0`.

`zcnblk` request frames are now v2 64-byte headers. In addition to op, flags,
shard, length, and offset, each request carries `lane_id`, `lane_count`,
`preferred_worker`, `queue_id`, `request_id`, `tier_id`, and topology flags.
The userspace target validates that a topology-marked frame arrived on the lane
it claims and that `tier_id` matches the userspace-selected target shard. This
is the framing hook for end-to-end lane preservation across the kernel block
edge, userspace sender, target tier backend, and userspace RAID/fanout policy.

For the concrete point-to-point single-target unencrypted `/dev/zcnblk0` fio
setup, including module config and recorded read/write benchmark numbers, see
[`zcnblk-single-target-howto.md`](zcnblk-single-target-howto.md).
For the broader block-vs-userspace comparison matrix and short recipes, see
[`block-vs-userspace-bench-plan.md`](block-vs-userspace-bench-plan.md).

High-IOPS results are only meaningful when the run is topology-explicit:
reserve hugetlb pages, raise memlock, pin target workers, pin the zcnblk client
kthreads, keep `hctx_affinity=1`, and state the lane-to-CPU mapping. The
zcnblk tools intentionally print `PERF WARNING` lines when these assumptions
are missing; treat those warnings as benchmark blockers unless the run is only a
functional smoke test. Set `URING_PLAY_TOPOLOGY_STRICT=1` or
`URING_PLAY_TOPOLOGY_FATAL=1` for representative runs; the zcnblk
sender/fan/target/leaf paths, WAL extent, OFI, zcraid mirror, and fanout
zipper/TCP/shared-memory benches then fail before printing benchmark summaries
when worker pinning, explicit CPU maps, lane/worker sizing, route proof, or
required fast-path knobs are wrong.

Use `scripts/zcnblk-shm-block-bench.sh` for the repeatable local
`/dev/zcnblk0` to userspace-memory control. It reserves the block edge with
`agent-coord`, derives one client, target, and kernel physical core per hctx,
pins every role, records whether its soft-exclusive CPU/memory request was
honored, runs repeated `zcblockbench` samples with context-switch counters, and
uses exact PID files for cleanup. The default is explicitly a noisy local
control because it uses small pages and leaves the governor unchanged:

```bash
LANES=4 REPEATS=3 scripts/zcnblk-shm-block-bench.sh
```

Set `REPRESENTATIVE=1 BUFFER_MODE=hugetlb` only after provisioning huge pages
and memlock headroom. Representative mode refuses an unhonored coordination
request or missing topology prerequisites. The hugetlb preflight accounts for
one huge page per registered io_uring slot, so the minimum free-page count is
`LANES * IODEPTH`. Representative mode uses a fixed 1 ms active completion
poll; ordinary control mode retains the short idle policy and enables longer
polling only after sustained traffic. Target summaries distinguish
block-request descriptor IOPS from 4K-equivalent logical IOPS because Linux
may merge adjacent requests before the userspace handoff.

The shared block ABI is version 2. Descriptor/completion rings use
`shm_ring_entries`, while payload ownership uses the independently sized
`shm_payload_entries` pool. A userspace stage may ACK a write and recycle its
descriptor while retaining the payload slot until reducer/leaf commit advances
`payload_lease_hwm`; the kernel gates reuse on both completion consumption and
that HWM. Keep the descriptor ring hot and small, and size the payload pool for
the longest permitted unsynced/writeback window.

`zcnblk-shm-target ... wal-memory` is the first concrete local writeback stage
on that ABI. It admits ordinary writes by shared-slot reference, serves dirty
read-after-write from the latest slot, materializes ordered batches into a
userspace RAM reduction arena at pressure or sync. It is volatile benchmark
media and deliberately rejects flush after reducing prior writes; a successful
block sync requires the `wal-tcp` path to remote leaves. Persistent leaves call
`sync_data(2)`. Remote retained-memory benchmarks must explicitly set
`URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1`; this proves the remote HWM
and readback contract but not power-loss durability. The module
advertises a volatile write cache and does not advertise FUA, so `fsync(2)`
reaches the `REQ_OP_FLUSH` to sync-HWM path instead of being silently absorbed.
This backend is currently restricted to one lane; it fails startup for multiple
lanes until cross-lane submit order and the global sync HWM have a dedicated
overlap test. It is a single-leaf baseline and makes no RAID placement decision.

Run the deterministic dirty-read/overwrite/fsync smoke before performance:

```bash
scripts/zcnblk-shm-walmem-smoke.sh
```

Then use the ordinary benchmark harness with a larger payload pool:

```bash
LANES=1 BACKEND=wal-memory SHM_RING_ENTRIES=128 \
SHM_PAYLOAD_ENTRIES=4096 URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH=2048 \
REPEATS=3 OPS_PER_WORKER=2000000 scripts/zcnblk-shm-block-bench.sh
```

The smoke reserves `/dev/zcnblk0`, tracks the daemon through its PID file, and
uses `dd count=0 conv=fsync` to issue `fsync(2)` on the block fd and requires it
to fail for volatile `wal-memory`. Do not replace that check with
`blockdev --flushbufs`: in a direct-I/O-only test the latter can sync/invalidate
the block page cache without submitting a device flush.

Use `BACKEND=wal-tcp` to keep `/dev/zcnblk0` as the client edge while sending
the userspace WAL stage to a `zcnblk-wal-leaf`. The harness starts a local
userspace leaf by default, assigns client/target/kernel/leaf to four distinct
physical cores, records every leaf thread in context-switch accounting, and
uses a 4,096-slot payload pool by default. A WAL run with payload entries less
than or equal to descriptor entries collapses the safe writeback window to one;
the harness warns loudly and representative mode rejects that geometry.

Local and non-representative controls default to the unified lane-owned request
path. A representative write-only `wal-tcp` run against an external leaf
defaults to the separate stable-owner userspace stage; forcing that write run
back to lane-inline transport is rejected as non-representative. Representative
read and mixed workloads retain lane-inline ingress because measured
stable-owner dispatch reduced both to about 1.00M IOPS. Both paths size WAL extents to
`SHM_RING_ENTRIES`, wait up to 20 us to fill deferred write extents, use
adaptive leaf receive spinning, and ask `zcblockbench` to reap at least 16
completions when `IODEPTH >= 128`. Local paired controls showed that changing
only the completion minimum from 1 to 16 raised four-lane random-write
throughput from 2.07M to 2.72M IOPS and reduced kernel context switches from
roughly 1.75-2.27 to 0.08-0.11 per 1K I/O. Disabling lane batching or using a
deep-queue completion minimum below 8 prints a `PERF WARNING`; representative
runs reject either configuration. For stable-owner controls whose aggregate
outstanding depth exceeds 128, the default owner queue depth is that aggregate
(`LANES * IODEPTH`); an explicit
`URING_PLAY_ZCNBLK_SHM_OWNER_QUEUE_DEPTH` still takes precedence.

Keep `SHM_RING_ENTRIES` and `SHM_PAYLOAD_ENTRIES` powers of two for block
performance runs. The kernel edge then indexes descriptor, completion,
inflight, and payload rings with masks; other positive geometries remain
compatible but require division. The harness prints a performance warning for
those geometries and rejects them under representative, topology-strict, or
topology-fatal execution. `MIN_IOPS_PER_REP` and `MIN_MEAN_IOPS` are optional
integer hard gates; use both when a hardware result must prove a floor rather
than merely print measurements. An OFI RMA-read run with
`MIN_MEAN_IOPS>=12000000` activates the physical-block 12M record gate. It
also requires `MIN_IOPS_PER_REP>=12000000`, representative mode, EFA-direct
device RDMA, selective completion with a stride at least as large as per-lane
RMA QD, deferred real-tail markers, and FI_MORE read doorbell staging. After
the repetitions, the gate checks every selective data-endpoint profile for
`provider=efa`, `fabric=efa-direct`, `efa_direct=1`,
`efa_emulated_read=0`, and `rma_read_more=1`; a requested environment setting
without matching provider evidence cannot produce a record artifact.

The benchmark harness enables the explicit volatile-sync option for its default
`zcmem` leaf. Ordinary writes complete after userspace dirty-lease admission;
they do not wait for a leaf result. Flush/FUA completion remains remote: it is
withheld until every frozen lane HWM has reached the leaf and every leaf sync
result has returned. `zcdevnull` cannot be used for this contract.

Set `ORDER_SMOKE_PAIRS=N` to run the linked same-sector ordering and global
sync-drain gate before throughput. Set `CONTRACT_SMOKE_BLOCK=N` to run a native
`RWF_DSYNC` FUA plus I/O-priority/write-lifetime/readback gate. The harness
records each gate's elapsed time and fails unless the final target summary
contains a nonzero matching `syncs` or `fua_requests` counter. These drain
timings are reported separately from early-local-write throughput.

The current block-to-WAL dirty directory has an explicit 4K record contract.
Do not lower the client logical block size or bypass the target's 4K checks for
a 1K experiment: four 1K sectors would otherwise alias one dirty-directory key
and violate read-after-write semantics. The userspace reduce microbenchmark can
vary `--record-bytes` safely. Its random 50/50 reference controls measured
about 378M logical IOPS at 1K and 376M at 4K, showing that descriptor/dirty-map
work is per record rather than payload-bandwidth limited.

`URING_PLAY_ZCNBLK_SHM_READ_BATCH=N` bounds each FIFO mixed request window.
Writes in the window are admitted by shared-slot reference, dirty reads are
resolved at their exact position in the window, and cold reads are combined
into one WAL `REQUEST_BATCH`. Result descriptors are checked before payloads
are received directly into their leased shared output slots. The target
publishes block completions in FIFO order. Leaf application order is per-sector
with a global sync HWM, not a claim that unrelated unsynced sectors are applied
in one global order. The target must flush on either write-count pressure or
payload-arena pressure; omitting the latter can deadlock a 50/50 workload when
the payload pool fills just before the write threshold.

For a `zcmem` terminal leaf, request batches mark their descriptor-first write
layout explicitly. The leaf reads only the descriptor table into heap memory,
builds destination iovecs, and receives write bodies directly into final arena
offsets. This applies to mixed batches as well as write-only batches; reads are
then emitted in descriptor order. The sender's sector-predecessor boundary
prevents an overlapping write and dependent read from sharing a batch. Verify
the fast path with nonzero `direct_memory_recv_bytes` and zero
`heap_payload_data_bytes` and `copy_submit_bytes` in the leaf summary.
Atomic-write batches deliberately omit the direct-receive marker and retain the
leaf submit path required by their all-or-nothing completion contract.

The high-IOPS `wal-tcp` harness defaults
`URING_PLAY_ZCNBLK_SHM_WAL_COMPACT_WRITES=1`, encoding an all-write batch as
32-byte `WRITE_EXTENT_BATCH` entries instead of 128-byte request descriptors.
Set it to zero for an A/B control. Mixed batches retain the full request format. The leaf receives
the compact descriptor table into heap memory, then uses `MSG_WAITALL` only for
that batch's scattered payload receive into final `zcmem` offsets; applying
`MSG_WAITALL` to full request batches was slower. Alternating noisy-local QD128
four-lane controls on 2026-07-12 averaged 2.696M write IOPS with compact
descriptors versus 2.612M without them, while descriptor traffic fell 75%
(1.536 GB to 384 MB per 12M writes). Random 50/50 controls were 3.184M versus
3.135M IOPS. Treat these as directional shared-host results, not a hardware
ceiling. Confirm `compact_write_batches`, `descriptor_bytes_saved`, and zero
`heap_payload_data_bytes`/`copy_submit_bytes` in each run summary.

The 20 us extent-fill default is operation-aware rather than a foreground I/O
delay: a read, sector dependency boundary, sync, shutdown, or full extent
forces an immediate send, while locally acknowledged writes can accumulate in
the remote writeback stream. In noisy-local four-lane controls, compact QD128
write throughput peaked near this knee at 2.812M IOPS; 40-160 us formed larger
batches but fell to 2.38-2.59M because the transport pipeline starved. At QD1,
random 50/50 throughput improved from 358K IOPS with 2 us to 420K with 20 us;
sampled average latency improved from about 11.0 to 9.6 us and read p50 stayed
at 16 us. Override `URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_FILL_US` for a measured
topology rather than assuming that a larger batch-fill interval is faster.

`URING_PLAY_ZCNBLK_SHM_SECTOR_ORDER_SLOTS` controls the power-of-two hashed
predecessor table used only for same-sector ordering metadata; it does not make
placement decisions. The harness records it and defaults to 65,536 outside the
stable-owner path. Stable-owner ingress instead rounds twice the active
per-worker page count up to a power of two (1,048,576 slots for the measured
32-lane topology). A 2026-07-12 four-lane write sweep measured 1.63M IOPS with
16,384 slots versus about 2.67M with either 65,536 or 262,144, showing that a
cache-small table can be slower when false dependencies serialize independent
lanes. Representative mode rejects an explicitly undersized geometry.

The global completion tracker now backpressures a lane before its sequence can
alias a still-live ring entry. This matters when reduced false dependencies let
one lane run far ahead: a one-million-slot stress control previously failed at
exactly a 32,768-sequence alias. The bounded tracker now leaves the next
descriptor in the kernel ring and drains transport completions until the global
HWM advances. Summaries expose `completion_window_stalls`; a collision remains
fatal because it indicates a broken admission invariant.

The request-fill delay is opt-in. Set
`URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_US=5` for a bulk/read-heavy experiment.
`URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_MIN=32` prevents that delay unless the
queue is already busy; this preserved the QD1 control while allowing fuller
QD128 batches. Run `scripts/zcnblk-shm-tcp-leaf-smoke.sh` first. It now includes
`zcnblk-order-smoke`, which verifies concurrent `read -> write` and
`write -> read` same-sector windows plus sync terminal state.

Noisy local one-lane, small-page, QD128 controls on 2026-07-11 measured
1.211-1.293M random 50/50 4K IOPS (1.258M mean), 1.000-1.058M cold-read IOPS
without fill, and 1.057-1.185M with a 5 us busy-window fill. QD1 cold reads were
77.5K IOPS both with no fill and with the minimum-32 adaptive guard; applying an
unconditional 5 us delay reduced QD1 to 54.9K. These are shared-host controls,
not hardware ceilings. Relevant artifacts are
`local-zcnblk-shm-tcp-mixed50-requestwindow-fixed-20260711T1100Z`,
`local-zcnblk-shm-tcp-coldread-long-fill0-paired-20260711T1110Z`, and
`local-zcnblk-shm-tcp-coldread-long-fill5-valid-20260711T1110Z`.

`scripts/zcnblk-fanwal-plan-bench.sh` also writes
`topology-preflight.log` and appends `topology_preflight_representative=0|1`
to `topology.env`. The preflight checks mapped CPU lists, exact CPU overlap,
SMT sibling placement, CPU governor, currently busy mapped CPUs, memlock, and
hugepage state before launching traffic. Set `TOPOLOGY_PREFLIGHT_FATAL=1` when
an automation job must fail instead of producing a known-noisy smoke result.
The helper defaults `SEND_WRITE_WINDOW` and `SEND_READ_WINDOW` to at least
`URING_PLAY_ZCNBLK_BATCH_DEPTH * URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW`, records
the values in `topology.env`, and passes them to the fan as diagnostic hints.
If the fan still reports `max_outstanding_batches` below the batch window while
the sender window hint is large enough, investigate fan-to-leaf egress, leaf
writeback, socket backpressure, copied 4K payload fallback, and shared-host
CPU/memory contention before treating the IOPS as architectural.
The fan summary also warns when the average upstream batch fill is much smaller
than `URING_PLAY_ZCNBLK_BATCH_DEPTH`. The fan reports both physical WAL frames
and `upstream_batch_logical_records`; extent runs should be judged by logical
records because a full `write-read-same` request can be two physical frames
covering hundreds of 4 KiB records. Read a low logical fill as a 4K IOPS blocker
until the sender window, mixed read/write ordering, and batch fallback counters
explain why the fan is not seeing large request batches.
`URING_PLAY_ZCNBLK_FAN_ORDER_MODE=lane-local` is also blocked in strict mode
unless a topology plan proves each lane owns a disjoint logical range; otherwise
use the default `global-record` ordering for random or overlapping traffic.

For multi-NIC tests, set `URING_PLAY_EXPECT_ROUTE_DEV=IFACE` and
`URING_PLAY_EXPECT_ROUTE_SRC=IP` on the client, fan, and edge processes. The
TCP mux/zcnblk/WAL tools source-probe established sockets with
`ip route get <peer> from <socket-local-ip>`. When either expected-route
variable is set, a mismatch or failed proof is a fatal topology error; fix host
routes, source binding, file-descriptor limits, or data-NIC selection before
using the result. Set `URING_PLAY_ROUTE_PROBE=1` without expected-route
variables to log the chosen route in warning-only exploratory runs. Set
`URING_PLAY_TOPOLOGY_STRICT=1` to make route-proof failures fatal even when no
expected interface/source is configured. `URING_PLAY_TOPOLOGY_FATAL=1` is an
alias for tools where the intent is clearer than "strict."

`zcwal-extent-send` and `zcwal-extent-recv` are isolated tcpmux-compatible
WAL extent smoke tools. They preserve lane/shard identity in a fixed extent
header, move megabyte-class extents by default, account logical IOPS as 4 KiB
records inside those extents, and can ack by extent range. The default framing
is `stream` with an io_uring bulk payload path: one lane header, bulk payload,
and one ack per lane. Pass `extent blocking` at the end to force the older
per-extent header/ack blocking path. The uring path defaults to a 32-deep send
pipeline and receive buffers up to 4 MiB; override with
`URING_PLAY_ZCWAL_URING_PIPELINE`, `URING_PLAY_ZCWAL_URING_ENTRIES`,
`URING_PLAY_ZCWAL_FRAME_BYTES`, `URING_PLAY_ZCWAL_SEND_BYTES`, and
`URING_PLAY_ZCWAL_RECV_BYTES` when tuning. `FRAME_BYTES` sets both send and
receive transport chunks; the side-specific variables override it. Logical WAL
extent accounting remains controlled by the positional `extent-bytes` argument,
so a run can keep 1 MiB WAL extents while using a 4 MiB receive chunk. On the
c8gn.48xlarge two-NIC TCP WAL transport path, `send=1M, recv=4M` was the best
tested frame split so far and reached about 529 Gbit/s aggregate receive-side
payload throughput across both NICs with explicit source binding and CPU/NIC
pinning. The receive summary prints CQE size/count distribution, io_uring wait
syscalls, CQ spin loops, and context switches; use `URING_PLAY_CQE_SPIN=N` for
a fixed spin window or opt into adaptive busy-period spinning with
`URING_PLAY_CQE_ADAPTIVE_SPIN=1`, `URING_PLAY_CQE_ADAPTIVE_SPIN_MIN=N`, and
`URING_PLAY_CQE_ADAPTIVE_SPIN_MAX=N`; the adaptive path grows when an
io_uring wait returns within `URING_PLAY_CQE_ADAPTIVE_WAIT_NS` nanoseconds
(default 50000) and shrinks on slower waits. `URING_PLAY_CQE_HOT_POLL=1` is
only for dedicated ultra-low-latency lanes where burning a core is acceptable.
`URING_PLAY_ZCWAL_URING_RECV_PIPELINE=N` preposts multiple receive buffers per
lane to reduce rearm churn after short TCP CQEs; state this value with the
lane/CPU mapping because it changes memory footprint and completion behavior.
It is diagnostic, not a recommended default: current c8gn TCP tests showed
depths above 1 collapsing throughput even though the run remained correct.
For receive-heavy TCP tests on kernels with io_uring NAPI registration, try
`URING_PLAY_REGISTER_NAPI=1`, `URING_PLAY_NAPI_BUSY_POLL_US=N`, and
`URING_PLAY_NAPI_PREFER_BUSY_POLL=1`; the log prints requested and effective
NAPI values because kernels may normalize the registration fields.
They intentionally do not implement
RAID, tiering, spill, or any block-device striping/mirroring path; those remain
userspace topology decisions outside this primitive.

`zcwal-ofi-relay` is the libfabric head/fan point for userspace RAID1 WAL
traffic. It receives one upstream extent stream per lane, forwards the same
slot to every configured tail, waits for every tail ACK for that logical range,
and then sends one upstream HWM ACK. `tail-addr` and `out-base-service` accept
CSV lists; a single value is expanded across all tails. Example local shape:

```bash
zcwal-ofi-recv sockets rdm 127.0.0.1 32000 2 8M 64K 2 true
zcwal-ofi-recv sockets rdm 127.0.0.1 34000 2 8M 64K 2 true
zcwal-ofi-relay sockets rdm 127.0.0.1 127.0.0.1 30000 32000,34000 2 8M 64K 2 true
zcwal-ofi-send sockets rdm 127.0.0.1 30000 2 8M 64K 2 true
```

The default TCP control offset is 1000, so keep each data-service range and its
control range disjoint as in this example.

The relay summary prints `tail_count`, logical payload throughput, branch wire
throughput, tail-gated ACK latency, context switches, and migrations. It is a
userspace RAID primitive; it must not be replaced by a block-device mirror.
Use `URING_PLAY_OFI_ACK_WINDOW=N` to let senders, relays, and terminal OFI WAL
receivers exchange one HWM/range ACK per contiguous batch instead of one ACK per
extent; the startup banners print `ack_window`, `range_ack_send`, and
`range_acks`. The relay preposts `URING_PLAY_OFI_RELAY_WINDOW` stable upstream
RX slots, derives headered message sequence independently of physical receive
slot, posts each window across every tail, fairly progresses tail TX CQs, and
joins nonblocking tail ACK masks before emitting the upstream HWM.
`URING_PLAY_OFI_RELAY_PREPOST_RECV=1` and
`URING_PLAY_OFI_RELAY_BRANCH_POST_NOWAIT=1` are enabled by default; disabling
either makes a strict representative run fail. The worker contract reports
branch-post skew, tail-mask/HOL latency, and configured and peak ACK depth. On
the local `sockets;rdm`
provider, direct one-hop numbers and
mirrored relay numbers must be interpreted by total data touches: a two-tail
mirror does one upstream receive plus two downstream sends, so equal aggregate
copy bandwidth appears as roughly one third of the direct one-hop logical IOPS.

AES-256-GCM is optional and off by default for zcnblk so existing plaintext
benchmarks remain comparable. Enable it on both sides with:

```bash
export URING_PLAY_ZCNBLK_ENCRYPTION=aes-256
export URING_PLAY_ZCNBLK_TOKEN="$(zc-tcpmux-receive --generate-token)"
export URING_PLAY_ZCNBLK_AES_FRAME_BYTES=65536
```

The kernel client module uses the same stream framing when loaded with
`aes256_gcm_token=...`; keep `aes256_gcm_frame_bytes` equal to the target's
`URING_PLAY_ZCNBLK_AES_FRAME_BYTES` for direct encrypted-vs-plaintext runs.
`zcnblk-target` logs `encryption=` and `aes_frame_bytes=` in its plan line, and
the summaries to compare are `zcnblk-target-summary`, `zcnblk-send-summary`,
and the final `zcnblk-target:` / `zcnblk-send:` throughput lines.

## Enterprise Mixed Workload

`zcworkload` generates a deterministic, destructive mixed block workload with
4-64 KiB logical operations, synchronous and io_uring engines, open-loop or
saturation pacing, latency distributions, completion-contract accounting,
context-switch counts, strict topology preflight, and repeated-run spread.
See [enterprise-workload-benchmark.md](enterprise-workload-benchmark.md) for
the workload shape, safety rules, commands, and result interpretation.

## Application And NFS Filer Benchmarks

`scripts/zcnblk-fs-app-bench.sh` creates a topology-explicit two-lane zcnblk
WAL edge and runs the etcd, Cassandra, Kafka, or NFSv4.2 harness against its
ext4 filesystem. The application harnesses record their distinct durability
contracts, latency, throughput, and context switches; the wrapper records the
target, kernel-lane, leaf-lane, application CPU, and hctx mappings. See
[zcnblk-application-benchmarks.md](zcnblk-application-benchmarks.md) for setup,
commands, completion semantics, and current local proof results.

## Dynamic Topology Evolution QEMU Harness

`scripts/zctopology-evolution-qemu.sh` runs the controller and five TCP
userspace tier/replica stages in six tiny QEMU guests. It injects physical TAP
failures and validates isolated supervisor-quorum loss, data-layer loss,
overlapping supervisor/data loss, snapshot-plus-WAL PITR, cold-DR growth, hot
promotion, replica replacement, cost-driven collapse, HWM-gated release, and
exact topology plus PITR committed-log replay.
`zctopology-emu` is the guest test driver; it is not a production daemon.

```bash
scripts/zctopology-evolution-qemu.sh
```

The harness is correctness-only and prints no representative storage
performance number. See [dynamic-topology.md](dynamic-topology.md).

## Compatibility

Existing benchmark subcommands remain available, including
`tcp-bench-uring-mux-send` and `tcp-bench-uring-mux-server`. The receive side
now defaults to `auto` ZCRX. Use `recv` explicitly to force the old copied path.

`tcp-bench-uring-mux-send ... send-zc-vectorized` exercises Linux 7.0
`IORING_SEND_VECTORIZED` without involving the block edge. Set
`URING_PLAY_TCP_SEND_VECTOR_IOVECS` to `1..=1024` (default `16`) to choose the
number of payload segments represented by each SQE. The existing host safety
gate still refuses every send-zc mode outside QEMU unless
`URING_PLAY_ALLOW_UNSAFE_SEND_ZC=1`; use an isolated adhoc host for performance
validation. `zcmux --zero-copy-send required` and
`URING_PLAY_TCP_SEND_ZC_REQUIRE_ZERO_COPY=1` make copied notifications or
completed bytes without notification CQEs fatal rather than merely reporting
them.
