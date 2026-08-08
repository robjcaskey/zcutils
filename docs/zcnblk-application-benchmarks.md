# zcnblk Application Benchmarks

These harnesses test real application and filer behavior over one zcnblk block
edge. They are correctness and regression tests first. They also record
application latency, throughput, process context switches, storage-stage context
switches, and the complete CPU/lane topology.

The tested topology is:

```text
application filesystem
  -> /dev/zcnblk0 client edge
  -> userspace WAL target
  -> two lane-local TCP streams
  -> userspace zcmem leaf
```

There is no mirror, stripe, tier, spill, or kernel placement decision in this
topology. A future userspace RAID stage belongs after the WAL target. A block
device must never be used as a mirror or stripe primitive.

## Harnesses

- `scripts/zcnblk-fs-app-bench.sh` creates, formats, mounts, records, and tears
  down the zcnblk edge. It accepts any application harness with the interface
  `APP_SCRIPT RESULT_DIR DATA_ROOT`.
- `scripts/zcnblk-etcd-bench.sh` uses the official etcd benchmark for durable
  puts, linearizable ranges, and mixed transactions.
- `scripts/zcnblk-cassandra-bench.sh` uses Apache Cassandra's
  `cassandra-stress`. `commitlog_sync=batch` means a successful write follows
  commitlog fsync.
- `scripts/zcnblk-kafka-bench.sh` uses Apache Kafka's bundled performance
  tools. It reports forced-flush production separately from leader-page-cache
  production and consumption.
- `scripts/zcnblk-nfs-filer-bench.sh` exports the supplied data root with
  kernel NFSv4.2 and `sync`, mounts it over TCP, verifies data, and runs
  metadata, fsync-per-write, random mixed, and streaming fio phases. It never
  edits `/etc/exports` and refuses to run when another export exists.

Every application harness supports `EXPECT_ZCNBLK=1`, which fails unless its
data root is mounted from exactly `/dev/zcnblk0`.

## Prerequisites

Build the zcnblk module and production userspace binaries first:

```bash
make -C kmods
cargo build --release --bin zcnblk-shm-target --bin zcnblk-wal-leaf
```

Use official binary distributions for etcd, Cassandra, and Kafka. Cassandra
and Kafka require Java 17 in these harnesses. NFS requires `nfs-kernel-server`,
`nfs-common`, and fio.

The wrapper obtains advisory ownership of `/dev/zcnblk0` and soft-exclusive
CPU/memory resources. It records these explicit defaults:

```text
target lanes:  lane0:cpu1,lane1:cpu9
kernel lanes:  lane0:cpu2,lane1:cpu10
leaf lanes:    lane0:cpu3,lane1:cpu11
application:   cpu0,cpu4-8,cpu12-31
```

Override all four CPU lists together when the host topology differs. Set
`URING_PLAY_TOPOLOGY_STRICT=1` or `URING_PLAY_TOPOLOGY_FATAL=1` to reject an
unhonored reservation, missing hctx affinity, missing HugeTLB pool, or low
memlock before benchmark numbers are printed.

## Example Commands

etcd:

```bash
ETCD_BIN=/opt/etcd/etcd \
ETCDCTL_BIN=/opt/etcd/etcdctl \
BENCHMARK_BIN=/opt/etcd/benchmark \
scripts/zcnblk-fs-app-bench.sh \
  bench-results/etcd-edge bench-results/etcd-app \
  scripts/zcnblk-etcd-bench.sh
```

Cassandra:

```bash
CASSANDRA_HOME=/opt/apache-cassandra-5.0.9 JAVA_HOME=/opt/java-17 \
scripts/zcnblk-fs-app-bench.sh \
  bench-results/cassandra-edge bench-results/cassandra-app \
  scripts/zcnblk-cassandra-bench.sh
```

Kafka:

```bash
KAFKA_HOME=/opt/kafka_2.13-4.3.1 JAVA_HOME=/opt/java-17 \
scripts/zcnblk-fs-app-bench.sh \
  bench-results/kafka-edge bench-results/kafka-app \
  scripts/zcnblk-kafka-bench.sh
```

