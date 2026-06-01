# zcbrd/zcwalblk C Module Lab Notes

These out-of-tree modules provide C equivalents of the `zcbrd_mod` and
`zcwalblk_mod` prototypes so they can run on kernels that have the io-slot API
but were not built with `CONFIG_RUST=y`. `zcwalblk_mod` is a thicker block
facade over generated WAL/composite descriptors; it is intentionally separate
from `zcbrd`, which remains a plain RAM block device.

`zcbrd` is an edge/backing tool. It gives block-speaking programs and fio a
RAM-backed surface, and it gives a userspace target a convenient local source
or sink when testing. It is not intended to own mux/demux fanout, fanin, RAID
policy, forwarding, tier spill decisions, backpressure, or descriptor lane
scheduling.

The SAN fabric client block device is `/dev/zcnblk0`. Target-side topology
should be userspace: mirroring, striping, tier spill, WAL, and snapshot
composition do not belong in custom block modules. Use `zctee` for mirror/RAID1
fanout and userspace split/fanplan primitives for stripe/RAID0 fanout.

Build against the running kernel:

```sh
make -C kmods
```

On Secure Boot systems, sign the modules with an enrolled MOK before loading:

```sh
sudo /usr/src/linux-headers-$(uname -r)/scripts/sign-file sha256 \
  /root/mok/MOK.priv /root/mok/MOK.pem kmods/zcbrd_mod.ko
sudo /usr/src/linux-headers-$(uname -r)/scripts/sign-file sha256 \
  /root/mok/MOK.priv /root/mok/MOK.pem kmods/zcwalblk_mod.ko
```

Load and create a pair of RAM block devices:

```sh
sudo insmod kmods/zcbrd_mod.ko
sudo mkdir /sys/kernel/config/zcbrd/zcbrd0
echo 256 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/size_mib
echo 4096 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/blocksize
echo 8 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/queues
echo 512 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/queue_depth
echo advertise | sudo tee /sys/kernel/config/zcbrd/zcbrd0/descriptor_mode
echo 1 | sudo tee /sys/kernel/config/zcbrd/zcbrd0/power
```

Repeat with `zcbrd1` only for local lab tests that need multiple RAM-backed
media devices.

These modules expose `descriptor_abi` through configfs and use blk-mq, so the
io-slot path accepts them on the `7.0.8-io-slots` kernel.

Create a WAL/composite descriptor block facade:

```sh
sudo insmod kmods/zcwalblk_mod.ko
sudo mkdir /sys/kernel/config/zcwalblk/zcwalblk0
echo 1024 | sudo tee /sys/kernel/config/zcwalblk/zcwalblk0/size_mib
echo 4096 | sudo tee /sys/kernel/config/zcwalblk/zcwalblk0/blocksize
echo 4096 | sudo tee /sys/kernel/config/zcwalblk/zcwalblk0/record_bytes
echo 393216 | sudo tee /sys/kernel/config/zcwalblk/zcwalblk0/extent_bytes
echo 32 | sudo tee /sys/kernel/config/zcwalblk/zcwalblk0/queues
echo 2048 | sudo tee /sys/kernel/config/zcwalblk/zcwalblk0/queue_depth
echo 32 | sudo tee /sys/kernel/config/zcwalblk/zcwalblk0/lanes
echo reject | sudo tee /sys/kernel/config/zcwalblk/zcwalblk0/write_mode
echo 1 | sudo tee /sys/kernel/config/zcwalblk/zcwalblk0/power
cat /sys/kernel/config/zcwalblk/zcwalblk0/descriptor_abi
```

`write_mode=reject` is the default and presents a read-only block view over the
descriptor stream. `write_mode=ack` exists only for synthetic write-ack timing;
it does not make the facade durable storage.

`zcwalblk_mod` also registers `/dev/zcwalctl`, a control character device for
batched descriptor commands. The first command is an SQE128 `uring_cmd` batch
resolver: userspace submits a compact WAL-record range description and the
kernel resolves the descriptor records, returning a tiny checksum/result. This
is not a normal block read and it does not fill 4K buffers; it is the fast path
we can layer a descriptor-aware WAL fulfiller or stream fan-in over when the
consumer does not need to round-trip through the block request path for every
logical 4K operation.

Build the direct `uring_cmd` bench:

```sh
cc -O3 -Wall -Wextra -o /tmp/zcwalblk_cmd_bench \
  tools/zcwalblk_cmd_bench.c -luring
```

If the installed liburing is older than the local kernel headers, build against
the local liburing checkout instead:

```sh
cc -O3 -Wall -Wextra -I/home/rob/src/liburing/src/include \
  -o /tmp/zcwalblk_cmd_bench tools/zcwalblk_cmd_bench.c \
  /home/rob/src/liburing/src/liburing.a
```

Run a contiguous 16 MiB logical extent per command, with checksum/result copy:

```sh
sudo /tmp/zcwalblk_cmd_bench \
  --commands 200000 \
  --batch 1 \
  --records-per-item 4096 \
  --stride 4096 \
  --inflight 128 \
  --entries 256
```

Local reference numbers on `7.0.8-io-slots`, 32 CPUs, 2 GiB `zcwalblk_uring`:

```text
fio block read path:
  io_uring/direct/fixedbufs/registerfiles/nonvectored, 32 jobs, iodepth 64
  28.7M 4K randread IOPS, 109 GiB/s, p99 77 usec, p99.9 359 usec

/dev/zcwalctl descriptor batch path:
  batch=1 records_per_item=4096 stride=4096 inflight=128
  273.7M logical 4K IOPS, 1044 GiB/s logical, ~100% of one CPU
  16 independent rings/processes: 4.0B logical 4K IOPS, 15.2 TiB/s logical

batch-only sweep:
  batch=256:   230.8M logical 4K IOPS
  batch=1024:  245.9M logical 4K IOPS
  batch=4096:  252.2M logical 4K IOPS
  batch=16384: 252.4M logical 4K IOPS
```
