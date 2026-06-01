# zccusan

`zccusan` means Zero Copy Cinematic Universe Storage Area Network.

It is the umbrella idiom for this repo's storage stack. The stack has a strict
chain of being:

1. descriptor zero-copy primitives;
2. zero-copy streams;
3. zero-copy WALs, including WALs of WALs;
4. `zcvolume` abstractions;
5. `zcsan`, the Zero Copy Storage Area Network layer;
6. `zccsi`, the convenience Kubernetes CSI adapter.

The dependency direction only points downward. `zccsi` can translate Kubernetes
CSI RPCs into `zcsan`/zccusan control calls, but CSI must not become the place
where stream, WAL, volume, SAN, topology, tenant, or replication semantics live.
Those belong to zccusan below it.

## Shape

- descriptor primitives: buffers, leases, slices, topology metadata, release
  accounting, and versioned frame contracts.
- zero-copy streams: ordered descriptor carriers, fanout/fanin, stream release,
  encryption framing, and later WebSocket or descriptor-native transport.
- zero-copy WALs: durable extent records, WAL snapshots, WAL catch-up, and
  WALs of WALs for composed replication domains.
- `zcvolume`: a block/image volume abstraction with snapshot and replication
  identity independent of Kubernetes.
- `zcsan`: the storage area network layer that owns placement, streams,
  replication, token buckets, topology, tenants, and gateway coordination.
- `zccsi`: a convenience adapter that maps Kubernetes PV/PVC/VolumeSnapshot
  objects onto zcsan operations.
- `zccusan` local agent: the node-local process that owns state directories,
  device paths, snapshot images, freeze barriers, and stream jobs. Today this is
  implemented by `zcblock-control`.

## Block Boundary

zccusan has one custom fabric-facing block device family: `/dev/zcnblk0`, backed
by the `zcnblk` wire protocol today. It is the block-speaking onramp for fio,
filesystems, databases, and CSI consumers. Target-side storage is a userspace
service. A target may choose ordinary files, allowlisted raw devices, or
optional `/dev/zcbrdN` RAM media as last-hop backing, but custom zc block
devices are not the place where SAN topology lives.

Mirroring, striping, forwarding, tier admission, tier spill, snapshot COW/WAL
resolution, compaction placement, and replica fanin/fanout are userspace
zccusan/gateway policy. Kernel block code should only expose the fabric edge or
act as optional final media behind a userspace target.

## zccsi Rule

The CSI driver is `zccsi`: an adapter. It must stay replaceable by another
client that uses the same zccusan protocol.

That means CSI should:

- validate and translate CSI requests into zccusan operations;
- keep CSI idempotency and Kubernetes object mapping local to the adapter;
- use the zccusan OpenAPI client for snapshots, streams, and later volume
  lifecycle operations;
- leave data movement to zccusan libraries/agents rather than shelling out to
  commands;
- avoid becoming the only place where storage policy, tenancy, topology, or WAL
  replication semantics exist.

## Upgrade Rule

The zccusan protocol is versioned independently from CSI. Kubernetes CRD and CSI
sidecar upgrades can stair-step through Helm-rendered manifests, but the control
protocol itself must support an N-1 rollout path for agents and clients. New
features should be additive in OpenAPI first, then made mandatory only after all
participating agents can serve the previous version.

## Durable State Logstream

`zcblock-control` now stores zccusan state transitions in a durable logstream.
The local implementation is file-backed at
`$stateDir/logstream/zccusan-state.log` unless `--logstream-path` or
`ZCCUSAN_LOGSTREAM_PATH` is set.

Each append is a framed, checksummed, fsynced record. Records carry a
monotonic sequence, byte offset, stream name, event kind, key, JSON payload, and
a `term` field reserved for the later Raft-backed implementation. On startup
the local agent replays snapshot create/delete events back into the materialized
snapshot state files, so the existing file layout remains an index rather than
the only source of truth.

For tests and operators, `GET /v1/logstream` replays the current node-local log.
This is intentionally a zccusan control API, not a CSI API; CSI is just one
client that causes state transitions.

