# Cross-region getting started 2: automatic failover and failback

> **Target-state design preview:** this tutorial is an implementation contract.
> Its end-to-end operator workflow is not yet available in the current chart.

This tutorial moves from tutorial one's single-cluster simulation to three
independent Kubernetes clusters. It adds automatic fenced failover and
failback, asymmetric regional storage, standby targets that are not permanently
hot, and performance admission that remains meaningful during recovery.

For an inexpensive rehearsal, use the three simulated regions from [tutorial
one](GETTING_STARTED_WITH_CROSS_REGION_REPLICATION_ON_KUBERNETES.md). You can
also reuse three clusters prepared with the [regional Kubernetes getting
started guide](GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md); leave its CSI
installed and use one prepared cluster per region.

## 1. Install one regional member in each cluster

This guide calls the kubeconfig contexts `region-a`, `region-b`, and `region-c`.
Each cluster needs enough independent nodes and media for the durability claim
it advertises. A smaller functional lab must report the missing failure-domain
guarantee.

```bash
kubectl --context region-a create namespace zccusan
kubectl --context region-b create namespace zccusan
kubectl --context region-c create namespace zccusan
```

Set `ZCCUSAN_VERSION` to the release supporting this tutorial, then install A:

```bash
helm upgrade --install zccusan zcutils/zcblock-csi \
  --version "$ZCCUSAN_VERSION" \
  --kube-context region-a --namespace zccusan \
  --set region.id=region-a \
  --set federation.controlVoter=true \
  --wait --timeout 120s
```

Install B:

```bash
helm upgrade --install zccusan zcutils/zcblock-csi \
  --version "$ZCCUSAN_VERSION" \
  --kube-context region-b --namespace zccusan \
  --set region.id=region-b \
  --set federation.controlVoter=true \
  --wait --timeout 120s
```

Install C:

```bash
helm upgrade --install zccusan zcutils/zcblock-csi \
  --version "$ZCCUSAN_VERSION" \
  --kube-context region-c --namespace zccusan \
  --set region.id=region-c \
  --set federation.controlVoter=true \
  --wait --timeout 120s
```

The three low-rate voters commit membership, policies, leases, topology epochs,
and summarized HWMs. Individual I/O and fsync acknowledgements do not traverse
Raft.

## 2. Establish independently authenticated membership

Create the federation in A and consume bounded, one-use invitations in B and C.
Invitation files contain bootstrap credentials: protect them, never commit
them, and delete them after use.

```bash
zcctl federation create tutorial-global --context region-a --region region-a
zcctl federation invite tutorial-global --context region-a \
  --region region-b --expires 10m --output region-b.invitation
zcctl federation join --context region-b --invitation region-b.invitation
zcctl federation invite tutorial-global --context region-a \
  --region region-c --expires 10m --output region-c.invitation
zcctl federation join --context region-c --invitation region-c.invitation
rm region-b.invitation region-c.invitation
```

The resulting passwordless workload identities and transport credentials
rotate without repeating bootstrap. Every cross-region segment is untrusted
and carries encrypted, authenticated payloads.

## 3. Separate data readiness from capacity assurance

A standby is not simply `hot` or `cold`. The scheduler represents four
independent properties:

| Property | Examples |
| --- | --- |
| Data readiness | WAL only, indexed checkpoint-plus-WAL overlay, materialized, cache warm |
| Capacity assurance | allocated static, reserved autoscaling, reserved manual, best effort |
| Activation authority | automatic controller, external provisioner, human action |
| Performance deadline | minimum usable service and later full service |

This prevents a region with safely retained WAL but no running database-sized
compute pool from being described as either empty or fully ready.

Use the regional [tiering
tutorial](GETTING_STARTED_WITH_TIERING_ON_KUBERNETES.md) to create asymmetric
local profiles. This exercise uses:

| Region | Data while on standby | Capacity plan |
| --- | --- | --- |
| A | materialized, DRAM hot, userspace mirrored WAL spilling sequentially to fast NVMe | `AllocatedStatic` |
| B | retained object/NVMe WAL plus a continuously maintained sparse overlay index | `ReservedAutoscaling` |
| C | sealed checkpoint and WAL segments in object storage; replay deferred | `ReservedManual` with best-effort fallbacks |

