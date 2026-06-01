# zcblock CSI Driver

This deploys the Rust `zcblock-csi` binary as a privileged CSI node plugin for a
local Kubernetes node. It can provision RAM-backed `zcbrd` devices, sparse
file-backed loop devices, guarded raw block devices, and full-image CSI
snapshots.

Architecturally this is `zccsi`, a convenience zccusan adapter. `zccusan` means
Zero Copy Cinematic Universe Storage Area Network. The chain is descriptor
primitives -> zero-copy streams -> zero-copy WALs, including WALs of WALs ->
`zcvolume` -> `zcsan` -> convenience `zccsi`. CSI is just one client of that
storage network; the zccusan control protocol and local agent own snapshots,
streams, freeze barriers, replication policy, topology, and later WAL/gateway
behavior. See `zccusan/docs/zccusan.md`.

Build the distroless image, import it into local containerd, install the
snapshot API/controller, and apply the driver:

```sh
zccusan/deploy/zcblock-csi/build-load-apply.sh
```

To simulate three regions in this same local cluster, install three independent
CSI identities while sharing the snapshot CRDs/controller:

```sh
zccusan/deploy/zcblock-csi/install-local-regions.sh -a -b -c
zccusan/deploy/zcblock-csi/test-local-regions-cohesive.sh
zccusan/deploy/zcblock-csi/test-local-regions-stream-repl.sh
```

The local region install creates `zcblock-csi-a`, `zcblock-csi-b`, and
`zcblock-csi-c` namespaces, with driver names `io.zcutils.zcblock.a`,
`.b`, and `.c`. Real multi-region clusters can keep the default
`io.zcutils.zcblock` driver name because each region has its own Kubernetes API
server. The CRD versioning and stair-step upgrade rule is documented in
`zccusan/deploy/zcblock-csi/CRD-UPGRADES.md`.

The minimal Kubernetes API role is captured in
`zccusan/deploy/zcblock-csi/rbac-minimal.yaml`. The Rust CSI/control containers do not
call the Kubernetes API directly; the ServiceAccount exists for the standard CSI
sidecars. The minimum cluster-wide permissions cover PV/PVC provisioning,
events, node/topology reads, storage class/CSINode reads, and snapshot content
updates. Leader-election leases are intentionally namespace-local through a
RoleBinding, with the manifests setting
`--leader-election-namespace=$(POD_NAMESPACE)` on the sidecars.

There is also a Helm chart at `zccusan/charts/zcblock-csi`. Chart rule: no Helm hooks.
It supports stair-step upgrades with `helm template ... | kubectl apply -f -`;
apply and wait for one region's DaemonSet rollout before applying the next
region. CRD installation and snapshot-controller stair-step upgrades stay
outside the chart and remain handled by
`zccusan/deploy/zcblock-csi/install-snapshot-api.sh`.

The CSI container now has a sibling `zcblock-control` sidecar in the same pod.
That sidecar is the first local zccusan agent. It exposes the local control
plane on `http://127.0.0.1:9788` using the OpenAPI contract in
`src/openapi/zcblock-control.yaml` and also serves that contract from
`/openapi.yaml`. CSI snapshot create/delete calls use this REST client path; the
older Unix socket remains available for local compatibility.

The stream replication test creates a snapshot in region `a`, starts encrypted
replication receivers in region `b` and `c`, and has region `a` stream the
snapshot image into the target volumes. `zcrepl csi-recv`, `zcrepl csi-send`,
and `zcrepl csi-status` can drive either the REST control URL or the legacy Unix
socket.

The concurrent C receive test keeps regions `a`, `b`, and `c` alive, starts two
receivers on region `c`, streams a live source volume from `a` to one C target,
and simultaneously streams a snapshot from `b` to another C target:

```sh
zccusan/deploy/zcblock-csi/test-local-regions-concurrent-c-receive.sh
```

Deploy a smoke-test PVC and pod:

```sh
kubectl apply -f zccusan/deploy/zcblock-csi/example.yaml
kubectl -n zcblock-demo get pods,pvc
kubectl get pv
kubectl delete -f zccusan/deploy/zcblock-csi/example.yaml
```

The file-backed loop smoke test uses the `zcfile` StorageClass:

```sh
kubectl apply -f zccusan/deploy/zcblock-csi/example-file-loop.yaml
kubectl -n zcblock-demo logs zcfile-smoke
kubectl delete -f zccusan/deploy/zcblock-csi/example-file-loop.yaml
```

The raw block example uses `volumeMode: Block` and the `zcraw` StorageClass.
It only exposes the allowlisted raw partition as a block device; the CSI driver
does not format, wipe, or delete it:

```sh
kubectl apply -f zccusan/deploy/zcblock-csi/example-raw-block.yaml
kubectl -n zcblock-demo logs zcraw-smoke
kubectl delete -f zccusan/deploy/zcblock-csi/example-raw-block.yaml
```

