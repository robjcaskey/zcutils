# i8g single-terminal-leaf fio benchmark

## Scope and completion semantics

Two `i8g.48xlarge` Spot hosts in `us-east-2c` were measured independently. Each host used exactly one 3.4-TB local instance-store NVMe device as a terminal leaf, through one 256-GiB preallocated ext4 file. No mirror, stripe, spill, tier, device mapper, or kernel placement primitive was used.

Direct reads complete when data is visible to fio. Direct writes in the low-QD and saturation tables have no durability barrier. The sync table uses the synchronous fio engine and follows every direct 4-KiB write with `fsync(2)`. This is ephemeral local PCIe instance storage: an fsync completion is a local barrier completion, not remote persistence, and instance loss destroys the leaf. Network transport RTT, network-RTT IOPS ceiling, and remote acknowledgement semantics are not applicable.

Runs are shared-system cloud measurements. There are six samples per point (three repeats on each host); ranges below expose host and repeat spread.

## Low-QD direct read

One lane and one worker were pinned to the NVMe-local NUMA node, so per-worker QD equals aggregate outstanding depth.

| QD | Mean IOPS | IOPS range | Mean latency | p99 completion | Matching QD/lat ceiling | Efficiency |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 13,581 | 12,879–14,272 | 73.59 us | 82.94 us | 13,625 | 99.68% |
| 2 | 27,041 | 25,663–28,431 | 73.91 us | 83.11 us | 27,126 | 99.69% |
| 4 | 53,643 | 51,048–56,199 | 74.49 us | 85.50 us | 53,817 | 99.68% |
| 8 | 104,960 | 100,134–109,658 | 76.06 us | 100.18 us | 105,381 | 99.60% |
| 16 | 197,872 | 190,145–205,456 | 80.40 us | 118.27 us | 199,303 | 99.28% |

## Low-QD direct write

One lane and one worker were pinned to the NVMe-local NUMA node, so per-worker QD equals aggregate outstanding depth.

| QD | Mean IOPS | IOPS range | Mean latency | p99 completion | Matching QD/lat ceiling | Efficiency |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 61,899 | 53,612–70,178 | 16.20 us | 21.32 us | 62,829 | 98.54% |
| 2 | 112,466 | 100,257–124,208 | 17.76 us | 26.24 us | 113,855 | 98.80% |
| 4 | 187,891 | 186,441–188,627 | 20.52 us | 102.91 us | 194,903 | 96.41% |
| 8 | 268,988 | 250,621–280,944 | 28.20 us | 121.17 us | 283,879 | 94.71% |
| 16 | 329,911 | 300,510–359,936 | 42.35 us | 136.53 us | 377,777 | 87.31% |

## Very-high-QD single-leaf saturation

Thirty-two lanes/workers were pinned within the leaf's NUMA node, with per-worker QD64 and aggregate outstanding depth 2,048. Each worker owned a disjoint 8-GiB region.

| Operation | Mean IOPS | IOPS range | Payload rate | Mean latency | Efficiency vs 2048/lat |
|---|---:|---:|---:|---:|---:|
| read | 649,459 | 647,258–653,840 | 21.29 Gbit/s | 2,908.61 us | 92.24% |
| write | 330,509 | 330,503–330,514 | 10.84 Gbit/s | 5,755.83 us | 92.89% |

## Per-write local fsync drain

One worker, one lane, per-worker QD1, aggregate outstanding depth 1.

| Fsync-completed cycles/s | Range | Write latency | fsync drain latency | Serial cycle time |
|---:|---:|---:|---:|---:|
| 63,003 | 56,717–69,451 | 14.56 us | 1.04 us | 15.87 us |

## Cost and teardown

Both instances are confirmed `terminated`. The measured lease window was 18.5 minutes at $1.6474/host-hour. Estimated compute, public IPv4, and root-volume cost is about $1.02, below the $3.23 hard launch ceiling.


## Validation

- Accepted 60 low-QD, 12 saturation, and 6 fsync samples; all fio job errors were zero.
- Strict preflight required 1,024 HugeTLB pages, at least 1 GiB memlock headroom, io_uring fixed buffers, registered files, NUMA pinning, and an explicit worker-to-hctx mapping before numbers were printed.
- An initial precondition attempt was rejected on both hosts because fio's io_uring `end_fsync` returned `EINVAL` on this kernel/ext4/device combination. The accepted matrix keeps io_uring for non-barrier direct I/O and measures barriers separately with the synchronous engine and `fsync(2)`.
- Full per-host topology maps and raw fio JSON are stored beside this report.
