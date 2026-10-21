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

## Initial complete RAM matrices

Both source revisions completed 84 configurations and 252 trials each using
identical client binary `5d59f5304e8f12e9e499dc19353f97e4c4c4469b4a5703d43ce47ecd4393b980`.
The first candidate is `7b67459a579ebeb18272170ba127c36f31195f60`; its target
binary is `d59a6600cea555332b25a39c14818b2d0f5a9ecf38d7adb7bc1a17fe46a6bdfb`.
Baseline target binary: `ce5f81ed20c94bb78f14720c18358d35443ffd49efb55869f3025496e950906a`.
Data verification passed throughout. Initial comparison found 29 cases faster
across the observed spread by over 5%, and one case slower by over 5%. This is
not sufficient to attribute gains or accept a regression: the long matrices
ran sequentially and large transfers hit a per-run byte cap, making some
trials shorter than the requested duration. Alternating confirmation uses
identical operation counts per case and longer runs, without that byte cap.
Results remain exploratory and unpinned.

## Persistent-leaf and completion semantics probes

The probe also supports `sync-rtt ADDRESS SAMPLES` and `write-sync`. The latter
sends an ordered WRITE/SYNC pair for each logical operation and returns pipeline
credit only after validating both acknowledgements. Reported QD counts logical
operations; `aggregate_wire_request_limit` additionally reports the two-frame
upper bound. `write-sync` requires a measured same-endpoint SYNC reference as
its last argument and either file/WAL durable semantics or explicit volatile
RAM semantics. An ordinary WRITE cannot be labeled as a durable completion.

A clean endpoint SYNC reference is an optimistic service-time floor. It does
not model dirty-data flush or WAL commit service time, so durable operations
report their matching theoretical ceiling/efficiency as unavailable and keep
the optimistic reference in a separate field. They are never compared to a
network-only RTT denominator as though that modeled durable completion.

`scripts/bench-portable-durability.py` alternates baseline/candidate file or
shared-WAL targets in A/B/B/A/A/B order, with fresh ordinary files per cycle.
It measures verified reads, write ACKs, and per-write SYNC separately, then
terminates and reopens each leaf to verify the entire image after acknowledged
SYNC. It retains logs/readback evidence and removes only its own disposable
backing files after successful validation unless `--keep-backings` is given.
Linux harness smoke checks passed for both file and WAL backends. Mac durable
results remain pending; do not infer them from RAM or Linux smoke results.

## Alternating confirmation and follow-up

The first A/B/B/A/A/B confirmation completed ten configurations with three
samples per source revision and identical operation counts. Five cases remained
faster across observed spread; the 1 MiB, one-worker, per-worker-QD64 verified
read remained slower (about 11% median). The PR stays draft. No overall Mac
improvement or Linux regression pass is claimed from these results.

Separate header-only and buffer-only builds passed all five native Mac leaf
tests. They are being measured independently against the original binary.
Stack samples of the problematic read case showed more socket send/receive
samples in the combined candidate, while its per-request zeroing disappeared.
This does not by itself distinguish kernel copy cost from socket waiting.
Sampled workloads are diagnostic only; their performance output is discarded.

A subsequent prototype combines one response's header/payload using vectored
writes. Linux tests cover interrupted writes, cross-slice partial progress, and
zero-progress failure in addition to the prior leaf tests (seven passed total).
Native Mac tests and performance results for that prototype remain pending.

The header-only alternating comparison completed with four cases faster across
spread and none slower across spread among its ten selected cases. Both large
read confirmation cases stayed within spread. Buffer-only isolation completed
with three cases faster and two slower across spread: 64 KiB, one worker,
per-worker QD8 writes (-6.8% median) and 1 MiB, one worker, per-worker QD256
verified reads (-11.5%). These exploratory results strengthen the case against
merging buffer reuse without further changes; they do not establish behavior
on other hosts.

The comparison tool now rejects trials shorter than one second for acceptance
classification. Applying that rule to the original full matrix marks twelve
cases as too short, while the longer alternating confirmation remains valid.
The persistent-leaf harness's future runs use a larger byte cap so fast cached
reads can reach their intended duration. Already-started runs retain their
actual elapsed times and must be assessed by those recorded durations.

The EBS-backed file suite completed all six cycles and full-image crash/reopen
readbacks. Of 36 comparisons, 21 fail the minimum-duration gate; the other 15
show no difference across observed spread. No file-performance gain is claimed.
The first WAL suite timed out after a short, fast calibration extrapolated to
187,282 writes and encountered slower sustained work. Partial results are
retained as failed-run evidence, not a completed comparison. Calibration now
grows by at most fourfold until it samples at least 250 ms and caps the final
extrapolation at fourfold, with every calibration recorded. Corrected runs use
new output directories; prior results are preserved.

