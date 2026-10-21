# macOS performance work, October 21, 2026

The active work uses the existing M1 EC2 host until its original termination
at 2026-10-21 20:42:59 UTC. No renewal or recurring CI is involved.

## Placement and transport evidence

- Mac: `mac2.metal`, macOS 15.8, 8 CPUs, 16 GiB, eu-west-1a.
- Its active private Ethernet address is 172.31.13.96, MTU 9001.
- EC2's instance-type API reports 10 Gigabit networking, up to 8 interfaces,
  and `EfaSupported=false`. No extra NIC is needed just to reach a Linux peer
  in the same VPC; additional NICs would not add RDMA support to this host.
- Apple's supported [Thunderbolt RDMA path](https://developer.apple.com/documentation/technotes/tn3205-low-latency-communication-with-rdma-over-thunderbolt)
  requires macOS 26.2 and Thunderbolt 5 Apple silicon hardware. This M1 has
  Thunderbolt 3 and cannot exercise that path.
- The current AWS account inventory has no running/stopped c8gn pair in its
  enabled regions. Saved August inventories are not evidence of live nodes.
  Linux TCP/EFA regression and same-region cross-host runs await a verified
  pair or an agreed temporary-pair budget; they are not claimed complete.

## Portable probe

`zcnblk-net-bench` uses the existing 64-byte version-2 wire header. It has a
bounded pipeline per connection and verifies correlation/topology on every
response. `read-verify` checks a deterministic byte-position pattern. A worker
owns one connection with dedicated sender/receiver threads. CPU scheduling is
explicitly OS-controlled. The target has one OS-scheduled thread per connection.
There are no placement decisions in either client or terminal leaf.

The probe reports per-worker QD, worker/lane count, aggregate outstanding limit,
completion latency, measured TCP echo RTT, a corresponding transport ceiling,
and actual/theoretical efficiency. Ordinary write ACKs and final SYNC/drain time
are separate. RAM SYNC remains a visibility operation, never durable storage.

These are **exploratory shared-system userspace TCP measurements**, not
representative high-IOPS kernel-block/mux results. The program warns about
missing pinning, hugepages, memlock reservation, batching and io_uring; either
strict/fatal topology environment flag makes it fail before benchmark output.
A separate warmed echo records minimum/median/maximum RTT. The repeated harness
uses the minimum as an observed transport reference floor and flags efficiency
ratios above one as an invalid reference comparison. An unpinned echo thread's
median can be slower than another service's round trips, so such a median must
not be presented as a hard transport limit.

Build the probe without the rest of the crate if desired:

```sh
rustc --edition=2024 -O src/bin/zcnblk-net-bench.rs -o /tmp/zcnblk-net-bench
python3 scripts/test-net-bench.py /tmp/zcnblk-net-bench
```

For a local matrix, build `zcnblk-portable-target` in release mode, then run:

```sh
python3 scripts/bench-portable-tcp.py \
  --probe /tmp/zcnblk-net-bench --target target/release/zcnblk-portable-target \
  --out /tmp/zc-tcp-baseline --seconds 3 --repeats 3
```

The harness creates only disposable loopback RAM/echo services, records child
output, owns exact child PIDs, and terminates those children afterward. The
matrix covers 4 KiB/64 KiB/1 MiB, 1/4 workers, per-worker QD 1/2/4/8/16/64/256,
random writes and verified reads. It reports repeated-run spread. These probes
must not replace the existing topology-strict Linux RDMA/mux regression suites.

## Candidate target changes

The first candidate keeps one bounded scratch buffer per connection, avoiding
allocation/zeroing/free on every request. Its retained memory is bounded by the
16 MiB maximum request and the existing 64-connection cap, and released when
that connection closes. Every read and accepted write overwrites its entire
exposed slice. SYNC preserves the allocation without exposing payload bytes.

It also reads as much of the 64-byte header as TCP provides initially, avoiding
the unconditional one-byte read followed by another header receive. Fragmented
headers and clean EOF/truncated-header distinctions remain supported.

Five Linux terminal-leaf tests pass, including WAL recovery, file reopen,
fragmented/truncated header handling, and alternating request sizes around SYNC.
Mac before/after performance and native regression results remain pending until
the recorded runs complete. No performance improvement is claimed yet.
