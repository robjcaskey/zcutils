# Getting started with tiering on Kubernetes

This continues the two-copy mirror walkthrough. The operator now accepts a
`TieringPolicy` referenced by a mirrored `StorageProfile` and installs a
separate userspace tier at each selected leaf. It does not put placement,
mirroring, or spill decisions in `/dev/zcnblk0`.

The reconciled path is:

```text
/dev/zcnblk0 client edge
  -> userspace onramp
  -> userspace mirror
  -> one userspace zctier stage per mirror leg
  -> tmpfs hot file + bounded asynchronous host-file spill
```

The first policy shape supports `MemoryEmptyDir` hot storage and a retained
`HostPathFile` spill. Reads after a leaf restart rehydrate from the spill file.
The hot write is the leaf completion authority; asynchronous spill is never
reported as a durable HWM.

## Apply the tiered profile

Choose three or more schedulable nodes: one client and two distinct leaves.
Then apply the example:

```bash
kubectl apply -f zccusan/deploy/zcblock-csi/getting-started/tiered-mirror-ram.yaml

kubectl get tieringpolicy ram-hot-host-spill -o yaml
kubectl get storageprofile tiered-two-copy -o yaml
```

Both status objects must reach `phase: Ready`. Create a PVC using the generated
`zc-tiered-two-copy` StorageClass and run the fio Pod from the mirror guide,
changing its claim name if necessary. `excludeClientNode: true` and the
hostname topology key keep the two terminal leaf Pods off the client and on
different nodes.

Inspect the realized tier contract:

```bash
kubectl get zcvolume -A -o yaml
kubectl get pods -A -l storage.zcutils.io/stage=terminal-leaf -o wide
```

Each leaf status includes its policy, hot/spill kinds, spill path,
backpressure budget, and the explicit `hot-only` acknowledgement statement.
Deleting and recreating a leaf Pod removes its tmpfs hot file; its retained
spill file becomes the read fallback and is rehydrated on demand.

Current boundaries are fail-closed: block-backed `MediaGrant` leaves cannot be
wrapped by this first tier policy, `rehydrateOnColdStart` must be true, and
`reclaimPolicy` must be `Retain`. A block device may still be terminal media
behind a later userspace tier implementation; it will never become the tier or
mirror primitive.

Continue with
[cross-region checkpoint replication](GETTING_STARTED_WITH_CROSS_REGION_REPLICATION_ON_KUBERNETES.md)
after the regional volume is healthy.
