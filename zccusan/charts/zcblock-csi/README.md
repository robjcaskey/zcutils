# zcblock-csi Helm Chart

This chart installs `zccsi`, the zccusan CSI adapter: the `zcblock-csi`
DaemonSet, the `zcblock-control` local zccusan agent sidecar, minimum RBAC, the
CSIDriver object, automatic host client-edge setup, and optional
StorageClasses. A `VolumeSnapshotClass` is an application/cluster policy
choice and is deliberately not created by this chart.

`zccusan` means Zero Copy Cinematic Universe Storage Area Network. CSI is only a
Kubernetes-facing client of that storage network. The durable control idiom is
the zccusan OpenAPI protocol and local agent, not the CSI process itself. The
layering is descriptor primitives -> zero-copy streams -> zero-copy WALs ->
`zcvolume` -> `zcsan` -> convenience `zccsi`.

## Chart Rules

- No Helm hooks. Do not add `helm.sh/hook` annotations or hook-only Jobs.
- All install, upgrade, and uninstall behavior must be represented as normal
  Kubernetes resources reconciled by the API server.
- The chart includes the zccusan `StorageProfile`, `MediaGrant`,
  `TieringPolicy`, `CrossRegionReplication`, and `ZcVolume` CRDs and runs one
  reconciler instance. Snapshot API CRDs remain outside this
  chart: use `zccusan/deploy/zcblock-csi/install-snapshot-api.sh` for the
  snapshot controller stair-step flow. If snapshots are wanted, create a
  `VolumeSnapshotClass` separately after that API is available; the repository
  example is `zccusan/deploy/zcblock-csi/snapshot-class.yaml`.

## Container Filesystems

The chart sets `security.readOnlyRootFilesystem=true` by default and applies it
explicitly to the CSI driver, local control agent, provisioner, snapshotter,
and node-driver registrar containers. Required writes go to explicit volumes:
the CSI socket uses `plugin-dir`, while driver state and the durable state log
use `state-dir`. A writable container image filesystem is not required by any
current zcutils component and disabling this setting is not expected to improve
data-plane performance.

For a third-party image that cannot run with a read-only root filesystem, the
global compatibility escape hatch is:

```yaml
security:
  readOnlyRootFilesystem: false
```

Prefer fixing or replacing the incompatible image rather than leaving this
override disabled.

## Telemetry routing and community survey

The CSI driver and control agent use two distinct endpoints:

1. When `telemetry.apiEndpoint` is defined, all raw telemetry goes only to that
   API. Edge processes do not directly participate in the community survey.
2. When no telemetry API is defined, `communitySurvey.enabled=true` permits a
   direct submission to `communitySurvey.apiEndpoint`, but only after Rust has
   transformed the record into `NonIdentifyingTelemetry`.
3. A telemetry collector independently decides whether to export anonymized
   signals to `communitySurvey.apiEndpoint`.

The chart deploys one cluster-local telemetry collector by default. If
`telemetry.apiEndpoint` is empty, node processes automatically use that
collector's Service URL. An explicit URL overrides the generated URL. Disable
`telemetryServer.enabled` to omit the Deployment and Service and use direct
survey fallback instead.

To use a separately managed collector, configure:

```yaml
telemetry:
  apiEndpoint: http://zccusan-telemetry:9899/v1/events
communitySurvey:
  enabled: true
  apiEndpoint: https://vdq4ma9dl2.execute-api.us-east-1.amazonaws.com/survey
```

For the deployed community endpoints, use the reviewed
`values-community-survey-dev.yaml` or `values-community-survey-prod.yaml`
profile, or set the community API endpoint explicitly.

