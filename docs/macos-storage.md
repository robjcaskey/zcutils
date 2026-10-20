# macOS storage and regional participation

Build on Apple Silicon or Intel macOS with Rust and Xcode command line tools:

```sh
brew install protobuf
bash scripts/build-macos-userspace.sh
cargo test --locked --lib --bin zcglobal-policy-node --bin zcglobal-kubernetes-adapter
python3 scripts/test-portable-storage.py
```

The build produces the TCP client and terminal target, racing userspace mirror,
regional policy node, Kubernetes federation adapter, operator, CRD generator,
and telemetry server. The macOS workflow was exercised on both architectures with shared contract
tests and executable recovery/backup/mirror checks. It is retained as a disabled,
manual reference; users should build locally. These are development artifacts, not notarized installers.

## File-backed target and shared WAL

```sh
mkdir -p "$HOME/zc-storage"
target/debug/zcnblk-portable-target --listen 127.0.0.1:19603 \
  --file "$HOME/zc-storage/base.img" --size-mib 512 \
  --wal-journal "$HOME/zc-storage/journal.img" --journal-mib 64
```

This uses the same persistent WAL format, CRC32C, recovery, retention, and
reducer as Linux and Windows. Darwin admission requires local APFS/HFS regular
files, exclusive custody before mutation, preallocation, and distinct file
identities. SYNC uses F_FULLFSYNC before advancing the durable WAL prefix.
O_DIRECT is explicitly unsupported; the buffered macOS path is not a claim of
Linux io_uring/OFI performance parity. The plain file target also performs a full
flush on macOS. `--ram-mib 512` creates a volatile target instead.

The target speaks the standard plain TCP zcnblk protocol: Linux userspace
placement stages and clients can use it as a terminal leaf. Mirroring and
placement remain userspace decisions; no block device is a RAID primitive.
Only expose this plain endpoint through a trusted network or SSH tunnel.
For example, `ssh -L 19603:127.0.0.1:19603 user@mac-host` exposes it locally.

## Remote backup and restore

Quiesce writers to the source before a full image backup. The plain target
protocol does not provide a live snapshot or an atomic multi-request copy.

```sh
target/debug/zcnblk-client backup 127.0.0.1:19603 536870912 backup.img --source-quiesced
target/debug/zcnblk-client restore 127.0.0.1:19604 backup.img
```

Backup refuses to overwrite an existing file and reports the image SHA-256.
Restore writes into a running target of sufficient capacity and issues SYNC
before success. Linux and macOS can each supply either end of the transfer.
A remote backup target can be another portable WAL leaf, including one on Linux.
Keep a destination offline to other writers/readers while restoring it.

## Userspace mirrors

`zcracing-mirror` uses the existing Linux framed-log protocol and contiguous
high-water rules on macOS. A first hop owns the local terminal log and forwards
frames to a remote leaf. Exclusive file custody precedes scanning and tail
repair. It acknowledges a batch only after both legs are
synced. Restart resumes at the recovered prefix and replays a missing remote
suffix. macOS uses bounded buffered payload copies; Linux retains splice/tee.

```sh
# On the backup host (use a tunnel for an untrusted network):
target/debug/zcracing-mirror leaf 127.0.0.1:19701 remote.log
# On the first-hop host:
target/debug/zcracing-mirror first-hop 127.0.0.1:19702 127.0.0.1:19701 local.log
# Protocol exercise; the production placement stage owns application placement:
target/debug/zcracing-mirror client 127.0.0.1:19702 128 4096 8
```

The executable client generates a known pattern for validation; it is not an
application block-device adapter. This log mirror and a terminal block leaf are
different stages. Do not describe a raw leaf as a complete HA mirror.

## Regional control

`zcglobal-policy-node` and `zcglobal-kubernetes-adapter` use the same regional
policy, trust, HA metadata, admission, secret lifecycle, and authenticated RPC
modules as Linux. See the existing regional/federation runbooks for membership
and policy administration. Platform availability does not itself authorize a new
region or grant its storage access. The Kubernetes CSI node/kernel data path
remains Linux-only; running the operator on a Mac does not create a macOS CSI
node. An experimental SCSI DriverKit edge and userspace broker are now included;
see [local desktop builds](native-desktop-builds.md) for build instructions and
the signing and runtime-validation requirements. Native mounting is not yet
verified on a provisioned Mac.

## Mixed Linux/macOS verification

With the binaries built on both hosts and a Mac checkout at `~/zcutils`, run:

```sh
python3 scripts/test-macos-linux-storage.py ~/robs-macos-build-machine/ssh.sh ~/mac-linux-test
```

The wrapper must execute SSH to the Mac and pass through supplied arguments.
The harness uses loopback ports 29701–29713 and a single SSH tunnel. It tests
WAL-backed backup/restore in both directions, the Mac as both mirror first hop
and leaf, and a three-voter regional policy quorum containing a real Mac voter.
It stores evidence under the requested output directory and stops only the
processes it started. Test credentials and files are disposable. The harness
currently assumes the EC2 Mac user is `ec2-user`; the ordinary tools have no such
account requirement. These are correctness checks, not representative throughput
or latency benchmarks.

For a new federation spanning a higher-latency path, set the same
`ZCGLOBAL_TIMING_SCALE=5` on every policy node and management CLI. This scales
heartbeat, authority, election, and RPC budgets together. Profiles are carried
inside authenticated RPCs; mismatches are rejected before consensus mutation.
The selected profile is persisted, and non-default profiles use a state identity
that older binaries refuse to reopen with shorter leases. Changing the profile
requires a quiesced, newly initialized federation state; do not edit live state
files. The default profile keeps the existing Linux timing and wire format.
`ZCGLOBAL_RPC_TIMEOUT` may tune the socket budget within that profile's bound.
