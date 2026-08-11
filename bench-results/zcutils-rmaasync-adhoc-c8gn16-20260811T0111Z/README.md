# Asynchronous EFA RMA-read QD ladder

Run ID: `zcutils-rmaasync-adhoc-c8gn16-20260811T0111Z`

Two dedicated `c8gn.16xlarge` instances ran in `us-east-2c`, with one EFA
interface (`ens50`, domain `efa_0-rdm`) per host. Both NICs had adaptive RX
coalescing disabled and zero RX/TX interrupt delay. Each host reserved 7,041
2 MiB HugeTLB pages and had unlimited login-session memlock. Every point used
16 lanes and 16 workers, identity lane-to-worker ownership, explicit CPU
pinning, `FI_EFA_USE_DEVICE_RDMA=1`, busy CQ polling, and three repeats.

## Completion contract

- Raw results measure `fi_read` post to initiator-local CQ completion with the
  requested data visible in a stable registered slot. They are not remote
  write acknowledgements or sync/FUA drains.
- Block results are 100% random 4 KiB reads through `/dev/zcnblk0`. Placement
  had already been decided by the separate userspace WAL stage; the terminal
  leaf was a remote userspace `zcmem:1G` arena, not a block mirror or stripe.
- The matching theoretical ceiling is Little's-law depth divided by the raw
  post-to-local-CQ time at the same QD. Block efficiency uses that matching
  raw ceiling. It is not compared with a TCP ACK or durability denominator.

## QD1/QD2/QD4/QD8/QD16 curve

| Per-worker QD | Aggregate depth | Raw RMA IOPS (mean) | Raw spread | Raw RTT | Raw ceiling | Raw efficiency | Block IOPS (mean) | Block spread | Block mean latency | Block p99 | Block / raw ceiling |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 16 | 757,556 | 8.10% | 19.492 us | 821,902 | 92.18% | 634,377 | 0.30% | 24.996 us | 35 us | 77.18% |
| 2 | 32 | 1,347,147 | 9.37% | 21.971 us | 1,459,066 | 92.34% | 795,795 | 2.61% | 37.982 us | 58 us | 54.54% |
| 4 | 64 | 2,233,027 | 4.73% | 26.463 us | 2,419,589 | 92.29% | 1,441,318 | 0.78% | 43.980 us | 78 us | 59.57% |
| 8 | 128 | 2,495,599 | 3.83% | 42.880 us | 2,985,673 | 83.58% | 2,007,146 | 1.32% | 63.289 us | 108 us | 67.23% |
| 16 | 256 | 2,640,526 | 7.89% | 70.426 us | 3,637,215 | 72.58% | 2,369,461 | 2.45% | 105.805 us | 173 us | 65.14% |

The raw path rose from 0.758M IOPS at aggregate QD16 to 2.641M at aggregate
QD256. CQ batches grew from 1.00 completion at QD1 to 8.81 at QD16. The block
path reached 2.369M IOPS at aggregate QD256 and retained 89.73% of the measured
raw IOPS there. QD2 is the least efficient block point relative to its matching
raw ceiling, so the curve exposes local handoff overhead that a single
high-depth saturation number would hide.

## Block topology and invariants

For block runs, client io_uring workers used CPUs
`0,4,...,60`, userspace target/transport workers used `1,5,...,61`, kernel lane
kthreads used `2,6,...,62`, and SQPOLL used `3,7,...,63`. Each hctx covered its
four-CPU lane group. Remote leaf workers used CPUs `32-47`, with lane `N`
owned by leaf worker `N`. Strict and fatal topology preflight were enabled,
HugeTLB and memlock checks passed, and the run declared all lane/worker/CPU
mappings before representative numbers.

At every QD, all 16 lane queues reported the requested `peak_in_flight`, zero
`final_in_flight`, zero `final_queued`, zero `final_batches`, and zero
`post_eagain`. The target completed 9.6 million reads per QD point across the
three repeats. Raw and block logs, topology manifests, and repeat summaries are
retained below this directory.

Both temporary instances were terminated after the evidence audit.
