# Stable WAL Owner Ingress

## TL;DR

The client path is `/dev/zcnblk0 -> shared-memory lane ingress -> stable WAL
owner -> TCP lane -> userspace leaf`. The kernel edge never makes mirror,
stripe, tier, spill, or placement decisions. Payload pages remain leased in the
shared arena while userspace descriptors move between ingress and owner
threads. A global sync drains every owner and advances the committed high-water
mark. Reads observe the dirty shared-page reference first and otherwise go to
the remote userspace leaf. Block media is allowed only behind a terminal leaf
writer after userspace placement.

## Owner Contract

- One ingress worker owns each block hctx lane, dirty-cache admission, and
  completion ordering.
- One stable owner owns each long-lived remote stream and coalesces fragments.
- The owner count may be smaller than the block-ingress lane count. Every
  placement decision still occurs in this userspace stage; with one owner,
  every ingress lane feeds the same stable remote stream.
- The ingress-to-owner queue copies descriptors, not 4K payloads.
- A transferred payload slot is released only after remote completion or safe
  dirty-cache retirement.
- Same-sector predecessors and the global sync high-water mark preserve order
  across blocking and io_uring callers.
- `URING_PLAY_ZCNBLK_SHM_SECTOR_ORDER_SLOTS` must be a power of two and at
  least twice the active benchmark page count for a representative run.

Representative write-only `wal-tcp` runs against an external leaf enable this
path by default. Read-only and mixed runs retain lane-inline ingress: August 9,
2026 controls measured about 1.00M IOPS through stable-owner versus prior
lane-inline results of about 1.81M read and 1.89M mixed IOPS. Set
`URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS` explicitly to override workload-aware
selection. Set the owner CPUs explicitly with
`URING_PLAY_ZCNBLK_SHM_OWNER_CPU_LIST`; the
benchmark harness records client, ingress, kthread, and owner CPU assignments
per lane. Whenever aggregate outstanding depth exceeds 128, the harness
defaults the stable-owner queue to that full depth (`LANES * IODEPTH`) so a
multi-lane fan-in or temporarily hot owner cannot be capped by the old
128-entry limit even when each worker's QD is lower than 128.

For a single EFA domain and one terminal memory leaf, set
`URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_OWNER_MODE=single-domain-fan-in`. The
block harness then defaults to one stable owner/OFI endpoint and a per-endpoint
RMA payload-operation QD of 64 while retaining all configured block hctx lanes.
This is a many-ingress-to-one-userspace-owner topology, not kernel striping or
mirroring. Keep `placement` mode for multiple actual leaf/rail owners. Because
multiple delivery-complete write endpoints on one configured EFA domain can
contend instead of scale, strict multi-endpoint runs require the explicit
`URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED=1` acknowledgement.

## x48 Dual-NIC Result

These July 12, 2026 controls used two `c8gn.48xlarge` instances, 32 lanes, 16
lanes per 300-Gbit/s NIC, hugetlb buffers, 1,048,576 exact-order slots, a
userspace `zcmem` leaf, random 4K 50/50 reads and writes, and eight linked
same-sector order pairs followed by sync. Every completed run passed the order
and terminal-sync check.

| Shape | Mixed IOPS | Overall p50 | Read p50 | Write p50 | Client ctx/1K | Target ctx/1K |
|---|---:|---:|---:|---:|---:|---:|
| QD1, blocking CQ wait | 82,270 | 211 us | 354 us | 52 us | 1,006.8 | 1,197.8 |
| QD1, hot CQ, progress 4096 | 89,566 | 173 us | 333 us | 57 us | 16.9 | 1,176.6 |
| QD1, hot CQ, progress 64 | 91,680 | 111 us | 265 us | 5 us | 17.7 | 1,177.8 |
| QD16, old completion wait | 412,709 | 2 ms | 2 ms | 129 us | 421.4 | 237.0 |
| QD16, hot CQ, 4.6 s | 697,109 | 170 us | 2 ms | 61 us | 0.15 | 98.3 |
| QD128, hot CQ, 2.3 s | 1,383,552 | 3 ms | 4 ms | 2 ms | 0.11 | 48.8 |

The separate write-only owner control reached 1.512M IOPS with 47.1 target
context switches per 1K I/O. The raw dual-NIC WAL transport ceiling remains
about 495 Gbit/s, or 15.1M logical 4K records/s, so the integrated mixed block
path is still scheduler/protocol limited rather than network limited.

## io_uring Hot Progress

Pure CQ memory polling can stall forever when this kernel defers a completion
through task work. `URING_PLAY_CQE_HOT_POLL=1` therefore issues a nonblocking
`IORING_ENTER_GETEVENTS` progress kick after
`URING_PLAY_CQE_HOT_POLL_PROGRESS_SPINS` empty spins. The default is 4096.
Using 64 improves QD1 median latency but generates millions of progress calls
and is reserved for explicitly CPU-greedy latency workloads.

## Remaining Bottleneck

Client and kernel wake churn is no longer first-order in hot mode. Stable owner
threads still block around small request/result cycles; the sustained QD16 run
averaged only 4.1 records per remote batch and paid about 98 target context
switches per 1K I/O. The next change should make each owner a persistent
asynchronous send/receive state machine with multiple in-flight extents and
range/HWM completion delivery. It must preserve the current dirty-reference,
same-sector predecessor, and all-owner sync contracts while separating the
latency fragment deadline from the throughput extent size.

## Adaptive Owner Scheduling

Owner command waits use an adaptive spin budget by default. The budget starts
at 4,096 iterations, grows toward 65,536 when work is already queued or arrives
within 50 us of a blocking miss, and shrinks after a genuinely quiescent wait.
The controls are `URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_ADAPTIVE_SPIN`,
`URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_SPIN_MIN`,
`URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_SPINS`, and
`URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_ADAPTIVE_WAIT_NS`.