Mirroring, placement, tiering, spill, and backpressure remain userspace stages.
NVMe is terminal media behind the userspace writer; no block device is used as
a mirror or stripe primitive.

## 4. Give each deferred target an activation owner

Deferring replay does not leave recovery ownerless. Each destination registers
a `RecoveryCapacityProvider`. It owns:

- the reservation or capacity evidence;
- passwordless authority to start its regional workers;
- the idempotent replay, indexing, compaction, and cleaning jobs;
- progress checkpoints for every lane;
- the minimum and full-performance readiness probes; and
- cleanup after failback or an abandoned attempt.

Region B's provider may scale a Kubernetes node pool automatically. Region C's
provider may represent reserved bare-metal capacity that an operator activates.
If no destination worker remains running, the globally replicated recovery
intent and a small out-of-region supervisor invoke the provider. No source
region is allowed to certify its own remote durability receipt.

Object storage—including S3-compatible stores, GCS, and Azure Blob—contains
immutable authenticated checkpoint manifests, sealed WAL segments, and sparse
extent indexes. The destination can initially serve a checkpoint plus WAL
overlay and compact it in the background; full replay is not automatically on
the ownership-switch critical path. Cleanup may delete a segment only after
retention policy and every required descendant receipt allow it.

Every deferred target separately forecasts live materialized bytes, retained
checkpoints, uncompacted WAL, bounded chunk-compaction workspace, and the exact
liability for ingress credits already granted but not yet landed. It need not
eagerly allocate all of that space: a reserved-elastic provider may commit the
capacity obligation and supply file, NVMe, or object extents in practical
increments before its measured provisioning lead time. A sparse file or cloud
quota alone is not such a reservation.

If idle-CPU compaction falls behind, sealed WAL overflows through a userspace
spill stage to another retained file, NVMe, or object-WAL tier. Exhausting both
admitted primary growth and overflow applies backpressure before the durable
HWM advances; it never forces compaction into foreground CPU or overwrites an
acknowledged segment.

## 5. Declare asymmetric activation plans

Create `automatic-regional-recovery.yaml`:

```yaml
apiVersion: storage.zcutils.io/v1alpha1
kind: GlobalVolumePolicy
metadata:
  name: automatic-regional-recovery
  namespace: zccusan
spec:
  federationRef: tutorial-global
  volumeSelector:
    matchNames: [zc-mirror]
  preferredWriteRegion: region-a
  ioShape:
    operationSize: 4Ki
    readPercent: 70
    aggregateOutstanding: 128
    completion: PolicyDurable
  serviceObjectives:
    minimumUsable:
      guaranteedIops: 10000
      guaranteedBandwidth: 40Mi
      maximumP99Latency: 20ms
      readyWithin: 2m
    fullPerformance:
      guaranteedIops: 50000
      readyWithin: 15m
    enforceDuring: [Snapshot, Migration, Failover, Failback]
  placements:
    - region: region-a
      profileRef: regional-fast
      custody: Durable
      access: ReadWrite
      standby:
        dataReadiness: MaterializedHot
        capacity:
          assurance: AllocatedStatic
        activation: Automatic
    - region: region-b
      profileRef: regional-balanced
      custody: Durable
      access: ReadThrough
      replication:
        mode: ContinuousWal
        targetLag: 1s
        maximumLag: 5s
      standby:
        dataReadiness: IndexedWalOverlay
        capacity:
          assurance: ReservedAutoscaling
          providerRef: region-b-recovery-pool
        activation: Automatic
    - region: region-c
      profileRef: regional-capacity
      custody: Durable
      access: Snapshot
      replication:
        mode: ContinuousWal
        targetLag: 30s
        maximumLag: 5m
      standby:
        dataReadiness: WalOnly
        capacity:
          assurance: ReservedManual
          providerRef: region-c-reserved-rack
        activation: ExternalProvisioner
        fallbacks:
          - assurance: BestEffort
            providerRef: region-c-on-demand
          - assurance: BestEffort
            providerRef: region-c-interruptible
  automation:
    failover:
      enabled: true
      requireSourceOwnershipFence: true
      requireControlQuorum: true
      requireDestinationCapacityAdmission: true
      maximumMissingOperationsPerLane: 0
    failback:
      enabled: true
      preferredRegion: region-a
      stabilityWindow: 10m
      requireCompleteResynchronization: true
      requireDestinationCapacityAdmission: true
```