The telemetry server logs accepted events to stdout as one-line NDJSON and
can transform and export them to the configured community API. Before export,
the collector hashes installation identity and removes sensitive and unknown
fields; raw telemetry never leaves the installation for the community survey.
It rejects individual inputs
over 4 KiB and retries unacknowledged events from a bounded 4 MiB indexed memory
ring. Ring eviction emits a `telemetry_buffer_overflow` event with the exact
unacknowledged evicted index range and records that its NDJSON copies are
already available in stdout before the new event is appended. Only that server needs
public HTTPS egress when the local telemetry API is configured. Edge publishers
never perform network I/O in their calling path and bound explicit telemetry
shutdown waiting to 1.5 seconds.

## Install

For a disposable, volatile-RAM smoke test, start with the
[narrated Kubernetes happy path](../../docs/GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md).
The [explained installation](../../docs/GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES_DETAILED.md)
covers unsupported kernels, real media, RDMA, topology checks, and cleanup.

```sh
helm repo add zcutils https://robjcaskey.github.io/zcutils
helm repo update
helm install zcblock-csi zcutils/zcblock-csi \
  --version 0.1.6 \
  --namespace zcblock-csi \
  --create-namespace
```

Each published chart defaults to the matching version of
`docker.io/robjcaskey/zcblock-csi`.

The one multi-architecture `zcblock-csi` image is also the node-setup image.
Each architecture manifest contains the Rust loader and only that
architecture's audited exact-ABI kernel modules. An air-gapped mirror therefore
copies one first-party image (plus the standard Kubernetes CSI sidecars), not a
second zccusan kernel-module repository:

```sh
skopeo copy --all \
  docker://docker.io/robjcaskey/zcblock-csi:0.1.6 \
  docker://registry.internal.example/zcblock-csi:0.1.6
```

## Automatic node setup

`nodeSetup.enabled=true` makes the CSI node DaemonSet load the client-edge
module before CSI registration. Module acquisition is explicit and
fail-closed: production modes never fall back to compiling on a worker.

1. Reuse a loaded module only when both `/dev/zcnblk0` and
   `/dev/zcnblk-shmctl` are present.
2. Acquire the module from the selected `host`, `image`, or `http` source.
3. Verify its configured SHA-256, module name, and running-kernel `vermagic`.
4. Load it with `transport=shm` and wait for both client-edge devices
   before the CSI containers can start.

The node must permit privileged Pods and out-of-tree modules. Secure Boot hosts
need an artifact signed by a key trusted by their kernel. A module artifact is
specific to its CPU architecture and compatible kernel build; `%ARCH%` and
`%KERNEL_RELEASE%` in paths and URLs expand on each node.

The default source is `image`. The init container reuses the exact main image
reference and selects
`/opt/zcutils/kmods/%ARCH%/%KERNEL_RELEASE%/zcnblk_client_mod.ko`; an unsupported
kernel fails before CSI starts. See the
[current exact-ABI matrix](../../../docs/kernel-module-build-matrix.md).

Fleets that bake or provision a custom module on each host can select `host`:

```yaml
nodeSetup:
  moduleSource:
    type: host
    hostPathTemplate: /opt/zcutils/kmods/%ARCH%/%KERNEL_RELEASE%/zcnblk_client_mod.ko
    sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
```

For a custom kernel, build the same full DaemonSet image with
`zccusan/deploy/zcblock-csi/build-kmod-image.sh` and select it by immutable OCI
digest. The init container and long-running containers deliberately use one
image reference:

```sh
MODULE_FILE=./zcnblk_client_mod.ko \
KERNEL_RELEASE=6.18.0-101.11.1.el10_1.x86_64 \
MODULE_ARCH=x86_64 \
IMAGE=registry.example.com/storage/zcblock-csi:6.18.0-x86_64 \
  zccusan/deploy/zcblock-csi/build-kmod-image.sh
```

```yaml
image:
  repository: registry.example.com/storage/zcblock-csi
  digest: sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
nodeSetup:
  moduleSource:
    type: image
    imagePathTemplate: /opt/zcutils/kmods/%ARCH%/%KERNEL_RELEASE%/zcnblk_client_mod.ko
    sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
```

