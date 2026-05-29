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

## Descriptor Commands

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
operation, `zcbrd` feature, `zcstripe` feature, or `zcnblk` mode. It only
describes a logical stream/WAL cut that future descriptor-aware stages can pin,
release, and replay.

### zcforward

`zcforward` is the fused B-node primitive for A -> B -> C replication. It reads
one stream from stdin, shares each chunk with bounded branch queues, and writes
to command or file branches in parallel. That keeps the forwarding branch from
serializing behind the local consume branch.

```bash
zc-tcpmux-receive --output - --token "$TOKEN" --already-encrypted |
zcforward \
  --queue-depth 8 \
  --to "zc-tcpmux-send --peer-address nodeC --port 43000 --token '$TOKEN' --already-encrypted" \
  --to "zcdecrypt --token '$TOKEN' | zcsink --consume checksum"
```

### zcraid-split, zcraid-merge, and Daemon Aliases

`zcraid-split` frames ordered input chunks with global offsets and stripes or
mirrors those frames across branches. `zcraid-merge` reads branch streams back,
deduplicates mirrored chunks, verifies optional checksums, and writes the
ordered byte stream. `zcraid-fanoutd` is an alias for `zcraid-split`, and
`zcraid-fanind` is an alias for `zcraid-merge`; use those names when the process
is acting as a long-running fanout or fanin daemon around tcpmux receive/send
commands.

```bash
zccat --generate --bytes 8g |
zcraid-split --mode raid10 --replicas 2 --chunk-bytes 1m \
  --to "zc-tcpmux-send --peer-address nodeB --port 44000 --encryption none --disable-authentication" \
  --to "zc-tcpmux-send --peer-address nodeC --port 44000 --encryption none --disable-authentication"

zcraid-merge \
  --from "zc-tcpmux-receive --output - --port 44000 --encryption none --disable-authentication" \
  --from "zc-tcpmux-receive --output - --port 44001 --encryption none --disable-authentication" \
  --output /tmp/reassembled
```

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

`zcnblk-target` receives mux-aligned block read/write frames for synthetic
targets such as `zcdevnullN`, `/dev/zcbrdN`, and `/dev/zcstripeN`.
`zcnblk-send` is the user-space generator for write, read, and mixed 4K block
traffic.

For the concrete point-to-point single-target unencrypted `/dev/zcnblk0` fio
setup, including module config and recorded read/write benchmark numbers, see
[`zcnblk-single-target-howto.md`](zcnblk-single-target-howto.md).

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