Apply it once through a healthy global member:

```bash
kubectl --context region-a apply -f automatic-regional-recovery.yaml
zcctl policy status automatic-regional-recovery --show-admission
```

The status reports B's hard reservation separately from C's conditional manual
reservation and best-effort alternatives. Quota alone is not a reservation.
Best-effort plans receive an estimated start-time distribution and confidence,
not a guaranteed RTO.

If C began as tutorial one's read-through cache, changing `custody` to
`Durable` first creates a pending custody facet. The controller pins eviction,
fills missing checkpoint extents, persists and verifies a hole-free WAL suffix,
installs independent retention, and commits the qualifying cut in a new
topology epoch. C may continue serving reads throughout; it does not count
toward a durability predicate until that sequence completes.

The scheduler can share a reservation between failure scenarios that cannot
coexist. It must count it more than once when a declared multi-loss scenario
requires those recoveries simultaneously.

## 6. Preview minimum service and full service

```bash
zcctl recovery preview zc-mirror --unavailable region-a
zcctl recovery preview zc-mirror --unavailable region-a,region-b
zcctl recovery preview zc-mirror \
  --unavailable region-a,region-b --allow-best-effort
```

For each candidate, the report separates:

- RPO from the newest qualifying durable WAL cut;
- compute and media acquisition time;
- index construction, targeted replay, and overlay attachment time;
- time to the minimum usable IOPS and latency contract;
- time to full performance;
- background compaction and cleaning completion; and
- whether each estimate is guaranteed, conditional on manual action, or best
  effort.

A target is not promoted merely because its WAL is durable. Conversely, it
need not finish rewriting the complete volume before serving if its indexed
overlay can meet the minimum service contract.

Once a target starts serving, its foreground guarantee has a hard lane-local
HTB floor. Replay, cache warming, compaction, and cleaning use a separately
reserved system-work budget and may borrow unused foreground capacity. They
must yield before reducing the application below its provisioned floor. If an
RTO requires more background bandwidth than can be borrowed, that bandwidth
must have been reserved during admission.

## 7. Exercise automatic failover to the elastic target

Start a monotonically increasing canary and watch transitions:

```bash
zcctl canary start zc-mirror --context region-a --continuous
zcctl volume watch zc-mirror --show-sessions --show-high-water-marks
zcctl transition watch --volume zc-mirror
```

Use the separately installed chaos toolbox to isolate region A's data and
control endpoints. A healthy operation proceeds:

```text
Detected → FencingSource → SelectingDurableCut → ActivatingCapacity
         → AttachingOverlay → ProvingMinimumService → SwitchingOwnership
         → RebindingSessions → ScalingTowardFullPerformance
```

The stable PVC, block identity, or userspace session remains attached. Requests
may queue briefly at the ownership fence, but clients must not reconnect or
observe two writable epochs.

```bash
zcctl canary status zc-mirror
zcctl metrics report zc-mirror --window 60s --include-system-work
```

The report separates reads, remotely acknowledged writes, local early
acknowledgements, and sync/FUA drains. Representative high-IOPS results also
state lane-to-worker and lane-to-CPU mapping.

## 8. Restore A and observe failback

Remove the fault. A rejoins as a fenced former owner, catches up continuously
from B, and proves its capacity again. Only after complete resynchronization
and the stability window does the controller perform the reverse prepare,
fence, commit, live-session rebind, and drain sequence.

```bash
zcctl region watch region-a --show-catchup
zcctl transition watch --volume zc-mirror
```

This is WAL catch-up, not a routine full-volume snapshot copy.

Continue with [policy-derived recovery ordering and an optional HTTPS
transition gate](GETTING_STARTED_WITH_CROSS_REGION_POLICY_ON_KUBERNETES.md), or
remove the policy and federation before uninstalling the three regional charts:

```bash
kubectl --context region-a delete -f automatic-regional-recovery.yaml
zcctl federation delete tutorial-global --wait
helm uninstall zccusan --kube-context region-c --namespace zccusan
helm uninstall zccusan --kube-context region-b --namespace zccusan
helm uninstall zccusan --kube-context region-a --namespace zccusan
```
