# Cost-bounded mirror regression isolation

Run date: 2026-08-15. Two Spot `c8gn.16xlarge` nodes in `us-east-2c`, one EFA device per node, 64 Neoverse-V2 CPUs and 128 GiB RAM per node. Spot price at launch was $0.6643/node-hour. The lease was bounded by automatic termination at 20:20 UTC.

## Result

The prior approximately 10K logical-4K-record/s result was not a TCP or EFA regression. It was dominated by baseline root gp3 persistence. Short isolation probes found:

| Case | Completion semantics | Result | Interval |
|---|---|---:|---:|
| Native EFA RMA to registered remote memory | delivery-complete CQ, remote-visible | 337,809 64K ops/s; 177.1 Gbit/s | 0.194 s |
| Two-branch TCP, topology-matched | no ACK; transport ceiling only | 3.637M logical 4K records/s; 238.9 aggregate branch Gbit/s | 0.072 s |
| Two-host userspace `zcmem`, ACK every 64 extents | both volatile branch HWMs | 4.210M logical 4K records/s | 0.062 s |
| Two-host userspace `zcmem`, sync HWM at 1,024 extents | normal write contract is local-admit; final both-branch volatile HWM | 2.014M logical 4K records/s | 0.130 s |
| Small root-gp3 conformance | both persistent journal HWMs | 33,659 logical 4K records/s | 0.122 s |

The RAM/TCP intervals are deliberately short cost-conscious probes, not representative sustained claims. They are sufficient to place the regression boundary: removing EBS restores multi-million-record/s execution, while native EFA reaches 99.23% of its matching aggregate-QD16 RTT ceiling. The EFA run used 16 workers/lanes, per-worker QD1, aggregate outstanding depth 16, mean delivery-complete RTT 47.001 us, and explicit lane/CPU/domain mappings.

The gp3 case transferred only 16 MiB logical data and exists solely as a persistence/conformance check. Both branch base files hashed to `3b6a07d0d404fab4e23b6d34bc6696a6a312dd92821332385e5af7c01c421351`. It must not be compared as a sustained throughput result. A larger gp3 run was stopped and excluded when its storage bottleneck became obvious.

## Deferred-sync deadlock found and fixed

The initial RAM-backed 1,024-extent sync window timed out. Storage was not involved. The sender capped a vectored transport batch to 512 extents (`IOV_MAX / 2`) and then waited for an ACK, while receivers correctly waited for all 1,024 extents before publishing the sync HWM.

The fix preserves the 1,024-extent durability window, emits it as multiple at-most-512-extent `writev` chunks, and waits only after the full durability window is sent. The same RAM topology then completed at 2.014M logical 4K records/s.

## Semantic boundary

`zcmem` is a userspace terminal backend, not a RAM block device and not a block mirror primitive. Its ACKs are explicitly labeled volatile. A real fsync/FUA may acknowledge a durable linear journal HWM without replaying random writes into the base image, provided crash recovery, look-aside reads, journal capacity/backpressure, and the selected failure-domain policy are enforced.

The tested `zcraid-mirror` path currently waits for both branch HWMs. This run does not claim that one-of-N persistent race-winner commitment is implemented in that path. It isolates the performance available to that future policy and confirms that final-base random I/O does not belong in the foreground journal admission path.

## QEMU scope

The separate three-VM KVM harness passed with userspace placement, independent virtio-blk/ext4 terminal leaves, splice/tee relay, zero reported userspace payload-copy bytes, restart recovery, and lagging-suffix replay. It explicitly reports `representative_benchmark=false`; QEMU is useful for topology and recovery conformance, not EFA/NVMe performance.

## Topology

- Sender workers 0-15 were pinned to CPUs 0-15.
- Local branch workers 0-15 were pinned to CPUs 16-31.
- Remote branch workers 0-15 were pinned to CPUs 32-47.
- EFA domain was explicitly `efa_0-rdm`; hugepage pool had 7,041 pages; fresh benchmark shells had unlimited memlock; irqbalance was stopped; adaptive RX and RX/TX coalescing were disabled.
- Userspace owned mirror placement. No block device, dm, md, loop, ramdisk, nullblk, or custom block module was used as a mirror or stripe primitive.

