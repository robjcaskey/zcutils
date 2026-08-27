# Getting started with cross-region replication on Kubernetes

This guide builds a disposable three-region laboratory in one Kubernetes
cluster. Regions `a`, `b`, and `c` each receive an independent CSI identity,
namespace, state directory, kubelet plugin path, StorageClass, and volume. The
three installations may run different releases, which makes the laboratory
useful for rehearsing region-by-region stair-step upgrades.

One cluster does **not** provide real regional fault isolation. It is the
smallest faithful control and data-path test environment; production regions
normally use separate clusters and trust boundaries.

## 1. Check the cluster

Start with a Kubernetes cluster containing at least one Ready Linux node. The
snapshot API must be installable, privileged CSI node Pods must be permitted,
and each node that may host a volume needs the normal zccusan CSI prerequisites.

```bash
kubectl get nodes
kubectl auth can-i create csidrivers.storage.k8s.io
kubectl auth can-i create daemonsets.apps --namespace default
```

The ordinary [Kubernetes getting-started guide](GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md)
explains those prerequisites and the production Helm installation.

## 2. Create the three regional namespaces

Install the cluster-wide snapshot API once, then create and label each simulated
region explicitly. The labels are descriptive; namespace isolation and the
different CSI identities are what allow the three installations to coexist.

```bash
zccusan/deploy/zcblock-csi/install-snapshot-api.sh

kubectl create namespace zcblock-csi-a
kubectl label namespace zcblock-csi-a zcutils.io/local-region=a

kubectl create namespace zcblock-csi-b
kubectl label namespace zcblock-csi-b zcutils.io/local-region=b

kubectl create namespace zcblock-csi-c
kubectl label namespace zcblock-csi-c zcutils.io/local-region=c
```

Add the published chart repository before installing the three releases:

```bash
helm repo add zcutils https://robjcaskey.github.io/zcutils
helm repo update zcutils
```

## 3. Install the old CSI version into region A

Install the old `0.1.4` CSI chart and image into namespace `zcblock-csi-a`.
This release gets its own CSI driver identity, host state directory, kubelet
plugin socket, and file-backed StorageClass.

```bash
helm install zcblock-csi-a zcutils/zcblock-csi \
  --version 0.1.4 \
  --namespace zcblock-csi-a \
  --set fullnameOverride=zcblock-csi-a \
  --set driverName=io.zcutils.zcblock.a \
  --set image.tag=0.1.4 \
  --set stateDir=/var/lib/zcblock-csi-a \
  --set nodeSetup.enabled=false \
  --set operator.enabled=false \
  --set storageClasses.zcbrd.enabled=false \
  --set storageClasses.zcfile.name=zcfile-a \
  --wait --timeout 120s

kubectl -n zcblock-csi-a rollout status daemonset/zcblock-csi-a-node
```

This installation owns driver `io.zcutils.zcblock.a`, state directory
`/var/lib/zcblock-csi-a`, kubelet plugin directory
`/var/lib/kubelet/plugins/io.zcutils.zcblock.a`, and StorageClass `zcfile-a`.

## 4. Install the newer CSI version into region B

After A is Ready, install the newer `0.1.5` CSI chart and image into namespace
`zcblock-csi-b`. The explicit `b` values prevent it from sharing A's CSI or
host-visible identities.

```bash
helm install zcblock-csi-b zcutils/zcblock-csi \
  --version 0.1.5 \
  --namespace zcblock-csi-b \
  --set fullnameOverride=zcblock-csi-b \
  --set driverName=io.zcutils.zcblock.b \
  --set image.tag=0.1.5 \
  --set stateDir=/var/lib/zcblock-csi-b \
  --set nodeSetup.enabled=false \
  --set operator.enabled=false \
  --set storageClasses.zcbrd.enabled=false \
  --set storageClasses.zcfile.name=zcfile-b \
  --wait --timeout 120s

kubectl -n zcblock-csi-b rollout status daemonset/zcblock-csi-b-node
```

B owns driver `io.zcutils.zcblock.b`, state directory
`/var/lib/zcblock-csi-b`, kubelet plugin directory
`/var/lib/kubelet/plugins/io.zcutils.zcblock.b`, and StorageClass `zcfile-b`.

## 5. Install the newest CSI version into region C

After B is Ready, install the newest `0.1.6` CSI chart and image into namespace
`zcblock-csi-c`. Installing and verifying one release at a time is the same
ordering used for a regional stair-step upgrade.

```bash
helm install zcblock-csi-c zcutils/zcblock-csi \
  --version 0.1.6 \
  --namespace zcblock-csi-c \
  --set fullnameOverride=zcblock-csi-c \
  --set driverName=io.zcutils.zcblock.c \
  --set image.tag=0.1.6 \
  --set stateDir=/var/lib/zcblock-csi-c \
  --set nodeSetup.enabled=false \
  --set operator.enabled=false \
  --set storageClasses.zcbrd.enabled=false \
  --set storageClasses.zcfile.name=zcfile-c \
  --wait --timeout 120s

kubectl -n zcblock-csi-c rollout status daemonset/zcblock-csi-c-node
```

C owns driver `io.zcutils.zcblock.c`, state directory
`/var/lib/zcblock-csi-c`, kubelet plugin directory
`/var/lib/kubelet/plugins/io.zcutils.zcblock.c`, and StorageClass `zcfile-c`.