HTTP(S) is solely for a user-provided static server; the chart contains no
default artifact URL. A digest may be pinned in values, or the server may
publish a `sha256sum`-style checksum file:

```yaml
nodeSetup:
  moduleSource:
    type: http
    http:
      urlTemplate: https://modules.storage.example/%ARCH%/%KERNEL_RELEASE%/zcnblk_client_mod.ko
      checksumUrlTemplate: https://modules.storage.example/%ARCH%/%KERNEL_RELEASE%/zcnblk_client_mod.ko.sha256
      delivery: nodeCacheDaemonSet
```

`delivery: nodeCacheDaemonSet` creates a small, operator-independent companion
DaemonSet with ordinary pod networking and no Kubernetes API token. It can
reach a static ClusterIP Service even when the privileged CSI Pod uses host
networking for RDMA. It atomically writes the verified artifact and digest to
a node-local cache; the CSI init container only reads that cache. The static
server must not itself require a zccusan PVC, which would create a bootstrap
cycle. Use `delivery: direct` only when the user-provided origin is reachable
from the CSI Pod's network namespace. Plain HTTP is rejected unless
`allowInsecureHttp: true`; HTTPS remains recommended even with content hashes.

On-node compilation remains available only as an explicit development/debug
mode. It requires matching host headers and tools, or explicit permission to
install them:

```yaml
nodeSetup:
  moduleSource:
    type: build
  developmentBuild:
    enabled: true
    installHostDependencies: false
```

This mode carries the chart's source ConfigMap. Production modes do not mount
source or invoke a compiler. Node setup and HTTP cache refresh are compiled Rust
programs; the normal DaemonSet image remains shell-free. Set
`nodeSetup.developmentBuild.sourceConfigMap` only to supply reviewed replacement
source with `zcnblk_client_mod.c`, `zcnblk_shm_abi.h`, `Makefile`, and `Kbuild`
keys. `Kbuild` is required because current kernels may generate an output
`Makefile` while preparing an external-module build.

### TCP and RDMA nodes

TCP is the default userspace backplane and requires no RDMA device:

```sh
helm upgrade --install zccusan ./zccusan/charts/zcblock-csi \
  --namespace zccusan --create-namespace
```

To require RDMA-capable nodes, opt in explicitly:

```sh
helm upgrade --install zccusan ./zccusan/charts/zcblock-csi \
  --namespace zccusan --create-namespace \
  --set backplane.rdma.enabled=true \
  --set backplane.rdma.provider=efa
```

Valid providers are `efa`, `efa-direct`, and `verbs`; set
`backplane.rdma.domain` when a particular libfabric domain is required. The
RDMA init container checks `/dev/infiniband`, an active sysfs port, and the
selected libfabric provider. It fails the Pod instead of silently changing the
requested transport to TCP. RDMA mode also gives the node Pod host networking
so libfabric sees the RDMA interface in the correct network namespace. The
runtime image contains libfabric 2.1 with EFA
and verbs support on both supported CPU architectures.

This selection does not change `zcnblk_client_mod`: its only permitted Helm
configuration is `transport=shm`. TCP/RDMA selection, lane ownership, mirror
placement, and backpressure remain in the separate userspace stage after the
block edge. The declarative two-copy mirror reconciler accepts either `TcpMux`
or `OfiRdm`. The RDMA gate qualifies the node image; the profile still has to
request RDMA explicitly because the operator never silently rewrites a TCP
volume. `OfiRdm` requires the extended resource exported by the cluster's RDMA
device plugin, and the operator checks that the client and both selected
storage nodes advertise it before creating data-plane Pods:

```yaml
transport:
  kind: OfiRdm
  ofiProvider: efa
  ofiEndpoint: rdm
  deviceResourceName: vpc.amazonaws.com/efa
  lanes: 8
  connectionsPerLane: 1
  requireOneSidedRma: false
```

