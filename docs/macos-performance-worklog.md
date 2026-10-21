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

The current candidate keeps complete-header receives and vectored read replies,
and restores per-request payload allocation. Buffer reuse was withdrawn after
the isolation results below; the early buffer/header figures describe historical
prototypes, not the current implementation. The cleaned-up source passed seven
Linux leaf tests; native Mac rebuild/confirmation follows.

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

Vectored RAM confirmation completed ten cases with three samples per revision:
six faster across observed spread, two slower, two inconclusive, no short trials
or invalid RTT references. The remaining slower cases were 64 KiB writes with
one lane/worker, per-worker QD8 (aggregate 8; -7.8% median), and 1 MiB verified
reads with one lane/worker, per-worker QD64 (aggregate 64; -6.3%). Vectored replies
improved small reads but did not remove all regressions when combined with
buffer reuse. The 1 MiB per-worker-QD256 read case remained inconclusive across
spread (-6.9% median). All runs remain exploratory and OS-scheduled. The PR
stays draft; the queued variant without buffer reuse is still necessary.

A supplementary Linux guest is queued **after** the Mac benchmark suites. This
uses the existing M1's reported virtualization support and pinned Lima v2.2.1 /
Ubuntu 26.04 minimal ARM64 image hashes. Configuration: two vCPUs, 2 GiB RAM,
an 8 GiB ordinary disk image, no host-directory mounts, private `vzNAT`, no
automatic guest-port forwarding, and no container runtime. All guest state stays
under this task's directory on the already-mounted internal Data volume. The
Mac lease remains unchanged and no additional EC2 instance is provisioned.

The Linux portable target, probe and client cross-built from the full repository
with the locked dependencies for `aarch64-unknown-linux-gnu`; crypto-policy
initialization is unchanged. `ZCUTILS_DISABLE_LIBFABRIC=1` explicitly produces
TCP-only helpers for this supplementary path; these binaries cannot serve as
EFA regression evidence. ELF hashes were checked after upload. Guest boot and
network runtime remain unverified until the queued setup executes. Host/guest
measurements will share the M1 CPU/memory and use a virtual NIC; the requested
physical cross-host and c8gn TCP/EFA checks remain separate and pending.

The pinned guest setup follows Lima's official VZ networking and image
configuration: https://lima-vm.io/docs/config/network/vmnet/ and
https://github.com/lima-vm/lima/blob/v2.2.1/templates/_images/ubuntu.yaml.

The vectored prototype's EBS-backed WAL confirmation also completed 216 trials
and all six full-image reopen/readbacks. Across 36 cases, 11 were faster across
observed spread, two slower, four too short and 19 inconclusive; no invalid RTT
references were found. The two slower write-ACK cases were one lane/worker:
4 KiB at per-worker QD16 (aggregate 16; -14.5% median), and 64 KiB at QD1
(aggregate 1; -16.6%). WAL reset/reducer/backpressure and shared-system phases
can affect these measurements; the observed ranges do not establish a root
cause. These additional regressions reinforce keeping the PR draft and testing
the framing-only variant. All raw data/calibrations/readbacks are retained
locally, with backing images excluded from collection.

The internal-SSD suite has now started with the same immutable baseline and
vectored binaries, first for ordinary files and then WAL, under the existing
mounted Data directory. Storage class remains explicit so EBS and internal-SSD
measurements are not mixed in a single acceptance comparison.

The internal-SSD ordinary-file suite completed 216 trials and all six full
64 MiB sequential readbacks. All 36 comparisons met the one-second gate and had
valid transport references: 14 faster across observed spread, one slower and
21 inconclusive. The slower case was 64 KiB write-plus-durable-SYNC, four lanes /
four workers at per-worker QD4 (aggregate 16; -11.9% median). Its matching durable
completion ceiling/efficiency remain unavailable; clean-SYNC timing is only a
separate optimistic reference. Source binary hashes matched the same baseline
`ce5f81ed...` and vectored candidate `f2f101c3...` used in the preceding tests.
Raw data, calibration records, storage location and readbacks are retained
locally. The internal-SSD WAL suite is running next. No overall performance win
or physical Linux transport regression pass is inferred from these results.

The internal-SSD WAL suite completed 216 trials and all six full-image
reopen/readbacks. Its 36 comparisons show 11 faster across observed spread,
none slower, five too short and 20 inconclusive. All eleven faster cases are
verified reads. The framing-only variant (header receive + vectored replies,
per-request allocation restored) then passed all seven native Mac leaf tests,
built successfully, and entered alternating RAM confirmation. Its binary SHA256
is `5974b37b9bba9f08679ae70c8f007a7a650153eeb3ee272f18f701ee204bb640`;
the repository source and original release binary were restored after building.

