# Local etcd storage controls

Date: 2026-08-08 UTC

The harness ran etcd v3.6.12 (`90b034a02766`) and its official
`tools/benchmark` client. The server was pinned to CPUs 0-3 and clients to
CPUs 4-11. Each full run used 64 clients, 8 gRPC connections, 200,000 puts,
200,000 linearizable ranges, and 100,000 operations requested from the
official 1:1 `txn-mixed` workload. Values were 256 bytes.

Every soft-exclusive `agent-coord` request was acquired but not honored due
to overlapping CPU and memory-bandwidth claims. These are shared-system
measurements, so the table reports the median and range of three repetitions.
The host's pre-existing Kubernetes etcd (PID 2076, `/var/lib/etcd`) remained
running; it was not touched and may contribute additional root-filesystem
contention.

| Medium | Phase | Median ops/s | Three-run range | p50 | p99 |
|---|---|---:|---:|---:|---:|
| tmpfs | put | 146,983 | 145,966-147,467 | 0.330 ms | 1.623 ms |
| tmpfs | linearizable range | 187,617 | 184,884-191,071 | 0.282 ms | 1.219 ms |
| tmpfs | 1:1 mixed txn | 1,701 | 1,676-1,782 | 24.625 ms | 41.465 ms |
| root ext4 | put | 13,890 | 13,834-13,891 | 4.597 ms | 10.326 ms |
| root ext4 | linearizable range | 192,549 | 192,230-193,020 | 0.275 ms | 1.164 ms |
| root ext4 | 1:1 mixed txn | 1,738 | 1,731-1,743 | 21.933 ms | 39.880 ms |

The original leader-thread context-switch sampling was invalid, so the
harness was corrected to sum `/proc/PID/task/*/status`. Short validation runs
then measured:

| Medium | Phase | Voluntary | Involuntary |
|---|---|---:|---:|
| tmpfs | put (50k) | 4,877 | 135 |
| tmpfs | range (50k) | 3,858 | 167 |
| tmpfs | mixed (20k requested) | 10,242 | 453 |
| root ext4 | put (50k) | 46,673 | 1,018 |
| root ext4 | range (50k) | 4,278 | 353 |
| root ext4 | mixed (20k requested) | 18,670 | 1,432 |

## Device comparison blocker

`/dev/zcnblk0` was absent and no pre-existing authorized zcnblk benchmark
filesystem was mounted. No raw device was formatted, mounted, or modified.
Consequently, this run establishes safe tmpfs and local-ext4 controls but does
not measure the zcnblk client block edge. Rerun the same harness with
`DATA_ROOT` pointing at an already-mounted authorized zcnblk filesystem to
produce the direct comparison.
