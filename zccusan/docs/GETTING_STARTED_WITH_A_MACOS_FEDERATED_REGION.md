# Untrusted federation 3: run a region on a Mac without Kubernetes

> **Target-state design preview:** the standalone peer daemon, macOS durability
> probe, and sender-side coalescing policy on this page define an implementation
> contract. They are not available end-to-end in the current release.

Your friend will not run Kubernetes. That does not prevent his Mac from being a
federated zccusan region. The Mac runs one unprivileged userspace daemon, keeps
ciphertext in a regular file, and connects outbound whenever the laptop is
awake. It does not install CSI, a kernel module, a block device, a container
runtime, or a local Kubernetes distribution.

This guide assumes:

- your always-on region sends an encrypted file-set copy;
- the Mac accepts no keys and cannot decrypt that copy;
- both regions are behind NAT and use outbound connections to an opaque relay;
- the Mac may be offline for hours or days;
- your region retains the unsent recovery stream while it is offline; and
- the offered freshness is one recoverable cut every five minutes, subject to
  laptop availability and the receiver's 16 KiB/s limit.

The five-minute stream is a sender-selected derived recovery view. It does not
change the friend's inbound ceiling and does not weaken your authoritative
local WAL.

The [WAL custody tier model](../../docs/wal-custody-tiers.md) describes how
this delayed copy composes with nearby memory holders, geographic forwarding,
and more immediate disaster-recovery obligations.

## 1. Understand the two WAL layers

```text
application writes
    ↓
authoritative local WAL ───────────────→ local durability and PITR
    │ asynchronous, post-ack consumer
    ↓
5-minute outbound coalescer
    ↓
derived retained WAL/backlog ─→ relay ─→ Mac file terminal
```

If one extent changes 10,000 times during a five-minute window, the derived
stream normally carries its final version once. Ordered discard/TRIM state and
the cut manifest are preserved. The remote receipt names the exact source cut
it can reconstruct; it does not pretend to contain every omitted intermediate
source sequence.

This distinction is essential:

- local PITR and any fine-grained replica continue consuming the authoritative
  WAL;
- the Mac can promise no better than the emitted five-minute cut granularity;
  and
- coalescing never participates in foreground write or fsync acknowledgement.

## 2. Install the standalone Mac binary

Install the signed universal zcutils package using the release mechanism for
the version supporting this tutorial:

```bash
brew install robjcaskey/tap/zcutils
zcctl version
```

The target package includes the same federation framing, receipt validation,
rate enforcement, and file-terminal code used by regional servers. It does not
shell out to a Kubernetes client or container runtime.

Create user-owned state and ciphertext directories:

```bash
mkdir -p "$HOME/Library/Application Support/zccusan/state"
mkdir -p "$HOME/Library/Application Support/zccusan/ciphertext"
chmod 700 "$HOME/Library/Application Support/zccusan"
```

Initialize the local region:

```bash
zcctl region init buddy-mac \
  --state-dir "$HOME/Library/Application Support/zccusan/state" \
  --platform macos \
  --no-kubernetes
```

The command creates a region identity in the macOS Keychain and a local
state-log database. It does not create a data-encryption key for your files.

## 3. Join the existing federation

Create a one-use invitation in your region:

```bash
zcctl federation invite friends-storage \
  --region buddy-mac --expires 10m --output buddy-mac.invitation
```

Transfer it over an already authenticated channel. On the Mac:

```bash
zcctl federation join \
  --region buddy-mac \
  --invitation buddy-mac.invitation
rm buddy-mac.invitation
```

The invitation joins only `friends-storage`. It does not allow either region to
enroll the other in another federation, delegate storage, request keys, or
become a global control leader without a separate explicit grant.

## 4. Configure the outbound NAT route

The Mac and your region both dial `relay.example.com:443`. The relay forwards
opaque frames but holds no federation vote, durability receipt, or volume key.

