# Agent Instructions

- Never implement mirroring or striping with a block device.
- Do not use `/dev/zcbrdN`, `/dev/nullbN`, `/dev/ramN`, dm, md, loop, or a custom block module as a mirror or stripe primitive.
- `/dev/zcnblk0` is the client block edge only.
- `zcnblk-target zcwal:...` is a userspace WAL socket onramp, not a block stripe/mirror backend.
- zcnblk client request batching may coalesce write payloads before a userspace WAL write, but it must not make placement, stripe, mirror, tier, or spill decisions.
- Userspace RAID primitives own mirror, stripe, spill, placement, lane selection, locality, and backpressure.
- TCP mux carries lane-aware frames between userspace RAID stages.
- Block devices may appear only as terminal leaf media behind a userspace leaf writer after userspace placement has already been decided.
- High-IOPS mux/block benchmarks must be topology-explicit: warn loudly when hugetlb, memlock headroom, worker/kthread CPU pinning, hctx affinity, batching, or io_uring fast-path knobs are missing.
- Do not accept benchmark numbers as representative unless the run states its lane-to-worker and lane-to-CPU mapping.
