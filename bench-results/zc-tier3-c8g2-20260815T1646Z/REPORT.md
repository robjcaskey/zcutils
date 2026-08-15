# Three-host cost-bounded tier architecture report

This is a bounded architecture run, not a final storage-performance claim.
The tests used three `c8g.2xlarge` Spot instances
in `us-east-2c`, each quoted at `$0.087900/hour` at launch.  Total compute is
`$0.2637/hour`; the first bounded lease ends at `2026-08-15T20:40:00Z`.
The launcher reserved a conservative `$1.2513` ceiling for that lease,
including public IPv4 and root gp3.  Instance identities and addresses are in
`inventory.json`.

All three instances launched at `2026-08-15T16:46:47Z` and were confirmed
terminated at `2026-08-15T17:42:13Z`, after 55 minutes 26 seconds.  At the
launch quote, the compute portion is approximately `$0.244`; the authoritative
bill can differ with Spot price changes and ancillary IPv4/EBS charges.
`termination.txt` records the exact instance IDs and final AWS states.

## Topology and qualification

```text
node 0: source/client + hot userspace materialization
  TCP mux lane 0
node 1: warm userspace materialization + bounded spill/WAL relay
  TCP mux lane 0
node 2: cold userspace terminal leaf
```

No block device, device mapper, MD, loop, ramdisk block device, or kernel
client performs mirror, stripe, tier, spill, or placement work.  The high-rate
leaves are dedicated tmpfs mounts used as userspace terminal media.  Root gp3
is used only for a 64 MiB persistence conformance test.

All hosts have eight CPUs in one NUMA node, 1,024 huge pages, unlimited
benchmark-shell memlock, ENA adaptive RX disabled, and zero RX/TX coalescing.
The final high-water curve maps lane 0 -> worker 0 -> CPU 4 on every host.
ENA queue IRQs 94--101 map to CPUs `5,6,7,5,6,7,5,6`; irqbalance is stopped.
The route is `ens34` with the private source address validated on every hop.
The exact before/after topology evidence is under each node's `topology/`
directory.

The byte-stream runs explicitly remain byte-copy paths.  The framed
high-water path reports zero userspace payload-copy bytes at both relay stages
by using persistent `vmsplice`/`tee`/`splice` pipes.  The high-water numbers
below remain architecture-only RAM-leaf results: the utility itself prints
`representative_benchmark=false`, no io_uring terminal path is involved, and
they must not be compared with persistent-device product numbers.

Pinned one-byte `TCP_NODELAY` echo measurements used 20,000 samples per hop:

| hop | minimum | median | p99 | maximum |
| --- | ---: | ---: | ---: | ---: |
| node 0 -> node 1 | 36.050 us | 38.630 us | 45.193 us | 113.900 us |
| node 1 -> node 2 | 35.041 us | 36.961 us | 41.233 us | 150.269 us |

The matching two-hop median transport term is 75.591 us.

## Architecture experiments

### Synchronous byte-stream chain

A 4 GiB stream was materialized on all three RAM leaves.  Every leaf had SHA
`38f14072e122f691a7f3f5ac226b23960909223da4e65954aff48e646c7ccce2`.
The node-0 process completed at 1,136.00 MiB/s.  This is EOF completion through
both TCP streams, not a remotely durable per-record acknowledgement.

### Nested hot admission and bounded spill

With a 2 GiB input and 256 MiB bounded queues, node 0 reached the hot boundary
at 1,275.41 MiB/s with the full 256 MiB backlog outstanding, then took 246.8 ms
to drain; final completion was 1,104.17 MiB/s.  Node 1 actively admitted at
1,104.92 MiB/s.  All three SHA values matched.  The telemetry now separates
listener idle, active hot admission, queued bytes at the acknowledgement
boundary, and post-admission drain.

This test also found and fixed a telemetry error: node-1 listener wait had
previously been counted as hot-admission time.  `input_wait_seconds`, active
`hot_admit_seconds`, `hot_admit_wall_seconds`, and `spill_drain_seconds` are
now separate.

### Bounded cold lag and replay

