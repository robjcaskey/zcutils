# Streaming Replication and Shaping

This is part of zccusan: the Zero Copy Cinematic Universe Storage Area Network.
The chain is descriptor primitives -> zero-copy streams -> zero-copy WALs,
including WALs of WALs -> `zcvolume` -> `zcsan` -> convenience `zccsi`. CSI is
one adapter into zccusan. Replication streams, topology, token buckets, snapshot
transfer, WAL catch-up, and future tenant policy belong to the zccusan
control/data plane rather than to the Kubernetes CSI process.

This repo already has the transport pieces for encrypted byte-stream
replication:

- `zc-tcpmux-send` and `zc-tcpmux-receive`: direct token-authenticated TCP mux
  transfer, encrypted with AES-256-GCM by default.
- `zc-tcpmux-xfer`: SSH-coordinated remote receive plus encrypted TCP data
  plane.
- `zcencrypt` and `zcdecrypt`: generic AES-256-GCM stream filters.
- `zcforward`: fused receive/fanout/forward primitive. It can forward an
  already-encrypted stream without decrypting at the intermediate node.
- `zctee` and `zcmaptee`: byte-compatible tee/fanout placeholders for the
  descriptor-native fanout model.
- `zcsnap`: byte-compatible snapshot/WAL cut manifest emitter.
- `tcp-wal-mux-server` and the WAL extent docs: current approximation of
  coalesced WAL extents and topological lane placement.

The missing production component is not encryption or TCP transport. It is a
replication gateway that owns admission, scheduling, accounting, and durability
feedback across many tenants and many destination disks.

## Controller Integration

`zcblock-control` is the first local zccusan agent. The CSI process uses it as a
client for snapshot create/delete, and `zcrepl csi-*` can use it with
`--control-url http://127.0.0.1:9788`. The same API has stream-start endpoints
for encrypted replication today; a later WebSocket transport can attach to the
same protocol shape without changing CSI orchestration.

The legacy Unix socket remains available for compatibility and accepts:

- `REPL_RECV volume=<id> listen=<addr> port=<port-or-0> token=auto`
- `REPL_SEND snapshot=<id>|volume=<id> peer=<ip> port=<port> token=<token>`
- `REPL_STATUS [repl_id=<id>]`

The controller opens the source snapshot/image or destination volume directly
and calls the Rust tcpmux/AES stream functions. It does not spawn
`zc-tcpmux-send`, `zc-tcpmux-receive`, `zcencrypt`, or shell pipelines for the
data path. `zccusan/deploy/zcblock-csi/test-local-regions-stream-repl.sh` exercises this
with three local region controllers in one Kubernetes cluster.

`zcrepl` exposes the same surfaces as a zc-style command:

```bash
zcrepl token
zcrepl recv --output /dev/target --listen 0.0.0.0 --port 42000 --generate-token
zcrepl send --input /dev/source --peer target-node --port 42000 --token "$TOKEN"
zcrepl csi-recv --control-url http://127.0.0.1:9788 --volume "$VOL" --token auto
zcrepl csi-send --control-url http://127.0.0.1:9788 --snapshot "$SNAP" --peer "$IP" --port "$PORT" --token "$TOKEN"
zcrepl csi-status --control-url http://127.0.0.1:9788 --repl-id "$REPL"

zcrepl csi-recv --socket /var/lib/zcblock-csi-b/control.sock --volume "$VOL" --token auto
```

`zcpit` exposes the file-level PIT snapshot primitive used by the CSI
file-loop zero-copy path:

```bash
zcpit snapshot --source /path/source.img --snapshot /path/snap.img --mode reflink
```

The current controller and `zcrepl send/recv` path is intentionally a correctness
baseline: it is encrypted, authenticated, and topology-addressable, but it still
uses normal read/write copies at the process boundary. The zero-copy production
path should promote the tcpmux parallel lane/topology metadata and descriptor
lease model into this API, so a source extent can stay aligned from source queue
to network lane to target disk queue without being collapsed through an
unlabeled process pipe.

## Usable Today

One-shot encrypted block or snapshot copy:

```bash
# target
TOKEN="$(zc-tcpmux-receive --generate-token)"
zc-tcpmux-receive \
  --listen-address 0.0.0.0 \
  --port 42000 \
  --token "$TOKEN" \
  --output /path/or/block-device

# source
zc-tcpmux-send \
  --peer-address target-node \
  --port 42000 \
  --token "$TOKEN" \
  --input /path/or/block-device
```