The snapshot example creates a file-backed PVC, snapshots it, and restores a
new file-backed PVC from the snapshot:

```sh
kubectl apply -f zccusan/deploy/zcblock-csi/example-snapshot-source.yaml
kubectl -n zcblock-demo wait --for=condition=Ready pod/zcfile-snap-writer --timeout=180s
kubectl apply -f zccusan/deploy/zcblock-csi/example-snapshot.yaml
kubectl -n zcblock-demo wait --for=jsonpath='{.status.readyToUse}'=true volumesnapshot/zcfile-snap --timeout=300s
kubectl apply -f zccusan/deploy/zcblock-csi/example-snapshot-restore.yaml
kubectl -n zcblock-demo wait --for=condition=Ready pod/zcfile-snap-reader --timeout=180s
kubectl -n zcblock-demo logs zcfile-snap-reader
kubectl delete -f zccusan/deploy/zcblock-csi/example-snapshot-restore.yaml
kubectl delete -f zccusan/deploy/zcblock-csi/example-snapshot.yaml
kubectl delete -f zccusan/deploy/zcblock-csi/example-snapshot-source.yaml
```

For a cohesive snapshot across regions, run a short-lived barrier against each
cluster's `zcblock-csi` node plugin before creating the VolumeSnapshots. The
barrier freezes staged filesystem volumes with `fsfreeze` and always auto-thaws
after the requested TTL, even if the external supervisor stops responding:

```sh
POD="$(kubectl -n zcblock-csi get pod -l app.kubernetes.io/name=zcblock-csi -o jsonpath='{.items[0].metadata.name}')"
kubectl -n zcblock-csi exec "$POD" -c zcblock-csi -- \
  zcblock-freeze --control-url http://127.0.0.1:9788 freeze --barrier global-snap-001 --ttl-ms 750
kubectl -n zcblock-csi exec "$POD" -c zcblock-csi -- \
  zcblock-freeze --control-url http://127.0.0.1:9788 status
kubectl -n zcblock-csi exec "$POD" -c zcblock-csi -- \
  zcblock-freeze --control-url http://127.0.0.1:9788 release --barrier global-snap-001
```

A supervisor should fan out `freeze` to every participating cluster, wait for
all `OK` responses, create the snapshots, then fan out `release`. If any region
does not acknowledge in time, stop the snapshot attempt and let each node's TTL
expire. The default maximum TTL is 5000 ms and the manifest sets it explicitly
with `--freeze-max-ttl-ms=5000`; sub-second values are expected for normal use.

This barrier only quiesces filesystem-mode PVCs that have been staged by kubelet
on that node. Block-mode volumes, including `backend=raw-block`, are not
filesystem-frozen; they need application-level quiescing or a later block-level
protocol.

The `zcbrd` module must already be loaded and configfs must be mounted on the
host. The driver does not load kernel modules.

StorageClass parameters:

- `backend`: `zcbrd`, `file-loop`, or `raw-block`. `mux` is reserved for a
  later gateway/control plane.
- `blocksize`: block size written to configfs. Default: `4096`.
- `queues`: blk-mq queue count. Default: `8`.
- `queueDepth`: queue depth. Default: `512`.
- `descriptorMode`: `advertise` or `disabled`. Default: `advertise`.
- `fileRoot`: optional absolute directory for `backend=file-loop` sparse
  images. Default: `/var/lib/zcblock-csi/files`.
- `rawPartUUID` or `rawDevice`: required for `backend=raw-block`; the matching
  PARTUUID must be listed in `/etc/zcblock-csi/allowed-raw-partitions.txt`.

Snapshots are stored under `/var/lib/zcblock-csi/snapshots`. For `backend=file-loop`,
the driver uses a reflink/COW PIT snapshot when the state directory filesystem
supports Linux `FICLONE`; `--snapshot-mode=reflink` makes that mandatory, while
the default `--snapshot-mode=auto` falls back to a full copy if reflink is not
available. `backend=zcbrd` and `backend=raw-block` still use full byte copies
until the userspace fabric target can serve COW/WAL PIT views directly.
Restoring to `backend=raw-block` writes the snapshot image back to the
allowlisted raw block device during `CreateVolume`.

The zccusan control API separately defines fabric snapshot exports:
`POST /v1/snapshot-devices` registers a read-only `cow` or `wal` PIT view for
the userspace fabric target/gateway. There are no separate COW/WAL snapshot
kernel providers, and it does not fall back to loop devices or writable
restores. Compaction is tracked through `/v1/compactions`: the normal strategy
is `stream-rewrite`, where a worker streams the old placement off-machine and
streams the compacted representation back to the new location. Workers
occasionally register small progress state with `PUT /v1/compactions/{jobId}`;
`strategy=in-place` means a userspace worker compacts the current placement
locally while reporting the same control-plane state.
