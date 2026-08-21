# EKS cheap-node cross-node TCP staging

This staging run used a `t3a.medium` client pod and a `t3.medium` target pod in
`eu-west-1c`, kernel `6.18.41-94.142.amzn2023.x86_64`. It exercised the
userspace TCP WAL transport directly across nodes; no block device or terminal
media participated in this transport measurement.

## Results

- One explicit ENI route (`ens5`): 2 lanes/workers pinned to CPUs `0-1`,
  io_uring pipeline depth 32 per worker (64 aggregate), 151,424 remotely
  acknowledged logical 4K records/s and 4.962 payload Gbit/s.
- Two explicit ENI routes (`ens5` and `ens6`): one lane/worker on CPU `0` and
  one on CPU `1`, pipeline depth 32 each (64 aggregate). The synchronized rate,
  calculated from both record counts over the slower sender's 2.601454 seconds,
  was 151,152 logical 4K records/s and 4.953 payload Gbit/s.

Both paths report zero worker migrations. The second ENI did not increase
throughput because these inexpensive T3 instances expose shared instance
network capacity rather than independent high-bandwidth network cards. This
was a connectivity/routing stage, not a representative high-IOPS result. The
logs also retain the harness warnings about capped socket buffers and a
transport frame size that was not extent-aligned.
