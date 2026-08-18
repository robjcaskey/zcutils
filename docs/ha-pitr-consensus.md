# HA and PITR consensus metadata

The HA metadata layer is separate from `/dev/zcnblk0` and from userspace
payload placement. It does not mirror, stripe, spill, or select a terminal
block device. Lane-local userspace WAL stages remain the data plane.

## Completion contracts

An ordinary write may be acknowledged according to the configured local WAL
admission policy. A sync, FUA, or other durability barrier completes only after
the configured durable witnesses cover the barrier. The common tiered policy is
one retained hop WAL plus one downstream leaf in a different failure domain.
This provides two copies without requiring two of three leaves.

The data durability policy and the metadata Raft quorum are independent. A
three-voter metadata group still requires two voters for leadership, while the
data policy may require `one hop + any one leaf`.

Durable reports are bound to:

```text
(group_id, term, config_epoch, log_id, replica_id, lane_hwms)
```

`certify_durable_hwm()` validates configured roles and failure domains and
produces a `DurabilityCertificate`. This direct certificate can complete a
client barrier; the client does not wait for an additional metadata Raft entry.
Reports must be persisted with the replica WAL before they are sent. A newly
elected hop can therefore reconcile retained reports and recover a valid
coverage HWM.

`HaCommand::PublishHwm` checkpoints a certificate through consensus. It bounds
failover reconciliation and supplies committed cuts for management operations,
but is not required for every client sync. Multiple client syncs may join one
lane drain and one durability certificate.

The hop may not evict a certified prefix until another admitted witness set
covers it and all snapshot/PITR retention pins permit retirement. Raft's
knowledge of a copy is metadata and never counts as a data copy.

## Authority and reads

A committed `GrantLease` binds leader identity and expiration to one Raft term
and configuration epoch. It requires a metadata-voter majority. A leader change
without a higher term is rejected. Expiration is fail-closed; explicit
revocation is only an optimization because a partition can suppress it.

Each lane worker resolves its `Arc<PublishedGroupView>` when routing changes and
caches the leader hash. The hot check is a cache-line-aligned atomic snapshot of:

```text
(term, config_epoch, placement_epoch, leader_hash,
 lease_expires_unix_nanos, certified_floor)
```

The registry lock is not acquired per I/O. Publishing uses a sequence counter;
readers observe either the old complete view or the new complete view. Payload
lookup remains in the existing dirty/read overlay. The overlay or applied HWM
must cover the certified HWM before a read is current.

Dynamic durability ownership, characteristic patches, and fenced handoff are
described in [dynamic-topology.md](dynamic-topology.md).

## Multi-Raft snapshot and PITR cuts

`CaptureSnapshot` and `CaptureRecoveryPoint` are committed metadata operations.
They capture the certified cut of every configured group belonging to a volume.
Each group cut records its term, configuration epoch, log identity, and per-lane
HWM vector. A recovery point may reference a base snapshot only when every
target group/lane is at or beyond that base cut.

Snapshots referenced by recovery points cannot be deleted. `retention_floor()`
returns the oldest named cut for a group/lane so the WAL extent manager can pin
the required suffix. This is metadata pinning only; block devices remain
terminal leaves after userspace placement.

The QEMU evolution harness exercises the payload side of this contract. Each
userspace node durably installs a versioned key/value WAL and materialized
snapshot before replying. A recovery bundle contains a digest-checked base
snapshot and only the contiguous WAL suffix through the selected recovery cut.
The target reconstructs the state in a staged replica, syncs it, rereads it,
and reports the selected sequence plus canonical SHA-256 digest. Only then does
the topology controller activate custody and publish a new two-region durable
HWM certificate. See [dynamic-topology.md](dynamic-topology.md#qemu-topology-evolution-proof).

The same harness separately removes two of three supervisor voters, removes a
payload witness, and overlaps supervisor-quorum loss with loss of both payload
sources. Supervisor loss rejects metadata and topology changes without durable
revision movement; payload loss invalidates durability coverage; the overlap
rejects both recovery reads and control-plane changes until the required
failure domains return.

## Persistence and integration boundary

Committed HA commands use the existing checksummed, torn-tail-recoverable
`FileLogStream`. The envelope now stores the real committed term instead of
hard-coding zero. State is reconstructed by replay, requires contiguous indexes,
and rejects term regression before publishing any read view.

`HaMetadataStore` is the Raft application state machine and durable committed
entry sink. It does not implement elections or networking itself. Production
deployment must put a maintained Raft runtime in front of `apply_committed()`
and persist vote/log state before replying to Raft RPCs. The older
`raft-leader` and `raft-wal-leader` commands remain transport benchmarks, not an
HA runtime.

## Performance gate

```bash
URING_PLAY_PIN_CPU_LIST=0 URING_PLAY_TOPOLOGY_STRICT=1 \
  target/release/zcha-readview-bench 1000000000 1
```

The output states lane/worker/CPU mapping and labels the result as a local
atomic-check microbenchmark, not end-to-end block IOPS. The EFA `/dev/zcnblk0`
5.064M random-read topology must be rerun on equivalent hardware after the
authority view is connected to the live route publisher.
