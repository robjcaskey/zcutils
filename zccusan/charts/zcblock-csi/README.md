# zcblock-csi Helm Chart

This chart installs `zccsi`, the zccusan CSI adapter: the `zcblock-csi`
DaemonSet, the `zcblock-control` local zccusan agent sidecar, minimum RBAC, the
CSIDriver object, optional StorageClasses, and an optional VolumeSnapshotClass.

`zccusan` means Zero Copy Cinematic Universe Storage Area Network. CSI is only a
Kubernetes-facing client of that storage network. The durable control idiom is
the zccusan OpenAPI protocol and local agent, not the CSI process itself. The
layering is descriptor primitives -> zero-copy streams -> zero-copy WALs ->
`zcvolume` -> `zcsan` -> convenience `zccsi`.

## Chart Rules

- No Helm hooks. Do not add `helm.sh/hook` annotations or hook-only Jobs.
- All install, upgrade, and uninstall behavior must be represented as normal
  Kubernetes resources reconciled by the API server.
- CRD installation stays outside this chart. Use
  `zccusan/deploy/zcblock-csi/install-snapshot-api.sh` for the snapshot CRDs and
  snapshot controller stair-step flow.

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

```sh
helm repo add zcutils https://robjcaskey.github.io/zcutils
helm repo update
helm install zcblock-csi zcutils/zcblock-csi \
  --version 0.1.0-nightly.20260819.1 \
  --namespace zcblock-csi \
  --create-namespace
```

The chart defaults to `docker.io/robjcaskey/zcblock-csi:nightly`. Pin the image
and chart to dated versions for a reproducible deployment.

For a checkout-local render instead:

```sh
helm lint zccusan/charts/zcblock-csi
helm template zcblock-csi zccusan/charts/zcblock-csi --namespace zcblock-csi \
  | kubectl apply -f -
```

The chart renders the Namespace by default so the same values work with
`helm template ... | kubectl apply -f -`.

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
  --set snapshotClass.name=zcblock-a \
  | kubectl apply -f -

kubectl -n zcblock-csi-a rollout status daemonset/zcblock-csi-a-node
```

Repeat for `b` and `c`, changing the release name, namespace, driver name,
state directory, StorageClass names, and snapshot class name. Snapshot CRDs and
the external snapshot controller are still upgraded separately before the chart
step.

Enable the raw block StorageClass only with a real allowlisted PARTUUID:

```sh
helm template zcblock-csi zccusan/charts/zcblock-csi \
  --namespace zcblock-csi \
  --set storageClasses.zcraw.enabled=true \
  --set storageClasses.zcraw.parameters.rawPartUUID=6dfb2c34-e1a4-4cd5-a4f6-d82bfadcd363 \
  | kubectl apply -f -
```

For a cross-node fabric volume, prepare `/dev/zcnblk0` on every eligible client
node and connect each edge through the separate userspace onramp to the same
logical volume. Then enable the topology-free fabric StorageClass:

```sh
helm upgrade --install zcblock-csi zccusan/charts/zcblock-csi \
  --namespace zcblock-csi --create-namespace \
  --set storageClasses.zcfabric.enabled=true
```

`backend=fabric` does not make placement, mirror, stripe, spill, or tier
decisions. It only stages the node's `/dev/zcnblk0` client edge. The downstream
userspace stage owns those decisions. Unlike `zcfile`, the resulting CSI volume
does not advertise node-local accessible topology, so a workload can move to a
different prepared client node without moving the remote leaf.

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