```bash
zcctl region route add \
  --region buddy-mac \
  --peer my-region \
  --mode outbound-tcp-relay \
  --relay relay.example.com:443 \
  --native-payload-encryption required \
  --tls required
```

TCP hole punching may be attempted as an optimization later, but correctness
never depends on it. When the Mac is asleep or disconnected, the relay does not
buffer the volume; your sender owns the backlog.

## 5. Offer a bounded local file terminal

Create a growable file arena. This example offers at most 100 GB of live
logical ciphertext, accepts at most 16 KiB/s of complete inbound frames and 100
logical IOPS, and imports no key material:

```bash
zcctl region media add-file buddy-file \
  --path "$HOME/Library/Application Support/zccusan/ciphertext/buddy.wal" \
  --maximum-logical-live-bytes 100G \
  --allocation best-effort-elastic \
  --preferred-increment 1G \
  --sync-probe macos-full-fsync

zcctl federation grant create buddy-inbound \
  --federation friends-storage \
  --grantee my-region \
  --media buddy-file \
  --maximum-logical-live-bytes 100G \
  --maximum-framed-ingress 16KiB/s \
  --maximum-logical-iops 100 \
  --ciphertext-only \
  --deny-key-escrow \
  --reject-key-material \
  --non-transitive
```

The daemon probes `F_FULLFSYNC` support and reports the actual filesystem and
media evidence. It must not advertise a power-safe HWM if the call or backing
store cannot supply the requested semantics.

`best-effort-elastic` grows the file in practical increments but does not claim
that free laptop space is reserved. An APFS capacity reservation or another
enforceable provider is required before this becomes `reserved-elastic`. A
sparse maximum file length by itself is not a capacity guarantee.

The data endpoint accepts encrypted replica envelopes only. A peer can always
hide arbitrary bytes inside opaque ciphertext, but the Mac never recognizes,
imports, releases, or uses those bytes as a key.

## 6. Run only while the laptop is available

Run the peer in the foreground:

```bash
zcctl region serve buddy-mac
```

Alternatively install the same command as a per-user launch agent:

```bash
zcctl region install-launch-agent buddy-mac
launchctl print "gui/$(id -u)/io.zcutils.buddy-mac"
```

The launch agent starts after login, reconnects after network changes, and
stops advertising credits before orderly sleep or shutdown. A sudden
disconnect is also safe: the sender retains anything not covered by the Mac's
last authenticated durable receipt.

The receive contract survives reconnection. A new TCP connection does not
reset byte credits, IOPS credits, custody quota, or a violation backoff.

## 7. Add the five-minute sender-side recovery view

In your Kubernetes region, create `buddy-mac-replication.yaml`:

```yaml
apiVersion: storage.zcutils.io/v1alpha1
kind: GlobalVolumePolicy
metadata:
  name: buddy-mac-copy
  namespace: zccusan
spec:
  federationRef: friends-storage
  volumeSelector:
    matchNames: [files-for-buddy-copy]
  placements:
    - region: my-region
      custody: Durable
      access: ReadWrite
    - region: buddy-mac
      custody: DurableCiphertext
      access: RestoreOnly
      grantRef: buddy-inbound
      replication:
        mode: CoalescedWal
        freshnessInterval: 5m
        targetLagWhileConnected: 5m
        maximumLag: 7d
        foregroundDurabilityContribution: false
        coalescing:
          key: LogicalExtent
          retainWithinWindow: FinalState
          preserve:
            - DiscardAndTrim
            - BarrierCuts
            - ConsistencyManifests
          cutConsistency: CrashConsistent
        disconnectedBacklog:
          retainAtSender: true
          capacity:
            assurance: ReservedElastic
            providerRef: local-replication-backlog
            preferredAllocationIncrement: 4G
          compactUnacknowledgedWindows: true
          preserveCuts:
            latestAcknowledgedBase: true
            protectedPitrCuts: true
          overflow:
            mediaClass: local-object-wal-overflow
          exhaustedAction: MarkNeedsReseedWithoutBlockingForeground
        compaction:
          scheduling: Opportunistic
          localCpu:
            runBelowUtilization: 30%
            minimumQuietPeriod: 30s
            mayPreemptForeground: false
```

