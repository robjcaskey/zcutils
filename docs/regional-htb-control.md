# Regional HTB control

The regional controller computes hierarchical borrowing from interval demand
snapshots. It publishes complete generations to lane-local mailboxes; it is not
called by descriptor admission or completion retirement.

`HtbBorrowingController` protects active guarantees before distributing spare
capacity by weight. `HtbGrantPublisher` splits each leaf grant across its lanes
and fences a generation at one node-local monotonic activation time. Borrowed
capacity should be published with `publish_with_lease`; expiration falls back
to the leaf guarantee independently in each lane.

`RegionalHtbLeaseBridge` is the consensus boundary. It validates the current
leader, term, configuration epoch, and committed lease through
`PublishedGroupView`, then rebases that wall-clock authority into a bounded
local-monotonic HTB lease. A controller outage, leader loss, or quorum loss
therefore stops renewals and removes only borrowed capacity. Guaranteed IOPS
remain locally schedulable.

Raft policy entries are intentionally coarse:

- policy/configuration changes are committed through `ConfigureRegionalHtb`;
- optional recovery/audit checkpoints use `CheckpointRegionalHtb`;
- periodic demand sampling, grant calculation, and mailbox renewal remain
  outside the replicated log and outside the I/O path.

## Below-volume system-operation budget

Foreground volume I/O and maintenance work are not peers. The regional/volume
HTB first protects the admitted foreground guarantee and then grants one
bounded `system_operations` child. Snapshot, live migration, restore/rebuild,
replication catch-up, scrub, and compaction share that child. Consequently a
maintenance deadline can consume reserved maintenance headroom, but it cannot
silently take a provisioned foreground IOP. If the admitted physical capacity
cannot satisfy both, the objective is reported infeasible.

`plan_volume_system_tasks` divides the child grant outside the I/O path. Its
ordering is based on the active objective dependency graph rather than a fixed
task-kind priority:

1. Protect every task's explicitly admitted hard floor. Floors normally encode
   an RPO or compliance promise.
2. Fund tasks on an active deadline's critical path. An upstream snapshot that
   is needed by a pipelined migration is funded first, and the migration uses
   the remaining system-operation grant.
3. If a snapshot is not an ancestor of the migration's RTO terminal, an
   RTO-critical migration can preempt the snapshot's ordinary and borrowed
   allocation, but not its hard floor or a separate active RPO/compliance
   deadline.
4. Bring non-critical tasks toward their provisioned rates, then distribute
   remaining capacity by borrowing weight up to each task ceiling.

The planner publishes complete, leased generations to per-task seqlock
mailboxes. A copy/snapshot worker samples its mailbox at a copy-chunk or batch
boundary. Descriptor admission and completion do not read a system-task
counter, clock, lock, or shared token bucket for every I/O.

The repository's `raft-leader`, `raft-follower`, and `raft-wal-*` commands are
transport/durable-majority benchmarks. They do not implement elections and
must not be presented as the production regional Raft service. The committed
HA state machine and lease bridge are ready for that service, but networked
election/reconfiguration still requires a real consensus runtime.

## 2026-08-20 performance gate

The test topology was `/dev/zcnblk0` client edge -> userspace shared target ->
64 lane-local userspace WAL transports -> two EFA-direct rails -> userspace
volatile leaf media. No block device performed placement, mirroring, or
striping. Each of 64 block workers used QD64 (aggregate depth 4096); lane,
worker, kernel-worker, CPU, EFA domain, and leaf-worker mappings are recorded
in the artifacts.

On the same fresh c8gn.48xlarge placement-group pair:

- HTB disabled: min 9,085,693, mean 9,090,153, max 9,098,287 IOPS (0.14% spread).
- HTB enabled with a nonbinding 20M target and 256-op lane quantum: min
  9,110,925, mean 9,118,947, max 9,127,027 IOPS (0.18% spread).

The measured delta was +0.32%, so no HTB regression was observable. These new
hosts did not reproduce the earlier accepted 11,925,650 mean / 11,951,195 max
IOPS run. The lower capacity affects both sides of the A/B and is not attributed
to HTB. Artifacts are under `bench-results/zc-htb-regional-20260820/`.
