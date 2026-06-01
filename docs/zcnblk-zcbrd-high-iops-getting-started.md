# High-IOPS zcnblk to zcbrd Getting Started

This is the cross-machine block IOPS harness for a real RAM-backed target leaf:

```text
fio on client
  -> /dev/zcnblk0
  -> zcnblk client kernel module
  -> 64 TCP lane connections over the target NIC
  -> zcnblk-target userspace service
  -> /dev/zcbrd0 terminal RAM-backed block leaf
```

`zcbrd` is a RAM block device implemented by `kmods/zcbrd_mod.c`. In this
setup it is only the final target leaf behind a userspace `zcnblk-target`.
It is useful because it removes SSD latency while still exercising a real block
target open through io_uring. It must not own mirroring, striping, tiering,
spill, placement, or fanout policy. Those decisions stay in userspace.

## Topology Used

The reference run used two `c8gn.48xlarge` Graviton instances in one AZ.

| Role | Value |
| --- | --- |
| Client block edge | `/dev/zcnblk0` on client host |
| Target userspace service | `zcnblk-target` on target host |
| Target block leaf | `/dev/zcbrd0` on target host |
| Client data IP | `172.31.22.141` on `ens146` |
| Target data IP | `172.31.27.240` on `ens146` |
| Target base port | `23600` |
| TCP lanes | `64` ports, one connection per lane |
| Client blk-mq queues | `192` |
| zcnblk client kthreads | CPUs `128-191` |
| zcnblk target workers | CPUs `128-191` |
| fio workers | CPUs `0-191`, shared |

Do not treat a number as representative unless the run states this mapping.
For this machine class, `queues=192` mattered: `queues=64` left IOPS on the
table even though the wire side still used 64 lanes.

## Build

Run on both hosts:

```bash
sudo apt-get update
sudo apt-get install -y build-essential clang cmake dwarves flex bison \
  ethtool git jq libssl-dev liburing-dev linux-headers-$(uname -r) \
  ninja-build rsync tmux fio

cargo build --release --bins
make -C kmods
```

Reserve huge pages on both hosts. The target command below uses hugetlb buffers.

```bash
sudo sysctl -w vm.nr_hugepages=32768
grep -E 'HugePages_Total|HugePages_Free|Hugepagesize' /proc/meminfo
```

## Target Host

Create the `zcbrd0` terminal leaf on the target host:

```bash
cd /home/ubuntu/zcutils

sudo insmod kmods/zcbrd_mod.ko
sudo mount -t configfs none /sys/kernel/config 2>/dev/null || true
sudo mkdir -p /sys/kernel/config/zcbrd/zcbrd0

echo 8192 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/size_mib
echo 4096 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/blocksize
echo 192 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/queues
echo 1024 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/queue_depth
echo 64 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/shards
echo copy | sudo tee /sys/kernel/config/zcbrd/zcbrd0/data_mode
echo 1 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/power

lsblk /dev/zcbrd0
cat /sys/kernel/config/zcbrd/zcbrd0/data_mode
```

Start `zcnblk-target` on the target data IP. Run it as root so it can open the
target block leaf and lock the hugetlb buffers.

```bash
cd /home/ubuntu/zcutils

TARGET_LISTEN_IP=172.31.27.240
BASE=23600

sudo env \
  URING_PLAY_ZCNBLK_ENCRYPTION=none \
  URING_PLAY_TCP_WAL_WRITE_MODE=fixed-file \
  URING_PLAY_ZCNBLK_READ_MODE=fixed-file \
  URING_PLAY_ZCNBLK_SUBMIT_BATCH=64 \
  URING_PLAY_ZCNBLK_WRITE_ACKS=1 \
  URING_PLAY_ENTER_NO_IOWAIT=1 \
  URING_PLAY_CQE_SPIN=64 \
  URING_PLAY_PIN_CPUS=1 \
  URING_PLAY_PIN_CPU_LIST=128-191 \
  ./target/release/zcutils zcnblk-target \
    /dev/zcbrd0 "$TARGET_LISTEN_IP" "$BASE" \
    64 1 2048G 4K 256 64 4096 hugetlb true
```

Expected target plan line:

