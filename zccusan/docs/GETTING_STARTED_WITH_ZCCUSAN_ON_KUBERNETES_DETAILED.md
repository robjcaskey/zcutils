# Getting started 2: explained Kubernetes installation

This is the complete walkthrough behind the
[short happy path](GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md). Use
it when the short path does not work, when the defaults do not describe the
cluster, or before treating the installation as anything beyond a disposable
test.

## Detailed walkthrough

1. **Experimental:** zccusan is an early-stage project whose installation, kernel module, networking, CPU placement, huge-page allocation, memlock, and storage topology still require careful configuration.

2. Expect to use a capable automation agent to inspect your nodes, adapt these examples, and tune the installation before evaluating performance or safety.

3. In one [recorded reference](../../bench-results/zc-current-dualefa-block-20260823T234720Z/client-pull/current-record-12m-gated/summary.log)—not a promise—a Linux-block `/dev/zcnblk0` client averaged 12.32 million 4 KiB remote-read IOPS across three runs on a tuned two-node `c8gn.48xlarge` configuration with 64 pinned lanes, QD64 per worker, 4,096 aggregate outstanding operations, two EFA devices, EFA-direct RMA, volatile remote memory, and initiator-local CQ data-visible completion.

4. In a separate [recorded reference](../../bench-results/zc-zcnblk-c8in96w2c-tcp-arena-write-kickfix-alias-qd128-long-20260822T1900Z/summary.log)—not directly comparable—the same block frontend averaged 1.68 million 4 KiB early-local-ack write IOPS across three runs on a tuned two-node `c8in.metal-96xl` configuration with 32 pinned lanes, QD128 per worker, 4,096 aggregate outstanding operations, two TCP rails, huge-page-backed shared arenas, and volatile remote memory, while remote sync and FUA were outside the timed workload.

5. Those results demonstrate multi-million-IOPS potential but do not predict this one-lane mirrored PVC tutorial, durable media, mixed workloads, fsync latency, smaller machines, different kernels, or an untuned cluster.

6. You need Helm 3, `kubectl`, three distinct Ready Linux workers, an encrypted pod or physical backplane, and permission to run the chart's privileged node setup and CSI containers.

7. The normal multi-architecture DaemonSet image contains the Rust node loader and the [current audited module matrix](../../docs/kernel-module-build-matrix.md); it selects only an exact architecture and full `uname -r` match and fails closed before CSI startup on an unsupported kernel.

8. Create the namespace explicitly so its ownership and lifecycle are visible.

   ```bash
   kubectl create namespace zccusan
   ```

9. Choose two example servers and one example client. The RAM `MediaGrant` uses the server label as its complete placement boundary, while the fio manifest asks Kubernetes for the client label.

   ```bash
   kubectl get nodes
   kubectl label node some-node-a storage.zcutils.io/example-server=true
   kubectl label node some-node-b storage.zcutils.io/example-server=true
   kubectl label node some-node-c storage.zcutils.io/example-client=true
   kubectl get nodes -L \
     storage.zcutils.io/example-server,storage.zcutils.io/example-client
   ```

10. If `InternalIP` is not the dedicated storage address, annotate both example servers with their storage addresses.

   ```bash
   kubectl annotate node some-node-a \
     storage.zcutils.io/backplane-address=10.20.0.11
   kubectl annotate node some-node-b \
     storage.zcutils.io/backplane-address=10.20.0.12
   ```

11. Leave `HELM_ARGS` empty for TCP, or add the strict RDMA capability gate and your provider only when every selected node exposes the required device and libfabric provider.

   ```bash
   HELM_ARGS=()

   # EFA example
   # HELM_ARGS+=(--set backplane.rdma.enabled=true)
   # HELM_ARGS+=(--set backplane.rdma.provider=efa)

   # A custom unsupported kernel can use reviewed host or HTTP module-source
   # values, or a custom full DaemonSet image; the matrix needs no extra values.
   ```

12. Install the CRDs, operator, CSI driver, and Rust node bootstrap in one Helm step; pin `image.digest` for a reproducible deployment, and provide custom module-source values only when your exact kernel is outside the bundled matrix.

   ```bash
   helm repo add zcutils https://robjcaskey.github.io/zcutils
   helm repo update
   helm upgrade --install zccusan \
     zcutils/zcblock-csi \
     --version 0.1.5 \
     --namespace zccusan \
     "${HELM_ARGS[@]}" \
     --wait \
     --timeout 10m
   ```

