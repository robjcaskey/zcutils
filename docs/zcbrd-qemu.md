# zcbrd + zcstripe + io-slot QEMU Smoke

`zcbrd` is the Rust RAM-backed block device used to keep the old-school block
device path honest while we experiment with descriptor-native zero-copy paths.
It is configured through configfs at `/sys/kernel/config/zcbrd`.
`zcstripe` is the first striped block target in the same lab stack. It is
configured through `/sys/kernel/config/zcstripe` and currently stripes across
explicit lower block devices such as `/dev/zcbrd0,/dev/zcbrd1`.

Use the combined kernel tree when testing slots against `zcbrd`:

- source: `/home/rob/src/linux-7.0.8-zcslots`
- release: `7.0.8-zcslots`
- config: `CONFIG_RUST=y`, `CONFIG_BLK_DEV_ZCBRD=m`,
  `CONFIG_BLK_DEV_ZCSTRIPE=m`, `CONFIG_IO_URING_SLOT_RW=y`

Build the kernel image and required modules:

```sh
make -C /home/rob/src/linux-7.0.8-zcslots \
  RUSTC=/usr/bin/rustc BINDGEN=/usr/bin/bindgen \
  -j"$(nproc)" bzImage

make -C /home/rob/src/linux-7.0.8-zcslots \
  RUSTC=/usr/bin/rustc BINDGEN=/usr/bin/bindgen \
  fs/configfs/configfs.ko \
  drivers/block/zcbrd/zcbrd_mod.ko \
  drivers/block/zcstripe/zcstripe_mod.ko
```

Run the VM smoke:

```sh
LINUX_TREE=/home/rob/src/linux-7.0.8-zcslots qemu-zcrx/zcbrd-qemu-smoke.sh
```

The guest loads `configfs.ko`, loads `zcbrd_mod.ko`, creates
`/sys/kernel/config/zcbrd/zcbrd0`, enables descriptor advertising, powers the
device on, creates `/dev/zcstripe0` over `/dev/zcbrd0,/dev/zcbrd1`, and then
runs topology, normal io_uring writes, slot WAL reads/writes, and the same
registered-slot concurrent write regression against both block targets.

Last passing smoke:

```text
kernel: 7.0.8-zcslots #2
slot api: IORING_OP_SLOT_RW=yes, IORING_REGISTER_IO_SLOT=yes
zcbrd descriptor: version=1 features=0x00000007 queues=4 shards=4
zcstripe descriptor: version=1 features=0x0000000f stripe_unit=4096 targets=/dev/zcbrd0,/dev/zcbrd1
zcbrd slot 4k write: 3772.16 MiB/s, 965673 ops/s
zcbrd slot 64k write: 19133.93 MiB/s, 306143 ops/s
zcbrd slot 64k read: 19465.43 MiB/s, 311447 ops/s
zcbrd same-slot concurrent 4k write: ok, 64 inflight, 4450.96 MiB/s, 1139445 ops/s
zcstripe fixed-file 4k write: 3841.98 MiB/s, 983547 ops/s
zcstripe slot 4k write: 5408.48 MiB/s, 1384571 ops/s
zcstripe slot 4k read: 4771.16 MiB/s, 1221418 ops/s
zcstripe same-slot concurrent 4k write: ok, 32 inflight, 4813.14 MiB/s, 1232163 ops/s
log: qemu-zcrx/zcbrd-qemu-smoke-1780021394.log
```

The same-slot regression specifically covers the inline-bio-to-overflow-bio
path for one registered IO slot. Overflow completions must report the successful
byte count, not `0`, or userspace sees short successful writes.

`zcstripe` uses a Rust configfs/blk-mq owner with a narrow C helper for lower
block-device open and bio submission. Lower bios are asynchronous; the original
request is completed through the Rust blk-mq complete callback. The first
synchronous helper used `submit_bio_wait()` in `queue_rq` and stalled RCU during
the guest smoke, so that shape is intentionally gone.

The important distinction for later benchmarking:

- `zcbrd` gives us block compatibility and kernel-visible descriptor ABI shape.
- `zcstripe` proves a descriptor-advertising striped block target can sit under
  the io-slot path today.
- A descriptor-native userspace daemon can avoid pretending everything is a
  block request and is the cleaner comparison for a true zero-copy data plane.