NFS filer:

```bash
NFS_THREADS=16 NCONNECT=8 \
scripts/zcnblk-fs-app-bench.sh \
  bench-results/nfs-edge bench-results/nfs-app \
  scripts/zcnblk-nfs-filer-bench.sh
```

The NFS harness can also measure another already-mounted data root directly:

```bash
scripts/zcnblk-nfs-filer-bench.sh bench-results/nfs-control /path/to/data-root
```

For a remote userspace leaf, start `zcnblk-wal-leaf` on the leaf host and make
the wrapper source every WAL connection from the private data NIC:

```bash
START_LOCAL_LEAF=0 \
LEAF_HOST=172.31.37.12 LEAF_SOURCE_ADDR=172.31.35.76 \
LEAF_PORT=29200 KERNEL_QUEUES=16 \
TARGET_CPU_LIST=97,109 KTHREAD_CPU_LIST=98,110 \
APP_CPU_LIST=99-107,111-151 SYNC_COORDINATOR_CPU=152 \
COORDINATION_SCOPE=dedicated-adhoc \
scripts/zcnblk-fs-app-bench.sh \
  bench-results/remote-edge bench-results/remote-app \
  scripts/zcnblk-etcd-bench.sh
```

The topology preflight checks the source route, lane workers, kernel queues,
hctx affinity, memory policy, and application connection hctxs before allowing
representative output.

## Completion Contracts

Do not compare every row to one network-RTT ceiling.

- etcd put: etcd WAL durability before application acknowledgment.
- etcd range: linearizable read from the single member.
- Cassandra write: commitlog batch fsync before acknowledgment.
- Kafka forced flush: `acks=all`, one-record requests, one in-flight request,
  and topic `flush.messages=1`. Kafka's producer latency includes time queued
  behind the single in-flight request; throughput is the serialized service
  rate.
- Kafka stream: `acks=1` after leader page-cache admission with 1 MiB producer
  batches. This is not a durable-write result.
- NFS sync write: one 4 KiB write followed by fsync against a `sync` export.
  The CSV reports write latency and `sync_lat_*` separately; the latter is the
  relevant durability latency.
- NFS random direct phases: QD1 is one worker at depth one. QD16 is four workers
  at depth four, for aggregate outstanding depth 16.
- NFS warm stream read: client direct I/O with server and lower-layer cache
  state intentionally not dropped.

## Local Proof, 2026-08-08

The final-code shared-host smoke runs completed with zero application errors:

| Workload | Phase | Rate | Mean | p99 |
|---|---|---:|---:|---:|
| etcd | durable put | 8,131/s | 3.926 ms | 6.867 ms |
| etcd | linearizable range | 124,683/s | 0.253 ms | 1.273 ms |
| etcd | mixed transaction | 4,988/s | 2.942 ms | 9.775 ms |
| Cassandra | durable write | 3,491/s | 2.9 ms | 8.1 ms |
| Cassandra | read | 9,382/s | 1.1 ms | 6.0 ms |
| Cassandra | 50/50 mixed | 5,978/s | 1.7 ms | 7.0 ms |
| Kafka | forced-flush producer | 413 records/s | queued | queued |
| Kafka | page-cache producer | 57,307 records/s | 4.55 ms | 16 ms |
| NFS | fsync-per-write | 496/s | 2.012 ms sync | 2.540 ms sync |
| NFS | 70/30 mixed, aggregate QD1 | 13,875 IOPS | read 20.9 us | write 2.114 ms |
| NFS | 70/30 mixed, aggregate QD16 | 13,028 IOPS | read 32.4 us | write 5.276 ms |
| NFS | 1 MiB stream write | 0.586 GB/s | n/a | n/a |
| NFS | 1 MiB warm stream read | 14.9 GB/s | n/a | n/a |

Artifacts:

```text
bench-results/zcnblk-etcd-final-20260808T154419Z/
bench-results/zcnblk-cassandra-final-20260808T154436Z/
bench-results/zcnblk-kafka-syncvector-20260808T154236Z/
bench-results/zcnblk-nfs-filer-20260808T154324Z/
```

