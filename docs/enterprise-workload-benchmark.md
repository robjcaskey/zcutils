# Enterprise Mixed Workload Benchmark

`zcworkload` is a deterministic, destructive block workload for comparing
latency, IOPS, throughput, completion semantics, topology, and context-switch
costs across conventional synchronous I/O and io_uring. It models a stable mix
of uniform, reusable-hot-set, sequential, and journal traffic instead of tuning
one synthetic operation for a headline number.

The benchmark is not a RAID stage. `/dev/zcnblk0` is the only supported network
block edge. Any mirror, stripe, tier, spill, placement, or backpressure stage
must remain in userspace after that edge. Block devices may only be terminal
leaf media after userspace placement has already been decided.

## Workload Shape

The active capacity is divided into 45% data, 45% user, and 10% journal areas.
Eight deterministic streams generate about 39.6% reads and 60.4% writes. The
logical transfer distribution is dominated by 8 KiB requests and also includes
4, 16, 32, and 64 KiB requests. Offsets and lengths are 4 KiB aligned.

A logical operation may span several block-queue requests. For example, a 64
KiB logical operation becomes at most 16 queue requests when `/dev/zcnblk0`
advertises a 4 KiB `max_hw_sectors_kb` limit. `zcworkload` counts and times the
application-visible logical operation, while its plan reports both the queue
limit and maximum fragment count.

## Build And Validate

```bash
cargo build --release --bin zcworkload
target/release/zcworkload sample \
  --capacity 1G --requests 1000000 --seed 42 --json
cargo test enterprise_workload::tests --lib
```

The sample command does not open a device. Use it to confirm the generated
distribution, alignment, and bounds before destructive testing.

## Required Topology

Representative network runs must provide:

- one lane entry per workload worker, such as
  `lane0:worker0:cpu4,lane1:worker1:cpu5`;
- one client-kthread entry per lane, such as
  `kthread0:cpu12,kthread1:cpu13`;
- explicit worker and transport-thread affinity;
- zcnblk hctx affinity;
- enough memlock and hugetlb capacity for the selected buffers;
- the measured raw transport RTT for low-depth efficiency reporting;
- the io_uring fast-path knobs reported by preflight.

Set `URING_PLAY_TOPOLOGY_STRICT=1` or
`URING_PLAY_TOPOLOGY_FATAL=1` to make a missing requirement fatal before any
result line. Do not publish a run as representative when the plan or result says
`topology_preflight_passed=false` or `representative=false`.

## Example Runs

These commands destroy data in the selected capacity. The target safety checks
accept the repository's synthetic devices and exactly `/dev/zcnblk0`; real
media additionally requires the existing PARTUUID allowlist and raw-write
confirmation gates.

```bash
export URING_PLAY_TOPOLOGY_FATAL=1
export URING_PLAY_PIN_CPUS=1
export URING_PLAY_PIN_CPU_LIST=4,5
export URING_PLAY_ENTER_NO_IOWAIT=1

target/release/zcworkload run \
  --target /dev/zcnblk0 --capacity 1G \
  --engine uring --workers 2 --depth 8 --ring-entries 256 \
  --completion-batch 8 --duration 10 --buffers hugetlb --pin true \
  --lane-map lane0:worker0:cpu4,lane1:worker1:cpu5 \
  --kthread-map kthread0:cpu12,kthread1:cpu13 \
  --completion remote-ack --transport-rtt-ns 16000 --repeats 3
```

For a blocking-I/O comparison, retain the same target, capacity, affinity, and
completion contract, then use `--engine sync --depth 1 --completion-batch 1`.
Use `--rate IOPS` for open-loop pacing or leave it at zero for saturation.

## Reading Results

`zcworkload-plan` records the exact depth, aggregate outstanding work, logical
sizes, queue-fragment limit, buffer mode, completion contract, and mappings.
`zcworkload-result` reports logical IOPS and bandwidth. Completion lines split
remote reads, remote-ack writes, local-ack writes, durable writes, and explicit
sync/FUA drains. Latency is measured from logical submission to logical
completion. Context output reports voluntary and involuntary switches per 1,000
logical operations. Three or more local repeats produce min/median/max spread
because this host is shared unless proven otherwise.

Never compare local-ack writes to an RTT-derived ceiling. For a mixed run, do
not use one theoretical IOPS denominator for operations with different
completion contracts. Use separate low-depth read, remote-write, local-write,
and sync/drain controls when deriving efficiency.

## Current KVM Proof

The two-VM proof in
`bench-results/zcworkload-qemu-logicalsplit-20260712T2345Z/` completed mixed
4-64 KiB logical I/O through this topology:

```text
/dev/zcnblk0 -> userspace shared-memory WAL onramp -> WAL-TCP -> userspace zcmem leaf
```

It used no RAID stage and no terminal block media. The run proved generation,
queue splitting, synchronous I/O, io_uring I/O, early write acknowledgements,
remote reads, and end-to-end completion. Its 63-83 synchronous QD1 IOPS and
114-125 io_uring aggregate-QD4 IOPS are diagnostics, not baselines: small-page
buffers and one unpinned WAL owner made topology preflight nonrepresentative,
and remote reads showed roughly 40 ms stalls while the leaf's measured compute
and copy phases consumed only about 0.13 seconds in total. Diagnose the
client/onramp wait and wake path before spending a cloud performance cycle.