The node-1 cold egress was temporarily limited to 400 Mbit/s.  With 256 MiB of
data and 32 MiB queues, both node 0 and node 1 reached exactly 32 MiB queued;
neither exceeded its configured memory budget.  Node 0 admitted at 79.10
MiB/s and drained for 637.8 ms.  Node 1 admitted at 54.82 MiB/s and drained for
698.8 ms.  All three images matched SHA
`85bac6a4bdb0b40d4bf0a307b6f5f9544c539b7f69f13a4eb5aba0b85c61aca0`.
The traffic-control qdisc was removed immediately after the run and the
original multiqueue/fq_codel arrangement was verified.

After deleting the cold terminal image, the surviving warm linear log replayed
to a restarted cold listener at 1,146.18 MiB/s and restored the same SHA.  That
byte-stream replay is correct but manual and whole-log; it has no remotely
persisted high-water handshake.

### Framed two-leg durable high water

The framed userspace WAL path completes only after both tmpfs `fdatasync`
high-water marks cover the window.  It does not issue an early-local ACK.
There is one worker and one lane, so per-worker QD and aggregate outstanding
depth both equal the reported window.  Each point is three repeats of 8,192
32 KiB records; every temporary local and remote log was independently graded
before removal.

The ceiling is matched to these semantics:

```text
ceiling = window /
    (window / slower_matching_terminal_record_rate + two_hop_TCP_RTT)
```

| per-worker QD / aggregate depth | mean IOPS | range | spread | matching ceiling | efficiency |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 6,529 | 6,466--6,583 | 1.80% | 12,582 | 51.89% |
| 2 | 10,529 | 10,496--10,592 | 0.92% | 24,121 | 43.65% |
| 4 | 15,549 | 15,252--15,731 | 3.09% | 44,437 | 34.99% |
| 8 | 19,576 | 19,534--19,629 | 0.48% | 76,869 | 25.47% |
| 16 | 22,342 | 22,133--22,547 | 1.85% | 121,075 | 18.45% |
| 64 | 24,658 | 24,544--24,721 | 0.72% | 212,961 | 11.58% |
| 256 | 24,606 | 24,542--24,685 | 0.58% | 261,088 | 9.42% |
| 1,024 | 24,619 | 24,561--24,684 | 0.50% | 279,083 | 8.82% |

Saturation begins around depth 64.  Separating ENA IRQs from CPU 4 tightened
the depth-64 spread from 5.12% to 0.72% and raised its mean by about 2.1%; it
did not move the roughly 24.6k-record/s single-lane ceiling.  The remaining
bottleneck is therefore the serial framed relay/control path, not RAM terminal
bandwidth or interrupt migration.

For recovery, the remote depth-16 log was truncated from HWM 8,192 to HWM
4,096.  On restart the first hop printed
`RACING_MIRROR_REPLAY_PASS from_hwm=4096 to_hwm=8192
payload_userspace_copy_bytes=0`, then the client recovered at 8,192 and
appended another 4,096 records.  Both legs independently graded 12,288 frames
with no incomplete tail.

### Live mid-window death

The remote leaf was killed by its recorded PID while a one-million-record,
depth-1,024 append was active.  The client failed with `ConnectionReset`; the
first hop failed with `BrokenPipe`.  Before restart, the local file contained
7,864 complete-record lengths plus a 64-byte header and the remote file 7,860
plus a header.  Neither partial prefix was acknowledged.

On restart, both scanners rolled back to the last completed durability window,
HWM 7,168, rather than treating structurally complete but unbarriered records
as durable.  A restarted client reported `recovered_from=7168`, appended 4,096
records, and both terminal logs independently graded 11,264 records with no
tail.  This proves fail-closed barrier recovery under a real process death,
not only an offline clean-boundary truncation.

### Lane-count scaling

Independent lane-local high-water machines were composed in parallel.  Each
lane had its own client, first-hop, remote-leaf process, port pair, terminal
log, sequence space, and CPU; there is no shared kernel placement owner.  Each
point is three repeats of 8,192 32 KiB records per lane at per-worker QD 256.
Every local and remote log was graded before removal.

| lanes / workers | aggregate depth | mean aggregate IOPS | range | spread | payload rate |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 256 | 24,606 | 24,542--24,685 | 0.58% | 6.450 Gbit/s |
| 2 | 512 | 48,487 | 48,345--48,560 | 0.44% | 12.711 Gbit/s |
| 3 | 768 | 54,592 | 53,768--55,886 | 3.88% | 14.311 Gbit/s |
| 4 | 1,024 | 53,868 | 52,430--54,954 | 4.69% | 14.121 Gbit/s |