These are not representative ceilings. The resource reservations were
honored, but this shared host had an 8 MiB memlock limit and no explicit
HugeTLB pool. Transparent Huge Pages were set to `always`, with roughly 42 GiB
of anonymous huge pages active. The current kernel/userspace shared zcnblk
arena is allocated with `vmalloc_user()` and mapped with
`remap_vmalloc_range()`, so merely reserving HugeTLB pages cannot change that
arena's backing. A future explicit HugeTLB arena requires an allocation/ABI
change rather than only a sysctl.

## Remote Pair Proof, 2026-08-08

The complete suite ran between two `c8gn.48xlarge` hosts in `us-east-2c` and
one cluster placement group. The private secondary NIC on NUMA node 1 carried
all block/WAL traffic at MTU 9001. The 100-packet path RTT averaged 48 us, both
hosts reserved 8,192 HugeTLB pages, and every final harness had an honored
resource reservation and zero preflight warnings.

Ordinary writes completed at bounded local dirty-lease admission. Sync/FUA
waited for a remote two-lane HWM. The remote leaf used explicitly permitted
volatile memory, which validates remote commit semantics but not power-loss
durability.

| Workload | Phase | Rate | Mean | p99 |
|---|---|---:|---:|---:|
| etcd | durable put | 11,955/s | 5.351 ms | 10.707 ms |
| etcd | linearizable range | 116,969/s | 0.531 ms | 1.841 ms |
| etcd | mixed transaction | 908/s | 44.432 ms | 65.693 ms |
| Cassandra | durable write | 6,617/s | 4.4 ms | 6.0 ms |
| Cassandra | read | 43,916/s | 0.3 ms | 1.4 ms |
| Cassandra | 50/50 mixed | 12,603/s | 2.4 ms | 6.5 ms |
| Kafka | forced-flush producer | 294 records/s | queued | queued |
| Kafka | page-cache producer | 43,764 records/s | 3.56 ms | 8 ms |
| Kafka | consumer fetch interval | 129,032 records/s | n/a | n/a |
| NFS | fsync-per-write, QD1 | 367 IOPS | 2.718 ms sync | 3.162 ms sync |
| NFS | 70/30 mixed, aggregate QD1 | 1,320 IOPS | R 44.8 us / W 2.345 ms | R 61.2 us / W 2.507 ms |
| NFS | 70/30 mixed, aggregate QD16 | 10,571 IOPS | R 59.8 us / W 4.832 ms | R 100.9 us / W 5.603 ms |
| NFS | 1 MiB stream write | 0.195 GB/s | n/a | n/a |
| NFS | 1 MiB warm stream read | 12.2 GB/s | n/a | n/a |
| PostgreSQL | pgbench scale 100 | 70,487 TPS | 0.905 ms | n/a |

PostgreSQL used 64 clients and eight jobs with synchronous commit, fsync, and
full-page writes enabled. It completed 703,647 transactions without failure
and measured 4.320 storage-stage context switches per 1,000 transactions.
Kafka's forced-flush producer used one in-flight request; its service rate was
about 3.4 ms per record, while Kafka's displayed multi-second latency included
producer queueing.

The full topology, logs, summaries, context-switch snapshots, checksums, and
cleanup evidence are in:

```text
bench-results/zc-appsuite-adhoc-c8gn48-20260808T175103Z/
```

## Concurrent Sync Regression

Kafka shutdown exposed a deadlock with simultaneous filesystem flushes on both
lanes. A later conservative admission vector included an earlier zero-payload
sync descriptor, while the per-lane transport HWM waited for that descriptor's
durable completion before starting the marker.

The transport HWM now crosses a consumed sync descriptor immediately because
it carries no leaf data. Its block completion remains pending until the
all-lane remote marker commits. The regression test is
`wal_sync_vector_crosses_queued_sync_descriptors`; the repeated Kafka and NFS
runs both completed and unmounted normally after this correction.
