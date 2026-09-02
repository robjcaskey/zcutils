# Getting started 1: Kubernetes happy path

This deliberately assumes everything goes right: run it from a zcutils checkout with Helm 3, `kubectl`, three Kubernetes nodes, a kernel included in the [module matrix](../../docs/kernel-module-build-matrix.md) (otherwise follow the [custom-kernel build guide](BUILD_KERNEL_MODULE.md)), and permission to run privileged CSI Pods. Replace the three example node names.

Create a namespace to hold the zccusan services and this example workload.

```bash
kubectl create ns zccusan
```

Get a list of nodes and, to demonstrate the CSI network path, label two as example servers and a different one as the example client.

```bash
kubectl get nodes
kubectl label node some-node-a storage.zcutils.io/example-server=true
kubectl label node some-node-b storage.zcutils.io/example-server=true
kubectl label node some-node-c storage.zcutils.io/example-client=true
```

Install the CSI.

On EKS with EFA-direct, first make sure the three selected nodes are
EFA-capable and install the pinned AWS EFA device plugin. TCP users skip this
block.

```bash
helm repo add aws-eks https://aws.github.io/eks-charts
helm repo update aws-eks
helm upgrade --install aws-efa-device-plugin \
  aws-eks/aws-efa-k8s-device-plugin \
  --version v0.5.30 --namespace kube-system --wait --timeout 120s
kubectl get nodes \
  -o 'custom-columns=NAME:.metadata.name,EFA:.status.allocatable.vpc\.amazonaws\.com/efa'
```

Each selected EKS node must report `1` in the `EFA` column before continuing.

```bash
helm repo add zcutils https://robjcaskey.github.io/zcutils
helm repo update
helm upgrade --install zccusan zcutils/zcblock-csi --version 0.1.6 --namespace zccusan --wait --timeout 120s
# If your nodes use EFA-direct, add --set backplane.rdma.enabled=true --set backplane.rdma.provider=efa-direct to this command.
```

Create `media-grant.yaml` with the following contents to tell zccusan that the example servers can provide fast, volatile RAM storage. It is lost on restart, but it is useful for end-to-end performance testing.

<!-- BEGIN FILE: zccusan/deploy/zcblock-csi/getting-started/media-grant.yaml -->
```yaml
apiVersion: storage.zcutils.io/v1alpha1
kind: MediaGrant
metadata:
  name: getting-started-ram
spec:
  nodeSelector:
    matchLabels:
      storage.zcutils.io/example-server: "true"
  mediaSets:
    - name: userspace-memory
      dynamicSources:
        - kind: MemoryArena
          maximumVolumeSize: 1Gi
      publishAs:
        mediaClass: getting-started-ram
        durability: Volatile
        mayContributeToDurableAcknowledgement: false
```
<!-- END FILE: zccusan/deploy/zcblock-csi/getting-started/media-grant.yaml -->

Apply it with `kubectl apply -f media-grant.yaml`.

Create `storage-profile.yaml` with the following contents to define a StorageClass named `zc-mirror-ram` that keeps two copies of each volume's data, one copy on each example server.

<!-- BEGIN FILE: zccusan/deploy/zcblock-csi/getting-started/storage-profile.yaml -->
```yaml
apiVersion: storage.zcutils.io/v1alpha1
kind: StorageProfile
metadata:
  name: getting-started-mirror-ram
spec:
  storageClass:
    name: zc-mirror-ram
  frontend: LinuxBlock
  placement:
    primitive: Mirror
    execution: Userspace
    copies: 2
    mediaClass: getting-started-ram
    excludeClientNode: false
    distinctTopologyKeys:
      - kubernetes.io/hostname
  transport:
    # For EFA-direct, change TcpMux to OfiRdm and uncomment the next three fields.
    kind: TcpMux
    # ofiProvider: efa-direct
    # ofiEndpoint: rdm
    # deviceResourceName: vpc.amazonaws.com/efa
    addressSource: NodeAnnotationThenInternalIP
    nodeAddressAnnotation: storage.zcutils.io/backplane-address
    lanes: 1
    connectionsPerLane: 1
    chunkBytes: 4096
```
<!-- END FILE: zccusan/deploy/zcblock-csi/getting-started/storage-profile.yaml -->

Apply it with `kubectl apply -f storage-profile.yaml`.

Create `mirror-pvc.yaml` with the following contents to request one mirrored volume.

<!-- BEGIN FILE: zccusan/deploy/zcblock-csi/getting-started/mirror-pvc.yaml -->
```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: zc-mirror
  namespace: zccusan
spec:
  accessModes:
    - ReadWriteOnce
  storageClassName: zc-mirror-ram
  resources:
    requests:
      storage: 256Mi
```
<!-- END FILE: zccusan/deploy/zcblock-csi/getting-started/mirror-pvc.yaml -->

Apply it with `kubectl apply -f mirror-pvc.yaml`.

Create `mirror-fio.yaml` with the following contents to start a small 4 KiB fio smoke test on the example client. This uses the zccusan-owned `zccusan-storage-test` image rather than installing fio from a third-party image at Pod startup.

<!-- BEGIN FILE: zccusan/deploy/zcblock-csi/getting-started/mirror-fio.yaml -->
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: zc-mirror-fio
  namespace: zccusan
