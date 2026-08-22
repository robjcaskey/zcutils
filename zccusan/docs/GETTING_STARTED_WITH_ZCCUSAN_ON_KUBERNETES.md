# Getting started with zccusan on Kubernetes

This walkthrough creates one PVC backed by a two-copy userspace mirror, runs
4 KiB random I/O through it, and proves that the workload is not running on
either storage-leaf node. You declare storage intent; the operator creates the
runtime. You do not create leaf, mirror, onramp, or data-path Services.

## What you need

- `kubectl` and Helm 3
- three distinct Ready Linux worker nodes
- an encrypted pod/backplane network between those nodes
- `/dev/zcnblk0` and `/dev/zcnblk-shmctl` prepared on every node that may host
  the client workload

`/dev/zcnblk0` is only the client block edge. The operator places a separate
userspace mirror after that edge and connects it directly to terminal leaves.
The kernel client never chooses mirror placement.

This first reconciler pins the PV to its selected client-edge node. The two
terminal leaves are remote from that node. It does not yet advertise
topology-free client failover: doing that safely requires fencing and draining
the old onramp before moving the attachment.

The current `TcpMux` mirror runtime is a preview and does not yet provide
native payload encryption. Use it only over a CNI or physical backplane that
provides encryption. It is not a production security benchmark configuration.

The client-edge module setup is host/kernel specific. Complete
[the zcnblk client setup](../../docs/zcnblk-single-target-howto.md) on eligible
client nodes before continuing.

## 1. Reserve two nodes for leaves

Choose two distinct worker nodes. Leave at least one other worker unlabeled so
Kubernetes can schedule the fio client there.

```bash
export LEAF_A=worker-a
export LEAF_B=worker-b

test -n "$LEAF_A"
test -n "$LEAF_B"
test "$LEAF_A" != "$LEAF_B"

kubectl label node "$LEAF_A" \
  storage.zcutils.io/getting-started-leaf=true --overwrite
kubectl label node "$LEAF_B" \
  storage.zcutils.io/getting-started-leaf=true --overwrite

kubectl get nodes \
  -L storage.zcutils.io/getting-started-leaf
kubectl get nodes \
  -l '!storage.zcutils.io/getting-started-leaf'
```

The last command must show at least one Ready worker. The fio Pod uses required
node affinity that excludes the two labeled leaf nodes.

If your storage backplane has dedicated node addresses, annotate all three
nodes. Otherwise the operator uses each node's `InternalIP`:

```bash
kubectl annotate node "$LEAF_A" \
  storage.zcutils.io/backplane-address=10.20.0.11 --overwrite
kubectl annotate node "$LEAF_B" \
  storage.zcutils.io/backplane-address=10.20.0.12 --overwrite
```

Annotate the eventual client node as well when `InternalIP` is not its storage
backplane address.

## 2. Install the chart

From a source checkout containing the v1alpha1 operator:

```bash
helm upgrade --install zccusan \
  ./zccusan/charts/zcblock-csi \
  --namespace zccusan \
  --create-namespace \
  --wait \
  --timeout 10m

kubectl -n zccusan rollout status \
  daemonset/zccusan-zcblock-csi-node --timeout=10m
kubectl -n zccusan rollout status \
  deployment/zccusan-zcblock-csi-operator --timeout=10m
```

Verify the declarative API:

```bash
kubectl get crd \
  storageprofiles.storage.zcutils.io \
  mediagrants.storage.zcutils.io \
  zcvolumes.storage.zcutils.io
```

The chart does not create a `VolumeSnapshotClass`. Snapshot policy remains a
separate cluster-administrator decision.

## 3. Choose the terminal media

Start with volatile userspace RAM for a non-destructive functional test:

```bash
kubectl apply -f \
  zccusan/deploy/zcblock-csi/getting-started/mirror-ram.yaml

kubectl get storageprofile getting-started-mirror-ram
kubectl get storageclass zc-mirror-ram
```

The RAM leaves are explicitly volatile and do not make a durable-acknowledgment
claim.

