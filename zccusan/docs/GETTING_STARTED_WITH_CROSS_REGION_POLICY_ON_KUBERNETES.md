# Cross-region getting started 3: recovery policy and approval gates

> **Target-state design preview:** the policy resources, HTTPS gate protocol,
> and end-to-end workflow on this page are the implementation contract. They are
> not yet delivered by the current chart.

This tutorial keeps automatic failover and failback, then adds two controls:

- recovery order is derived from solution RPO, RTO, consistency, and business
  impact rather than a user-assigned numeric priority; and
- an optional organization-operated HTTPS service can approve, deny, or hold the final
  ownership switch for selected volumes.

The transition gate is distinct from tutorial two's capacity provider. A
capacity provider obtains compute and media. A transition gate decides whether
an otherwise safe, ready ownership change may proceed.

The gate is never in the storage data path. Replication, catch-up, capacity
activation, and safety checks continue while a transition waits. Approval can
permit an exact safe plan; it cannot waive fencing, durability, trust,
consistency, or performance requirements.

You can reuse the one-cluster laboratory from [tutorial
one](GETTING_STARTED_WITH_CROSS_REGION_REPLICATION_ON_KUBERNETES.md). This page
shows three production-like clusters explicitly. Clusters prepared with the
[regional getting-started
guide](GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md) are also suitable.

## 1. Install the three regional members

Create namespace `zccusan` in contexts `region-a`, `region-b`, and `region-c`:

```bash
kubectl --context region-a create namespace zccusan
kubectl --context region-b create namespace zccusan
kubectl --context region-c create namespace zccusan
```

Set `ZCCUSAN_VERSION` to the release supporting this tutorial. Install A:

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

Create or rejoin `tutorial-global` using the bounded invitation workflow from
[tutorial two](GETTING_STARTED_WITH_CROSS_REGION_FAILOVER_ON_KUBERNETES.md).
Do not create three unrelated federations with the same display name.

## 2. Describe solutions instead of assigning priorities

The example estate has three solution groups:

- `checkout` has PostgreSQL data and WAL volumes that must move at one
  consistency cut;
- `catalog-search` tolerates a longer interruption but has a bounded regional
  RPO; and
- `development` should remain in ordered durable backlog during severe
  capacity shortage instead of displacing production recovery.

Create `recovery-policies.yaml`:

```yaml
apiVersion: storage.zcutils.io/v1alpha1
kind: ConsistencySet
metadata:
  name: checkout
  namespace: zccusan
spec:
  members:
    - volume: checkout-postgres-data
    - volume: checkout-postgres-wal
  recoveryCut: Atomic
---
apiVersion: storage.zcutils.io/v1alpha1
kind: GlobalVolumePolicy
metadata:
  name: checkout-recovery
  namespace: zccusan
spec:
  federationRef: tutorial-global
  consistencySetRef: checkout
  preferredWriteRegion: region-a
  recoveryObjectives:
    - failureSet: [region-a]
      rpo: 1s
      rto: 30s
    - failureSet: [region-a, region-b]
      rpo: 1m
      rto: 3h
  businessImpact:
    downtimeCostPerHour:
      amount: "100000"
      currency: USD
    rtoBreachCost:
      amount: "500000"
      currency: USD
  performance:
    guaranteedIops: 20000
    enforceDuring: [Failover, Failback, Snapshot, Migration]
---
apiVersion: storage.zcutils.io/v1alpha1
kind: GlobalVolumePolicy
metadata:
  name: catalog-search-recovery
  namespace: zccusan
spec:
  federationRef: tutorial-global
  volumeSelector:
    matchNames: [catalog-search]
  preferredWriteRegion: region-a
  recoveryObjectives:
    - failureSet: [region-a]
      rpo: 30s
      rto: 15m
  businessImpact:
    downtimeCostPerHour:
      amount: "10000"
      currency: USD
  performance:
    guaranteedIops: 5000
    enforceDuring: [Failover, Failback]
---
apiVersion: storage.zcutils.io/v1alpha1
kind: GlobalVolumePolicy
metadata:
  name: development-recovery
  namespace: zccusan
spec:
  federationRef: tutorial-global
  volumeSelector:
    matchLabels:
      environment: development
  recoveryObjectives:
    - failureSet: [region-a]
      rpo: 15m
      rto: 8h
    - failureSet: [region-a, region-b]
      action: HoldDurably
  businessImpact:
    downtimeCostPerHour:
      amount: "100"
      currency: USD
```

Apply the objects once through a healthy federation member:

```bash
kubectl --context region-a apply -f recovery-policies.yaml
zcctl recovery preview --unavailable region-a --all-affected
zcctl recovery preview --unavailable region-a,region-b --all-affected
```

The scheduler evaluates affected flows together. It accounts for destination
IOPS, bandwidth, CPU, NIC, PCIe, WAL, media, replay, targeted demultiplexing,
and time to a mountable view. It may leave development volumes multiplexed in
durable WAL storage while admitting checkout as one consistency gang.

