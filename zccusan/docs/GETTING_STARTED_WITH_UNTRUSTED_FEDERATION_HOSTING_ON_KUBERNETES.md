# Untrusted federation 1: offer bounded storage to a friend

> **Target-state design preview:** the peer grant, relay route, and connection
> enforcement on this page define an implementation contract. They are not yet
> available in the current chart.

You and a friend each operate a region and do not trust the other's
administrators. You want to let your friend's authenticated region retain at
most 100 GB of live logical data on one dedicated file-backed userspace
terminal while enforcing:

- 16 KiB/s of long-term framed inbound network traffic;
- 100 accepted logical IOPS;
- ciphertext-only storage;
- no key escrow or key delivery in either direction; and
- no transitive authority to invite another federation or peer.

This guide interprets “16k/sec” as 16 kibibytes of complete encrypted frames per
second. Use an explicit bit-rate unit if you intend 16 kbit/s. At this limit,
4 KiB inbound writes are bandwidth-limited to roughly four per second before
framing overhead even though the independent IOPS ceiling is 100.

Both regions are behind NAT. TCP hole punching is not reliable enough to be a
storage prerequisite, so each region establishes an outbound connection to a
publicly reachable relay. The relay is not a federation member, durability
copy, key holder, or control voter.

## 1. Start from two independent regional installations

Each person completes the [regional Kubernetes getting-started
guide](GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md) in their own cluster.
This guide calls your region `my-region` and your friend's region
`buddy-region`.

Create a bounded invitation for exactly one federation. The invitation grants
no storage, key, or onward-federation authority by itself:

```bash
zcctl federation create friends-storage --region my-region
zcctl federation invite friends-storage \
  --region buddy-region --expires 10m --output buddy.invitation
```

Transfer `buddy.invitation` over an already authenticated channel. Your friend
uses it once and deletes it:

```bash
zcctl federation join --invitation buddy.invitation
rm buddy.invitation
```

Membership is directional and non-transitive. Your friend may operate other
federations, but cannot add another region to `friends-storage`, enroll your
region or this link in another federation, delegate this grant, or create a
storage placement in your region without a separate grant committed by you.

## 2. Configure an outbound-only relay route

Operate or select a public relay at `relay.example.com:443`. Do not use the
community survey or telemetry endpoint as a data relay.

Create `relay-route.yaml` in both regions, changing `localRegion` appropriately:

```yaml
apiVersion: storage.zcutils.io/v1alpha1
kind: FederationTransportRoute
metadata:
  name: friends-storage-relay
  namespace: zccusan
spec:
  federationRef: friends-storage
  localRegion: my-region
  peerRegion: buddy-region
  connection:
    mode: OutboundTcpRelay
    endpoint: relay.example.com:443
  payloadProtection:
    nativeAuthenticatedEncryption: Required
    tls: Required
  relayAuthority:
    mayDecryptPayload: false
    mayJoinFederation: false
    mayAcknowledgeDurability: false
```

Both peers dial out. Native framing encryption remains end-to-end between the
regions; relay TLS protects the outer connection and does not give the relay a
volume key. The relay can still observe connection endpoints, timing, and
traffic volume, which the trust report must disclose.

## 3. Dedicate an elastically allocated file-backed userspace terminal

Label only the node on which you intend to store your friend's ciphertext:

```bash
kubectl label node some-storage-node \
  storage.zcutils.io/friends-file-terminal=true
```

Create `buddy-file-media.yaml`:

```yaml
apiVersion: storage.zcutils.io/v1alpha1
kind: MediaGrant
metadata:
  name: buddy-file-terminal
spec:
  nodeSelector:
    matchLabels:
      storage.zcutils.io/friends-file-terminal: "true"
  mediaSets:
    - name: buddy-file
      staticSources:
        - kind: FileArena
          path: /var/lib/zccusan/federated/buddy-100g.wal
          maximumLogicalLiveBytes: 100G
          allocation:
            mode: ElasticExtents
            preferredIncrement: 4G
            capacityPlanRef: buddy-file-pool
          preparation:
            create: IfMissing
            requireDedicatedEmptyPath: true
      publishAs:
        mediaClass: buddy-file-terminal
        durability: FilesystemFsync
        failureDomains:
          - kubernetes.io/hostname
    - name: buddy-overflow
      staticSources:
        - kind: ObjectWal
          url: s3://replace-with-private-bucket/buddy-overflow
          authentication:
            kind: WorkloadIdentity
      publishAs:
        mediaClass: buddy-object-overflow
        durability: RetainedObjectCommit
```

