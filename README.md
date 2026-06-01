# zcutils

`zcutils` is an experimental Linux utility crate for zero-copy descriptor
streams, lane-multiplexed TCP transfer, and low-level io_uring storage/network
benchmarks. It builds one umbrella binary plus direct command binaries for the
main pipeline tools.

## Status

The current release is `1.0.0`. This repository declares descriptor protocol
version 1 as the initial descriptor stream format. Descriptor-native transport
is specified in `docs/zero-copy-descriptor.md`, but the local pipeline tools
still use byte-compatible stdin/stdout paths until Unix socket fd passing,
credits, and release accounting are implemented. The TCP mux transfer path and
AES-256-GCM frame format are implemented today.

## Build

```bash
cargo build --release --bins
```

Both invocation styles are supported:

```bash
zcutils zcprobe
zcprobe
zcutils zc-tcpmux-xfer ./file nodeB:/tmp/file
zc-tcpmux-xfer ./file nodeB:/tmp/file
```

## Main Commands

- `zcprobe`: inspect io_uring, ZCRX, send-zc, NAPI, and io-slot capabilities.
- `zc-tcpmux-xfer`: coordinate a remote receiver over SSH, then transfer bytes
  over TCP data sockets.
- `zc-tcpmux-send` and `zc-tcpmux-receive`: direct token-authenticated TCP mux
  sender and receiver primitives.
- `zcencrypt` and `zcdecrypt`: AES-256-GCM framed stream filters.
- `zcflow`: descriptor-aware pipeline runner; byte pipes today.
- `zccat`, `zcout`, `zcmap`, `zcmaptee`, `zctee`, `zctier`, `zcsink`, `zcstat`,
  `zcmeter`, and `zcgrep`: byte-compatible forms of the planned descriptor
  pipeline tools.
- `zcsnap`: descriptor/WAL snapshot cut marker; byte-compatible manifest
  emission today.
- `zcmux`, `zcdemux`, and `zcnc`: lane-multiplexed TCP and netcat-like
  network experiments.
- `zcraid-split`, `zcraid-merge`, and `zctier`: userspace RAID/fanout,
  reassembly, hot-tier, and bounded spill building blocks.
- `/dev/zcnblk0`: the client-side block onramp to the SAN fabric. It is backed
  by the `zcnblk` wire protocol today, but the block device users point fio,
  databases, and filesystems at is `/dev/zcnblk0`.
- `zcbrd`: optional RAM-backed block media for targets and tests. Mid-tree
  fanout, fanin, forwarding, RAID0/RAID1 policy, tier routing, tier spill
  decisions, and backpressure stay in userspace. Block is only the client
  onramp or the last hop where a userspace target finally lands bytes.
- `zcwritebench` and the umbrella-only benchmark subcommands such as
  `slot-wal-bench`, `slot-rand-bench`, `slot-rand-sharded-bench`,
  `zckv-page-bench`, and `zckv-compact-bench`: low-level io_uring and storage
  benchmark helpers.

## Examples

Run a local byte-compatible smoke test:

```bash
printf 'hello\n' | zcmap | zcsink --consume checksum
```

Transfer a file to a remote host:

```bash
zc-tcpmux-xfer ./payload.bin nodeB:/tmp/payload.bin
```

Pin TCP mux lanes to a CPU list on both sides:

```bash
zc-tcpmux-xfer ./payload.bin nodeB:/dev/null \
  --lanes 128 \
  --pin-cpus \
  --cpu-list 0-95
```

## Descriptor Spec and Upgrade Path

The descriptor stream spec is versioned separately from the crate. This
repository declares the initial descriptor protocol as protocol version 1,
carried by a fixed `ZcStreamHeader` with `protocol_version`,
`min_reader_version`, feature flags, and `header_len`. Compatible additions
append length-delimited fields or frames and use optional feature bits.
Incompatible wire changes must bump `protocol_version`, set
`min_reader_version` to the oldest safe reader, and keep old readers from
silently accepting streams they cannot interpret.

The implemented TCP mux lane header is currently V2. Receivers still accept the
legacy V1 lane header, and V2-compatible extensions can append bytes behind the
declared body length. See `docs/zero-copy-descriptor.md` for the exact stream,
frame, and topology-header rules.

## Documentation

For the smallest complete sample of the repo's intended style, start with
`zc-tcpmux-send` and `zc-tcpmux-receive`: token-authenticated transport,
lane-aware TCP muxing, AES framing, clear CLI shape, and direct command binaries
without dragging in the SAN stack.

- `docs/zero-copy-descriptor.md`: descriptor, collection, list, lease, and
  protocol upgrade model.
- `docs/faq.md`: terminology and design notes, starting with what zcutils
  means by lanes.
- `docs/wal-extent-framing.md`: lane-preserving WAL extent framing for logical
  4K records over coalesced physical appends.
- `docs/zccu-target-architecture.html`: rendered single-page topology for the
  concrete `/dev/zcnblk0 -> tcpmux -> zctee -> split tree -> zcbrd` layout.
- `scripts/render-zccu-volume-layout.py`: `uv` Python renderer for that HTML
  topology artifact.
- `docs/zcutils-commands.md`: command design and examples.
- `src/bin/zc-tcpmux-send.rs` and `src/bin/zc-tcpmux-receive.rs`: the golden
  child sample app pair for stream transport behavior.
- `docs/block-vs-userspace-bench-plan.md`: benchmark plan and short recipes
  for comparing `/dev/zcnblk0`, the zcnblk wire path, TCP userspace, and zcraid
  logical paths.
- `docs/man/zcutils.1.md`: man-page style command reference.
- `docs/nvme-slot-topology-todo.md`: storage topology notes.

## zccusan Docs

The SAN/Kubernetes storage-network material lives under `zccusan/`, separate
from the main stream and pipeline documentation. Start with
`zccusan/README.md`.

## License

`zcutils` is multi-licensed under your choice of MIT, Apache-2.0, or
BSD-2-Clause. See `LICENSE.md`.
