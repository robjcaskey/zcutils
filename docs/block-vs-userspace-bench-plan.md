# Block Vs Userspace Benchmark Plan

This plan compares the conventional block path against the userspace and
descriptor-shaped paths without mixing incompatible counters. The key rule is:
always report both physical operation rate and logical 4K record rate.

`/dev/zcnblk0` is the client-side block onramp to the SAN fabric. It is the
single block device family that fio, databases, filesystems, and block-speaking
applications should use on the client side. It uses the `zcnblk` wire protocol
and the userspace `zcnblk-target` service.

Target hosts should be userspace services. They may use `zcdevnullN`,
`zctier:...`, ordinary files, real allowlisted block devices, or optional
`/dev/zcbrdN` RAM media as backing. They should not use custom target-side zc
block topology; userspace owns RAID, fanout, fanin, tiering, and spill policy.

Block devices are edge adapters in this plan. They are useful for fio, database
compatibility, RAM-backed media, real backing devices, and final exposure to
block-speaking clients, but the fanout/fanin topology should be modeled in
userspace. A benchmark may read from or write to `/dev/zcbrdN` as a convenient
edge, but mux/demux routing, forwarding, RAID0/RAID1 policy, tiering, tier
spill decisions, backpressure, and descriptor lane scheduling must be accounted
for as userspace work. A tier may land hot or spill bytes on block media, but
that block device is only the last hop.

For a 4K block fio run, one physical block operation is one logical 4K record.
For a 384K WAL extent, one physical append contains 96 logical 4K records. A
run can therefore show high logical IOPS while issuing far fewer kernel block
requests.

## Measurement Contract

Every run should log:

- exact command line and environment
- host, kernel, NIC, block device, module parameters, and `zcprobe`
- physical bytes/sec and physical ops/sec
- logical 4K records/sec, computed as `bytes / 4096 / seconds`
- user/sys CPU, context switches, migrations, IRQ/softirq spread, and NUMA
  placement when the run is serious
- latency percentiles for fio or request/reply paths
- whether the path used kernel block requests, userspace frames, zcnblk frames,
  io-slot WAL writes, or descriptor-only simulation

For EC2 cluster runs, use only private IPv4 addresses and hold the shared lock:

```bash
(
  flock -n 9 || { echo "cluster busy"; exit 1; }
  # Run the short benchmark here.
) 9>/tmp/cluster.lock
```

Current private node inventory:

| Node | Private IPv4 |
| --- | --- |
| n1 | `172.31.1.49` |
| n2 | `172.31.12.58` |
| n3 | `172.31.2.55` |
| n4 | `172.31.3.213` |
| n5 | `172.31.4.1` |
| n6 | `172.31.4.236` |
| n7 | `172.31.6.136` |
| n8 | `172.31.9.178` |

## Experiment Matrix

| Case | Path | Primary Question | Main Counter |
| --- | --- | --- | --- |
| Local null block | `fio -> /dev/nullb0` | What does the Linux block layer cost when the device does almost nothing? | fio IOPS and clat |
| Local RAM block edge | `fio -> /dev/ram0` or `fio -> /dev/zcbrd0` | What is the local block ceiling with memory-backed media? | fio IOPS and sys CPU |
| Local io-slot block lab | `slot-* -> /dev/zcbrd0`  | What does the low-level block/WAL lab path do below fio? | `ops_per_sec`, `MiBps` |
| Local zcnblk loopback | `fio -> /dev/zcnblk0 -> zcnblk-target -> zcdevnull0` | What does the zcnblk block protocol cost without real fabric distance? | fio plus target summaries |
| Remote userspace transport | `zcnc` or `tcp-bench-uring-mux-*` | What can TCP lanes move without block requests? | sender/server Gbit/s |
| Remote zcnblk generator | `zcnblk-send -> zcnblk-target -> zcdevnull0` | What is the remote zcnblk frame path before the kernel block client? | `zcnblk-send-summary` |
| Remote kernel block client | `fio -> /dev/zcnblk0 -> target` | What does the full client-side block surface cost? | fio IOPS, clat, sys CPU |
| Userspace RAID/WAL | `zcraidd wal-bench`, `zcraid-*`, `zctier` | How fast is logical record fanout/fanin/tiering when records are batched into extents? | `record_iops`, `segment_iops`, and spill queue high-water |

## Local Block Recipes

Build once:

```bash
cargo build --release --bins
make -C kmods
```

Optional Linux `null_blk` target:

```bash
sudo modprobe null_blk nr_devices=1 gb=8 bs=4096 queue_mode=2 irqmode=0 completion_nsec=0
lsblk /dev/nullb0
```