The Mac's private en4 interface already exists in the Ireland VPC; another NIC
is unnecessary for a same-VPC Linux peer. Root/home benchmark files are on EBS.
A separate ordinary directory on the already-mounted internal SSD Data volume
is prepared for a storage comparison. No partitions, raw devices, EFI state,
mounts, or source-device flags were changed. Linux peer/account or temporary-pair
budget information is still needed before the cross-host tests can proceed.

## Bounded cross-host burst preparation

`scripts/bench-portable-cross-host.py` supports either client direction against
an independently started **disposable 8 MiB RAM leaf** plus the echo service on
the same remote host. It overwrites the entire specified leaf capacity. The
same-region check, numeric endpoint validation and protocol-byte budget run
before connections; strict topology mode fails before the RTT probe. Default
limits are 60 seconds overall and 10 seconds per probe. The plan includes
initialization and all headers/drains: 40,939,008 application-protocol bytes for
30 trials (three repeats of read/write at per-worker QD1/2/4/8/16, one worker).
TCP/IP overhead and retransmissions are outside that byte accounting.

Example after a private Linux peer exists, with an 8 MiB disposable Mac RAM leaf
on port 19620 and echo on 19621, restricted to that peer's private IP:

```sh
python3 scripts/bench-portable-cross-host.py \
  --probe ./zcnblk-net-bench --address 172.31.13.96:19620 \
  --echo-address 172.31.13.96:19621 \
  --local-region eu-west-1 --peer-region eu-west-1 \
  --network-path private --direction linux-to-mac --out ./linux-to-mac-bursts
```

Use `--plan-only` to inspect the budget without connections. For the reverse
direction, run the client on the Mac with the Linux leaf/echo addresses and
`--direction mac-to-linux`. Private addressing and region labels must match
actual routing and cloud inventory; argument labels alone do not prove either.
Short bursts are exploratory correctness/latency evidence, never representative
throughput or acceptance of the requested Linux TCP/EFA regression. Linux
loopback smoke passed all 30 verified trials and budget/region rejection checks;
that is harness validation, not a cross-host result.

For the pending Linux regression, the pre-desktop-port reference is
`5b2910174fb71a2df6ba675182b2366b0ffe7b51` (parent of the macOS storage port).
The Mac portable-leaf baseline `a563ddf0` already includes that port, and the
portable leaf did not exist at the earlier revision. Comparing only the two
portable-leaf binaries therefore cannot establish that the desktop port left
existing Linux performance unchanged. The common OFI Rust/C source blobs are
unchanged from the pre-port revision, but this is source-review evidence only;
shared-WAL adaptation, build/link effects, and actual hardware performance still
need the matching Linux tests and controlled TCP/EFA runs. Keep both references
and each build's lockfile/compiler/feature/binary identity in the run manifest.

The current Linux shared-WAL test filter passed 13 tests, including direct-I/O
buffer admission, durable recovery, unsynced-tail rejection, corruption detection,
reduction/reset and backing custody. This is correctness coverage on the local
Linux host, not performance evidence on c8gn hardware.

An additional private source variant keeps complete-header receives and vectored
read replies while restoring per-request allocation. It is queued after the
internal-SSD suites, with native leaf tests and the same ten-case alternating
RAM confirmation. It records source/binary hashes and restores the repository
source and release binary in `finally`; it does not change the PR implementation.

Comparison summaries now retain the completion semantics, backend/access pattern,
lane count, CPU mapping, and the min/median/max raw RTT, matching ceiling and
actual/theoretical efficiency for each variant. Durable SYNC's matching ceiling
and efficiency remain null; its measured SYNC reference stays separate. Unlike
completion/backing/access-pattern comparisons are rejected, and impossible RTT
efficiency is flagged instead of being classified as an accepted gain. Existing
RAM classifications were unchanged; checks covered rejected backing mismatches,
invalid RTT references and unavailable durable ceilings.

The corrected EBS-backed WAL suite completed 216 measured trials (36 cases per
variant, three repeats) and all six full-image crash/reopen readbacks. Four
comparisons were faster across observed spread, none slower, and five failed
the one-second duration gate. The other 27 were inconclusive across spread.
These are exploratory shared-system results for the original buffer/header
candidate; they do not resolve its known RAM large-read regression. Corrected
WAL raw data, calibration records, readback proofs and logs were copied locally;
no backing images were included.

The vectored prototype built successfully on the M1 and all seven native leaf
tests passed. Its immutable binary SHA256 is
`f2f101c3d4431d9d84241ce7771a8578c3428b5396d968954a30f2342e6f0c92`
(source revision `66700841`; benchmark/report-only changes after the framing
prototype). Alternating RAM confirmation is running before its WAL comparison.