spec:
  restartPolicy: Never
  securityContext:
    runAsNonRoot: true
    runAsUser: 999
    runAsGroup: 999
    fsGroup: 999
    fsGroupChangePolicy: OnRootMismatch
    seccompProfile:
      type: RuntimeDefault
  affinity:
    nodeAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        nodeSelectorTerms:
          - matchExpressions:
              - key: storage.zcutils.io/example-client
                operator: In
                values:
                  - "true"
  containers:
    - name: fio
      image: docker.io/robjcaskey/zccusan-storage-test:0.1.6
      imagePullPolicy: IfNotPresent
      command: [fio]
      args:
        - --name=zc-mirror-randrw
        - --filename=/data/fio.bin
        - --size=96Mi
        - --rw=randrw
        - --rwmixread=70
        - --bs=4k
        - --ioengine=libaio
        - --iodepth=16
        - --direct=1
        - --runtime=30
        - --time_based=1
        - --group_reporting=1
        - --unlink=1
        - --output-format=json
      securityContext:
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
        capabilities:
          drop: [ALL]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: zc-mirror
```
<!-- END FILE: zccusan/deploy/zcblock-csi/getting-started/mirror-fio.yaml -->

Apply it with `kubectl apply -f mirror-fio.yaml`, then follow fio live until it finishes.

```bash
kubectl -n zccusan logs --follow --pod-running-timeout=120s zc-mirror-fio
```

Now that fio has finished, delete its Pod so it releases the volume.

```bash
kubectl delete -f mirror-fio.yaml
```

Create `mirror-pgbench.yaml` with the following contents to use the same PVC for a real PostgreSQL data directory and run pgbench. The database listens only on the Pod's Unix socket, and the Pod stops PostgreSQL after the benchmark.

<!-- BEGIN FILE: zccusan/deploy/zcblock-csi/getting-started/mirror-pgbench.yaml -->
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: zc-mirror-pgbench
  namespace: zccusan
spec:
  restartPolicy: Never
  securityContext:
    runAsNonRoot: true
    runAsUser: 999
    runAsGroup: 999
    fsGroup: 999
    fsGroupChangePolicy: OnRootMismatch
    seccompProfile:
      type: RuntimeDefault
  affinity:
    nodeAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        nodeSelectorTerms:
          - matchExpressions:
              - key: storage.zcutils.io/example-client
                operator: In
                values:
                  - "true"
  containers:
    - name: pgbench
      image: docker.io/robjcaskey/zccusan-storage-test:0.1.6
      imagePullPolicy: IfNotPresent
      command: [/bin/bash, -ec]
      args:
        - |
          export PGDATA=/data/postgres
          export PGHOST=/tmp
          mkdir -p "${PGDATA}"
          if [ ! -s "${PGDATA}/PG_VERSION" ]; then
            initdb --auth=trust --encoding=UTF8 --no-locale
          fi
          pg_ctl -w start -o "-c listen_addresses= -c unix_socket_directories=/tmp -c min_wal_size=32MB -c max_wal_size=128MB"
          trap 'pg_ctl -m fast -w stop' EXIT
          if ! psql --dbname=postgres --tuples-only --command \
            "SELECT 1 FROM pg_database WHERE datname = 'pgbench'" | grep -q 1
          then
            createdb pgbench
            pgbench --initialize --scale=1 pgbench
          fi
          pgbench --client=8 --jobs=4 --time=30 --progress=5 pgbench
      securityContext:
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
        capabilities:
          drop: [ALL]
      volumeMounts:
        - name: data
          mountPath: /data
        - name: tmp
          mountPath: /tmp
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: zc-mirror
    - name: tmp
      emptyDir:
        sizeLimit: 64Mi
```
<!-- END FILE: zccusan/deploy/zcblock-csi/getting-started/mirror-pgbench.yaml -->

Apply it with `kubectl apply -f mirror-pgbench.yaml`, then follow pgbench live until it finishes.

```bash
kubectl -n zccusan logs --follow --pod-running-timeout=120s zc-mirror-pgbench
```

That exercised one mirrored RAM copy on each example server through fio and PostgreSQL from the separate example client. The RAM copies are intentionally **volatile**.

## Optional: validate the single-region HA boundary

Before tearing the example down, you can continue with [the optional single-region reliability validation](VALIDATING_SINGLE_REGION_HA_ON_KUBERNETES.md). It installs a separate chaos toolbox and runs one simple, graded check while the mirrored volume stays under a continuously synchronized canary workload.

## Tear down

If you skipped the optional validation, delete the finished pgbench Pod. Then delete the PVC; the generated ZcVolume finalizer deletes its userspace storage Pods and releases both RAM arenas. Delete the StorageProfile so the operator removes its generated `zc-mirror-ram` StorageClass, and finally remove the MediaGrant that offered the RAM capacity. Leave the CSI installed for the next tutorial.

```bash
kubectl delete -f mirror-pgbench.yaml --ignore-not-found
kubectl delete -f single-region-ha-canary.yaml --ignore-not-found
kubectl delete -f single-region-ha-hostpath-comparator.yaml --ignore-not-found
kubectl delete -f mirror-pvc.yaml
kubectl delete -f storage-profile.yaml
kubectl delete -f media-grant.yaml
```

If any command fails—or before using real disks, RDMA, or performance tuning—continue with [getting started 2](GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES_DETAILED.md), then try [tiering](GETTING_STARTED_WITH_TIERING_ON_KUBERNETES.md) and review the [cross-region replication](GETTING_STARTED_WITH_CROSS_REGION_REPLICATION_ON_KUBERNETES.md) guide's distinction between the current checkpoint compatibility test and the intended continuous-WAL path.