Now verify all three identities together. Do not continue if a driver,
StorageClass, image, or namespace has accidentally been shared.

```bash
kubectl get namespace -l zcutils.io/local-region
kubectl get csidriver io.zcutils.zcblock.a io.zcutils.zcblock.b io.zcutils.zcblock.c
kubectl get storageclass zcfile-a zcfile-b zcfile-c
kubectl get daemonset -A -l app.kubernetes.io/name=zcblock-csi \
  -o custom-columns=NAMESPACE:.metadata.namespace,NAME:.metadata.name,IMAGE:.spec.template.spec.containers[0].image
```

This laboratory disables node setup because it uses file-backed terminal
volumes and assumes the ordinary Kubernetes getting-started installation has
already prepared the nodes. It disables the three chart-local operators because
the direct CSI/control test below owns this laboratory's replication sequence;
the production checkpoint section installs the declarative operator contract.

This suffixing is only necessary when several simulated regions share one
Kubernetes API server. Separate production clusters may each use the default
`io.zcutils.zcblock` driver identity.

## 6. Establish the test trust graph and exercise failover

With all three regional installations Ready, the test establishes the complete
planned-failover graph: A→B, A→C, and B→C. Each edge receives its own
transfer-scoped `zct1` credential from the target. Credentials are random,
bounded-lifetime, accepted only for new authentication before expiry, and never
printed by the test. This is deliberately narrower than giving one simulated
region a reusable credential for every other region.

Run the executable acceptance test:

```bash
zccusan/deploy/zcblock-csi/test-local-regions-failover.sh
```

The test performs the following observable sequence:

1. It provisions three distinct PVCs through `zcfile-a`, `zcfile-b`, and
   `zcfile-c`, then writes unique state to the A volume.
2. It deletes every Pod mounting those volumes, establishing an explicit writer
   fence and application-consistent cut before any data moves.
3. The userspace replication stage transfers A to B and A to C over
   authenticated AES-256 encrypted TCP. Block devices are terminal media only;
   no block device performs mirroring, striping, placement, or failover.
4. It mounts B, validates the transferred state, writes a B promotion record,
   and unmounts B.
5. It transfers B to C, mounts C, and proves that both the original state and
   the B promotion record survived the stair-step failover.
6. It verifies that no source-volume writer is still running.

A successful run ends with a machine-readable line similar to:

```text
ZCCUSAN_LOCAL_REGIONS_FAILOVER_PASS namespaces=zcblock-csi-a,zcblock-csi-b,zcblock-csi-c volumes=3 volume_handles_distinct=true source_writer_fenced=true first_promotion=b second_promotion=c replication=aes-256-authenticated-tcp placement=userspace block_raid=false
```

This is a **planned, fenced, asynchronous checkpoint failover**. It proves
interoperability across the selected releases and exact data continuity at the
declared cut. It does not claim zero-RPO failover, a writer lease, or automatic
promotion after an ambiguous partition.

To retain the test namespace for inspection, run:

```bash
CLEANUP=0 zccusan/deploy/zcblock-csi/test-local-regions-failover.sh
kubectl -n zcblock-local-regions-failover get pvc,pod -o wide
```

## 7. Reproduce the isolated QEMU proof

The repository also contains a self-contained QEMU acceptance test. It boots a
real K3s API server, imports the three pinned release images, installs all three
CSI identities, provisions three volumes, and runs the same A-to-B-to-C
failover. KVM, QEMU, Podman, and the pinned K3s binary are required.

```bash
WORK_DIR=/mnt/bulk_data/zccusan-local-regions-qemu \
  scripts/zccusan-local-regions-qemu.sh
```

Its final proof marker includes `instances=3`, the exact three image versions,
`cross_region_replication=pass`, and `planned_failover=a-to-b-to-c`.

## Production checkpoint CRD boundary

`CrossRegionReplication` is the declarative asynchronous-checkpoint contract
for separate production clusters. It creates one short-lived sender Pod and
one receiver Pod and transfers directly between configured node backplane
addresses. It creates no Service and requires no multicast.

Create a manifest from the template, then edit the node names, paths, byte
count, and Secret reference:

```bash
cp zccusan/deploy/zcblock-csi/getting-started/cross-region-checkpoint.template.yaml \
  /tmp/cross-region-checkpoint.yaml
$EDITOR /tmp/cross-region-checkpoint.yaml
kubectl apply -f /tmp/cross-region-checkpoint.yaml
kubectl get crossregionreplication example-us-to-uk-checkpoint -w
```

Generate a bounded-lifetime `zct1` credential with `zcrepl token`, store it in
the referenced Secret, and rotate it before expiry. The operator does not copy
the Secret into the CRD, command arguments, logs, or status. The phases advance
through `StartingReceiver`, `Replicating`, and `Ready`; `Ready` is published
only after the receiver has synchronized the target and the accepted, durable,
and applied high-water marks equal the declared byte count.

The native payload framing is authenticated and encrypted with AES-256. It is
not TLS; deployments that require check-the-box TLS must enable the separate
TLS transport option. Neither this CRD nor the local test grants a writer lease.
Promotion remains a separate quorum-fenced operation.

Continue with [global transport security](../../docs/GLOBAL_TRANSPORT_SECURITY.md),
[global volume failover](../../docs/global-volume-failover.md), and the
[CRD stair-step upgrade contract](../deploy/zcblock-csi/CRD-UPGRADES.md).
