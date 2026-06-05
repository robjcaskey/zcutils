# zcfanout-logtcp local TCP run

Date: 2026-06-01 UTC

Host topology:
- CPU: AMD Ryzen 9 9950X3D, 32 logical CPUs, 16 cores, 1 NUMA node
- Pinning: `URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-31`
- TCP: `127.0.0.1`, one TCP result-log stream per lane/branch

Bench shape:
- Command: `zcfanout-logtcp-bench`
- Branches: 2
- Payload accounting: 4 KiB logical I/O per emitted result
- Wire record: 32-byte compact result-log descriptor
- Batch: 1024 records for full runs
- Block devices: none
- Sorting/global queue: none
- Payload copy/deep payload inspection: none

Short scale runs used 250,000 records per lane:

| mode | lanes | receiver workers | sender streams | emitted IOPS | TCP descriptor Gbit/s | seconds |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| mirror-write | 1 | 1 | 2 | 59,118,300 | 30.269 | 0.004229 |
| mirror-write | 2 | 2 | 4 | 133,334,151 | 68.267 | 0.003750 |
| mirror-write | 4 | 4 | 8 | 157,342,572 | 80.559 | 0.006356 |
| mirror-write | 8 | 8 | 16 | 240,849,872 | 123.315 | 0.008304 |
| mirror-write | 16 | 16 | 32 | 188,962,904 | 96.749 | 0.021168 |
| mirror-write | 32 | 32 | 64 | 189,483,370 | 97.015 | 0.042220 |
| stripe-read | 1 | 1 | 2 | 61,703,729 | 31.592 | 0.004052 |
| stripe-read | 2 | 2 | 4 | 122,088,823 | 62.509 | 0.004095 |
| stripe-read | 4 | 4 | 8 | 188,218,465 | 96.368 | 0.005313 |
| stripe-read | 8 | 8 | 16 | 260,558,990 | 133.406 | 0.007676 |
| stripe-read | 16 | 16 | 32 | 182,600,701 | 93.492 | 0.021906 |
| stripe-read | 32 | 32 | 64 | 202,772,195 | 103.819 | 0.039453 |
| mirror-read | 32 | 32 | 64 | 198,126,099 | 101.441 | 0.040378 |

Longer runs:

| mode | lanes | records/lane | emitted IOPS | TCP descriptor Gbit/s | seconds |
| --- | ---: | ---: | ---: | ---: | ---: |
| mirror-write | 8 | 2,000,000 | 219,433,653 | 112.350 | 0.072915 |
| stripe-read | 8 | 2,000,000 | 224,461,944 | 114.925 | 0.071282 |
| mirror-write | 32 | 1,000,000 | 206,078,542 | 105.512 | 0.155281 |
| stripe-read | 32 | 1,000,000 | 198,562,089 | 101.664 | 0.161159 |

Interpretation:
- The result-log zipper over real TCP is above 198M logical 4K IOPS in the longer 32-lane runs.
- 8 lanes is cleaner on this host because it uses 16 sender threads plus 8 receiver workers. 16 lanes and 32 lanes oversubscribe the local CPU topology when both ends run in one process on one machine.
- `wire_Gbitps` is compact descriptor-log TCP traffic, not payload block traffic. `logical_4k_iops` is WAL/result-log accounting for 4 KiB operations represented by those descriptors.
- The command now emits a `PERF WARNING` when active sender plus receiver threads exceed online CPUs.
