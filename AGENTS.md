# Agent Instructions

- Never implement mirroring or striping with a block device.
- Do not use `/dev/zcbrdN`, `/dev/nullbN`, `/dev/ramN`, dm, md, loop, or a custom block module as a mirror or stripe primitive.
- `/dev/zcnblk0` is the client block edge only.
- `zcnblk-target zcwal:...` is a userspace WAL socket onramp, not a block stripe/mirror backend.
- zcnblk client request batching may coalesce write payloads before a userspace WAL write, but it must not make placement, stripe, mirror, tier, or spill decisions.
- Userspace RAID primitives own mirror, stripe, spill, placement, lane selection, locality, and backpressure.
- A userspace RAID primitive may be co-located on the client host or described as client-side; it must remain a separate userspace stage after the `/dev/zcnblk0` edge, and the kernel client must not own placement.
- TCP mux carries lane-aware frames between userspace RAID stages.
- Block devices may appear only as terminal leaf media behind a userspace leaf writer after userspace placement has already been decided.
- High-IOPS mux/block benchmarks must be topology-explicit: warn loudly when hugetlb, memlock headroom, worker/kthread CPU pinning, hctx affinity, batching, or io_uring fast-path knobs are missing.
- Under `URING_PLAY_TOPOLOGY_STRICT=1` or `URING_PLAY_TOPOLOGY_FATAL=1`, topology/performance preflight problems must be fatal before representative benchmark numbers are printed.
- Do not accept benchmark numbers as representative unless the run states its lane-to-worker and lane-to-CPU mapping.
- Treat both the QD1/QD2/QD4/QD8/QD16 latency-efficiency curve and the very-high-QD random read/write saturation curve as performance north stars. Low-QD reports must state per-worker QD, worker/lane count, aggregate outstanding depth, measured raw transport RTT, the matching theoretical IOPS ceiling, and actual/theoretical efficiency; never label a run only "QD1" when aggregate depth is greater than one.
- Match theoretical ceilings to completion semantics: report remote reads, remotely acknowledged writes, early local write acknowledgements, and sync/FUA drains separately instead of comparing them all with one network-RTT denominator.
- Treat local benchmarks as shared-system measurements unless proven otherwise: repeat runs when results matter and report spread because CPU and memory bandwidth contention may be hard to observe.
- Before using `pkill -f`, `killall`, or other process-pattern cleanup, verify the pattern cannot match the current shell, harness, or Codex command text; prefer PID files or bracketed patterns and check with `pgrep -af` first.