The default transport is AES-256-GCM. Use `--encryption none` only for explicit
plaintext tests.

Forward encrypted data through a middle node without decrypting there:

```bash
zc-tcpmux-receive \
  --listen-address 0.0.0.0 \
  --port 42000 \
  --token "$TOKEN" \
  --already-encrypted \
  --output - |
zcforward \
  --queue-depth 8 \
  --to-tcpmux target-node:43000 \
  --token "$TOKEN" \
  --already-encrypted \
  --to "zcdecrypt --token '$TOKEN' | zcsink --consume checksum"
```

This shape is useful for A -> B -> C tests where B must forward ciphertext and
also locally consume or inspect the plaintext view.

## Required Gateway

For real async replica service, add a gateway with these roles:

1. Ingest zc-originated block/WAL records from many sources.
2. Coalesce records by `(tenant, policy, source volume, target topology,
   destination disk, lane)`.
3. Schedule each coalesced stream through hierarchical token buckets.
4. Send extents over one or more `zc-tcpmux` aggregate links.
5. Charge downstream disk admission and durable write completion back to the
   same replication budget.
6. Emit durable watermarks for snapshot catchup and WAL replay.

The gateway should schedule by cost, not by raw bytes alone. A write consumes
tokens from every applicable bucket:

```text
root
  tenant
    tenant class or policy
      volume
        stream kind: snapshot | wal | catchup
          destination region
            destination disk / topology group
```

Each node in the tree can provide:

- `rate_bytes_per_sec`
- `burst_bytes`
- `priority`
- `max_inflight_extents`
- `deadline_ms` or `latency_class`
- `drop_or_defer` policy for temporary overload

The downstream disk must participate in the same accounting. The target should
ACK only after the extent is admitted to its durable path, or after it has been
admitted to a bounded hot tier whose spill/backing policy is part of the SLA.
That ACK returns credits to the gateway. If the target disk slows down, the
tenant's replication tokens drain and source backpressure follows naturally.

## Stream Types

`snapshot`: bulk transfer of a frozen snapshot image or raw volume image.
Usually large, low priority, high burst, resumable by byte range.

`wal`: continuous logical record stream. Usually smaller extents, latency
bounded, ordered per lane, ACKed by durable extent sequence.

`catchup`: replay from a snapshot cut to current WAL watermarks. It should be
scheduled between snapshot and live WAL priority so replicas converge without
starving foreground replication.

## Topology Alignment

Keep lane and placement metadata all the way through:

```text
source queue -> zc lane -> aggregate link lane -> receiver worker
             -> destination shard/queue -> durable WAL/disk extent
```

The existing `zc-tcpmux-xfer` lane topology header and WAL extent framing are
the right base. The gateway should not collapse lanes into one global FIFO.
Order is per lane unless a snapshot/barrier manifest explicitly requests a
global cut.

## Snapshot and WAL Catchup

A coherent async replica is:

1. Create a source snapshot under the freeze/barrier protocol.
2. Transfer the snapshot image through the gateway as `snapshot` traffic.
3. Record the WAL cut manifest: per lane durable sequence and byte range.
4. Replay `catchup` extents after the snapshot cut.
5. Switch to live `wal` once the replica reaches the current watermark.

The `zcsnap` manifest and `docs/wal-extent-framing.md` define the logical cut
shape. The CSI snapshot implementation gives us a source image today; the
gateway should eventually learn how to stream that image directly instead of
materializing every transfer as an intermediate file.

## Near-Term Build Order

1. Add a `zcshape` byte-compatible token-bucket filter for single-stream tests.
2. Extend `zcrepl` with explicit tenant, policy, lane, topology, and stream-kind
   labels, and keep those labels visible in logs and manifests.
3. Add a `zcrepl-gateway` config format with hierarchical buckets and labels:
   tenant, policy, volume, stream type, destination region, destination disk.
4. Teach the gateway to launch/own `zc-tcpmux` links and `zcforward` branches.
5. Add target-side durable ACK records for snapshot and WAL extents.
6. Wire CSI snapshot/volume metadata into gateway stream labels.
7. Replace byte pipes with descriptor leases and zero-copy release accounting.

The first step is intentionally small; the important production behavior starts
at steps 2-4, where scheduling and target durability become part of the protocol
rather than a side effect of shell pipe throughput.
