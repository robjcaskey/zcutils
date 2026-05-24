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
zcmaptee \
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
forwarding modes. Defaults to required receive zero-copy.

```bash
zcdemux \
  --bind 0.0.0.0 \
  --base-port 9000 \
  --lanes 240 \
  --connections-per-lane 1 \
  --expected-bytes 512m \
  --workers 40 \
  --zero-copy-receive required
```

Important flags:

- `--zero-copy-receive required`: default; fail if ZCRX cannot be used.
- `--zero-copy-receive auto`: try ZCRX, but fall back to io_uring recv.
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
  --zero-copy-send required
```

Important flags:

- `--send-mode send-zc`: default.
- `--send-mode send-zc-fixed`: use send-zc with registered buffers.
- `--send-mode send`: explicit copied fallback.
- `--zero-copy-send required`: default.
- `--zero-copy-send auto`: try send-zc, but fall back to copied send.
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
printf 'abc\n' | zcgrep --pattern b
printf 'abc\n' | zcstat
```

## Compatibility

Existing benchmark subcommands remain available, including
`tcp-bench-uring-mux-send` and `tcp-bench-uring-mux-server`. The receive side
now defaults to `auto` ZCRX. Use `recv` explicitly to force the old copied path.
