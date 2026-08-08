# c8gn.48xlarge cluster-placement regression run, 2026-08-08

Run ID: `zc-cpg-adhoc-c8gn48-20260808T131500Z`

Two `c8gn.48xlarge` Spot instances ran in `us-east-2c`, subnet
`subnet-c66ddd8b`, and cluster placement group
`up-zc-ramtarget-us-east-2a`. The placement-group name has an old AZ suffix;
AWS placement metadata in `placement-proof.json` proves that both test
instances were in that group and in `us-east-2c`. Each host had two EFA-capable
NICs and exactly one public control address. Bulk traffic used private IPv4.

Both hosts had 8,192 2 MiB huge pages. Benchmark shells had unlimited memlock.
The random-I/O and stream tests used card1, `ens146`, and NUMA-1 CPUs 96-191.
The PostgreSQL test retained the previous card0/NUMA-0 shape. The client block
edge did not perform placement; terminal media was a userspace `zcmem` leaf.

## Placement A/B

| workload | no placement group | cluster placement group | change |
| --- | ---: | ---: | ---: |
| 4 KiB random 50/50 RW, 8 lanes, QD128/lane | 1.280M IOPS | 1.482M IOPS three-run mean | +15.8% |
| sampled completion latency, same workload | 775 us mean | 641 us three-run mean | -17.4% |
| WAL stream, 64 lanes, CQ hot poll, sender | 202.4 Gbit/s | 249.7 Gbit/s | +23.3% |
| WAL stream, 64 lanes, CQ hot poll, receiver interval | 157.1 Gbit/s | 222.8 Gbit/s | +41.8% |
| WAL stream, 64 lanes, blocking, sender | 193.1 Gbit/s | 264.3 Gbit/s | +36.9% |
| WAL stream, 64 lanes, blocking, receiver interval | 150.5 Gbit/s | 236.1 Gbit/s | +56.9% |
| PostgreSQL scale 10, 32 clients, synchronous commits | 8.047K TPS | 8.160K TPS three-run mean | +1.4% |

The placement-group mixed-RW repetitions were 1.475M, 1.472M, and 1.498M
IOPS, a 1.80% spread. There were eight workers at QD128 each, so aggregate
outstanding depth was 1,024. Writes used bounded dirty-cache acknowledgement;
remote reads missed through to the leaf. No sync was issued in this saturation
workload. Mean read and early-write completion latencies were 789 us and 495 us
respectively.

The blocking WAL result delivered 264.3 Gbit/s, 88.1% of one card's advertised
300 Gbit/s. It also caused 533,060 receiver voluntary context switches. CQ hot
polling reduced that count to 1,185 but delivered 249.7 Gbit/s sender-side.

## Remote-Read QD Curve

Private-card1 ICMP RTT over 100 probes was 37/45/254 us min/mean/max. The
ceiling below is the optimistic `aggregate_depth / 45 us` remote-read network
ceiling; it excludes software and 4 KiB payload costs. Each row uses eight
workers and eight lanes, so aggregate depth is eight times per-worker QD.

| QD/worker | aggregate depth | actual IOPS | RTT ceiling | efficiency | mean | p50 | p99 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 8 | 72,881 | 177,778 | 41.0% | 77 us | 69 us | 263 us |
| 2 | 16 | 181,057 | 355,556 | 50.9% | 86 us | 70 us | 151 us |
| 4 | 32 | 266,342 | 711,111 | 37.5% | 103 us | 92 us | 264 us |
| 8 | 64 | 453,566 | 1,422,222 | 31.9% | 131 us | 129 us | 279 us |
| 16 | 128 | 840,958 | 2,844,444 | 29.6% | 143 us | 132 us | 337 us |

These are 100% random remote reads. They must not be compared with early-local
write acknowledgements using the same RTT denominator.

## PostgreSQL

PostgreSQL 16 used ext4 on `/dev/zcnblk0`, scale 10, 32 clients, four pgbench
threads, synchronous commits, and three 5-second repetitions. Results were
7,937.9, 8,220.2, and 8,321.6 TPS with mean latencies of 3.823, 3.681, and
3.644 ms. All 94,207 logical syncs became remote sync epochs.

Placement substantially improved the measured remote sync leg: lane averages
fell from 60.8/62.8 us without placement to 31.2/28.1 us with placement.
End-to-end TPS did not move by a comparable amount, showing that this workload
is currently limited above the raw network-sync leg.

An initial PostgreSQL setup attempt mistakenly paired a 64 GiB client capacity
with a 16 GiB external leaf. `mkfs` received an I/O error and no benchmark
number was emitted. The valid runs used 16 GiB on both sides.

## Cleanup

Both instances were explicitly terminated and both temporary Elastic IPs were
released after the artifacts were copied. The shared, no-hourly-cost placement
group remains available for future adhoc runs.
