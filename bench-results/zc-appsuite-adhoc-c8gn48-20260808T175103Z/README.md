# Remote zcnblk application suite

Run ID: `zc-appsuite-adhoc-c8gn48-20260808T175103Z`

Two `c8gn.48xlarge` instances ran in `us-east-2c` in the same cluster
placement group. Block/WAL traffic used the private secondary EFA-capable NIC
on both hosts (`ens146`, NUMA node 1, MTU 9001); the public address was used
only for control. The measured 100-packet TCP-path ping was 48 us average,
41 us minimum, with no loss.

The client used 16 kernel queues and two WAL lanes. Lane 0 occupied hctx CPUs
96-107 and lane 1 occupied CPUs 108-119. Target workers ran on CPUs 97 and
109, kernel lane workers on 98 and 110, application workers on
99-107,111-151, and the sync coordinator on 152. Remote leaf workers ran on
CPUs 97 and 109. Both hosts reserved 8,192 HugeTLB pages. Every final harness
reported `topology_representative=1`, honored its resource reservation, and
completed with zero preflight warnings.

## Completion contract

- `/dev/zcnblk0` was the only block edge. There was no mirror, stripe, tier,
  spill, or kernel placement decision.
- An ordinary write completed after bounded local dirty-lease admission.
- Flush/FUA completed only after both userspace TCP lanes reported the remote
  leaf high-water mark.
- The remote leaf retained data in a 16 GiB userspace `zcmem` arena. Explicit
  `URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1` made that remote volatile
  commit valid for this benchmark, but it is not power-loss durability.

## Results

All application phases completed with zero workload errors.

| Workload | Phase | Rate | Mean | p99 |
|---|---|---:|---:|---:|
| etcd 3.6.12 | durable put | 11,955/s | 5.351 ms | 10.707 ms |
| etcd 3.6.12 | linearizable range | 116,969/s | 0.531 ms | 1.841 ms |
| etcd 3.6.12 | mixed transaction | 908/s | 44.432 ms | 65.693 ms |
| Cassandra 5.0.9 | durable write | 6,617/s | 4.4 ms | 6.0 ms |
| Cassandra 5.0.9 | read | 43,916/s | 0.3 ms | 1.4 ms |
| Cassandra 5.0.9 | 50/50 mixed | 12,603/s | 2.4 ms | 6.5 ms |
| Kafka 4.3.1 | forced-flush producer | 294 records/s | queued | queued |
| Kafka 4.3.1 | page-cache producer | 43,764 records/s | 3.56 ms | 8 ms |
| Kafka 4.3.1 | consumer, fetch interval | 129,032 records/s | n/a | n/a |
| NFSv4.2 | fsync-per-write, QD1 | 367 IOPS | 2.718 ms sync | 3.162 ms sync |
| NFSv4.2 | 70/30 mixed, aggregate QD1 | 1,320 IOPS | R 44.8 us / W 2.345 ms | R 61.2 us / W 2.507 ms |
| NFSv4.2 | 70/30 mixed, aggregate QD16 | 10,571 IOPS | R 59.8 us / W 4.832 ms | R 100.9 us / W 5.603 ms |
| NFSv4.2 | 1 MiB stream write | 0.195 GB/s | n/a | n/a |
| NFSv4.2 | 1 MiB warm stream read | 12.2 GB/s | n/a | n/a |
| PostgreSQL 16.14 | pgbench scale 100 | 70,487 TPS | 0.905 ms | n/a |

Kafka's forced-flush latency columns are intentionally not summarized as
service latency: the producer allowed one in-flight request, so its reported
3.369-second average includes queueing behind earlier records. The serialized
service rate was 294 records/s, approximately 3.4 ms per record. The
page-cache producer was not a durable-write test. The consumer fetched
129,032 records/s after its one-time rebalance; overall startup-inclusive rate
was 19,268 records/s.

The PostgreSQL run used 64 clients, eight jobs, a 10-second measured interval,
scale 100, `synchronous_commit=on`, `fsync=on`, and
`full_page_writes=on`. It completed 703,647 transactions with no failures,
issued 55,128 PostgreSQL WAL syncs, and incurred 3,040 measured storage-stage
context switches, or 4.320 per 1,000 transactions.

The neighboring raw-path proof in
`bench-results/zc-syncack-adhoc-c8gn48-20260808T171804Z/` used the same host
class and topology. It reached 1.399M mean random 4 KiB mixed IOPS at aggregate
QD 1024 and 213.553 Gbit/s on the one-NIC bulk WAL sender. Those controls show
that application synchronization and filesystem behavior, rather than the
bulk transport ceiling, dominate several application rows above.

The first NFS orchestration attempt stopped before a workload because
`nfsdctl` was unavailable. The harness now falls back to `rpc.nfsd`; `nfs/`
and `nfs-edge/` contain the successful clean rerun, while the failed setup logs
remain in `nfs-leaf/` for diagnosis.

Both instances were terminated and both temporary Elastic IPs were released.