13. Wait for the operator and node DaemonSet, then inspect setup logs if `/dev/zcnblk0` and `/dev/zcnblk-shmctl` do not appear on eligible client nodes.

   ```bash
   kubectl -n zccusan rollout status \
     deployment/zccusan-zcblock-csi-operator --timeout=10m
   kubectl -n zccusan rollout status \
     daemonset/zccusan-zcblock-csi-node --timeout=10m
   kubectl -n zccusan get pods -o wide
   ```

14. Apply the safest first storage declaration: two userspace mirror copies over volatile RAM on nodes selected by the `MediaGrant`. The operator turns that declaration into the `zc-mirror-ram` StorageClass. For real media, inspect and render `mirror-block.template.yaml` only when you intentionally dedicate two empty partitions and approve destructive raw writes.

   ```bash
   kubectl apply -f \
     zccusan/deploy/zcblock-csi/getting-started/mirror-ram.yaml
   kubectl get mediagrant getting-started-ram -o yaml
   kubectl get storageprofile getting-started-mirror-ram
   kubectl get storageclass zc-mirror-ram
   ```

15. For a qualified RDMA cluster, review `mirror-rdma.template.yaml`, set its device-plugin resource and provider for your environment, apply it, and use `zc-mirror-rdma-ram` instead of `zc-mirror-ram` below without expecting silent TCP fallback.

   ```bash
   less zccusan/deploy/zcblock-csi/getting-started/mirror-rdma.template.yaml
   kubectl apply -f \
     zccusan/deploy/zcblock-csi/getting-started/mirror-rdma.template.yaml
   ```

16. Create the TCP PVC and fio Pod from their standalone files, or substitute the reviewed RDMA StorageClass in the PVC before applying the same workload.

   ```bash
   kubectl apply -f \
     zccusan/deploy/zcblock-csi/getting-started/mirror-pvc.yaml
   kubectl apply -f \
     zccusan/deploy/zcblock-csi/getting-started/mirror-fio.yaml

   # RDMA alternative
   # sed 's/zc-mirror-ram/zc-mirror-rdma-ram/' \
   #   zccusan/deploy/zcblock-csi/getting-started/mirror-pvc.yaml | \
   #   kubectl apply -f -
   # kubectl apply -f \
   #   zccusan/deploy/zcblock-csi/getting-started/mirror-fio.yaml
   ```

17. Follow fio live through the functional 4 KiB `randrw` run, remembering that this QD16 pod is deliberately not the tuned record configuration described above.

   ```bash
   kubectl -n zccusan logs --follow \
     --pod-running-timeout=10m \
     zc-mirror-fio
   ```

18. Verify that both storage-copy Pods run on the example servers, fio runs on the example client, and `/dev/zcnblk0` remains only the client edge while the userspace mirror owns placement.

   ```bash
   export ZCV="$(kubectl -n zccusan get zcvolumes \
     -o jsonpath='{.items[0].metadata.name}')"
   kubectl -n zccusan get zcvolume "$ZCV" -o yaml
   kubectl -n zccusan get pods \
     -l "storage.zcutils.io/volume=$ZCV" -o wide
   kubectl -n zccusan get pod zc-mirror-fio \
     -o jsonpath='{.spec.nodeName}{"\n"}'
   ```

19. Treat RAM media as explicitly volatile, provide transport encryption outside these preview data paths, and create any `VolumeSnapshotClass` separately because the chart does not create one.

20. Remove the test pod and PVC when finished so CSI can delete the `ZcVolume` after the operator tears down its userspace runtime.

   ```bash
   kubectl delete -f \
     zccusan/deploy/zcblock-csi/getting-started/mirror-fio.yaml
   kubectl delete -f \
     zccusan/deploy/zcblock-csi/getting-started/mirror-pvc.yaml
   ```

21. Continue with [tiering](GETTING_STARTED_WITH_TIERING_ON_KUBERNETES.md), then [cross-region replication](GETTING_STARTED_WITH_CROSS_REGION_REPLICATION_ON_KUBERNETES.md), and consult the detailed performance documentation before publishing representative claims.
