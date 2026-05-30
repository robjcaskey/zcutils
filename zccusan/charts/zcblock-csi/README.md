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

## Install

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