On the noisy local two-lane QD16 path, fixed 4,096 spinning produced 34.2
target context switches per 1K I/O. Fixed 65,536 produced 1.12, and the adaptive
policy produced 1.44 while preserving the same 111K IOPS and roughly 201 us
p50. The adaptive owners blocked only 37-61 times over 1.2M I/O and ended at a
32,768-spin budget. A continuous QD1 control remained classified as busy and
delivered 107K IOPS at 13 us overall p50; truly quiescent waits shrink toward
the configured floor.

The leaf TCP receiver uses the same sustained-activity idea with a 256-spin
floor, 65,536-spin ceiling, and 10 ms hysteresis. On the same local QD16 path,
the old per-wait adaptive rule stayed at its floor and incurred about 40 leaf
switches per 1K I/O. Hysteresis reduced that to 0.15 per 1K and ended at 32,768
spins. It intentionally raised each busy leaf lane from about 14% to 92% CPU;
throughput stayed at 111K because this local two-lane control was capped by the
remaining kernel/protocol path. The leaf now prints its final spin budget,
spin hits, blocking fallbacks, polls, and grow/shrink counts.

Ingress fragment sizing is topology-derived: the default is the owner count,
capped at 16 records. A two-owner local sweep measured 103.5K, 135.8K, 132.3K,
95.4K, 121.8K, and 99.7K IOPS at fragment targets 1, 2, 4, 8, 16, and 64,
respectively. A fixed bug had reset the oldest-tail deadline on every submit,
preventing sparse owner tails from ever reaching their 500 us deadline under
continuous traffic. The deadline now retains the timestamp of the oldest
pending tail; the topology-derived target handles the remaining crossbar
sparsity.

The owner write-fill sweep also exposed a latency/throughput policy boundary.
At two lanes and QD16, mixed 50/50 traffic rose from 29.8K IOPS with a 1 ms
fill delay through 62.6K, 135.8K, 221.1K, 322.2K, and 425.1K at 500, 200,
100, 50, and 20 us, then reached 486.0K with no timed fill. Immediate dispatch
still drains fragments already present in the owner queue; skipping that
opportunistic drain doubled protocol batches and reduced the control to 262K
IOPS. A separate pure-write sweep reached 557.5K, 550.8K, and 182.7K IOPS at
0, 20, and 200 us, respectively. Even at zero timed fill, natural queue
draining coalesced 16.1 records per remote batch. The default is therefore
zero timed fill; a nonzero fill remains an explicit bulk-throughput experiment
that must prove a benefit on its target topology. The owner also bypasses any
configured timed fill for 10 ms after observing a read.

The zero-fill default then repeated at 550.6K/553.0K/554.5K pure-write IOPS
(min/mean/max, 0.71% spread), with 58 us p50 latency and 0.475-0.657 target
context switches per 1K I/O. Natural coalescing remained 16.1 records per
remote batch.

Low-QD validation found a separate read-tail penalty: with fragment fill still
set to 500 us, QD1 mixed traffic reached only 10.6K IOPS and remote-read p50
was 507 us. A read now marks only its stable extent owner urgent and flushes
that owner's existing tail in order, including preceding writes. Mixed QD1
then reached 38.8K IOPS with 52 us read p50, while target context switches fell
from 27.5 to 4.8 per 1K I/O. QD16 remained healthy at 502.0K mixed IOPS, 58 us
read p50, 0.092 target context switches per 1K, and six records per remote
batch. Both live controls passed same-sector ordering and terminal-sync checks.

The QD1 client completion policy matters independently of the transport. A
conventional synchronous mixed control reached 182.1K IOPS with 4 us overall
p50 and 19 us read p50, but incurred 1,006 client context switches per 1K I/O.
io_uring hot polling with a progress kick every 256 spins reached 179.0K IOPS
at 4/17 us p50 and 5.9 client switches per 1K, using 3.3 nonblocking progress
syscalls per I/O. A 64-spin ultra-low-latency setting reached 207.6K IOPS at
3/18 us p50 and 8.7 switches per 1K, at the cost of 11 progress syscalls per
I/O. The harness now defaults to 256 spins at QD1-4 and retains 4,096 at deeper
queues; explicit override remains available for the CPU/syscall-heavy setting.

`zcblockbench --engine hybrid` alternates fixed-buffer io_uring and synchronous
workers behind one start barrier, so both request types are active against the
same block edge rather than being tested sequentially. A two-worker QD1 live
smoke reached 177.6K mixed IOPS at 4 us overall p50 and 19 us read p50. The
per-worker log identified worker 0 as `uring-fixed` and worker 1 as `sync`;
same-sector ordering and terminal sync also passed. Worker regions are
disjoint, so this proves concurrent engine coexistence while the dedicated
ordering smoke remains responsible for same-sector conflict semantics.

After preserving the queue drain, the repeated noisy-host control reached
491.9K/492.8K/493.5K mixed 4K IOPS (min/mean/max, 0.32% spread). Overall p50
latency was 57 us, read p50 was 58 us, and write p50 was 57 us. Target context
switches were 0.095-0.133 per 1K I/O, kernel completion-thread switches were
0.007-0.015 per 1K, and client switches were 1.440-1.452 per 1K. The run used
explicit client CPUs 0/8, target CPUs 1/9, kernel CPUs 2/10, owner CPUs 3/11,
and userspace zcmem leaf CPUs 4/5; all same-sector and terminal-sync checks
passed. It remains a shared-host, small-page control rather than a
representative hugetlb result.

Compact logs, topology manifests, context deltas, and correctness output are in
`bench-results/cloud-owner-ingress-x48-20260712/`.
