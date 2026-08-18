# Userspace partitioning and live volume migration

`volume_partition` owns logical partitions after the `/dev/zcnblk0` client
edge. It does not put placement, migration, mirroring, striping, or spill
policy in the kernel client. Backings accepted by this implementation are
regular-file terminal leaves; a staged migration destination is not a
durability witness.

The logical-offset path uses an immutable userspace placement layout sorted by
byte range. Topology changes build a replacement layout and publish it with one
atomic swap. The generic volume API pins the current layout with an `ArcSwap`
guard; the high-IOPS `VolumeIoHandle` resolves and pins it once per userspace
lane/placement epoch. Both use binary range lookup and then enter the selected
leaf's independent atomic route. Neither takes the volume control `RwLock` nor
scans the manifest. A topology change explicitly refreshes lane handles; a
stale handle fails closed for retired routes and cannot discover newly mapped
space. Pre-resolved `PartitionHandle`s remain available when an upstream lane
already owns a stable partition assignment.

Using multiple terminal files is intentional: each is a leaf chosen after the
client edge by this userspace placement stage. It is not block-device RAID.
Separating active lanes across leaf inodes avoids making every buffered write
contend on one inode's page-cache, dirtying, allocation, and writeback state.

## Geometry and lifecycle

- Volume capacity, partition starts, partition lengths, I/O, copy chunks, and
  resize boundaries are multiples of 4096 bytes.
- Partitions must not overlap and may not extend past volume capacity.
- Partitions can be added, grown, shrunk, or retired online. Shrink is rejected
  if it would cross another partition or the volume boundary.
- Retirement removes the route and fences previously issued handles, but
  deliberately retains terminal media. Media deletion is a later explicit
  action after custody release.
- The manifest uses write, `sync_data`, rename, and parent-directory `sync_all`
  publication. A process restart can reopen an interrupted migration and
  safely recopy its base.

## Live migration and snapshot cut

1. Provision and durably record a staged destination.
2. Copy the source in aligned chunks while reads and writes remain admitted.
3. Foreground writes increment one dirty generation per affected 4 KiB page
   after the normal source write. They do not duplicate payload into a private
   migration journal.
4. At seal, take the partition route write fence, recopy dirty pages, drain the
   destination, publish the manifest route (migration) or snapshot record, and
   release the fence.
5. For a move, publish the destination HWM and activate the caught-up topology
   handoff atomically. Only then release and eventually retire source custody.

On the production WAL path, the desired optimization is to pin the existing
retained log at migration start and replay its suffix into the destination.
That replaces dirty-page recopy without adding another foreground payload
write.

That path is now explicit. `MigrationTracking::RetainedWal` publishes no dirty
tracker in the foreground route. `PersistentWalRetention` pins the existing
linear WAL suffix across reducer progress, prevents reset/reclamation, and
coalesces repeated writes by logical page. A controller may replay durable
prefixes opportunistically with `replay_range_into`, advance its cursor, then
pass only the final suffix to `commit_migration_with_retained_wal` after the
atomic admission fence. The standalone regular-file path retains page
generations as a correctness fallback and can pre-drain them online with
`drain_dirty_pages_paced`.

The WAL start sequence is persisted in the pending-migration descriptor. While
a suffix is pinned, the WAL may apply it to the local base but publishes an
on-disk reduced HWM no later than the oldest retained frame. After a crash the
same suffix is therefore reconstructed and can be restored with
`pin_retained_from` before reduction resumes; reset remains blocked until the
pin is released.

Partition routes are immutable `ArcSwap` snapshots. Foreground I/O no longer
takes the old shared `RwLock`; begin tracking and final cutover atomically swap
routes and wait only for operations admitted through the preceding snapshot.

Copy pacing is available through `copy_migration_base_paced`,
`migrate_partition_paced`, and `snapshot_partition_paced`. Sleeping happens
between copy chunks while no route or manifest lock is held.
`copy_migration_base_controlled_with_method` supports a per-chunk feedback
controller and Linux `copy_file_range`, avoiding the userspace copy buffer when
the source and destination filesystem support it. The benchmark controller
targets 98% of the immediately preceding foreground baseline and reports its
rate adjustments. It also reports the actual cutover-fence duration separately
from total migration time.

NUMA-aware callers use `MigrationLocality` with the corresponding
`*_with_locality` migration and snapshot methods. The selected CPU and
expected NUMA node are persisted in the pending-migration descriptor, so crash
resume cannot silently move copy work to another socket. Destination
provisioning, dirty-map and copy-buffer first touch, base copy, final dirty
replay, and destination sync all run under a scoped affinity pin. Strict mode
fails before provisioning when the mapping cannot be proved. For tmpfs this
also places destination pages locally; terminal-device DMA locality must also
be checked against the device or NIC NUMA node by the topology controller.

## Benchmark