`FileArena` is a regular file written by a userspace WAL terminal. It
is not attached through loop, device mapper, software RAID, or a custom block
mirror. This example provides one custody copy and makes no regional-HA claim.
The operator must refuse `FilesystemFsync` durability if the mounted filesystem
and underlying media cannot prove the requested sync semantics.

The backing file is not eagerly allocated to a guessed worst-case size. It
grows in filesystem extents, preferably 4 GB at a time. A sparse maximum length
alone is not a reservation: `buddy-file-pool` must admit a pool-level elastic
capacity obligation that other tenants cannot consume.

Apply the grant and wait for its exact resolved path, filesystem, capacity, and
durability evidence:

```bash
kubectl apply -f buddy-file-media.yaml
kubectl get mediagrant buddy-file-terminal --watch
```

## 4. Grant bounded inbound custody

Create `buddy-capacity-grant.yaml`:

```yaml
apiVersion: storage.zcutils.io/v1alpha1
kind: FederatedCapacityGrant
metadata:
  name: buddy-inbound
  namespace: zccusan
spec:
  federationRef: friends-storage
  granteeRegion: buddy-region
  direction: Inbound
  placement:
    mediaClass: buddy-file-terminal
    maximumLogicalLiveBytes: 100G
    maximumVolumes: 16
    physicalCapacity:
      assurance: ReservedElastic
      providerRef: buddy-file-pool
      preferredAllocationIncrement: 4G
      forecastHorizon: 6h
      include:
        - LiveMaterializedBytes
        - RetainedWal
        - BoundedCompactionWorkspace
        - GrantedIngressLiability
      reservationShortfallAction: MarkBestEffortThenBackpressure
    overflow:
      mediaClass: buddy-object-overflow
      capacity:
        assurance: ReservedElastic
        providerRef: buddy-overflow-pool
        preferredAllocationIncrement: 16G
      beginWhen:
        uncompactedWalBytes: 10G
        predictedPrimaryFillWithin: 6h
      stopWhen:
        uncompactedWalBytesBelow: 5G
        primaryFreeBytesAbove: 25G
      unavailableAction: Backpressure
  traffic:
    maximumFramedIngressBytesPerSecond: 16Ki
    ingressBurstBytes: 64Ki
    maximumAcceptedLogicalIops: 100
    iopsBurst: 20
  encryption:
    acceptedPayload: CiphertextOnly
    keyEscrow: Denied
    rejectKeyMaterial: true
    mayServeEncryptedRestore: true
  delegation:
    transitive: false
    mayInviteRegions: false
    mayDelegateGrant: false
    mayEnrollInAnotherFederation: false
  maintenance:
    compaction:
      scheduling: Opportunistic
      localCpu:
        runBelowUtilization: 30%
        minimumQuietPeriod: 30s
        maximumCpu: "1"
        mayPreemptForeground: false
      maximumRewriteChunkBytes: 1G
      whenBehind: Overflow
```

```bash
kubectl apply -f buddy-capacity-grant.yaml
zcctl federation grant status buddy-inbound --show-effective-limits
```

The 100 GB value is a hard live-logical custody ceiling. Physical overhead is a
changing planner obligation, not extra logical space available to your friend.
Its provider policy independently bounds spend and total pool exposure.
Admission rejects a placement when the required obligation cannot be reserved
and never evicts retained WAL silently to make room.

The key prohibition is structural. The data RPC accepts only encrypted replica
envelopes, while key escrow is a separate typed operation denied by this grant.
A frame declared as plaintext, a wrapped DEK, or a key envelope is rejected.
Your region never imports remote material into its key store and therefore
cannot mount or inspect the friend's files.

No protocol can prove that a malicious peer did not hide a key inside arbitrary
opaque ciphertext. The meaningful guarantee is that your system cannot
recognize, activate, release, or treat those opaque bytes as escrowed key
material.

