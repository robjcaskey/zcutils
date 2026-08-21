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