The WAL's commit path performs ordered payload and durable-tail-superblock
drains. macOS uses `sync_data` plus `F_FULLFSYNC` for each drain; preserving this
ordering is required for the existing crash-recovery contract. Descriptive
same-binary observations across EBS/internal SSD are retained separately, with
explicit shared-system/time/cache confounders and duration gates. They are not
a controlled causal storage comparison or a reason to weaken flushing.

The framing-only isolation completed all ten configurations with three samples
per revision: four faster across observed spread, none slower, six inconclusive,
with no short trials or invalid RTT references. Both 1 MiB read cases remained
within spread (+1.1% and +1.7% median at one lane/worker, per-worker QD64/QD256).
The implementation now removes per-connection scratch state and retains the
framing changes. Seven Linux leaf tests passed after this change; payload
isolation test naming now describes its invariant rather than buffer reuse.
The Linux VZ guest booted and executed the cross-built full-source target;
its kernel is Ubuntu 7.0.0-38-generic ARM64 and private IP 192.168.64.2.
It was stopped cleanly for the native final-source build and to explicitly
ignore automatic TCP **and** UDP port forwarding before direct private-IP tests.

The cleaned-up implementation at `634f33ea` passed all seven native Mac leaf
tests and rebuilt successfully for both macOS ARM64 and Linux ARM64. Its Mac
binary SHA256 is `387445373bf7e22d84ae1569b0e704874d9bcd145d137892444a3c9da66d4b91`.
A direct private-vNIC guest/host smoke then passed 30 trials in **each** direction
(read verification and write ACKs, one lane/worker, per-worker QD1/2/4/8/16,
three repeats). Both directions charged 40,939,008 protocol bytes; no invalid RTT
references were found. The guest uses Ubuntu 26.04 / kernel 7.0.0-38 ARM64 with
two vCPUs and 2 GiB on the same physical M1; the data path is guest lima0
192.168.64.2 to Mac bridge100 192.168.64.1, MTU1500. Lima control is separate from
the measured TCP data path. No host-directory mounts or automatic TCP/UDP port
forwarding are used. The guest's temporary staging directory was recreated after
reboot, and final helper hashes were reverified before testing.

These bursts are only milliseconds long, explicitly nonrepresentative and
labeled with the shared-host VZ topology in every row. They prove mixed-OS wire
compatibility and bounded-pipeline correctness; they do not replace sustained
performance confirmation or the outstanding physical c8gn TCP/EFA regression.
Full raw results, topology, source/binary manifests and logs are copied locally.

The standard Linux block edge exposed a pre-existing direct-TCP admission bug:
`queue_rq` required SHM daemon state even for TCP, rejecting every request with
EIO before it reached the Mac. TCP also consulted SHM ordering state. The fix
caches the validated transport at initialization and applies daemon/epoch checks
only to SHM; the chart-packaged module source stays identical. This is isolated
in draft PR #6 (`codex/tcp-admission-guard`, commit `5c7fc19b`); performance PR #5
is based on that branch so the functional fix can be reviewed separately.

The fixed module compiled natively against ARM64 Ubuntu 7.0.0-38 and against the
local x86 7.2 headers. `scripts/test-zcnblk-client-transport.py` passed actual
kernel tests for rejected SHM epochs=0, offline-SHM read/write EIO, and TCP with
SHM epochs enabled completing 8 MiB CRC write/read plus visibility flush. These
checks are correctness-only and use owned userspace RAM leaves. Existing host
block devices were never loaded, unloaded, mounted or written by these tests.

The guest `/dev/zcnblk0` then passed 36 two-second fio cases through the Mac's
64 MiB RAM leaf: 4 KiB random read/write at per-worker QD1/2/4/8/16/256, one lane,
one worker, three repeats, plus initial/final CRC readbacks. All trials reached
at least two seconds and all RTT-reference comparisons were valid. fio ran on
guest CPU0, connection worker on CPU1; hctx0 covered CPUs0/1. Physical VZ CPU
placement and Mac threads remained OS-scheduled. Missing hugepages/reserved
memlock, batch_depth=1 and no SQPOLL were warned explicitly; registered io_uring
files/buffers were enabled. The guest kernel also reported a suspect-GRO warning
for lima0. These remain shared-host observations, not physical TCP/EFA evidence.

The failed original module/run, patched source hashes, corrected fio results,
CPU/hctx mappings, RTT/ceiling/efficiency ranges and plots are retained locally.
The guest module was unloaded and the guest stopped before final-source Mac
RAM confirmation. Do not compare a broken direct-TCP baseline's zero IOPS with
the restored path as a performance gain; hold this admission fix common to
before/after performance controls. Persistent TCP FUA capability/flush semantics
still need a dedicated audit before persistent-storage acceptance; current
kernel runtime tests used volatile RAM and make no durability claim.

