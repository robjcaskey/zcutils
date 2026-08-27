# Getting started with zccusan on Kubernetes

## Twenty-sentence quickstart

1. **Experimental:** zccusan is an early-stage project whose installation, kernel module, networking, CPU placement, huge-page allocation, memlock, and storage topology still require careful configuration.

2. Expect to use a capable automation agent to inspect your nodes, adapt these examples, and tune the installation before evaluating performance or safety.

3. In one [recorded reference](../../bench-results/zc-current-dualefa-block-20260823T234720Z/client-pull/current-record-12m-gated/summary.log)—not a promise—a Linux-block `/dev/zcnblk0` client averaged 12.32 million 4 KiB remote-read IOPS across three runs on a tuned two-node `c8gn.48xlarge` configuration with 64 pinned lanes, QD64 per worker, 4,096 aggregate outstanding operations, two EFA devices, EFA-direct RMA, volatile remote memory, and initiator-local CQ data-visible completion.

4. In a separate [recorded reference](../../bench-results/zc-zcnblk-c8in96w2c-tcp-arena-write-kickfix-alias-qd128-long-20260822T1900Z/summary.log)—not directly comparable—the same block frontend averaged 1.68 million 4 KiB early-local-ack write IOPS across three runs on a tuned two-node `c8in.metal-96xl` configuration with 32 pinned lanes, QD128 per worker, 4,096 aggregate outstanding operations, two TCP rails, huge-page-backed shared arenas, and volatile remote memory, while remote sync and FUA were outside the timed workload.

5. Those results demonstrate multi-million-IOPS potential but do not predict this one-lane mirrored PVC tutorial, durable media, mixed workloads, fsync latency, smaller machines, different kernels, or an untuned cluster.

6. You need Helm 3, `kubectl`, three distinct Ready Linux workers, an encrypted pod or physical backplane, and permission to run the chart's privileged node setup and CSI containers.

7. Before installing, use a supplied module only for an exact architecture and full `uname -r` match, or follow the [kernel-module build example](BUILD_KERNEL_MODULE.md) to build, validate, sign, and publish one; the first supplied profile is for Amazon Linux 2023 x86_64 with kernel `6.12.100-125.179.amzn2023.x86_64`.

8. Select two storage workers and leave at least one other worker available for the client workload.

   ```bash
   export LEAF_A=worker-a
   export LEAF_B=worker-b
   test "$LEAF_A" != "$LEAF_B"
   kubectl label node "$LEAF_A" \
     storage.zcutils.io/getting-started-leaf=true --overwrite
   kubectl label node "$LEAF_B" \
     storage.zcutils.io/getting-started-leaf=true --overwrite
   kubectl get nodes -L storage.zcutils.io/getting-started-leaf
   kubectl get nodes -l '!storage.zcutils.io/getting-started-leaf'
   ```

9. If `InternalIP` is not the dedicated storage address, annotate each eligible leaf and client node with `storage.zcutils.io/backplane-address`.

   ```bash
   kubectl annotate node "$LEAF_A" \
     storage.zcutils.io/backplane-address=10.20.0.11 --overwrite
   kubectl annotate node "$LEAF_B" \
     storage.zcutils.io/backplane-address=10.20.0.12 --overwrite
   ```

10. Leave `HELM_ARGS` empty for TCP, or add the strict RDMA capability gate and your provider only when every selected node exposes the required device and libfabric provider.

   ```bash
   HELM_ARGS=()

   # EFA example
   # HELM_ARGS+=(--set backplane.rdma.enabled=true)
   # HELM_ARGS+=(--set backplane.rdma.provider=efa)

   # Add this when using image or HTTP module-source values
   # HELM_ARGS+=(--values node-module-values.yaml)
   ```

