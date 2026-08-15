# Persistent WAL leaf backing

`zcnblk-wal-leaf` accepts a persistent journal/final-image target in this form:

```text
zcpwal:JOURNAL_PATH,BASE_PATH,LOGICAL_SIZE,JOURNAL_SIZE
```

Both paths must independently name either a regular file or a terminal block
device. They cannot alias the same inode or block-device identity. A block
device is used only as terminal leaf media after userspace placement; it is
never used as a mirror or stripe primitive.

Regular files default to `fallocate` provisioning through the configured size,
followed by a FIEMAP coverage check. This turns ENOSPC into an open-time error
instead of a later write-path failure. Set
`URING_PLAY_ZCNBLK_PWAL_FILE_PROVISIONING=require-allocated` to require an
already provisioned file without changing its allocation. The startup label
reports whether allocation was verified by FIEMAP or, on filesystems without
FIEMAP, by the weaker allocated-block-count fallback.

Block devices are sized with `BLKGETSIZE64`; the WAL never calls `set_len` or
`fallocate` on them. The requested logical/journal size must fit.

Set `URING_PLAY_ZCNBLK_PWAL_IO=direct` to open both backings with `O_DIRECT`.
The frame header, recovery buffers, reducer page, and superblocks are explicitly
4096-byte aligned. Append sources and read destinations are caller-owned, so
direct mode rejects an unaligned address with `InvalidInput`; it never silently
adds a bounce buffer. This removes the buffered page-cache copy but does not
weaken persistence: `sync_data` still drains the frame prefix before the
alternate superblock is published and drained. The default remains `buffered`.

`sync` uses two persistence phases: drain frame headers and payloads, then
publish and drain the alternate durable-tail superblock. Ordinary appends do
not perform either drain. CRC32C payload protection remains the default.
Framing-only mode is refused unless the upstream userspace topology presents a
successful integrity admission during connection setup.

Run the destructive-on-temporary-images QEMU recovery matrix with:

```sh
scripts/zcpwal-qemu-smoke.sh
```

The harness creates private raw images under `target/qemu-zcpwal-smoke`, uses
two `virtio-blk` devices as journal/base terminal leaves, and uses a third ext4
image for regular-file allocation tests. It includes an O_DIRECT aligned-buffer
lifecycle phase, performs real QEMU process kills between persistence phases,
and reboots against the same images.