The exact cleaned source/binary (`634f33ea`, Mac SHA256 `38744537...`) completed
its own final ten-case A/B/B/A/A/B confirmation with the Linux guest stopped.
Four cases were faster across observed spread, none slower, six inconclusive;
no short trials or invalid RTT references occurred. Both 1 MiB read cases stayed
within spread (+2.2% at one worker/lane per-worker QD64, +0.2% at QD256).
This confirms the focused cleanup result; full-matrix and physical-host
performance coverage remain pending. The canonical raw trials and comparison
were copied locally before proceeding to the persistent-TCP capability audit.

The persistent-TCP audit found two real completion-contract bugs, fixed in PR #6
at `469c6625` and merged into this feature branch. Raw block RWF_DSYNC initially
passed, so it was not used as evidence of a native-FUA failure. An ext4 O_DSYNC
file write did expose the failure: tracefs recorded a native-FUA journal commit
(`WFSM`, sector 184), but TCP sent only WRITE offset 94208 after the preceding
SYNC and returned success without a post-write SYNC. Advertise native FUA only
for SHM; TCP now gets Linux's postflush emulation. The final RAM fixture passes
raw SYNC ACK withholding, ext4 journal postflush and unmount/remount readback.

A second fixture received but deliberately held a write unapplied on one TCP
connection. With early local ACKs, fsync on the other connection returned success
while that write remained held. Flushes in this multi-connection, early-ACK mode
now fail with EIO. An intermediate EOPNOTSUPP implementation was tested and
rejected: raw block fsync converted it into success. The final EIO regression
passes. This preserves ordinary volatile writes and SHM's userspace WAL FUA
contract; no placement/mirror logic or weaker WAL drains were introduced.

The final module again compiled on ARM64 Ubuntu 7.0.0-38 and local x86 7.2 headers.
It passed the transport regression (SHM epochs required, offline-SHM read/write
EIO, TCP 8 MiB CRC read/write plus fsync) after both corrections. All fixtures
operate on a newly created userspace RAM target inside the owned Linux VZ guest;
its module was unloaded afterward. RAM protocol tests do not establish physical
persistence or a global durable fence across independent target processes/media.
The detailed procedure and limits are in `docs/zcnblk-tcp-correctness.md`.

At 12:19 UTC the Linux guest was stopped and an exact-binary 84-configuration
A/B/B/A/A/B Mac RAM comparison started. `bench-portable-abba.py` now offers
`--all-configurations` to reuse its fixed-operation-count approach over every
configuration in a prior comparison. The baseline/final target and unchanged
probe hashes were verified before launch. Full results remain pending; duration
and RTT validity gates still apply. No extra EC2 instance was created, the Mac
termination deadline is unchanged, and desktop workflows remain disabled.

The remaining exact-source validation is serialized to avoid adding guest load
to native Mac measurements. After the full RAM run, the final target is queued
for internal-SSD file and WAL comparisons (36 cases per backend, three alternating
samples per binary, four-second calibration target, six full-image restart
readbacks). Only ordinary task-owned backing files are involved. A collector
copies terminal results locally, excludes backing images, and preserves failures.

A full-source Linux ARM64 baseline also built successfully with locked
dependencies, libfabric disabled, and unchanged crypto initialization. Its
userspace source is identical to `a563ddf0`; the build worktree contains only
additional kernel/docs/tests changes. Binary SHA256 is
`7c1bd1f470379ca329f8129da56b25337075192d8bcf0700d63fa4aeec699be3`.
The final Linux helpers remain separately preserved with their earlier manifest.
A focused 16-case, six-cycle A/B/B/A/A/B comparison in each direction is queued
after the persistent suite. It uses direct Mac bridge100/Linux lima0 TCP,
calibrates fixed operation counts from the first baseline sample, and retains
raw RTT, completion ceilings, efficiency, duration and spread. These will still
be shared-M1 VZ measurements with OS-scheduled CPUs and a two-vCPU guest, not
physical-host TCP or RDMA acceptance. Runtime results are pending.

The kernel correction at `469c6625` passed all 14 build targets in the kernel ABI
matrix (GitHub run `39598744555`, exact head SHA verified). These are compilation
checks; the isolated-guest integration fixtures remain the runtime evidence.
Live EC2 tags and EventBridge were rechecked: termination remains
2026-10-21T20:42:59Z and the five-minute cleanup schedule is enabled.

Persistent-leaf calibration now has a configurable minimum duration, defaulting
to one second rather than 250 ms. This reduces the risk of selecting a short
measured trial from an unusually cheap warm-journal sample; bounded growth and
operation/byte caps remain. The updated harness passed a same-binary Linux WAL
smoke with 36 trials and all six complete 64 MiB restart readbacks. This validates
the harness and readback path, not a Linux before/after performance comparison.
The queued Mac file/WAL harness and its recorded SHA were updated while that
supervisor was still verified waiting for RAM; no active trial was altered.
