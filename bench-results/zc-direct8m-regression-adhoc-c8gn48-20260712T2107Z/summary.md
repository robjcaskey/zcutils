# Direct zcnblk 8M Regression Check

Run: `zc-direct8m-regression-adhoc-c8gn48-20260712T2107Z`

## Topology

- Two `c8gn.48xlarge` hosts in `us-east-2a`, one EFA ENI per network card.
- Data used card 1 only: `ens146`, NUMA node 1, private IPv4 in both directions.
- 64 TCP lanes, 192 blk-mq queues, 96 fio jobs at QD128 (aggregate QD 12,288).
- Client lane kthreads and target workers: lane 0/CPU 128 through lane 63/CPU 191.
- Target buffers: 16,384 2 MiB huge pages with unlimited systemd memlock.
- Real leaf: one 8 GiB `/dev/zcbrd0`, 64 sharded arenas, queues 192, depth 1024.
- The `zcbrd` device was created under a NUMA-1 memory policy. `/proc/vmallocinfo`
  confirmed every 128 MiB backing arena used 32,768 pages from N1.
- This was a single terminal leaf. No block device was used for mirror or stripe placement.

## Results

| Path | Workload | Runtime | IOPS | Bandwidth |
| --- | --- | ---: | ---: | ---: |
| `zcdevnull0` control | 4K random read | 10.017 s | 9.028M | 34.4 GiB/s |
| `zcdevnull0` control repeat | 4K random read | 10.003 s | 9.012M | 34.4 GiB/s |
| NUMA-1 `zcbrd0` | 4K random read | 10.005 s | 9.033M | 34.5 GiB/s |
| NUMA-1 `zcbrd0` | 4K acknowledged random write | 5.004 s | 8.908M | 34.0 GiB/s |

The old references were 7.956M null-read, 7.255M zcbrd-read, and 5.497M
zcbrd-write IOPS. The current representative runs therefore disprove a direct
single-target regression. The real RAM leaf is effectively tied with the null
control at this saturation point.

One initial real-leaf run transferred 344 GiB but suffered a 57.2-second maximum
completion tail, stretching fio runtime to 64.3 seconds and reporting 1.405M
IOPS. Two diagnostic runs without `IORING_ENTER_NO_IOWAIT` reached 8.886M and
8.997M, but a repeated strict run with that flag reached 9.004M. The evidence
does not attribute the isolated tail to the flag. The final 10-second run had
zero TCP retransmissions and zero TCP timeouts.

These 9M controls do not include the shared-memory WAL owner, dirty cache,
per-sector ordering, sync high-water mark, fanout, mirror commit, or ACK join.
The approximately 1.5M integrated numbers exercise those additional semantics
and remain an architectural optimization target rather than a direct-path
regression.

## PostgreSQL

The direct target was fixed to accept a block `SYNC`, flush the terminal fd,
and return `SYNC_ACK`. A live `blockdev --flushbufs` smoke completed in 7 ms.

PostgreSQL 16.14 ran on ext4 over `/dev/zcnblk0` with scale 100, 64 clients and
64 threads, `fsync=on`, `synchronous_commit=on`, `full_page_writes=on`, and
256 MiB shared buffers. PostgreSQL used CPUs 96-127, pgbench used CPUs 0-95,
and zcnblk kthreads remained on CPUs 128-191.

- 1,222,403 transactions in 30 seconds; zero failed transactions.
- 40,764.36 TPS.
- 1.570 ms average transaction latency; 1.488 ms standard deviation.
- 597,310 block writes and 71,774 block flushes during the timed interval.
- PostgreSQL reported 71,557 WAL writes and 71,557 WAL syncs during the interval.

## Validation And Cleanup

- `cargo test --release`: all 154 library tests and all binary/integration tests passed.
- Both adhoc instances reached `terminated` state.
- Both tagged Elastic IPs were released; post-cleanup remaining EIP count was zero.