To use two real partitions instead, obtain the stable PARTUUID on each leaf
node. Both partitions must be dedicated and empty: the leaf writer will
overwrite them. Render and inspect the template before applying it:

```bash
export LEAF_A_PARTUUID=11111111-2222-3333-4444-555555555555
export LEAF_B_PARTUUID=aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee

sed \
  -e "s/LEAF_A_NODE/$LEAF_A/g" \
  -e "s/LEAF_A_PARTUUID/$LEAF_A_PARTUUID/g" \
  -e "s/LEAF_B_NODE/$LEAF_B/g" \
  -e "s/LEAF_B_PARTUUID/$LEAF_B_PARTUUID/g" \
  zccusan/deploy/zcblock-csi/getting-started/mirror-block.template.yaml \
  > /tmp/zc-mirror-block.yaml

less /tmp/zc-mirror-block.yaml
kubectl apply -f /tmp/zc-mirror-block.yaml
kubectl get storageclass zc-mirror-block
```

Selection alone never authorizes raw writes. The block template therefore
contains a conspicuous, per-source `destructivePreparation.allowRawWrites`
approval; its default is `false`.

## 4. Create the PVC and run fio

The checked-in workload uses the RAM-backed StorageClass:

```bash
kubectl -n zccusan apply -f \
  zccusan/deploy/zcblock-csi/getting-started/mirror-fio.yaml

kubectl -n zccusan get pvc zc-mirror -w
```

For the real-partition profile, change only the PVC's `storageClassName` from
`zc-mirror-ram` to `zc-mirror-block` before applying it.

Wait for fio to finish and print its JSON result:

```bash
kubectl -n zccusan wait pod/zc-mirror-fio \
  --for=jsonpath='{.status.phase}'=Succeeded \
  --timeout=15m

kubectl -n zccusan logs zc-mirror-fio
```

This is a functional 4 KiB `randrw` run. It is not a representative high-IOPS
result: the example does not declare huge pages, memlock headroom, worker and
kthread CPU pinning, hctx affinity, lane-to-CPU maps, or strict fast-path
settings.

## 5. Verify placement and the backplane path

CSI creates the `ZcVolume`; the operator fills in its selected runtime:

```bash
export ZCV="$(kubectl -n zccusan get zcvolumes \
  -o jsonpath='{.items[0].metadata.name}')"

kubectl -n zccusan get zcvolume "$ZCV" -o yaml
kubectl -n zccusan get pods \
  -l "storage.zcutils.io/volume=$ZCV" \
  -o wide
```

Prove that fio is not on either selected leaf:

```bash
export FIO_NODE="$(kubectl -n zccusan get pod zc-mirror-fio \
  -o jsonpath='{.spec.nodeName}')"
export LEAF_NODES="$(kubectl -n zccusan get zcvolume "$ZCV" \
  -o jsonpath='{.status.runtime.leaves[*].nodeName}')"

for node in $LEAF_NODES; do
  test "$FIO_NODE" != "$node"
done

printf 'fio_node=%s leaf_nodes=%s\n' "$FIO_NODE" "$LEAF_NODES"
```

The runtime status also shows each direct backplane address, transport, fan
node, and terminal-media kind. No ClusterIP sits in the data path:

```bash
kubectl -n zccusan get service \
  -l "storage.zcutils.io/volume=$ZCV"
```

The expected result is `No resources found`.

## 6. Clean up

```bash
kubectl -n zccusan delete pod zc-mirror-fio
kubectl -n zccusan delete pvc zc-mirror
```

PVC deletion causes CSI to delete the `ZcVolume`. Its finalizer keeps the CR
until the operator has removed the fan, onramp, terminal leaves, and raw-media
allowlist ConfigMaps.

## Next

Continue with [tiering on Kubernetes](GETTING_STARTED_WITH_TIERING_ON_KUBERNETES.md),
then [cross-region replication](GETTING_STARTED_WITH_CROSS_REGION_REPLICATION_ON_KUBERNETES.md).