Optional Linux `brd` target:

```bash
sudo modprobe brd rd_nr=1 rd_size=8388608 max_part=0
lsblk /dev/ram0
```

Optional `zcbrd` target:

```bash
sudo modprobe configfs
mountpoint -q /sys/kernel/config || sudo mount -t configfs configfs /sys/kernel/config
sudo insmod kmods/zcbrd_mod.ko
sudo mkdir /sys/kernel/config/zcbrd/zcbrd0
echo 8192 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/size_mib
echo 4096 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/blocksize
echo "$(nproc)" | sudo tee /sys/kernel/config/zcbrd/zcbrd0/queues
echo 1024 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/queue_depth
echo "$(nproc)" | sudo tee /sys/kernel/config/zcbrd/zcbrd0/shards
echo advertise | sudo tee /sys/kernel/config/zcbrd/zcbrd0/descriptor_mode
echo 1 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/power
lsblk /dev/zcbrd0
```

Run the same fio shape against each block device:

```bash
DEV=/dev/zcbrd0
sudo fio \
  --name=block-4k-randread \
  --filename="$DEV" \
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

```bash
DEV=/dev/zcbrd0
sudo fio \
  --name=block-4k-randwrite \
  --filename="$DEV" \
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

Run the project-local block/WAL benches against the same device:

```bash
sudo env URING_PLAY_SLOT_BUFFER_LAYOUT=shared-buffer \
  ./target/release/zcutils slot-wal-bench /dev/zcbrd0 16g 4k 256 1024 write small-pages

sudo ./target/release/zcutils \
  slot-rand-sharded-bench /dev/zcbrd0 64 100000 4k 64 512 read 100 8g small-pages true

sudo env URING_PLAY_URING_WRITE_COMPLETION_BATCH=64 \
  ./target/release/zcutils uring-write-bench /dev/zcbrd0 64 256m 4k 64 512 small-pages fixed-file true
```

The existing wider local sweep is:

```bash
COUNTS="1 2 4 8" CHUNKS="4096 65536 393216" \
  scripts/ec2-ram-topology-matrix.sh
```

## Local zcnblk Loopback

This isolates zcnblk framing and the kernel client from real network distance.
Use it as a protocol overhead check, not as production evidence.

Terminal 1:

```bash
BASE=23600
URING_PLAY_ZCNBLK_ENCRYPTION=none \
URING_PLAY_TCP_WAL_WRITE_MODE=null \
URING_PLAY_ZCNBLK_READ_MODE=null \
URING_PLAY_ZCNBLK_SUBMIT_BATCH=64 \
URING_PLAY_ZCNBLK_WRITE_ACKS=1 \
  ./target/release/zcutils zcnblk-target \
    zcdevnull0 127.0.0.1 "$BASE" \
    64 1 256G 4K 256 64 4096 small-pages true
```

Terminal 2:

```bash
BASE=23600
sudo insmod kmods/zcnblk_client_mod.ko \
  remote_ip=127.0.0.1 \
  remote_port_base="$BASE" \
  lanes=64 \
  connections_per_lane=1 \
  shard_count=1 \
  size_mib=8192 \
  logical_block_size=4096 \
  max_frame_bytes=4096 \
  queues=64 \
  queue_depth=256 \
  pipeline_depth=128 \
  fill_timeout_ms=0 \
  batch_depth=1 \
  write_acks=1 \
  hctx_affinity=1

sudo fio --name=zcnblk-loop-randread --filename=/dev/zcnblk0 \
  --rw=randread --bs=4k --iodepth=64 --numjobs=64 --ioengine=io_uring \
  --direct=1 --time_based=1 --runtime=10 --group_reporting=1 \
  --randrepeat=0 --norandommap=1
```

Repeat with `--rw=randwrite`, then unload:

```bash
sudo rmmod zcnblk_client_mod
```

## Remote Target Recipes

Use n2 as target and n1 as client for a short two-node run. Keep the target in a
tmux pane or service manager so the command line and logs are preserved.

Target n2, synthetic null-ish backend:

```bash
BASE=23600
URING_PLAY_ZCNBLK_ENCRYPTION=none \
URING_PLAY_TCP_WAL_WRITE_MODE=null \
URING_PLAY_ZCNBLK_READ_MODE=null \
URING_PLAY_ZCNBLK_SUBMIT_BATCH=64 \
URING_PLAY_ZCNBLK_WRITE_ACKS=1 \
  ./target/release/zcutils zcnblk-target \
    zcdevnull0 0.0.0.0 "$BASE" \
    64 1 512G 4K 256 64 4096 small-pages true
```