## 5. Defer compaction without risking the retained WAL

Foreground ingest only appends sealed immutable segments and updates bounded
lane-local indexes. It never performs compaction inline. When local CPU remains
below 30% for the quiet window, a low-priority worker compacts at most a 1 GB
chunk into a new immutable checkpoint and yields immediately if foreground CPU
or I/O demand returns.

The planner continuously recalculates required physical capacity from current
live bytes, WAL creation and reclamation rates, retained checkpoint policy, one
bounded compaction chunk, provisioning lead time, and granted ingress
liability. That last value is exact: outstanding byte credits plus the open
segment tail and bounded manifest overhead. Capacity is reserved before those
credits are issued and becomes ordinary WAL consumption as frames land. The
planner requests additional extents before the forecast crosses the provider's
lead time; it does not reserve a fixed percentage that is permanently wrong
for both quiet and high-churn file sets.

Compaction is allowed to remain deferred indefinitely while overflow is
healthy. When uncompacted WAL or predicted time-to-full crosses the configured
threshold, whole sealed segments spill through the userspace placement stage
to the object-WAL media class. A manifest records whether each segment resides
in the file arena, object overflow, or both. The low-rate manifest commit—not a
block-layer remap—changes custody.

An ephemeral recovery worker may later read segments from object storage,
create a replacement checkpoint, commit its content manifest, and reclaim old
segments only after retention and descendant receipts allow it. If local
headroom and overflow are both exhausted, the receiver stops granting credits
before overwriting or acknowledging unretained data.

The pressure controller uses hysteresis so segments do not bounce between the
file and bucket at one threshold. Status reports live logical bytes,
uncompacted WAL, compaction workspace, granted-but-not-landed ingress,
overflow bytes, predicted time-to-full, and the exact HWM retained by each
tier.

## 6. Enforce cooperative shaping without trusting the sender

The receiver publishes a generation-bound traffic contract and grants bounded
byte and operation credits. A cooperative sender shapes before transmission.
The receiving lane remains authoritative and uses exact byte and IOPS token
buckets at these low rates.

Normal jitter exhausts credit and applies backpressure; it does not immediately
punish the peer. The server terminates a connection when a sender transmits
beyond issued application credits, violates framing, attempts a forbidden key
operation, or reconnects to bypass an active delay.

Each `RateContractViolation` response includes a signed `resumeAfter` bound to:

```text
(federation ID, authenticated peer ID, grant ID, grant generation)
```

Reconnect delay escalates with bounded exponential backoff and jitter:

```text
1s, 2s, 4s, 8s, ... up to 5m
```

The regional receiver persists the violation level outside the connection, so
new TCP sessions, relay circuits, source ports, or NAT mappings cannot reset
it. Ten minutes of compliance reduces the level one step rather than clearing
it instantly. An administrator can reset it through an audited policy
operation.

During the delay, admission rejects the authenticated peer before allocating a
data arena or opening the file terminal. Stateless edge cookies and coarse
pre-authentication source limits protect the handshake itself. Per-frame
violations are summarized regionally; they do not flood global Raft.

Inspect both accepted work and rejected attempts:

```bash
zcctl federation grant watch buddy-inbound \
  --show-rate --show-backoff --show-custody
```

The report distinguishes accepted logical IOPS from rejected frames and counts
the entire authenticated wire frame against the byte ceiling.

## 7. Revoke and clean up

Revocation blocks new writes immediately but retains existing ciphertext until
the agreed retention or explicit return/delete protocol completes:

```bash
zcctl federation grant revoke buddy-inbound --retain-existing
zcctl federation grant status buddy-inbound --watch
kubectl delete -f buddy-capacity-grant.yaml
kubectl delete -f buddy-file-media.yaml
kubectl label node some-storage-node \
  storage.zcutils.io/friends-file-terminal-
```

Continue with [placing 100 MB of your own encrypted files in an untrusted
region](GETTING_STARTED_WITH_UNTRUSTED_FEDERATION_STORAGE_ON_KUBERNETES.md).
If your friend refuses to run Kubernetes, use the sibling guide for [running a
federated region directly on a Mac](GETTING_STARTED_WITH_A_MACOS_FEDERATED_REGION.md).
