# PostgreSQL TPS ceiling investigation

Date: 2026-08-08

## Topology

- Two `c8gn.48xlarge` spot instances in `us-east-2c` and cluster placement group
  `up-zc-ramtarget-us-east-2a`.
- Client `/dev/zcnblk0` -> shared-memory userspace onramp -> two TCP WAL lanes ->
  remote userspace `zcmem:64G` leaf.
- PostgreSQL 16.14, 32 clients, 4 pgbench workers, prepared statements,
  synchronous commit, fsync, and full-page writes enabled.
- Writes receive early local acknowledgement; commit flushes wait for the remote
  per-lane admission vector and all-lane sync marker.

## Results

| Workload | Scale | Warmup | TPS | Average latency |
|---|---:|---:|---:|---:|
| TPC-B-like, clean, 3 x 5 s | 10 | none | 21.59K-23.92K, 22.73K mean | 1.335-1.480 ms |
| TPC-B-like, clean, 3 x 5 s | 100 | none | 32.82K-42.88K, 36.23K mean | 0.743-0.972 ms |
| Simple-update control, 3 x 5 s | 10 | none | 38.22K-48.04K, 44.13K mean | 0.664-0.835 ms |
| Simple-update timed diagnostic, 10 s | 10 | none | 51.60K | 0.618 ms |
| TPC-B-like timed diagnostic, 10 s | 100 | 10 s | 35.24K | 0.891 ms |

The earlier placement-group score of about 8.16K TPS was invalid. PostgreSQL 16
interprets pgbench `-d` as `--debug`, not as the database selector. The harness
therefore printed every client protocol transition to a roughly 32 MiB log while
the test ran. Passing `pgbench` as the positional database name removed the
trace and recovered about 2.8x at scale 10. Clean repeat logs are about 1.4 KiB.

## Latency accounting

The 51.60K TPS simple-update diagnostic measured:

- PostgreSQL transaction latency: 618 us.
- `END`: 492 us.
- PostgreSQL WAL sync: 8,824.258 ms / 46,869 = 188.3 us average.
- Remote target lane sync: 34.6-35.1 us average.
- Storage target and kernel workers: 1.67 context switches per 1,000 transactions.
- pgbench process: 858,260 voluntary switches / 520,403 transactions = 1.65
  per transaction.

The warmed scale-100 standard sample measured 891 us per transaction. Branch and
teller updates consumed 174 us, `END` consumed 586 us, PostgreSQL WAL sync averaged
334.4 us, and the remote target lanes averaged 35.7-38.1 us. The remaining commit
time is in ext4/block handoff, PostgreSQL group-commit coordination, and local
backend/client scheduling, not remote network latency.

## Additional topology finding

The original PostgreSQL CPU list was entirely inside hctx 0 (`0-95`), while hctx 1
owned CPUs `96-191`. The scale-100 timed run consequently sent 1,584,675 writes to
lane 0 and 31,219 to lane 1. These are valid device-correctness and single-active-
hctx measurements, but they are not representative balanced two-lane results.
The harness now exposes every CPU list and rejects missing hctx coverage under
strict/fatal topology mode.

## Harness changes

- Pass the PostgreSQL database positionally; reject per-client debug traces.
- Select `tpcb-like` or `simple-update` through `PGBENCH_BUILTIN`.
- Support an explicit unreported warmup interval.
- Optionally enable PostgreSQL WAL I/O timing and emit named JSON statistics.
- Make target, kthread, leaf, PostgreSQL, and pgbench CPU lists explicit.
- Validate that PostgreSQL CPUs intersect every advertised block hctx.

PostgreSQL 16 pgbench reference: <https://www.postgresql.org/docs/16/pgbench.html>
