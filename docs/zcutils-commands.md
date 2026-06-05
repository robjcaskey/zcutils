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
are last-hop backing media, not the topology. Custom stripe block targets are
not SAN target backends.

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
payload is forwarded. Writes wait for leaf `ZCNBLK_OP_WRITE_ACK` before that
sector order is released, so leaf targets must run with
`URING_PLAY_ZCNBLK_WRITE_ACKS=1`.

The WAL engine is selected with `--engine wal`. It speaks fixed 128-byte fan WAL
descriptor/result frames to `zcnblk-wal-leaf`, supports `--mode stripe|mirror`,
and keeps block devices only as terminal leaf media. Blocking and io_uring
callers share the same ordered descriptor/result contract; the selected leaf
submit adapter must not change placement, ack, sync, or freshness semantics.

```bash
zcnblk-wal-leaf zcdevnull0 127.0.0.1 24600 1 1 4K 1 false mixed
zcnblk-wal-leaf zcdevnull1 127.0.0.2 24600 1 1 4K 1 false mixed

URING_PLAY_ZCNBLK_WRITE_ACKS=1 \
URING_PLAY_ZCNBLK_BATCH_DEPTH=64 \
URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW=16 \
zcnblk-fan --engine wal --leaves 127.0.0.1,127.0.0.2 --bind 127.0.0.1 \
  --base-port 23600 --ports 1 --connections-per-port 1 \
  --bytes-per-connection 4M --chunk-bytes 4K --stripe-bytes 4K \
  --leaf-base-port 24600 --pin-handlers false --mode stripe
```

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
each leaf as coalesced `WRITE_BATCH` WAL chunks with a descriptor array followed
by one payload area. Leaves return coalesced `RESULT_BATCH` descriptor arrays,
and the fan interleaves those leaf result logs in the lane-local handler before
answering the client with a `BATCH_RESP` containing the write ACK headers. Treat
unbatched 4K WAL fan numbers as a topology smoke only.
`URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW` controls how many disjoint leaf write
batches the fan can keep outstanding before draining result batches; `1`
intentionally exposes the serialized wakeup-per-batch path. For terminal
io_uring leaves, `URING_PLAY_ZCNBLK_WAL_LEAF_RING_ENTRIES` and
`URING_PLAY_ZCNBLK_WAL_LEAF_CQ_ENTRIES` override the leaf ring size and are
printed by `zcnblk-wal-leaf`.

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

For high-IOPS runs, size `URING_PLAY_ZCNBLK_BATCH_DEPTH` so each leaf gets
multi-MiB WAL chunks rather than 4K descriptor chatter, and record the
lane-to-worker and lane-to-CPU mapping with the result.

`zcfanout-logzip-bench` isolates that zipper. It consumes materialized monotonic
branch result logs in memory and reports descriptor-equivalent IOPS without
claiming TCP or block-device throughput:

```bash
URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-31 \
  zcfanout-logzip-bench mirror-write 32 2 1000000 4K 8192 32 64 true
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
CPU time plus context-switch counters.

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
functional smoke test.

For multi-NIC tests, set `URING_PLAY_EXPECT_ROUTE_DEV=IFACE` and
`URING_PLAY_EXPECT_ROUTE_SRC=IP` on the client, fan, and edge processes. The
TCP mux/zcnblk/WAL tools source-probe established sockets with
`ip route get <peer> from <socket-local-ip>` and warn if Linux would route a peer
through a different interface or source address. Set `URING_PLAY_ROUTE_PROBE=1`
to log the chosen route without enforcing an expected device/source.

`zcwal-extent-send` and `zcwal-extent-recv` are isolated tcpmux-compatible
WAL extent smoke tools. They preserve lane/shard identity in a fixed extent
header, move megabyte-class extents by default, account logical IOPS as 4 KiB
records inside those extents, and can ack by extent range. The default framing
is `stream` with an io_uring bulk payload path: one lane header, bulk payload,
and one ack per lane. Pass `extent blocking` at the end to force the older
per-extent header/ack blocking path. The uring path defaults to a 32-deep send
pipeline and receive buffers up to 4 MiB; override with
`URING_PLAY_ZCWAL_URING_PIPELINE`, `URING_PLAY_ZCWAL_URING_ENTRIES`, and
`URING_PLAY_ZCWAL_RECV_BYTES` when tuning. They intentionally do not implement
RAID, tiering, spill, or any block-device striping/mirroring path; those remain
userspace topology decisions outside this primitive.

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

## Compatibility

Existing benchmark subcommands remain available, including
`tcp-bench-uring-mux-send` and `tcp-bench-uring-mux-server`. The receive side
now defaults to `auto` ZCRX. Use `recv` explicitly to force the old copied path.
