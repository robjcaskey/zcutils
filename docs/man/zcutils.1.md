# zcutils(1)

## Name

zcutils - zero-copy descriptor stream utility experiments

## Synopsis

```bash
zcutils zcprobe
zcutils zc-tcpmux-xfer HOST:DEST [options]
zcutils zc-tcpmux-xfer SRC HOST:DEST [options]
zcutils zc-tcpmux-receive --output PATH [options]
zcutils zc-tcpmux-send --peer-address ADDR --port PORT [options]
zcutils zcencrypt --token TOKEN [options]
zcutils zcdecrypt --token TOKEN [options]
zcutils zcflow 'STAGE' ['STAGE' ...]
zcutils zcflow --spec FILE
zcutils zccat [options]
zcutils zcout [options]
zcutils zcmap [options]
zcutils zctee [options]
zcutils zcmaptee [options]
zcutils zcsink [options]
zcutils zcstat [options]
zcutils zcmeter [options]
zcutils zcgrep --pattern BYTES [options]
zcutils zcmux --peer-addr ADDR [options]
zcutils zcdemux --bind ADDR [options]
zcutils zcnc listen [options]
zcutils zcnc connect [options]
```

## Description

`zcutils` is organized around descriptor streams: payload memory is owned by a
pool authority, commands pass leases, and terminal consumers release leases after
their async work completes. A descriptor-native pipeline needs a supervised
control channel for fd passing, credits, and releases. `zcflow` is the command
reserved for that role; today it runs byte-compatible pipes.

Normal shell composition should work without an explicit manager command:

```bash
zcdemux ... | zcmap --preserve-lanes | zcmux --peer-addr C ...
```

In that form, each downstream `zc*` command discovers the session from the
descriptor stream header. `zcflow` resolves stages through `PATH`, so
descriptor-native utilities can be provided by other packages. They do not need
to be compiled into `zcutils`; they need to follow the descriptor control-fd
protocol or intentionally operate in byte-compatible stdin/stdout mode.

Cargo builds separate command binaries such as `zcmux`, `zcdemux`, `zcmaptee`,
and `zcsink` from the same implementation. Users can keep `zcutils SUBCOMMAND`
as a fallback while using short names in pipelines.

## Commands

- `zcprobe`: print io_uring, ZCRX, send-zc, NAPI, and io-slot capability data.
- `zc-tcpmux-xfer`: SSH-coordinated TCP transfer; data is not sent over SSH.
- `zc-tcpmux-receive`: receive token-authenticated TCP into a file.
- `zc-tcpmux-send`: send stdin or a file over token-authenticated TCP.
- `zcencrypt`: encrypt a zc descriptor/frame stream with AES-256-GCM.
- `zcdecrypt`: decrypt a zc AES-256-GCM descriptor/frame stream.
- `zcflow`: run a descriptor-aware command chain; byte-compatible pipes today.
- `zccat`: source descriptors from files, stdin, block ranges, or generated data.
- `zcout`: materialize descriptors to stdout bytes.
- `zcmap`: transform descriptor metadata/views.
- `zctee`: fan out descriptors.
- `zcmaptee`: fused map plus fanout.
- `zcsink`: terminal count, drop, or checksum consumer.
- `zcstat`: count bytes/chunks and report throughput.
- `zcmeter`: pass bytes through while printing live receive rate to stderr.
- `zcgrep`: filter stdin lines by byte pattern.
- `zcdemux`: receive lane-multiplexed TCP flows into a descriptor stream.
- `zcmux`: send a descriptor stream over lane-multiplexed TCP flows.
- `zcnc`: simple listen/connect/probe frontend.

## Zero-Copy Options

- `--send-mode send-zc`: send-side zero-copy. Default for `zcmux` and `zcnc connect`.
- `--send-mode send-zc-fixed`: send-zc with registered buffers.
- `--send-mode send`: explicit copied send fallback.
- `--zero-copy-send auto`: default; try send-zc and fall back to copied send when setup is not allowed.
- `--zero-copy-send required`: fail if send-zc setup is not allowed.
- `--zero-copy-send off`: force copied send.
- `--zero-copy-receive auto`: default; try ZCRX and fall back to copied recv when unavailable or unauthorized.
- `--zero-copy-receive required`: fail if ZCRX cannot be enabled.
- `--zero-copy-receive off`: force copied recv.

## TCP Mux Transfer Encryption

`zc-tcpmux-xfer` uses SSH only to coordinate the remote receiver and pass a
one-use token. Payload bytes are not sent through SSH. AES-256-GCM is the
default and only encrypted payload mode; use `--encryption none` only for
explicit plaintext tests.

Receive-side options can be set through xfer with a `--receive-` prefix:
`--receive-listen-address`, `--receive-listen-port-range`,
`--receive-buffer-bytes`, `--receive-token`,
`--receive-disable-authentication`, `--receive-encryption`, and
`--receive-already-encrypted`.

Use `--already-encrypted` on `zc-tcpmux-receive` to emit the AES-256 frame
stream without decrypting it, and on `zc-tcpmux-send` to forward such a stream
without re-encrypting. `zcencrypt` and `zcdecrypt` are generic pipeline
elements for AES-256-GCM zc descriptor/frame streams and are not tied to
tcpmux. They accept topology hint flags: `--topology`, `--lane-id`,
`--queue-id`, `--preferred-cpu`, `--numa-node`, and `--ordered global|per-lane`.
- `--recv-mode auto`: select ZCRX auto-discovery receive mode.
- `--recv-mode zcrx`: require ZCRX setup for receive.
- `--recv-mode recv`: explicit copied receive fallback.
- `--must-zero-copy`: compatibility alias for required receive zero-copy.

## Examples

A to B terminal receive:

```bash
zcflow \
  'zcdemux --bind 0.0.0.0 --base-port 9000 --lanes 240' \
  'zcsink --consume checksum'
```

A to B to C relay:

```bash
zcflow \
  'zcdemux --bind 0.0.0.0 --base-port 9000 --lanes 240' \
  'zcmap --preserve-lanes --ordered per-lane' \
  'zcmux --peer-addr C --base-port 9000 --lanes 240'
```

Fanout:

```bash
zcflow \
  'zcdemux --bind 0.0.0.0 --base-port 9000 --lanes 240' \
  'zcmaptee --preserve-lanes \
     --to "zcmux --peer-addr C --base-port 9000 --lanes 240" \
     --to "zcsink --consume checksum"'
```

Byte-compatible smoke test:

```bash
printf 'hello\n' | zcmap | zcsink --consume checksum
```

Heredoc flow spec:

```bash
zcflow --spec - <<'EOF'
zccat --generate --bytes 1m --chunk-bytes 64k
zcmap --preserve-lanes
zcsink --consume checksum
EOF
```

Remote transfer orchestration:

```bash
zcxfer push nodeB \
  --source 'zccat --generate --bytes 128g --chunk-bytes 1m' \
  --sink 'zcsink --consume checksum' \
  --lanes 240
```

## Files

- `docs/zero-copy-descriptor.md`: descriptor, collection, list, and lease model.
- `docs/zcutils-commands.md`: command design and examples.