The local `/dev/zcnblk0` edge still enters the separate userspace mirror over a
node-local TCP session. Only the mirror-to-storage-copy backplane uses OFI/RDM. WAL
writes, reads, sync/FUA results, and the mirrored HWM remain part of the same
bidirectional libfabric protocol. The optional one-sided RMA payload window is
not synonymous with RDMA transport: this release rejects
`requireOneSidedRma=true` so an initiator cannot write through one copy's
registered window and bypass the other mirror leg. See
`zccusan/deploy/zcblock-csi/getting-started/mirror-rdma.template.yaml`.

The defaults are a functional one-lane edge. Set module arguments in values:

```yaml
nodeSetup:
  module:
    parameters:
      - transport=shm
      - lanes=8
      - connections_per_lane=1
      - shard_count=1
      - size_mib=4096
      - queues=8
      - queue_depth=256
      - pipeline_depth=128
      - pin_threads=0
```

`shard_count=1` is mandatory: the block client is only the edge. Userspace
stages after `/dev/zcnblk0` continue to own mirroring, striping, placement,
tiering, locality, lane selection, and backpressure.

For fleet-owned configuration, create a ConfigMap containing one `name=value`
token per line and point Helm at it:

```sh
kubectl -n zcblock-csi create configmap zcnblk-node-config \
  --from-literal=module-parameters=$'transport=shm\nlanes=8\nconnections_per_lane=1\nshard_count=1\nsize_mib=4096\nqueues=8\nqueue_depth=256\npipeline_depth=128\npin_threads=0'

helm upgrade --install zcblock-csi zcutils/zcblock-csi \
  --namespace zcblock-csi --create-namespace \
  --set nodeSetup.existingConfigMap=zcnblk-node-config
```

Disable `nodeSetup` only when the node image or a separate lifecycle manager
already guarantees the two devices. Helm uninstall deliberately does not
unload an in-use module or destroy a live block edge.

For a checkout-local render instead:

```sh
helm lint zccusan/charts/zcblock-csi
helm template zcblock-csi zccusan/charts/zcblock-csi --namespace zcblock-csi \
  | kubectl apply -f -
```

`namespace.create` defaults to `false`; `helm install --create-namespace`
creates it in the normal Helm path. For `helm template ... | kubectl apply`,
create the namespace first or set `namespace.create=true`.

## Template/Apply Upgrade

The supported stair-step path does not require Helm release state. Render the
chart and apply it, then wait for the DaemonSet rollout before moving to the
next region or cluster:

```sh
helm template zcblock-csi zccusan/charts/zcblock-csi \
  --namespace zcblock-csi \
  | kubectl apply -f -

kubectl -n zcblock-csi rollout status daemonset/zcblock-csi-node
```

For three local regions, apply one release at a time with explicit driver and
state values:

```sh
helm template zcblock-csi-a zccusan/charts/zcblock-csi \
  --namespace zcblock-csi-a \
  --set namespace.name=zcblock-csi-a \
  --set driverName=io.zcutils.zcblock.a \
  --set stateDir=/var/lib/zcblock-csi-a \
  --set storageClasses.zcbrd.name=zcbrd-a \
  --set storageClasses.zcfile.name=zcfile-a \
  | kubectl apply -f -

kubectl -n zcblock-csi-a rollout status daemonset/zcblock-csi-a-node
```

Repeat for `b` and `c`, changing the release name, namespace, driver name,
state directory, and StorageClass names. Snapshot CRDs, the external snapshot
controller, and any `VolumeSnapshotClass` are still managed separately.

Enable the raw block StorageClass only with a real allowlisted PARTUUID:

```sh
helm template zcblock-csi zccusan/charts/zcblock-csi \
  --namespace zcblock-csi \
  --set storageClasses.zcraw.enabled=true \
  --set storageClasses.zcraw.parameters.rawPartUUID=6dfb2c34-e1a4-4cd5-a4f6-d82bfadcd363 \
  | kubectl apply -f -
```

