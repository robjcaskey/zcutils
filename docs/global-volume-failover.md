# Global volume failover

This design moves volume custody between regions without making Kubernetes the
source of truth. Kubernetes is one workload adapter; raw block clients,
userspace clients, VMs, and other orchestrators consume the same committed
global operation.

## Data topology

The regional correctness topology is:

```text
/dev/zcnblk0 client edge
  -> userspace WAL failover stage
  -> userspace regional HA route (two already-connected frontends)
  -> userspace regional quorum frontend A or B
  -> three independent userspace terminal leaf writers (matching 2-of-3)
  -> terminal block media
```

Placement, replication, quorum, lane selection, failover, and backpressure are
owned in userspace. `/dev/zcnblk0` is only the client edge. No block device,
device mapper target, loop device, or kernel client performs mirroring or
striping. A terminal block device appears only behind its leaf writer after
userspace placement has been decided.

Each tested data region has three durable replicas in three failure domains,
requires two matching completions, and exposes two frontends on separate VMs.
The QEMU fault matrix independently removes storage VM A, B, and C in both
regions. A and B each contain a frontend and leaf; C contains the third leaf.
The A and B cases force the open client session to change frontend, while all
three cases continue on the other two leaves without reconnecting.

QEMU uses isolated TAP interfaces connected to Linux bridges. Storage and
control traffic inside the guests is unicast TCP. There is no multicast
product dependency, and multicast is not an RDMA substitute. These tests do
not claim RDMA behavior or performance.

## Committed API

`GlobalFailoverCommand` is the platform-neutral mutation API committed through
the low-rate global Raft log:

- `put_volume` declares custody, placement epoch, and workload bindings.
- `put_consistency_set` atomically assigns volumes and a quiesce dependency DAG.
- `observe_region` records revision/epoch-bound durable and applied lane HWMs,
  reachability, quiescence, replica count, quorum, and failure domains.
- `publish_checkpoint` records an immutable application-consistent vector cut.
- `register_session` records the placement epoch seen by a client frontend and
  whether it can transparently rebind a clean cut.
- `request_failover` creates an idempotent clean or explicit-loss operation.
- `reconcile` advances only as far as committed observations permit.
- `acknowledge_workload_action` records adapter completion.

Configuration mutations use compare-and-swap revisions. Observations must name
the exact committed revision and placement epoch used by the decision. Durable
and applied HWM maps must name the same lanes, applied cannot exceed durable,
and the target must report three replicas, a majority quorum of at least two,
enough live replicas, and three distinct durable failure domains. Drift is
reported rather than silently adopted.

The global controller is three full Raft replicas in three control regions.
Any one control-region loss leaves two full state copies and permits failover;
loss of quorum expires the short authority lease and rejects mutations. Volume
data and encryption keys are not stored in this Raft log.

## Consistency sets

A volume may belong to only one atomic consistency set. Updating a set after it
has a checkpoint or failover requires a new set version. Dependencies form a
directed acyclic graph; unknown endpoints, self-edges, duplicate edges, cycles,
and overlapping atomic membership are rejected before an operation exists.

For Kafka -> PostgreSQL -> Cassandra, quiesce order is Kafka, PostgreSQL,
Cassandra and resume order is the reverse. A checkpoint covers every member
and every lane. The selected rollback point is the highest-sequence
application-consistent checkpoint whose complete vector is durable in the
target. A clean cut additionally requires the vector to equal every quiesced
source HWM. This prevents independently choosing three locally recent but
mutually inconsistent cuts.

## Clean lifecycle

1. Expand any requested member to its complete atomic consistency set.
2. Validate fresh source observations and a fully HA target.
3. Quiesce applications in deterministic dependency order.
4. Wait for a common application-consistent checkpoint that exactly equals the
   quiesced source cut and is durable at the target.
5. Transparently rebind capable live sessions to the next placement epoch;
   fence legacy sessions that cannot perform an atomic redirect.
6. Commit target custody and the new epoch.
7. Emit typed workload actions and wait for adapter acknowledgements.
8. Resume in reverse dependency order and complete.

A `Stay` workload keeps its source replicas. The tested stay pod retains its
UID, node, process, and open block FD while the userspace data stage redirects
the volume. `FollowVolume` adds a source custody `NoSchedule` taint, scales the
source ReplicaSet to zero, removes the target custody taint, and scales the
target ReplicaSet up. `ObserveOnly` changes no workload state.

## Declared-loss lifecycle

Declared loss is never inferred from a timeout. An authorized request must name
the reason and maximum acceptable missing operations per lane. Reconciliation
requires every source observation to be unreachable and the destination to be
HA with a complete application-consistent checkpoint. It then records, per
volume and lane:

- the last known durable source HWM;
- the accepted target HWM;
- the inclusive first and last missing sequence.

If any gap exceeds the declared limit, promotion is rejected. Otherwise all
old sessions are fenced—even frontends capable of clean transparent rebind—so
clients cannot continue from state that included now-abandoned writes. The
placement epoch advances, custody moves, and workload actions carry the
committed `source_region_lost=true` bit. Only that bit permits the Kubernetes
adapter to force-remove a stale source pod API object.

## Kubernetes adapter

The adapter supports Deployments, StatefulSets, and ReplicaSets selected by a
binding label and region label. It implements custody taints, source scale-down,
termination waiting, target untainting, target scale-up, and a typed
acknowledgement. The API owns custody and loss acceptance; the adapter cannot
promote a volume or declare a region lost by itself.

## Verification

Run the complete sequential matrix with:

```console
scripts/zcglobal-failover-qemu-matrix.sh
```

It runs Rust invariants, a three-control-region Raft fault test, clean nine-VM
regional HA tests for every A/B/C storage role, a declared-loss nine-VM
regional test, and clean and declared-loss three-VM Kubernetes tests. It
rejects active failover harnesses that use a multicast QEMU backend and emits
one final `ZCGLOBAL_FAILOVER_QEMU_COMPLETE_PASS` marker only after checking the
detailed evidence.

These are correctness tests on a shared host, not representative IOPS or
latency benchmarks. Their topology logs explicitly report lane/worker mapping,
aggregate depth, missing huge-page or memlock preconditions, and the absence of
a measured transport RTT rather than presenting misleading performance data.
