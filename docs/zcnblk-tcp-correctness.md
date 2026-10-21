# Direct TCP client correctness checks

The kernel client caches its validated transport at initialization. Shared-memory
admission and ordering epochs apply only to SHM; a TCP connection has no SHM
daemon. An absent SHM daemon still fails read/write requests with EIO.

TCP v2 does not carry a per-write FUA flag. The client therefore advertises a
write cache, without native FUA, for TCP. Linux supplies a postflush for FUA
writes, including ext4 journal commits. SHM retains native FUA and its userspace
WAL contract. Both source copies (kmods and the CSI chart) must stay identical.

With `write_acks=0`, completion means local transmission, not remote application.
A SYNC on one connection cannot fence unapplied writes on other connections.
Until a cross-connection fence exists, TCP flushes with more than one connection
and early write acknowledgement fail with EIO. EOPNOTSUPP is insufficient: raw
block fsync can turn an unsupported flush into success. Ordinary volatile writes
remain available; use `write_acks=1` when requesting flushes with multiple TCP
connections. This guard does not establish durability across independent target
processes or leaf media: userspace must own that ordering and persistence.

Run the integration tests as root **inside an isolated Linux VM** with a module
built for that VM's kernel. They refuse an existing module/device and use new
userspace RAM leaves only. Never run these on a host with an active client.

```sh
python3 scripts/test-zcnblk-client-transport.py --module /path/client.ko \
  --target /path/zcnblk-portable-target --out /tmp/transport-check
python3 scripts/test-zcnblk-tcp-sync.py --module /path/client.ko \
  --out /tmp/tcp-sync-check
python3 scripts/test-zcnblk-tcp-sync.py --module /path/client.ko \
  --early-ack-multiple --out /tmp/tcp-early-ack-check
```

The first checks SHM fail-closed admission and TCP CRC write/read plus fsync. The
second withholds SYNC ACK to check raw RWF_DSYNC completion, then formats the
owned 64 MiB RAM target as ext4 and requires a post-write SYNC before O_DSYNC
returns. It also checks file readback after unmount/remount. The third holds a
received write unapplied on one connection and requires fsync on the other to
fail visibly. e2fsprogs and mount utilities are required for the filesystem test.

On Ubuntu ARM64 kernel 7.0.0-38, the old capability advertisement failed the ext4
journal postflush check even though raw RWF_DSYNC passed. With two early-ACK
connections, fsync also returned success while a write was held unapplied. The
corrected module passes both checks and the transport admission regression.
These fixtures test protocol ordering and completion semantics, not crash
persistence, physical storage performance, RDMA, or all multi-target topologies.
