# c8gn.48xlarge regression smoke, 2026-08-08

Run ID: `zc-brief-adhoc-c8gn48-20260808T124019Z`

Two `c8gn.48xlarge` adhoc instances in `us-east-2c` were used. The client and
leaf each had two EFA-capable 300 Gbit/s NICs. Bulk traffic used private card1
addresses and NUMA-1 CPUs; PostgreSQL used private card0 addresses and NUMA-0
CPUs. Both instances and both temporary Elastic IPs were explicitly removed
after the artifacts were collected.

## Results

| workload | topology and completion contract | result |
| --- | --- | --- |
| random 4 KiB 50/50 read/write | `/dev/zcnblk0` -> 8 userspace WAL/TCP lanes -> remote userspace `zcmem`; 8 workers, QD 128 each, aggregate QD 1024; writes acknowledged from the bounded local dirty cache; no syncs | 1,279,942 IOPS, 39.06 Gibit/s, 775 us sampled mean, 729 us p50, 2 ms p99 |
| PostgreSQL `pgbench` | ext4 on `/dev/zcnblk0` -> 2 userspace WAL/TCP lanes -> remote userspace `zcmem`; scale 10, 32 clients, 4 jobs, 5 s; synchronous commits | 8,046.86 TPS, 3.840 ms mean transaction latency, 40,400 transactions, 23,973 remote sync epochs |
| linear WAL stream, first pass | one private NIC, 96 lanes, 384 KiB extents, io_uring, no acknowledgements, 16 MiB socket buffers | sender 152.92 Gbit/s / 4.67M logical 4 KiB records/s; receiver 146.77 Gbit/s / 4.48M records/s; 510,086 receiver voluntary context switches |
| linear WAL stream, hot-poll retest | one private NIC, 64 lanes, 1 MiB sends, 4 MiB receives, 128 MiB socket buffers, CQ hot polling, no acknowledgements | sender 202.43 Gbit/s / 6.18M records/s; receiver interval 157.07 Gbit/s / 4.79M records/s; 2,342 receiver voluntary context switches; receive workers saturated their CPUs |
| linear WAL stream, blocking retest | same 64-lane framing without CQ hot polling | sender 193.09 Gbit/s / 5.89M records/s; receiver interval 150.48 Gbit/s / 4.59M records/s; 624,869 receiver voluntary context switches |

The receiver interval begins while lane connections are still being established,
so sender and receiver aggregate rates use different wall intervals. Keep both
endpoint values when comparing stream runs.

## Interpretation

The random workload is representative of high aggregate depth, not low-QD
latency. Its client benchmark threads caused 162,191 context switches
(20.274/1K I/O), while the userspace target and kernel path caused only 1,391
and 577 respectively. Read misses reached the remote leaf; dirty-cache hits
satisfied 743,955 reads by shared-slot reference.

The PostgreSQL result exercises durability: the target observed 276,327 writes,
293 reads, and 23,973 syncs. Vector-HWM ordering was enabled and every logical
sync became a remote sync epoch in this run.

The linear stream path remains below the documented 218.4 Gbit/s one-card
baseline. Larger framing and socket buffers recovered much of the initial
regression. CQ hot polling removed almost all voluntary switches but consumed
the dedicated receive cores and did not eliminate lane startup skew.

These brief runs are not all shape-identical to the older gates. The closest
integrated cloud random-RW results were 1.529M-1.538M IOPS with 32 workers and
aggregate QD 4096; this run used 8 workers and aggregate QD 1024, so its 1.280M
IOPS is a warning rather than proof of a code regression. The earlier direct
target PostgreSQL run reached 40.76K TPS with scale 100, 64 clients, and 64
transport lanes. This run deliberately exercised the newer two-lane WAL/vector
HWM path at scale 10 and 32 clients, so the 8.05K TPS result likewise requires
an exact-shape A/B before attribution.
