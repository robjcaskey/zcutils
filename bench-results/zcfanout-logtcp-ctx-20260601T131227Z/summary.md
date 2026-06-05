# zcfanout-logtcp context-switch run

Date: 2026-06-01 UTC

Host topology:
- AMD Ryzen 9 9950X3D, 32 logical CPUs, 16 cores, 1 NUMA node
- Pinning: `URING_PLAY_PIN_CPUS=1 URING_PLAY_PIN_CPU_LIST=0-31`
- Transport: real `127.0.0.1` TCP, one stream per lane/branch
- Block devices: none

Instrumentation:
- `zcfanout-logtcp-bench` now reports per-worker and aggregate thread CPU time.
- It reports voluntary context switches, involuntary context switches, and CPU migrations from `/proc/self/task/<tid>/status` and `/proc/self/task/<tid>/sched`.
- `perf stat` could not be used for software context switches on this host; it forced user-only event accounting and returned zero switches.

Baseline context-counted runs:

| mode | lanes | records/lane | emitted IOPS | descriptor Gbit/s | CPU sec | voluntary cs | involuntary cs | migrations |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| mirror-write | 8 | 2,000,000 | 206,035,013 | 105.490 | 1.172244 | 179 | 82 | 0 |
| stripe-read | 8 | 2,000,000 | 197,609,383 | 101.176 | 1.227041 | 164 | 68 | 0 |
| mirror-write | 32 | 1,000,000 | 202,728,121 | 103.797 | 4.208204 | 423 | 1,826 | 0 |
| stripe-read | 32 | 1,000,000 | 206,570,082 | 105.764 | 4.095234 | 375 | 1,373 | 0 |

The 32-lane local tests emitted the oversubscription warning:

```text
PERF WARNING: zcfanout-logtcp-bench active_threads=96 exceeds online_cpus=32; this topology is locally oversubscribed and should not be treated as representative
```

8-lane batch-size spot check, same 32M result records:

| mode | batch records | emitted IOPS | descriptor Gbit/s | CPU sec | voluntary cs | involuntary cs | migrations |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| mirror-write | 256 | 211,819,468 | 108.452 | 1.113707 | 168 | 117 | 0 |
| mirror-write | 1,024 | 206,035,013 | 105.490 | 1.172244 | 179 | 82 | 0 |
| mirror-write | 8,192 | 207,335,848 | 106.156 | 1.199970 | 289 | 84 | 0 |
| stripe-read | 256 | 220,491,356 | 112.892 | 1.090332 | 161 | 96 | 0 |
| stripe-read | 1,024 | 197,609,383 | 101.176 | 1.227041 | 164 | 68 | 0 |
| stripe-read | 8,192 | 219,825,619 | 112.551 | 1.093005 | 277 | 60 | 0 |

Interpretation:
- Clean 8-lane runs are not context-switch dominated in this descriptor-log shape: roughly 230-285 total context switches over 32M result records.
- Local 32-lane runs keep IOPS high but are not representative because 64 sender streams plus 32 receiver workers oversubscribe 32 logical CPUs; involuntary switches rise sharply.
- Larger batches did not reduce context switches in this loopback TCP shape. The next useful reduction is not bigger batches here; it is avoiding the one-thread-per-branch sender model when testing locally, or moving to true cross-machine lanes where sender and receiver CPU budgets are separate.
