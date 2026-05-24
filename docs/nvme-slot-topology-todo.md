# NVMe io_uring Slot Topology TODO

Goal: turn the current raw io-slot WAL path into a CPU/NVMe-local topology that
uses the newest slot APIs cleanly and can be tested against real hardware.

## 1. Map CPU to NVMe Queue to io_uring Ring

- Read NVMe queue topology from `/sys/block/<dev>/mq/*/cpu_list`.
- Read device NUMA node from `/sys/block/<dev>/device/numa_node`.
- Add a `slot-topology-plan` command that prints worker CPU, NVMe queue,
  io_uring ring, and WAL region assignments.
- Prefer one ring per worker CPU, aligned with the queue CPU mask when possible.
- Acceptance: planner output shows no accidental cross-NUMA workers unless
  explicitly requested.
- Status: implemented in `slot-topology-plan`; the TCP ZCRX WAL path also reads
  the target block topology and pins each worker to the planned CPU/NVMe queue
  when worker pinning is enabled.

## 2. Add Per-Queue Slot Allocation Policy

- Extend `slot-wal-sharded-bench` so each worker owns its own registered buffer
  set, file slot registrations, and WAL offset region.
- Keep slot IDs local to the worker/ring that submits them.
- Add validation that no two workers write overlapping WAL ranges.
- Acceptance: sharded WAL run reports per-worker slot count, region base,
  region length, and no overlap.
- Status: implemented for the linear sharded WAL layout.

## 3. Make Buffer Allocation NUMA and Device Local

- First-touch buffers after worker CPU pinning.
- Add optional `numactl`/`mbind` policy for WAL workers when the target device
  exposes a NUMA node.
- Keep hugepage and small-page modes, but report actual alignment and segment
  size for each worker.
- Acceptance: bench output includes worker CPU, NUMA node, buffer mode,
  alignment, and whether affinity was applied.
- Status: first-touch after pinning and reporting implemented. Optional
  preferred-node `mbind` is available with `URING_PLAY_WAL_MEMBIND=1`; the host
  ZCRX WAL smoke enables WAL and ZCRX memory binding by default for the
  synthetic `/dev/nullb0` run.

## 4. Track Jens’ New liburing Slot Helpers

- Add a local compatibility layer for the slot APIs because upstream liburing
  does not expose helpers yet.
- Keep the raw-uapi implementation underneath so the project builds on kernels
  and liburing versions without upstream helpers.
- Add a probe line that distinguishes kernel slot support from liburing helper
  support.
- Acceptance: `probe` reports both kernel io-slot availability and liburing
  helper availability, and the WAL bench logs which path was used.
- Status: implemented as `uring-play/io-slot-v0(raw-uapi)`. Test first with
  `qemu-zcrx/slot-compat-qemu-smoke.sh`; do not move to bare metal until the
  QEMU null_blk smoke passes.
