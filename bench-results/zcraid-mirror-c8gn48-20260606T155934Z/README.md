# c8gn TCP Userspace Mirror Experiments

Run ID: `zc-arch-adhoc-c8gn48-20260606T155103Z`

Topology:
- node0 sender: `172.31.37.217` on `ens68`, `172.31.43.35` on `ens146`
- node1 receiver: `172.31.46.6` on `ens68`, `172.31.47.66` on `ens146`
- branch 0: node0 `172.31.37.217` -> node1 `172.31.46.6`
- branch 1: node0 `172.31.43.35` -> node1 `172.31.47.66`
- source binding used `URING_PLAY_RAID_MIRROR_TCP_SOURCE_IPS=172.31.37.217,172.31.43.35`
- plans explicitly set `block_device_raid_primitive=false`; no block device was used as a mirror primitive

Best committed mirror result:

| variant | lanes | extent | ACK | sender logical IOPS | sender branch wire | receiver branch wire | sender voluntary CS | ACK p50 |
| --- | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| `tcp384k-w32-srcbind` | 32 | 384K | window 32 | 3.44M | 225.8 Gbit/s | 113.3/115.2 Gbit/s | 31,955 | 10 ms |
| `tcp4k-w32-srcbind` | 32 | 4K | window 32 | 1.71M | 115.6 Gbit/s | 58.1/59.8 Gbit/s | 56,729 | 436 us |
| `tcp1m-w32-srcbind` | 32 | 1M | window 32 | 2.85M | 186.5 Gbit/s | 93.9/93.6 Gbit/s | 17,418 | 26 ms |
| `tcp384k-w64-srcbind` | 64 | 384K | window 32 | 2.72M | 178.6 Gbit/s | 91.1/93.1 Gbit/s | 28,647 | 14 ms |
| `tcp384k-w32-mixedsender2` | 32 | 384K | window 32 | 2.40M | 157.2 Gbit/s | 78.8/80.0 Gbit/s | 33,838 | 11 ms |

Controls:

| variant | result |
| --- | --- |
| `tcp384k-noack-srcbind` | sender reported 255.5 Gbit/s, but receivers only drained 77.4/78.8 Gbit/s; ACK-off sender timing is not representative because it can exit after queueing into socket buffers. |
| `zcwal-parallel-384k-ack` | two branch-local WAL streams reached 124.3 Gbit/s on branch 0 but only 79.3 Gbit/s on branch 1 when run together. |
| `zcwal-card1-alone-384k-ack` | card1 alone reached 125.2 Gbit/s, so the parallel two-branch drop is aggregate contention, not a bad card1 route. |

Conclusion:

The right shape is not per-4K RPC and not larger-than-384K extents. The best point found here is 32 lanes with 384K ordered WAL extents and HWM ACKs. Source binding was required; without branch-local source IPs Linux routes both same-subnet destinations over `ens68`.

The remaining bottleneck is architectural. The current mirror sender is still a blocking loop where each lane worker writes branch 0 then branch 1 and waits for branch ACKs. Increasing lanes, using 1M extents, disabling ACKs, or mixing sender NUMA CPUs did not improve committed throughput. A real next step should keep the 384K-ish WAL coalescing, but replace the sender with a branch-aware, paced bulk pipeline: per-branch/per-NIC workers, explicit HWM result zipping, receiver-side measured completion, and no unbounded ACK-off flooding.
