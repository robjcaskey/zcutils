# Remote write-ack / sync-HWM validation

Run ID: `zc-syncack-adhoc-c8gn48-20260808T171804Z`

Two `c8gn.48xlarge` instances ran in `us-east-2c` and the same placement
group. Block/WAL traffic used the private secondary EFA-capable NIC on both
hosts (`ens146`, NUMA node 1). Eight active lanes, their userspace workers,
kernel threads, benchmark workers, leaf workers, and the huge-page-backed
remote `zcmem` arena were pinned to NUMA node 1. The measured 100-packet ping
RTT was 54 us average (45 us minimum).

## Completion contract

- An ordinary block write completes after the userspace target has admitted
  its shared payload lease to the bounded dirty cache.
- The payload continues asynchronously over the userspace WAL/TCP lane.
- A read first consults the dirty cache and otherwise waits for the remote
  leaf response.
- Flush/FUA freezes the per-lane submission HWM and completes only after every
  remote lane acknowledges that boundary.
- The remote test leaf retained readable data in volatile memory. Its explicit
  benchmark-only opt-in was `URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1`;
  process or host loss can lose a sync-acknowledged test image.

The cross-lane order smoke passed old-value-before-write,
new-value-after-write, same-sector ordering, and terminal post-sync readback.
Four logical smoke syncs became four remote epochs and 32 lane sync results.

## Random 4 KiB

The 50/50 deep-queue control used eight workers at QD 128 each (aggregate QD
1024), fixed-buffer io_uring, huge-page buffers, and three repeats:

| min IOPS | mean IOPS | max IOPS | spread |
| ---: | ---: | ---: | ---: |
| 1,370,294 | 1,398,958 | 1,415,948 | 3.26% |

Target context switches were 0.077-0.137 per 1K I/O and kernel switches were
0.046-0.049 per 1K. Client CQ waiting cost 19.94-20.14 switches per 1K I/O.

The mixed low-QD curve uses eight workers. QD is per worker; aggregate depth is
shown explicitly. Read latency includes both dirty hits and remote misses;
write latency is local dirty-cache admission.

| QD/worker | aggregate QD | IOPS | read mean | write mean |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 8 | 107,779 | 91.5 us | 52.1 us |
| 2 | 16 | 201,028 | 98.1 us | 56.8 us |
| 4 | 32 | 380,992 | 104.8 us | 60.0 us |
| 8 | 64 | 569,539 | 132.9 us | 73.7 us |
| 16 | 128 | 595,948 | 208.9 us | 113.3 us |

The read-only curve gives the matching network-RTT theoretical comparison.
Ceiling is `aggregate QD / 54 us`; efficiency is actual divided by that raw
transport ceiling.

| QD/worker | aggregate QD | actual IOPS | RTT ceiling | efficiency | p50 | p99 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 8 | 93,541 | 148,148 | 63.1% | 76 us | 140 us |
| 2 | 16 | 176,026 | 296,296 | 59.4% | 83 us | 145 us |
| 4 | 32 | 273,913 | 592,593 | 46.2% | 96 us | 246 us |
| 8 | 64 | 489,978 | 1,185,185 | 41.3% | 114 us | 262 us |
| 16 | 128 | 691,908 | 2,370,370 | 29.2% | 143 us | 309 us |

## PostgreSQL

`pgbench` used scale 100, 64 clients, eight jobs, synchronous commit, two
vector-HWM lanes, and a 10-second measured interval:

- 77,258 TPS
- 0.825 ms mean transaction latency
- 770,965 transactions
- 69,288 target logical syncs, 69,288 remote sync epochs, and 138,576 expected
  remote lane sync results
- 67,088 PostgreSQL WAL syncs and 12,327.762 ms total WAL sync time, or about
  184 us per WAL sync
- 2.093 storage context switches per 1K transactions

## Bulk WAL transport

A 64-lane one-NIC, 32 GiB stream used 1 MiB sends, 4 MiB receives, io_uring,
and no acknowledgements. The sender reached 213.553 Gbit/s and 6.517M logical
4 KiB records/s, within about 2.2% of the documented 218.4 Gbit/s one-card
baseline. Receiver wall accounting, which includes lane connection skew, was
186.244 Gbit/s and 5.684M records/s with 2,811 context switches total.

Both instances were terminated and both temporary Elastic IPs were released.