11. Install the CRDs, operator, CSI driver, artifact cache, and node bootstrap with the checked command below, which rejects every kernel or architecture mismatch before selecting the immutable module image; other Linux hosts should use the generic Helm form after supplying their reviewed image or HTTP values.

   ```bash
   # Supplied fast path: exact AL2023 x86_64 kernel match only
   scripts/zccusan-install-kmod-fastpath.sh \
     al2023-6.12.100-125.179-x86_64 \
     -- "${HELM_ARGS[@]}"

   # Other exact-match Linux kernels, after creating node-module-values.yaml
   # helm upgrade --install zccusan \
   #   ./zccusan/charts/zcblock-csi \
   #   --namespace zccusan \
   #   --create-namespace \
   #   --values node-module-values.yaml \
   #   "${HELM_ARGS[@]}" \
   #   --wait \
   #   --timeout 10m
   ```

12. Wait for the operator and node DaemonSet, then inspect setup logs if `/dev/zcnblk0` and `/dev/zcnblk-shmctl` do not appear on eligible client nodes.

   ```bash
   kubectl -n zccusan rollout status \
     deployment/zccusan-zcblock-csi-operator --timeout=10m
   kubectl -n zccusan rollout status \
     daemonset/zccusan-zcblock-csi-node --timeout=10m
   kubectl -n zccusan get pods -o wide
   ```

13. For the safest first run, create a two-copy userspace mirror over volatile RAM, or inspect and render `mirror-block.template.yaml` if you intentionally dedicate two empty real partitions and approve destructive raw writes.

   ```bash
   kubectl apply -f \
     zccusan/deploy/zcblock-csi/getting-started/mirror-ram.yaml
   kubectl get storageprofile getting-started-mirror-ram
   kubectl get storageclass zc-mirror-ram
   ```

14. For a qualified RDMA cluster, review `mirror-rdma.template.yaml`, set its device-plugin resource and provider for your environment, apply it, and use `zc-mirror-rdma-ram` instead of `zc-mirror-ram` below without expecting silent TCP fallback.

   ```bash
   less zccusan/deploy/zcblock-csi/getting-started/mirror-rdma.template.yaml
   kubectl apply -f \
     zccusan/deploy/zcblock-csi/getting-started/mirror-rdma.template.yaml
   ```

15. Create the TCP PVC and fio pod as checked in, or substitute the reviewed RDMA StorageClass before applying the same workload.

   ```bash
   kubectl -n zccusan apply -f \
     zccusan/deploy/zcblock-csi/getting-started/mirror-fio.yaml

   # RDMA alternative
   # sed 's/zc-mirror-ram/zc-mirror-rdma-ram/' \
   #   zccusan/deploy/zcblock-csi/getting-started/mirror-fio.yaml | \
   #   kubectl -n zccusan apply -f -
   ```

16. Read fio's JSON after the functional 4 KiB `randrw` run succeeds, remembering that this QD16 pod is deliberately not the tuned record configuration described above.

   ```bash
   kubectl -n zccusan wait pod/zc-mirror-fio \
     --for=jsonpath='{.status.phase}'=Succeeded \
     --timeout=15m
   kubectl -n zccusan logs zc-mirror-fio
   ```

17. Verify that the operator placed both terminal leaves away from the fio node and that `/dev/zcnblk0` remains only the client edge while the userspace mirror owns placement.

   ```bash
   export ZCV="$(kubectl -n zccusan get zcvolumes \
     -o jsonpath='{.items[0].metadata.name}')"
   kubectl -n zccusan get zcvolume "$ZCV" -o yaml
   kubectl -n zccusan get pods \
     -l "storage.zcutils.io/volume=$ZCV" -o wide
   kubectl -n zccusan get pod zc-mirror-fio \
     -o jsonpath='{.spec.nodeName}{"\n"}'
   ```

18. Treat RAM media as explicitly volatile, provide transport encryption outside these preview data paths, and create any `VolumeSnapshotClass` separately because the chart does not create one.

19. Remove the test pod and PVC when finished so CSI can delete the `ZcVolume` after the operator tears down its userspace runtime.

   ```bash
   kubectl -n zccusan delete pod zc-mirror-fio
   kubectl -n zccusan delete pvc zc-mirror
   ```

20. Continue with [tiering](GETTING_STARTED_WITH_TIERING_ON_KUBERNETES.md), then [cross-region replication](GETTING_STARTED_WITH_CROSS_REGION_REPLICATION_ON_KUBERNETES.md), and consult the detailed performance documentation before publishing representative claims.
