# Untrusted federation 2: store 100 MB of files in another region

> **Target-state design preview:** the encrypted file-set resource and commands
> shown here define the intended user experience; they are not implemented
> end-to-end in the current chart.

This is the consumer side of [offering bounded storage to an untrusted
friend](GETTING_STARTED_WITH_UNTRUSTED_FEDERATION_HOSTING_ON_KUBERNETES.md).
You will continuously protect up to 100 MB of local files in another region
without giving that region a data-encryption key.

File names, directory structure, attributes, and contents are encrypted before
leaving your region. The remote region sees an opaque volume identity, sealed
WAL segments, sizes, timing, and the minimum metadata required for quota and
protocol enforcement.

## 1. Check the remote grant

Your friend's region must issue a `FederatedCapacityGrant` naming your
authenticated region. Membership alone grants no capacity.

```bash
zcctl federation peer status buddy-region
zcctl federation grant accepted --from buddy-region --show-effective-limits
zcctl federation route status buddy-region
```

When both peers are behind NAT, the route should show two outbound relay legs
and end-to-end native payload encryption. The relay never receives your volume
key.

## 2. Define the encrypted remote file set

Create `remote-files.yaml`:

```yaml
apiVersion: storage.zcutils.io/v1alpha1
kind: FederatedFileSet
metadata:
  name: buddy-copy
  namespace: zccusan
spec:
  federationRef: friends-storage
  source:
    persistentVolumeClaimRef:
      name: files-for-buddy-copy
      namespace: zccusan
    pathWithinVolume: /
    maximumLogicalBytes: 100M
  destination:
    region: buddy-region
    grantRef: buddy-inbound
  protection:
    mode: ContinuousWal
    initialCheckpoint: Automatic
    targetLag: 5m
    maximumLag: 30m
  encryption:
    metadata: Encrypted
    contents: Encrypted
    keyAuthority: SourceRegionOnly
    keyEscrow: Denied
  retention:
    minimum: 7d
    immutableCheckpoints: 2
```

Create or reuse a 100 MB PVC named `files-for-buddy-copy`, populate it through
your normal Pod workflow, then apply the object. The adapter follows the
volume's ordered WAL; it does not depend on periodic directory scans or expose
the Pod's path to another cluster.

```bash
kubectl apply -f remote-files.yaml
zcctl fileset watch buddy-copy --show-high-water-marks
```

The initial checkpoint seeds the destination and the file-change WAL remains
attached afterward. Routine updates do not repeatedly copy a complete
100 MB image.

At 16 KiB/s, transferring 100,000,000 payload bytes has a physical lower bound
of about 102 minutes before encryption and framing overhead. The remote
100-IOPS limit is not the bottleneck for ordinary 4 KiB writes at that byte
rate. Status must use the effective lower ceiling when estimating RPO, initial
seed time, and recovery.

If the source exceeds 100 MB, local admission stops before transmitting the
new extent and reports `QuotaExceeded`. It does not rely on the remote peer to
discard arbitrary older data.

## 3. Observe cooperative rate limiting

```bash
zcctl fileset status buddy-copy \
  --show-rate-contract --show-estimated-completion
```

Your sender shapes against the receiver's generation-bound byte and operation
credits. A reconnect resumes the same contract; it does not create a fresh
burst allowance.

If a broken or modified sender violates the credit window, the remote closes
the circuit and returns a signed retry time. Reconnecting early escalates the
identity-bound backoff. The sender records the reason, waits locally, and never
spins in a reconnect loop.

Changing a contract creates a new committed generation. Existing connections
must acknowledge that generation before using its limits; neither side may
select whichever generation is more favorable.

## 4. Prove that the remote copy is usable

First inspect custody without retrieving data:

```bash
zcctl fileset status buddy-copy \
  --show-remote-durable-hwm --show-retention
```

Then restore into an empty local PVC:

```bash
zcctl fileset restore buddy-copy \
  --at latest-remote-durable \
  --destination-pvc zccusan/buddy-copy-restore
zcctl fileset verify buddy-copy \
  --source-pvc zccusan/files-for-buddy-copy \
  --restored-pvc zccusan/buddy-copy-restore
```

The remote returns ciphertext. Your local client authenticates and decrypts
the file metadata and contents, then verifies the checkpoint manifest and
hole-free WAL suffix. A successful remote durability receipt says the
ciphertext is retained under the grant; it does not claim that your friend can
read it.

This remote copy is a recovery source, not automatically a global plaintext
read point. To serve reads in the remote region, attach an authorized
client-side decrypting endpoint or explicitly grant key access to a trusted
execution boundary there.

## 5. Stop updates or remove the copy

Pausing leaves the last remote durable cut retained:

```bash
zcctl fileset pause buddy-copy
zcctl fileset status buddy-copy --show-retention
```

Deletion is a two-party custody transition. The remote must acknowledge the
authorized deletion and advance its tombstone generation before local status
reports the copy absent:

```bash
kubectl delete -f remote-files.yaml
zcctl fileset tombstone buddy-copy --watch
```

If the peer is unreachable, local state remains `DeletionPending` rather than
claiming that the remote ciphertext disappeared.

The receiving region does not need Kubernetes. Continue with [the standalone
macOS peer tutorial](GETTING_STARTED_WITH_A_MACOS_FEDERATED_REGION.md) when the
remote operator wants to run only a local userspace daemon.