```text
zcnblk-target: targets=0:/dev/zcbrd0:8589934592B:align4096 ...
  ports=64 connections_per_port=1 total_connections=64
  chunk_bytes=4096 pipeline=256 workers=64 ring_entries=4096
  buffer_mode=hugetlb write_mode=fixed-file read_mode=fixed-file
  shard_policy=port-lane pin_workers=true
```

The security group or firewall must allow TCP `23600-23663` from the client
private IP.

## Client Host

Load `/dev/zcnblk0` on the client. Use the target's private data IP, not the
public IP.

```bash
cd /home/ubuntu/zcutils

TARGET_PRIV_IP=172.31.27.240
BASE=23600

sudo insmod kmods/zcnblk_client_mod.ko \
  remote_ip="$TARGET_PRIV_IP" \
  remote_port_base="$BASE" \
  lanes=64 \
  connections_per_lane=1 \
  shard_count=1 \
  size_mib=8192 \
  logical_block_size=4096 \
  max_frame_bytes=4096 \
  queues=192 \
  queue_depth=256 \
  pipeline_depth=128 \
  fill_timeout_ms=0 \
  batch_depth=1 \
  write_acks=1 \
  hctx_affinity=1 \
  pin_threads=1 \
  pin_base_cpu=128 \
  pin_cpu_count=64 \
  pin_stride=1

lsblk /dev/zcnblk0
ps -eLo pid,tid,psr,comm | awk '/zcnblk/ {print}'
```

Important settings:

| Setting | Why it matters |
| --- | --- |
| `shard_count=1` | Keeps `/dev/zcnblk0` as the client block edge only. |
| `lanes=64 connections_per_lane=1` | One TCP connection per target lane. |
| `queues=192` | Gives blk-mq enough hardware queues for the 192-vCPU client. |
| `hctx_affinity=1` | Maps blk-mq queues directly onto target connections. |
| `pin_threads=1 pin_base_cpu=128 pin_cpu_count=64` | Pins zcnblk lane kthreads to the NIC-local CPU range. |
| `batch_depth=1` | Best observed 4K random shape for this point-to-point path. |
| `write_acks=1` | Writes complete only after target acknowledgement. |

If any of huge pages, memlock headroom, worker pinning, kthread pinning,
`hctx_affinity`, or explicit lane-to-CPU mapping is missing, stop and fix the
setup before believing the IOPS result.

## fio Runs

Read:

```bash
sudo fio \
  --name=zcnblk-zcbrd-randread \
  --filename=/dev/zcnblk0 \
  --rw=randread \
  --bs=4k \
  --iodepth=128 \
  --numjobs=96 \
  --ioengine=io_uring \
  --direct=1 \
  --time_based=1 \
  --runtime=5 \
  --group_reporting=1 \
  --randrepeat=0 \
  --norandommap=1 \
  --cpus_allowed=0-191 \
  --cpus_allowed_policy=shared
```

Acknowledged write:

```bash
sudo fio \
  --name=zcnblk-zcbrd-randwrite \
  --filename=/dev/zcnblk0 \
  --rw=randwrite \
  --bs=4k \
  --iodepth=128 \
  --numjobs=96 \
  --ioengine=io_uring \
  --direct=1 \
  --time_based=1 \
  --runtime=5 \
  --group_reporting=1 \
  --randrepeat=0 \
  --norandommap=1 \
  --cpus_allowed=0-191 \
  --cpus_allowed_policy=shared
```

## Reference Numbers

These are cross-machine numbers for the topology above:

| Target leaf | fio mode | Runtime | IOPS | Bandwidth |
| --- | --- | ---: | ---: | ---: |
| `/dev/zcbrd0` | `randread` | 5 s | `7.255M` | `27.7 GiB/s` |
| `/dev/zcbrd0` | `randwrite`, acknowledged | 5 s | `5.497M` | `21.0 GiB/s` |

The read result used the command shape above and is the quick check that the
client queueing, TCP lanes, target workers, and zcbrd leaf are aligned. Use
longer runtimes for acceptance testing, but keep the same topology notes in the
result record.

## Cleanup

On the client:

```bash
sudo rmmod zcnblk_client_mod
```

On the target:

```bash
# Stop the foreground zcnblk-target with Ctrl-C, or stop its tmux/systemd unit.
echo 0 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/power
sudo rmmod zcbrd_mod
```
