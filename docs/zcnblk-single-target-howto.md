# Single Unencrypted zcnblk Target Howto

This is the basic block-device howto for a single unencrypted `zcnblk` target.
It is intentionally not yet as pipeliney, code-sharing, or decomposed as it
should be for its proper place in the zc cinmeatic uniuverse. Treat it as the
current reproducible block harness: good for conventional `/dev/zcnblk0` fio
comparisons and transport hot-path work, not yet the final shape for WAL,
RAID, descriptor lanes, or Raft integration.

## What This Measures

This setup measures a kernel block client, `/dev/zcnblk0`, talking to one
unencrypted user-space `zcnblk-target`.

The recorded benchmark used:

| Field | Value |
| --- | --- |
| Client | EC2 `c8gn.48xlarge`, same AZ as target |
| Target | EC2 `c8gn.48xlarge`, same AZ as client |
| Network | Private IPv4 path |
| Kernel | `6.17.0-1017-aws` on both hosts |
| Target backend | `zcdevnull0` |
| Encryption | Disabled |
| Client device | `/dev/zcnblk0` from `kmods/zcnblk_client_mod.ko` |
| fio engine | `io_uring`, `direct=1` |
| fio concurrency | `numjobs=64`, `iodepth=64` |
| fio block size | `4k` |
| fio runtime | 10 seconds |

`zcdevnull0` is a synthetic target. Writes are accepted and reads are generated
by the target, so the numbers below validate the client, network, framing,
target dispatch, and block submission path. They do not prove persistent media
speed, read-after-write correctness, or checksum durability.

## Point-To-Point Config

The documented benchmark is a direct point-to-point block path:

```text
fio io_uring
  -> /dev/zcnblk0 kernel client
  -> 64 direct TCP lane connections over private IPv4
  -> zcnblk-target
  -> zcdevnull0 synthetic target
```

There is no `tcpmux`, no encryption, no WAL file, no RAID fanout, and no remote
block persistence in this run. That is deliberate: it isolates the point-to-point
kernel block client, TCP lane framing, and user-space target dispatch cost.

Use this concrete two-host shape:

| Role | Value |
| --- | --- |
| Target host | `172.31.3.193` private IPv4 |
| Client host | `172.31.15.246` private IPv4 |
| Target bind | `0.0.0.0` |
| Base port | Any free range start, examples use `23600` |
| Port range | `BASE..BASE+63` for 64 lanes |
| Client target address | Target private IPv4 |
| Block device on client | `/dev/zcnblk0` |
| Target backend | `zcdevnull0` |

The target security group or host firewall must allow the client to connect to
the full TCP lane range. With `BASE=23600` and `lanes=64`, allow TCP
`23600-23663` from the client private IP or its security group.

## Build

Build the user-space target and the kernel block client:

```bash
cargo build --release --bin zcutils
make -C kmods
```

## Target Config

On the target host, pick a free base port range. The example below uses 64 lanes,
so reserve `BASE` through `BASE + 63`.

```bash
TARGET_LISTEN_IP=0.0.0.0
BASE=23600

URING_PLAY_ZCNBLK_ENCRYPTION=none \
URING_PLAY_TCP_WAL_WRITE_MODE=null \
URING_PLAY_ZCNBLK_READ_MODE=null \
URING_PLAY_ZCNBLK_SUBMIT_BATCH=64 \
URING_PLAY_ZCNBLK_WRITE_ACKS=1 \
  ./target/release/zcutils zcnblk-target \
    zcdevnull0 "$TARGET_LISTEN_IP" "$BASE" \
    64 1 2048G 4K 256 64 4096 small-pages true
```

Important target settings:

| Setting | Meaning |
| --- | --- |
| `URING_PLAY_ZCNBLK_ENCRYPTION=none` | Plaintext transport. Do not set `URING_PLAY_ZCNBLK_TOKEN`. |
| `URING_PLAY_TCP_WAL_WRITE_MODE=null` | Do not write target traffic to a WAL file. |
| `URING_PLAY_ZCNBLK_READ_MODE=null` | Generate read payloads from the synthetic target. |
| `URING_PLAY_ZCNBLK_WRITE_ACKS=1` | Send write completions back to the client. |
| `64 1` | 64 lanes, 1 connection per lane. |
| `2048G` | Per-connection byte budget for the target run. |
| `4K` and `4096` | 4 KiB block/framing shape for this run. |
| `256 64 4096` | Target pipeline, worker count, and ring entries. |
| `small-pages true` | Use the small-page buffer path for this target shape. |

