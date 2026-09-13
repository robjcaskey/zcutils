# Cross-region getting started 1: replication and recovery estimates

> **Target-state design preview:** this tutorial defines the operator experience
> we will implement next. The current `CrossRegionReplication` object performs
> bounded checkpoint transfer; it does not yet provide the continuous-WAL,
> recovery-preview, or promotion behavior shown here.

This is the first of three progressively deeper tutorials:

1. Replicate a volume and inspect estimated RPO and RTO in a three-region lab.
2. [Exercise automatic failover and failback while preserving performance
   guarantees](GETTING_STARTED_WITH_CROSS_REGION_FAILOVER_ON_KUBERNETES.md).
3. [Schedule several recovery classes and optionally require an HTTPS approval
   before switching ownership](GETTING_STARTED_WITH_CROSS_REGION_POLICY_ON_KUBERNETES.md).

Two follow-on peer-federation tutorials cover [offering bounded ciphertext
custody to an untrusted friend](GETTING_STARTED_WITH_UNTRUSTED_FEDERATION_HOSTING_ON_KUBERNETES.md)
and [placing your own encrypted files in that friend's
region](GETTING_STARTED_WITH_UNTRUSTED_FEDERATION_STORAGE_ON_KUBERNETES.md).
A sibling guide shows how that friend can [operate the receiving region on a
Mac without Kubernetes](GETTING_STARTED_WITH_A_MACOS_FEDERATED_REGION.md).

This first exercise represents regions `a`, `b`, and `c` with three isolated
zccusan installations in one Kubernetes cluster. It is inexpensive and easy to
inspect, but it does not create independent regional power, network,
administrative, or Kubernetes failure domains.

You may reuse the cluster and nodes from [getting started with zccusan on
Kubernetes](GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md). Leave that CSI
installation in place; this tutorial uses separate namespaces and CSI
identities.

## What you will build

```text
region A: writable regional volume
  ├─ continuous encrypted WAL → region B: durable replica + read point
  └─ continuous encrypted WAL → region C: read-through cache
                                      (not a durability copy)
```

B retains a complete checkpoint and hole-free WAL suffix. C can serve recent
cached extents and fetch misses from an authoritative read view, but its
contents remain evictable. C therefore contributes nothing to durability even
if it happens to have cached every byte.

At the end you will preview node loss, loss of A, loss of A and B together, and
loss of A while B is lagging. Estimates come from observed high-water marks,
reserved lanes, replay rate, materialization time, and attachment time; they
are not static labels.

## 1. Create three simulated regions

Set `ZCCUSAN_VERSION` to the release that declares support for this target-state
tutorial. Unique driver names and host paths let the installations share one
API server and the same nodes.

```bash
kubectl create namespace zccusan-region-a
kubectl create namespace zccusan-region-b
kubectl create namespace zccusan-region-c

helm repo add zcutils https://robjcaskey.github.io/zcutils
helm repo update zcutils
```

Install A:

```bash
helm upgrade --install zccusan-region-a zcutils/zcblock-csi \
  --version "$ZCCUSAN_VERSION" \
  --namespace zccusan-region-a \
  --set fullnameOverride=zccusan-region-a \
  --set driverName=io.zcutils.zcblock.region-a \
  --set stateDir=/var/lib/zccusan-region-a \
  --set region.id=region-a \
  --set federation.simulatedFailureDomain=true \
  --wait --timeout 120s
```

Install B:

```bash
helm upgrade --install zccusan-region-b zcutils/zcblock-csi \
  --version "$ZCCUSAN_VERSION" \
  --namespace zccusan-region-b \
  --set fullnameOverride=zccusan-region-b \
  --set driverName=io.zcutils.zcblock.region-b \
  --set stateDir=/var/lib/zccusan-region-b \
  --set region.id=region-b \
  --set federation.simulatedFailureDomain=true \
  --wait --timeout 120s
```

Install C:

```bash
helm upgrade --install zccusan-region-c zcutils/zcblock-csi \
  --version "$ZCCUSAN_VERSION" \
  --namespace zccusan-region-c \
  --set fullnameOverride=zccusan-region-c \
  --set driverName=io.zcutils.zcblock.region-c \
  --set stateDir=/var/lib/zccusan-region-c \
  --set region.id=region-c \
  --set federation.simulatedFailureDomain=true \
  --wait --timeout 120s
```

The simulation marker remains visible in status and prevents this laboratory
from being reported as real regional compliance.

```bash
kubectl get csidriver \
  io.zcutils.zcblock.region-a \
  io.zcutils.zcblock.region-b \
  io.zcutils.zcblock.region-c
kubectl get pods -A -l app.kubernetes.io/name=zcblock-csi -o wide
```

## 2. Link the simulated regions