## Live Replication Switching

The local control API has two durable replication policy surfaces:

- `PUT /v1/replication/modes` switches a scope between `async` and `sync`.
- `PUT /v1/replication/routes` switches a scope to a target cluster, gateway
  endpoint, and spillover tier.

Both append to the durable state log before updating live in-memory stream job
metadata. Route changes do not contact the old gateway endpoint, so a volume can
be train-track-switched away from an unresponsive target. Today this is the
control-plane contract; snapshot transfer uses the existing stream replication
path, and WAL catch-up becomes real when the WAL/gateway journal primitives land.

## Snapshot Devices And Compaction

Point-in-time snapshots can be registered as read-only fabric snapshot exports
via `POST /v1/snapshot-devices`. The requested `mode` is explicit: `cow` or
`wal`, but that mode is resolved by the userspace fabric target/gateway. There
are no separate `zccowsnap` or `zcwalsnap` kernel providers, and the controller
does not fall back to loop devices, copied restore volumes, or other short-term
substitutes.

Snapshot compaction is tracked as a workflow, not just a local kernel command:

- `strategy=stream-rewrite` is the default. The compactor streams data off the
  machine, then streams the compacted representation back into the new location.
- `strategy=in-place` means a userspace worker compacts the current placement
  locally and reports the same small registered state.
- `PUT /v1/compactions/{jobId}` lets the worker register the small control-plane
  state we need to remember: phase, outbound/inbound stream ids, byte counters,
  target location, worker id, and a compact checkpoint/watermark.

Bulk data stays on the zccusan stream path. The compaction endpoint is only for
durable job state and operator visibility.

Replication progress is visible through two read paths:

- `GET /v1/stats` returns the general monitoring summary plus hierarchical
  placement and logical-volume trees. Use `summary.attention_required` as the
  master internal signal, then drill into `hierarchy.placement` or
  `hierarchy.logical` only when an operator needs target/tier/volume/replica
  detail. It also includes the current compaction jobs so snapshot COW/WAL debt
  is visible from the same broad endpoint.
- `GET /v1/replication/delay` returns JSON samples with current bytes,
  configured byte limit, remaining bytes, elapsed milliseconds, and idle
  milliseconds since the last observed byte progress.
- `GET /metrics` exposes the master `zccusan_replication_attention_required`
  signal plus the same job progress as Prometheus-compatible text gauges under
  `zccusan_replication_job_*`. Job metrics keep the raw `subject` label and
  also derive explicit `stream_kind`, `volume_id`, and `snapshot_id` labels so
  volume and snapshot traffic can be queried separately.

## Day-1 Monitoring

Start with one page-level question: is there anything I need to look at?

The controller computes that answer internally. For Prometheus, alert on the
single label-free master signal:

```promql
zccusan_replication_attention_required == 1
```

If a deployment wants the minimum split without volume-level cardinality, watch
`zccusan_replication_attention_failed_jobs` and
`zccusan_replication_attention_idle_jobs`. The idle side uses the controller's
published `zccusan_replication_attention_idle_threshold_seconds` value. Today
that threshold is 30 seconds.

When the alert fires, fetch the single programmatic drill-down endpoint:

```sh
curl -fsS http://127.0.0.1:9788/v1/stats
```

Read `summary` first. `summary.attention_required` is the same master signal as
the metric. `summary.attention_failed_jobs` and `summary.attention_idle_jobs`
explain the broad reason. Only then drill into `hierarchy.placement` to find the
target cluster, spillover tier, or gateway endpoint, or into
`hierarchy.logical` to see which logical volume or snapshot replica is affected.

## Helm Rule

The Helm chart is only a rendering mechanism for ordinary Kubernetes resources.
There are no Helm hooks. Stair-step upgrades should work with:

```sh
helm template RELEASE zccusan/charts/zcblock-csi --namespace NAMESPACE | kubectl apply -f -
kubectl -n NAMESPACE rollout status daemonset/RELEASE-node
```