The report explains the derived order, capacity reservations, estimated RPO and
RTO, and every objective miss. It never reduces these inputs to an unexplained
numeric priority.

## 3. Optionally define an HTTPS transition gate

The end-user organization's service implements:

```text
POST /v1/transition-decisions
Content-Type: application/json
Idempotency-Key: <transition-id>
```

The request contains no volume payload or encryption key. It binds a decision
to one immutable plan:

```json
{
  "schemaVersion": "v1alpha1",
  "transitionId": "01J...",
  "transitionType": "Failover",
  "sourceRegion": "region-a",
  "targetRegion": "region-b",
  "volumeSetDigest": "sha256:...",
  "planDigest": "sha256:...",
  "estimatedRpoMillis": 0,
  "estimatedRtoMillis": 18000,
  "allMandatoryObjectivesSatisfied": true,
  "approvalExpiresAt": "2026-09-05T18:30:00Z"
}
```

The service returns `Approve`, `Deny`, or `Hold` for the same transition and
digest:

```json
{
  "transitionId": "01J...",
  "planDigest": "sha256:...",
  "decision": "Approve",
  "validUntil": "2026-09-05T18:30:00Z"
}
```

`Hold` may use HTTP 202; the controller retries with the same idempotency key.
Network errors, malformed or trickle-fed responses, expiry, a changed plan
digest, and timeout all fail closed to `Hold`.

Store the service CA and client identity in a Secret populated by the normal
secret manager. Then create `transition-gate.yaml`:

```yaml
apiVersion: storage.zcutils.io/v1alpha1
kind: TransitionApprovalPolicy
metadata:
  name: production-change-control
  namespace: zccusan
spec:
  appliesTo:
    transitionTypes: [Failover, Failback]
    policyNames:
      - checkout-recovery
      - catalog-search-recovery
  endpoint:
    url: https://change-control.example.com/v1/transition-decisions
    caBundleSecretRef:
      name: change-control-client
      key: ca.crt
    clientCertificateSecretRef:
      name: change-control-client
      certificateKey: tls.crt
      privateKeyKey: tls.key
  requestTimeout: 2s
  decisionTtl: 5m
  failurePolicy: Hold
```

```bash
kubectl --context region-a apply -f transition-gate.yaml
zcctl transition-gate status production-change-control
```

The global leader calls HTTPS outside the deterministic Raft state machine. It
commits the authenticated decision and exact plan digest through Raft before
switching ownership. A new leader can therefore resume safely without calling
an external service during log replay.

Volumes not selected by the gate retain automatic behavior. Removing a gate
returns them to automation only after the policy change commits; deletion does
not implicitly approve an already waiting transition.

## 4. Exercise gated failover and failback

Start one canary per solution and isolate A with the separately installed chaos
toolbox:

```bash
zcctl canary start checkout --context region-a --continuous
zcctl canary start catalog-search --context region-a --continuous
zcctl canary start development --context region-a --continuous
zcctl transition watch --all
```

Replication and target preparation continue. Checkout and catalog stop at
`AwaitingApproval` with a stable plan digest. Development follows its own
policy and may remain in durable backlog. Approve the exact pending digest
through the organization-operated service, then observe the transition resume:

```bash
zcctl transition describe <transition-id>
zcctl transition watch <transition-id>
```

Approval of an old digest is rejected if capacity, target, consistency-set
membership, or selected recovery cut changes. Restore A and repeat the
observation for failback: resynchronization and capacity admission happen
first, then the final switch waits for its independently scoped approval.

## 5. Audit and clean up

```bash
zcctl transition audit --volume-set checkout
zcctl recovery preview --unavailable region-a --all-affected
```

The audit identifies observed HWMs, the selected cut, fencing evidence,
capacity reservation, policy revision, plan digest, approval identity, and
Raft commit. It contains no plaintext volume key or user payload.

Remove the gate before its TLS Secret, then remove policies and federation
state before uninstalling:

```bash
kubectl --context region-a delete -f transition-gate.yaml
kubectl --context region-a delete -f recovery-policies.yaml
zcctl federation delete tutorial-global --wait
helm uninstall zccusan --kube-context region-c --namespace zccusan
helm uninstall zccusan --kube-context region-b --namespace zccusan
helm uninstall zccusan --kube-context region-a --namespace zccusan
```

Continue with [global volume failover](../../docs/global-volume-failover.md),
[lane/flow scheduling](../../docs/lane-flow-scheduler.md), and [global
transport security](../../docs/GLOBAL_TRANSPORT_SECURITY.md). For a different
multi-region trust model, continue with [offering tightly bounded storage to an
untrusted federated peer](GETTING_STARTED_WITH_UNTRUSTED_FEDERATION_HOSTING_ON_KUBERNETES.md).