Two lanes preserve 98.5% of ideal two-lane scaling and are the efficient
default for this eight-CPU node.  Three lanes reach the network envelope.  A
fourth lane adds contention and slightly regresses the mean, consistent with
the instance's advertised `up to 15 Gbps` network limit.  Lane count should
therefore be topology-selected rather than equated with CPU count.

### Random look-aside and cold fallback

A strict one-lane block-framed conformance topology used node 0
`zcnblk-send` -> node 1 userspace WAL fan/dirty cache -> node 2 userspace
`zcmem` terminal leaf.  The node maps were client CPU 2, fan CPU 3, leaf CPU
2 in a separate host domain; placement and cache ownership remained on node 1.

For random `write-read-same`, the client verified all 8,192 4 KiB reads
(32 MiB) with matching checksums.  The fan reported 8,192 leased cache hits,
32 MiB leased bytes, zero materialized frames/bytes, and 32 coalesced response
`writev` calls.  The remote leaf reported 32 MiB written and exactly zero read
bytes.  This is the desired hot look-aside behavior.

For random `write-sync-read`, the volatile leaf first refused `SYNC`, proving
the durability guard is fail-closed.  A second RAM-only conformance explicitly
enabled `ALLOW_VOLATILE_SYNC`; its log loudly states that process/host loss can
lose acknowledged data.  The client again verified 32 MiB.  The remote leaf
reported 32 MiB written plus 32 MiB read, proving the post-sync cold read path,
while the fan retained zero-copy leased overlays and materialized zero payload
bytes.  This is read-path conformance, not persistent durability or a
representative IOPS result; the gp3 check is the only persistent result here.

## Small gp3 persistence conformance

A terminal-only 64 MiB fio run used 1 MiB direct io_uring writes, QD1, SHA-256
verification, and an fsync after every write.  It completed with `error=0`,
99.84 writes/s, mean fsync latency 2.169 ms, and p99 2.572 ms.  This is a
correctness result only; root gp3 is intentionally excluded from the hot
performance path.  The copied 64 MiB payload was removed after its recorded
SHA was verified; the fio JSON and hash remain.

The framed two-leg high-water path was also exercised against ordinary
terminal files on the node 1 and node 2 root ext4/gp3 filesystems.  Each clean
repeat appended 8,192 4 KiB frames at window 64, waited for both terminal
`fdatasync` completions at every barrier, and independently scanned and hashed
both logs before the payload files were removed.  Repeats r1, r3, and r4 took
0.730799 s, 0.714671 s, and 0.916717 s: mean 10,536 records/s (41.16 MiB/s),
range 8,936--11,463 records/s, spread 23.98%.  This is persistent correctness
and variability evidence, not a representative performance result.

Repeat r2 is explicitly excluded.  Listener/client orchestration accidentally
queued two clients; the surviving client completed after 105.380513 s and its
empty verifier logs do not constitute an independently graded repeat.  The
three accepted repeats have nonempty local and remote verifier logs.

## Refinement decisions and open work

1. Keep the fast hot-admit byte path for admission/throughput, but do not call
   it durable: TCP close and pipe completion are not a terminal high-water ACK.
2. Reuse the framed high-water handshake and missing-suffix recovery contract
   for durable tier spill.  Whole-log manual replay is insufficient.
3. Preserve the splice/tee data plane; high-water, barriers, and placement stay
   small control metadata outside payload movement.
4. Use two framed lanes as the efficient `c8g.2xlarge` default and three only
   when maximizing wire rate.  Four lanes are counterproductive on this
   15-Gbit/s topology.  Keep lane high-water and replay state independent.
5. Reuse the WAL fan's proven lane-local leased look-aside and remote fallback
   in the framed tier protocol.  The behavior exists and passed over three
   hosts, but the standalone racing-high-water prototype still has no read
   messages.
6. The clean persistent terminal path now passes small ext4/gp3 conformance.
   RAM testing covers live mid-window death, partial tails, lag, and bounded
   overflow; a forced mid-barrier crash-and-restart against persistent leaves
   remains the next targeted durability test.

The run was stopped early after the architecture questions had converged, to
avoid paying for idle spot capacity.  It therefore refines the design and
records bounded conformance/performance evidence; it does not claim exhaustive
six-hour soak coverage.