For a cross-node fabric volume, the Helm-managed node init prepares
`/dev/zcnblk0` on every selected client node. The operator connects each edge
through a separate userspace onramp to the declared logical volume. Enable the
fabric StorageClass with:

```sh
helm upgrade --install zcblock-csi zccusan/charts/zcblock-csi \
  --namespace zcblock-csi --create-namespace \
  --set storageClasses.zcfabric.enabled=true
```

`backend=fabric` does not make placement, mirror, stripe, spill, or tier
decisions. It only stages the node's `/dev/zcnblk0` client edge. The downstream
userspace stage owns those decisions. A manually managed fabric can be
topology-free only when the administrator has connected every eligible node
edge to the same logical volume. A fabric StorageClass generated from a
`StorageProfile` advertises the operator-reconciled client node until fenced
client handoff is implemented; its terminal storage copies are still remote and may be
placed on other nodes.

### Direct userspace filesystem volumes (no local block edge)

Kubernetes requires a block-special device only for `volumeMode: Block`. For an
ordinary filesystem PVC, zcblock CSI can instead publish a FUSE filesystem that
is owned by a separate userspace volume service. Enable the topology-free class:

```sh
helm upgrade --install zcblock-csi zccusan/charts/zcblock-csi \
  --namespace zcblock-csi --create-namespace \
  --set storageClasses.zcuserspace.enabled=true
```

For volume `<volume-id>`, the userspace service must create a FUSE mount at
`<userspaceMountRoot>/<volume-id>` on every eligible client node. The chart
shares that host directory with bidirectional mount propagation. During
`NodeStageVolume`, CSI verifies that the source is an actual `fuse`/`fuse.*`
mount and bind-mounts it into kubelet's staging tree. It never formats a device,
creates a loop device, or makes userspace placement decisions.

This backend intentionally rejects `volumeMode: Block`. It is the CSI adapter
for a userspace filesystem service, not that filesystem implementation itself;
the current zcutils WAL protocol is byte/block oriented and cannot truthfully be
presented as POSIX storage without a filesystem layer. Local snapshot copying
and byte-stream replication are likewise rejected; those operations must use
the userspace volume control API.

## Durable State Log

The `zcblock-control` sidecar writes the node-local zccusan state log to
`stateDir/logstream/zccusan-state.log` by default. Override it only when the
host path layout requires a separate location:

```sh
helm template zcblock-csi zccusan/charts/zcblock-csi \
  --namespace zcblock-csi \
  --set logstream.path=/var/lib/zcblock-csi/logstream/zccusan-state.log \
  | kubectl apply -f -
```

The chart still renders normal Kubernetes resources only; no Helm hooks are
used for log initialization or replay.

## Metrics

`zcblock-control` serves Prometheus-compatible metrics from `/metrics`. The
default control listener is `127.0.0.1:9788`, which is only reachable inside the
pod network namespace. To let an external Prometheus scrape the DaemonSet pod,
bind the control sidecar to the pod IP and enable scrape annotations:

```sh
helm template zcblock-csi zccusan/charts/zcblock-csi \
  --namespace zcblock-csi \
  --set control.listen=0.0.0.0:9788 \
  --set metrics.enabled=true \
  | kubectl apply -f -
```

The master day-1 metric is the label-free
`zccusan_replication_attention_required` signal. The metrics also include
`zccusan_replication_job_bytes`,
`zccusan_replication_job_bytes_remaining`,
`zccusan_replication_job_elapsed_seconds`, and
`zccusan_replication_job_idle_seconds`.

For day-1 monitoring, start with one alert:

```promql
zccusan_replication_attention_required == 1
```

If you need the minimum split, watch
`zccusan_replication_attention_failed_jobs` and
`zccusan_replication_attention_idle_jobs`. When the master signal fires, fetch
`/v1/stats` from the same control endpoint and read
`summary.attention_required`, `summary.attention_failed_jobs`, and
`summary.attention_idle_jobs` before drilling into `hierarchy.placement` or
`hierarchy.logical`.
