# zcnblk-fan local targeted benchmark

Date: 2026-06-01 UTC

Topology:
- Host: local loopback, 16 physical cores / 32 SMT threads.
- Direct path: `zcnblk-send -> zcnblk-target zcdevnull0`.
- Fan path: `zcnblk-send -> zcnblk-fan -> two zcnblk-target zcdevnull0 leaves`.
- CPU maps: direct target `0-15`, direct sender `16-31`; fan leaf0 `0-7`, leaf1 `8-15`, fan handlers `16-23`, sender `24-31`.
- Local smoke constraints: `hugepages=0`, `RLIMIT_MEMLOCK=8192 KB`, `small-pages`, loopback TCP.

4K lane sweep, client-observed `zcnblk-send-summary` frame/s:

| lanes | direct write IOPS | direct read IOPS | fan write IOPS | fan read IOPS |
|---:|---:|---:|---:|---:|
| 1 | 290283 | 122591 | 63105 | 50456 |
| 2 | 717754 | 283400 | 119291 | 95903 |
| 4 | 1324854 | 542322 | 237494 | 187948 |
| 8 | 1947241 | 909548 | 425042 | 340789 |
| 16 | 2534844 | 2167068 | 484084 | 439456 |
| 32 | 2009992 | 1725282 | 476796 | 408577 |

16-lane chunk/stripe sweep:

| case | write frame/s | write GiB/s | write 4K-eq IOPS | read frame/s | read GiB/s | read 4K-eq IOPS |
|---|---:|---:|---:|---:|---:|---:|
| 4K-4K | 475453 | 1.814 | 475529 | 450065 | 1.717 | 450101 |
| 16K-16K | 327402 | 4.996 | 1309671 | 312882 | 4.774 | 1251475 |
| 64K-64K | 112747 | 6.882 | 1804075 | 129245 | 7.888 | 2067792 |
| 64K-4K | 41860 | 2.555 | 669778 | 47610 | 2.906 | 761790 |
| 1M-1M | 3229 | 3.153 | 826540 | 4198 | 4.100 | 1074790 |

Initial conclusion:
- Direct zcnblk userspace target is healthy locally; it reaches ~2.5M write IOPS and ~2.2M read IOPS at 16 lanes.
- Current `zcnblk-fan` is capped around ~0.48M write IOPS and ~0.45M read IOPS for 4K frames on this host.
- Larger unsplit stripes improve byte throughput, so raw byte movement is not the only limiter.
- 64K frames over 4K stripes are much slower than 64K frames over 64K stripes, which points to synchronous per-segment leaf request/ACK/response handling inside the fan.
- Next code work should pipeline per-leaf requests and preserve same-sector order with completion queues, instead of waiting for each leaf segment before issuing the next segment/frame.