Create the federation, then add each directional relationship explicitly. The
next tutorial replaces this local shortcut with independently authenticated
cluster invitations.

```bash
zcctl federation create tutorial-global \
  --region region-a --namespace zccusan-region-a
zcctl federation link tutorial-global \
  --from region-a --to region-b --namespace zccusan-region-b
zcctl federation link tutorial-global \
  --from region-b --to region-a --namespace zccusan-region-a
zcctl federation link tutorial-global \
  --from region-a --to region-c --namespace zccusan-region-c
zcctl federation link tutorial-global \
  --from region-c --to region-a --namespace zccusan-region-a
zcctl federation link tutorial-global \
  --from region-b --to region-c --namespace zccusan-region-c
zcctl federation link tutorial-global \
  --from region-c --to region-b --namespace zccusan-region-b
zcctl federation status tutorial-global --watch
```

Every link treats its network segment as untrusted. User payloads use native
authenticated encryption by default. TLS is an optional outer compliance
layer, but it is not enabled for published performance figures.

## 3. Declare global volume behavior

Create `cross-region-volume.yaml`. The tutorial profiles make A writable, B a
retained durable destination, and C an evictable read-through destination.

```yaml
apiVersion: storage.zcutils.io/v1alpha1
kind: GlobalVolumePolicy
metadata:
  name: cross-region-getting-started
  namespace: zccusan-region-a
spec:
  federationRef: tutorial-global
  volumeSelector:
    matchNames: [zc-mirror]
  preferredWriteRegion: region-a
  placements:
    - region: region-a
      profileRef: tutorial-regional
      custody: Durable
      access: ReadWrite
    - region: region-b
      profileRef: tutorial-durable-replica
      custody: Durable
      access: ReadThrough
      replication:
        mode: ContinuousWal
        targetLag: 5s
        maximumLag: 30s
    - region: region-c
      profileRef: tutorial-read-cache
      custody: CacheOnly
      access: ReadThrough
      replication:
        mode: ContinuousWal
        targetLag: 1s
        maximumLag: 10s
  recoveryObjectives:
    - failureSet: [RegionNode]
      rpo: 0s
      rto: 10s
    - failureSet: [region-a]
      rpo: 5s
      rto: 2m
    - failureSet: [region-a, region-b]
      action: HoldDurably
```

Apply it once through A. The adapter commits it to the global state log; do not
create competing copies independently in all three regions.

```bash
kubectl apply -f cross-region-volume.yaml
kubectl -n zccusan-region-a get globalvolumepolicy \
  cross-region-getting-started --watch
```

The policy becomes `Ready` only after B has a complete checkpoint plus a
hole-free live WAL suffix and C has joined the live feed. A snapshot may seed a
destination, but routine replication remains an attached WAL stream.

## 4. Exercise the global read points

```bash
zcctl volume write zc-mirror --region region-a --text tutorial-record
zcctl volume read zc-mirror --region region-b --consistency read-your-writes
zcctl volume read zc-mirror --region region-c --consistency read-your-writes
zcctl volume read zc-mirror --region region-c --consistency read-your-writes
zcctl volume status zc-mirror --show-regions --show-high-water-marks
```

B can satisfy the read from its durable projection. C's first read may fetch a
missing extent; the second should be a local hit. Both carry a minimum HWM, so
the cache never mistakes an old extent for a current value. B publishes a
durable HWM; C publishes coverage, a closed/readable HWM, and
`durabilityContribution: false`.

## 5. Preview recovery without causing a failure

```bash
zcctl recovery preview zc-mirror --unavailable region-a
zcctl recovery preview zc-mirror --unavailable region-a,region-b
zcctl recovery preview zc-mirror \
  --unavailable region-a --assume-lag region-b=20s
```

Every report includes:

- the newest qualifying durable cut and estimated data-loss window;
- target selection or an explicit `HoldDurably` result;
- provisioning, queue, replay, materialization, and attachment time;
- estimated RPO and RTO with the observations used to calculate them;
- whether each declared objective is met; and
- missing durability, trust, key, capacity, or performance prerequisites.

The A+B-loss preview must not count C as durable. Promotion requires complete
durable coverage, a hole-free WAL suffix, independent retention, and a
committed topology epoch; it is never a label-only change.

## Keep the lab or clean it up

Keep the installations to run tutorial two in simulation mode. Otherwise:

```bash
zcctl federation delete tutorial-global --wait
helm uninstall zccusan-region-c --namespace zccusan-region-c
helm uninstall zccusan-region-b --namespace zccusan-region-b
helm uninstall zccusan-region-a --namespace zccusan-region-a
kubectl delete namespace \
  zccusan-region-c zccusan-region-b zccusan-region-a
```

Continue with [automatic cross-region failover and
failback](GETTING_STARTED_WITH_CROSS_REGION_FAILOVER_ON_KUBERNETES.md).