```bash
kubectl apply -f buddy-mac-replication.yaml
zcctl volume watch files-for-buddy-copy \
  --show-destination buddy-mac \
  --show-coalescing --show-backlog
```

The outbound coalescer consumes the WAL asynchronously using its own lane,
arena, CPU budget, and LatestMap. The existing foreground lane gains no
allocation, shared atomic, topology lookup, or per-I/O coalescing decision.

The receiver's byte limit is a ceiling, not a freshness guarantee. At
16 KiB/s, only about 4.9 MB of framed traffic fits into each five-minute
interval. If more than that much unique final extent state changes per window,
lag grows even while the laptop remains connected.

## 8. Compact the disconnected sender backlog

The sender retains the last Mac-acknowledged base and newer derived cuts. While
the laptop is offline, repeated changes to the same extents may be collapsed
again into one delta from that acknowledged base to the newest eligible cut.

Compaction is copy-on-write:

1. Build a new immutable extent map from the acknowledged base to target cut.
2. Persist its data and manifest in local backlog or overflow.
3. Verify coverage and commit the replacement manifest.
4. Reclaim superseded derived segments only after the commit.

Fine-grained authoritative WAL and protected PITR cuts are not reclaimed merely
because this low-bandwidth destination does not need them.

If opportunistic CPU never becomes available, the derived backlog grows into
its elastic overflow. If both admitted local capacity and overflow are
exhausted, this policy marks the Mac replica `NeedsReseed` and may discard only
the optional derived stream. It does not block local application writes because
the policy explicitly excludes the Mac from foreground durability.

For a destination that does contribute to durability, `never block foreground`
would be invalid: the sender must retain or backpressure rather than discard
the only required recovery material.

## 9. Watch an offline and online cycle

Stop or sleep the Mac. In your region:

```bash
zcctl volume status files-for-buddy-copy \
  --destination buddy-mac \
  --show-last-receipt --show-backlog --show-estimated-rpo
```

The destination becomes `Offline` immediately, its remote RPO age increases,
and the local derived backlog grows or coalesces. It must not remain displayed
as a current global read point.

Wake the Mac and start `zcctl region serve buddy-mac` if it is not managed by
launchd. The peers authenticate, exchange the last durable cut and current
contract generation, then resume through the 16 KiB/s credit window:

```bash
zcctl volume watch files-for-buddy-copy \
  --destination buddy-mac \
  --show-catchup --show-rate-contract
```

If the Mac no longer has the acknowledged base, the sender performs a fresh
checkpoint seed. Otherwise it sends the compacted delta and joins subsequent
five-minute cuts without replaying superseded hot-block versions.

## 10. Verify and remove the standalone region

Restore and verify through the source-side client, which owns the decryption
key:

```bash
zcctl fileset restore files-for-buddy-copy \
  --from buddy-mac \
  --at latest-remote-durable \
  --destination-pvc zccusan/buddy-mac-restore
zcctl fileset verify files-for-buddy-copy \
  --restored-pvc zccusan/buddy-mac-restore
```

Remove the sender policy before revoking the Mac grant:

```bash
kubectl delete -f buddy-mac-replication.yaml
zcctl federation grant revoke buddy-inbound --retain-existing
```

On the Mac, wait for the agreed return/delete transition, then remove the
launch agent and local region:

```bash
zcctl region uninstall-launch-agent buddy-mac
zcctl region delete buddy-mac
```

Deleting the daemon does not prove the ciphertext file was erased. Erasure is a
separate, explicit custody operation whose remote tombstone and local file
removal must both be observed.
