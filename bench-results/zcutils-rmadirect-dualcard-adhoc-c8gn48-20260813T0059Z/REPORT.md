# `/dev/zcnblk0` EFA-direct high-IOPS report

Run ID: `zcutils-rmadirect-dualcard-adhoc-c8gn48-20260813T0059Z`

## Outcome

The fastest repeated, representative result through the real client block edge was **5,063,508 4 KiB random-read IOPS** (mean of three runs, 0.24% spread) on one EFA card. The three results were 5,057,314, 5,063,569, and 5,069,642 IOPS. The target independently measured 5,061,064 active IOPS and 154.45 Gibit/s of read payload.

The path was:

`zcblockbench -> /dev/zcnblk0 -> kernel blk-mq/shared-memory queues -> separate userspace WAL/placement stage -> EFA-direct RMA -> remote zcmem leaf`

This is not a null-device or bypass result. Every read traversed `/dev/zcnblk0` and completed only after the initiator's OFI CQ reported the remotely sourced data visible in its request-owned shared slot. The remote leaf was registered volatile memory (`zcmem:4G`), not persistent media; these numbers therefore measure the block edge, userspace stage, EFA transport, and remote-memory path rather than SSD durability.

The best short single-card point was **5,138,866 IOPS**, but the repeated 5,063,508 result is the number to use.

Using both cards did not improve the single logical device. The final repeated dual-card lane-inline run averaged **2,306,687 IOPS** (4.17% spread). The topology-aligned stable-owner userspace stripe completed without transport/protocol errors, kept placement out of the kernel, and peaked at **2,020,292 IOPS** in a short tuning run. These dual-card numbers are lower than one card, so they are not presented as wins.

## Representative block results

| Path | Workers / lanes | Per-worker QD | Aggregate depth | IOPS | Repeats / spread | Status |
|---|---:|---:|---:|---:|---:|---|
| Untuned same-instance starting baseline, card 0 | 16 / 16 | 256 | 4,096 | 2,830,870 mean | 3 / 0.25% | Representative |
| Tuned card 0 | 16 / 16 | 512 | 8,192 | **5,063,508 mean** | 3 / 0.24% | Representative |
| Tuned card 0 short peak | 16 / 16 | 512 | 8,192 | 5,138,866 | 1 | Exploratory |
| Both cards, lane-inline | 32 / 32 | 256 | 8,192 | 2,306,687 mean | 3 / 4.17% | Representative |
| Both cards, topology-aligned stable-owner stripe | 32 / 32 owners | 512 | 16,384 | 2,020,292 | 1 | Exploratory |
| Both cards, high worker count | 64 / 64 | 256 | 16,384 | 1,400,324 | 1 | Exploratory |
| Both cards, requested maximum-depth point | 64 / 64 | 1,024 | 65,536 | 1,307,418 | 1 | Exploratory |

The tuned repeated single-card result is **78.87% faster** than the same-instance starting baseline. Increasing the same 16-worker path from per-worker QD256 to QD512 helped, while QD1024 fell to 5,026,982 IOPS in the short sweep. Increasing to 32 workers on one card also fell to 4,248,456 IOPS. That is the saturation evidence used to stop the single-card search.

The QD1024/64-worker run required 144 GiB of client HugeTLB pages and unlimited target memlock. It completed honestly, but the 64-way CPU sharing and cross-NUMA shared edge made it slower, not faster.

## Topology

Both hosts were `c8gn.48xlarge` Spot instances in `us-east-2c`, each with 192 physical CPUs, two NUMA nodes, and two active physical EFA network cards. EFA-direct was required (`FI_EFA_USE_DEVICE_RDMA=1` and `URING_PLAY_OFI_EFA_FABRIC=efa-direct`). Strict/fatal topology preflight was enabled before representative output.

For the winning single-card run, lane `n` (0-15) mapped to:

- client worker CPU `6n`
- userspace target CPU `6n+1`
- blk-mq/kthread CPU `6n+2`
- remote leaf CPU `n`
- NUMA node 0 and OFI domain `efa_0-rdm`

For the 32-lane dual-card runs, lane `n` mapped to client/target/kernel/owner/leaf CPUs `6n`, `6n+1`, `6n+2`, `6n+3`, and `6n+4`. Lanes 0-15 used NUMA node/card/domain 0; lanes 16-31 used NUMA node/card/domain 1. The stable-owner stripe kept placement in the separate userspace stage and used 16 owners per card. `/dev/zcnblk0` did not select stripes, mirrors, tiers, or leaves.

Representative runs used HugeTLB-backed shared arenas, pinned workers and kthreads, explicit hctx affinity, normal io_uring submission, no SQPOLL, no IOPOLL, request batch 1, and explicit lane/domain maps. The large 64-lane runs used an opt-in unlimited-memlock wrapper after strict preflight correctly rejected insufficient memlock.

## Dual-card transport ceiling

The raw userspace RMA test is useful only as a transport ceiling; it did **not** traverse `/dev/zcnblk0` and must not be compared as block-device IOPS. At per-worker QD256 it saturated at 32 workers per card:

| Workers per card | Combined raw RMA IOPS | Combined payload rate |
|---:|---:|---:|
| 16 | 10,060,906 | about 329.7 Gbit/s |
| 32 | **15,651,820** | about 512.9 Gbit/s |
| 48 | 15,401,101 | about 504.7 Gbit/s |
| 64 | 15,248,806 | about 499.7 Gbit/s |