`zcvolume-live-bench` runs pinned random 4 KiB foreground reads/writes through
baseline, migration, a fresh baseline, snapshot, and recovery phases:

```text
ZCVOLUME_COPY_RATE_MIB_S=512 \
ZCVOLUME_MIN_COPY_RATE_MIB_S=64 \
ZCVOLUME_LEAF_PARTITIONS=8 \
URING_PLAY_TOPOLOGY_STRICT=1 \
zcvolume-live-bench /dev/shm 1024 8 0,1,2,3,4,5,6,7 2000
```

`ZCVOLUME_LEAF_PARTITIONS` divides the logical volume into equal contiguous
4-KiB-aligned userspace leaves. Benchmark workers issue logical offsets through
lane-local `VolumeIoHandle::read_page_at` and `write_page_at`; the harness does
not select a leaf handle on their behalf. Thus reported IOPS include the
production userspace range lookup and per-leaf migration route, but do not
repeatedly reload a placement epoch that is stable for the lane.

For native 4-KiB client requests, `read_page_at` and `write_page_at` retain the
same logical userspace placement but bypass general multi-segment splitting.
Uniform, contiguous, power-of-two leaf layouts compile to a shift-indexed
table; irregular layouts use binary range lookup. Larger requests continue to
split safely at leaf boundaries.

On a dedicated `c8gn.48xlarge`, 32 workers pinned to NUMA-node-1 CPUs 96–127,
32 equal tmpfs terminal leaves, aggregate depth 32, and copy CPU 128 produced
the following strict-topology confirmation spread:

- Baseline: 24.66M–26.16M 4-KiB operations/s.
- Migration active: 24.16M–25.29M operations/s.
- Migration efficiency: 96.68%–97.95%.
- One complete snapshot phase measured 24.74M baseline and 24.48M active
  operations/s (98.97%).

These are local terminal-memory completion results, not remotely acknowledged
writes or durable sync completions. The report emits the sum and maximum of
the 32 sequential per-leaf cutover fences separately; their sum is not a
single volume-wide hitch.

The same native path's per-worker-QD1 scaling curve (one lane and one terminal
leaf per worker) measured 1.95M, 3.00M, 5.30M, 9.75M, and 18.48M IOPS at
aggregate depths 1, 2, 4, 8, and 16 respectively; aggregate depth 32 measured
24.66M–26.16M. Worker and lane CPUs were explicitly mapped from NUMA-node-1
CPU 96 upward. Raw transport RTT and an RTT-derived theoretical ceiling are
not applicable to these local terminal completions.

`ZCVOLUME_COPY_CPU` may select a dedicated copy CPU. Otherwise the harness
chooses an unused CPU on the foreground workers' common NUMA node, falling
back to the first worker CPU only if no dedicated local CPU exists. In strict
mode, mixed or unknown foreground NUMA placement and a non-local copy CPU are
fatal. Results report the worker CPU/node map, copy CPU/node, and a
`numa_local` verdict.

The report includes worker-to-CPU, lane-to-worker, aggregate outstanding
depth, completion semantics, sampled latency, operation efficiency, dirty
replay volume, copy throughput, restore throughput, and the exact artifact
directory. Results on a shared host must be repeated and reported as a spread.

Initial shared-host tmpfs measurements showed why pacing is necessary:

- Unthrottled 1 GiB migration: 4.39M baseline IOPS and 2.72M active IOPS
  (61.8% efficiency).
- 1 GiB/s pacing: 4.27M baseline and 3.88M active IOPS (90.7%).
- 512 MiB/s pacing, two runs: 4.16M–4.61M baseline and 4.08M–4.37M migration
  IOPS (94.8%–98.0%).
- After adding an immediate per-operation baseline, one 512 MiB/s run measured
  4.35M -> 4.30M migration IOPS (98.9%) and 3.89M -> 3.78M snapshot IOPS
  (96.9%). The 1 GiB restore completed in 100.8 ms (10.2 GiB/s) with foreground
  admission deliberately fenced.

These are local regular-file/tmpfs completion results, not remote zcmem/RDMA
results and not evidence that the existing 5M-IOPS transport ceiling changed.

After lock-free route publication, NUMA-local `copy_file_range`, and adaptive
pacing, three consecutive 256 MiB shared-host runs measured 6.12M–6.21M
baseline and 5.79M–5.92M migration IOPS (94.5%–95.3%). Their snapshot phases
measured 4.75M–4.77M baseline and 4.47M–4.51M active IOPS (94.1%–94.6%). A
subsequent contended run fell to 4.94M -> 4.46M migration IOPS (90.2%) and
3.90M -> 3.62M snapshot IOPS (92.8%), demonstrating the required shared-host
spread. That run measured 28.8 ms and 30.3 ms respectively inside the fallback
page-dirty fence while replaying about 128 MiB. Retained-WAL prefix replay is
therefore required to shrink the final suffix rather than merely increasing
base-copy bandwidth.