Client n1, userspace zcnblk generator:

```bash
TARGET=172.31.12.58
BASE=23600
URING_PLAY_ZCNBLK_ENCRYPTION=none \
  ./target/release/zcutils zcnblk-send \
    "$TARGET" 1 "$BASE" 64 1 4G 4K 64
```

Client n1, block device if present:

```bash
if [ -b /dev/zcnblk0 ]; then
  sudo fio --name=zcnblk-remote-randread --filename=/dev/zcnblk0 \
    --rw=randread --bs=4k --iodepth=64 --numjobs=64 --ioengine=io_uring \
    --direct=1 --time_based=1 --runtime=10 --group_reporting=1 \
    --randrepeat=0 --norandommap=1
else
  echo "/dev/zcnblk0 is not present; load kmods/zcnblk_client_mod.ko first"
fi
```

If the client device is absent, load it on n1 with the target private IP:

```bash
TARGET=172.31.12.58
BASE=23600
sudo insmod kmods/zcnblk_client_mod.ko \
  remote_ip="$TARGET" remote_port_base="$BASE" lanes=64 connections_per_lane=1 \
  shard_count=1 size_mib=8192 logical_block_size=4096 max_frame_bytes=4096 \
  queues=64 queue_depth=256 pipeline_depth=128 \
  fill_timeout_ms=0 batch_depth=1 write_acks=1 hctx_affinity=1
```

The existing full howto for this path is
[`zcnblk-single-target-howto.md`](zcnblk-single-target-howto.md).

## Non-Block Userspace Recipes

These runs remove the kernel block request lifecycle from the hot path.

Target n2 with `zcnc`:

```bash
./target/release/zcnc listen \
  --bind 0.0.0.0 \
  --port 25000 \
  --connections 64 \
  --expected-bytes 256G \
  --workers 64 \
  --recv-bytes 64k \
  --ring-entries 4096 \
  --zero-copy-receive auto
```

Client n1:

```bash
./target/release/zcnc connect \
  --peer-addr 172.31.12.58 \
  --port 25000 \
  --connections 64 \
  --bytes-per-connection 4G \
  --chunk-bytes 64k \
  --pipeline 128 \
  --workers 64 \
  --ring-entries 4096 \
  --zero-copy-send auto
```

Local descriptor/RAID logical-record comparison:

```bash
./target/release/zcraidd wal-bench \
  --shape 'stripe(64,mirror(2))' \
  --bytes 24g \
  --record-bytes 4k \
  --segment-bytes 384k \
  --scheduler wave \
  --wave-segments 64 \
  --fanin-mode tree \
  --tree-fanout 4 \
  --tree-depth 4 \
  --verify none
```

Local descriptor-only tree simulation:

```bash
./target/release/zcraidd tree-sim \
  --levels 1,4,16,64 \
  --bytes 24g \
  --record-bytes 4k \
  --segment-bytes 384k
```

Simple byte-compatible zcraid pipe smoke:

```bash
zccat --generate --bytes 8g --chunk-bytes 1m |
  zcraid-split --mode raid10 --replicas 2 --chunk-bytes 1m \
    --to 'zcsink --consume count' \
    --to 'zcsink --consume count'
```

## Reading Divergence

Expect block and userspace paths to diverge for structural reasons:

- fio 4K block IOPS counts one kernel request per 4K operation. A coalesced WAL
  path can count many logical 4K records inside one userspace frame or one
  physical append.
- blk-mq request allocation, tag accounting, bio setup, queue mapping,
  completion, and wakeups are per block operation unless the path explicitly
  batches above the block layer.
- `/dev/zcnblk0` adds the client block driver, request-to-frame translation,
  connection selection, remote acknowledgements, and block completion semantics.
  `zcnblk-send` removes the fio and blk-mq surface while keeping the zcnblk
  frame protocol.
- `zcnc` and `tcp-bench-uring-mux-*` measure the lane transport without block
  request accounting. Their throughput can be high while their result says
  little about 4K block latency.
- `zcraidd wal-bench` reports logical record IOPS and segment IOPS. The record
  IOPS number is the right application-level WAL comparison, while segment IOPS
  is closer to the physical scheduling pressure.
- If a block device appears in a fanout/tiering test, label it as `edge=source`
  or `edge=sink`. Do not describe it as a mid-tree fanout node unless the code
  is deliberately testing block-layer overhead.

The clean conclusion line for each run should be:

```text
path=<name> physical_ops_per_sec=<n> logical_4k_records_per_sec=<n> MiBps=<n> p99=<n/a-or-value> sys_cpu=<n>
```