If fio stops a time-based run before the target's byte budget is consumed, the
target may log `UnexpectedEof` during cleanup. That is expected shutdown noise
after fio has already printed a valid result.

## Client Config

On the client host, load the block client module. Use the target's private IP
for `remote_ip`.

```bash
TARGET_PRIV_IP=172.31.3.193
BASE=23600

sudo insmod kmods/zcnblk_client_mod.ko \
  remote_ip="$TARGET_PRIV_IP" \
  remote_port_base="$BASE" \
  lanes=64 \
  connections_per_lane=1 \
  shard_count=1 \
  size_mib=8192 \
  logical_block_size=4096 \
  stripe_unit=4096 \
  max_frame_bytes=4096 \
  queues=64 \
  queue_depth=256 \
  pipeline_depth=128 \
  fill_timeout_ms=0 \
  batch_depth=1 \
  write_acks=1 \
  hctx_affinity=1
```

The fast 4K path depends on `hctx_affinity=1`, which maps blk-mq hardware
queues directly onto target connections and avoids the old global connection
picker. Keep `batch_depth=1` for this shape; request batching was slower in the
single-target 4K fio run.

After loading, verify that the device exists:

```bash
lsblk /dev/zcnblk0
```

Unload it after the benchmark:

```bash
sudo rmmod zcnblk_client_mod
```

## fio Read And Write Runs

Use the same fio shape for read and write so the comparison is clean.

Read:

```bash
sudo fio \
  --name=zcnblk-plain-randread \
  --filename=/dev/zcnblk0 \
  --rw=randread \
  --bs=4k \
  --iodepth=64 \
  --numjobs=64 \
  --ioengine=io_uring \
  --direct=1 \
  --time_based=1 \
  --runtime=10 \
  --group_reporting=1 \
  --randrepeat=0 \
  --norandommap=1
```

Write:

```bash
sudo fio \
  --name=zcnblk-plain-randwrite \
  --filename=/dev/zcnblk0 \
  --rw=randwrite \
  --bs=4k \
  --iodepth=64 \
  --numjobs=64 \
  --ioengine=io_uring \
  --direct=1 \
  --time_based=1 \
  --runtime=10 \
  --group_reporting=1 \
  --randrepeat=0 \
  --norandommap=1
```

The aggregate queue depth presented by fio is `64 jobs * 64 iodepth = 4096`.
The module queue and pipeline settings are separate: they control how the block
client maps that fio pressure onto lanes and in-flight network frames.

## Recorded Numbers

Recorded on the two-node `c8gn.48xlarge` private-network setup above:

| fio mode | IOPS | Bandwidth | fio IO/run | avg clat | p50 | p95 | p99 | p99.9 | CPU usr/sys |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `randread` | 7.625M | 29.1 GiB/s | 291 GiB / 10.002 s | 534.96 us | 437 us | 1.205 ms | 1.745 ms | 3.163 ms | 8.53% / 26.50% |
| `randwrite` | 6.091M | 23.2 GiB/s | 236 GiB / 10.172 s | 659.69 us | 570 us | 1.254 ms | 1.778 ms | 2.999 ms | 7.56% / 21.54% |

These are acknowledged writes: each 4 KiB write payload goes from client to
target, and the client waits for the target's write ACK before completing the
block request.

The same config also produced a shorter 5 second repeat at `7.353M` read IOPS
and `6.706M` acknowledged write IOPS. Use the 10 second table above as the
documented baseline.

## Related Baseline

The raw user-space `zcnblk-send` plaintext transport has measured about
`25.96 GiB/s` on a 32 GiB transfer in the same general EC2 lab. That path avoids
the kernel block-device fio surface, so compare it separately from `/dev/zcnblk0`
numbers.