Both cards and both NUMA nodes were therefore healthy. The loss is in scaling the one logical block edge/userspace ownership path across NUMA, not in EFA capacity.

## Topology-aligned userspace stripe

The dual-card RAID-like experiment remained a userspace stage after `/dev/zcnblk0`. `WalTcpStableOwnerExtent` assigned stable extents to 32 topology-pinned owners, with owners 0-15 on EFA 0/NUMA 0 and owners 16-31 on EFA 1/NUMA 1. No kernel placement or block-device stripe primitive was added.

An OFI bidirectional progress bug was fixed by preposting a receive before the peer can enter a blocking send. With the current single preposted reply buffer, the stable-owner pipeline must remain one batch per owner; larger pipelines can still block when more than one reply is outstanding. Fragment/fill tuning at QD512 produced:

| Owner fragment records | Fragment fill | Debounce | IOPS |
|---:|---:|---:|---:|
| 16 (starting point) | 500 us | 2 us | 1,801,457 |
| 64 | 20 us | 10 us | 1,883,258 |
| 128 | 20 us | 10 us | 1,947,999 |
| 256 | 20 us | 10 us | **2,020,292** |
| 512 | 20 us | 10 us | 1,939,213 |

Fragment 256 is the measured peak. The next architectural improvement should be a fixed multi-receive reply ring per owner endpoint so multiple batches can remain in flight, followed by per-NUMA userspace ownership/order domains with only the minimum global synchronization needed for flush/FUA. The scaling shape also suggests a shared ordering/cacheline bottleneck, but that needs profiling before a semantic change.

## Historical comparison

- The immediately preceding representative EFA block-read curve on `c8gn.16xlarge` reached 2,369,461 mean IOPS at 16 lanes, per-worker QD16, aggregate depth 256. It used a lane-local RMA bounce buffer followed by a shared-slot copy. The new 5,063,508 result is 113.70% higher, but the instance size and queue depth differ, so this is directional rather than a controlled A/B comparison.
- The controlled comparison is the starting baseline on the same two hosts: 2,830,870 to 5,063,508 IOPS, a 78.87% gain.
- A historical local-memory `/dev/zcnblk0` result averaged 5,263,069 IOPS, but it was a local 50/50 read/write memory-backend workload with four workers, so it is not completion-semantics-equivalent to the remote-read result.
- The 15.65M dual-card raw RMA result is transport-only and is deliberately excluded from logical block-device comparisons.

## Implemented changes

- ABI-v6 opt-in sealed HugeTLB memfd import for the kernel/userspace shared arena, with strict size/seal validation, retained kernel references, and QEMU coverage. The default backing remains unchanged unless requested.
- Direct OFI RMA reads into request-owned shared payload slots; there is no userspace copy after CQ completion.
- Opt-in blk-mq hctx NUMA mapping: default kernel mapping, force one node, or deterministically split queues across nodes. This changes scheduling/locality only; it does not make placement decisions.
- Explicit per-lane OFI domain selection at both target and leaf, allowing deterministic use of `efa_0-rdm` and `efa_1-rdm`.
- Preposted OFI receive progress and message chunking that preserves Rust `Read`/`Write` byte-stream behavior.
- Strict benchmark reporting/preflight for HugeTLB, memlock, hctx/kthread affinity, worker/lane/CPU maps, batching, and EFA-direct state.
- QEMU smoke coverage for ABI-v6 HugeTLB arenas with lane batching both disabled and enabled.

## Validation

- `cargo fmt --all -- --check`: pass
- `cargo check --all-targets`: pass
- `cargo test --lib`: 217 passed, 0 failed, 8 provider/hardware tests ignored
- Ignored libfabric sockets test `vectored_wal_message_round_trips_without_tcp_bridge`: pass
- Clean release build (`QEMU_CARGO_CLEAN=1`) and QEMU ABI-v6 HugeTLB read/write/FUA/sync-order smoke, lane batching 0: pass
- QEMU ABI-v6 HugeTLB read/write/FUA/sync-order smoke, lane batching 1: pass
- Kernel module build inside both QEMU smoke runs: pass
- `git diff --check`, shell syntax checks, and final formatting check: pass

QEMU validates correctness and ABI behavior; it is not a performance result. The high-IOPS AWS workload measured random reads only, so no remotely acknowledged write, early-local-write, or FUA-drain IOPS is inferred from it.

## Artifacts and teardown

- Winning three-run result: `client-host/zcnblk0-tuning/single-card0-q512-latest-rep3/`
- Dual-card repeated result: `client-host/zcnblk0-dual/dual-l32-q256-laneinline-payload-rep3/`
- Tuned stable-owner stripe: `client-host/zcnblk0-dual/dual-l32-q512-stableowner-frag256/`
- 64-worker/QD1024 result: `client-host/zcnblk0-dual/dual-l64-q1024-hugetlb144g/`
- Clean QEMU log: `qemu-hugetlb-lane0-clean.log`
- Lane-batched QEMU log: `qemu-hugetlb-lane1.log`

The instances ran from 00:56:57Z to the user-initiated termination at 02:29:05Z. At the observed `c8gn.48xlarge` Spot price of $1.7573/hour, estimated pair compute cost was about **$5.40**, below the $10 budget (excluding incidental IPv4/data charges).

Instances `i-095adf00773b66b46` and `i-0304bceecbe0c8764` are verified `terminated`. EIP allocations `eipalloc-08f880dc882766ce2` and `eipalloc-088495dfeed177b75` were released and subsequently returned `InvalidAllocationID.NotFound`.
