# zccusan

`zccusan` means Zero Copy Cinematic Universe Storage Area Network.

Canonical chain:

```text
descriptor zero-copy primitives
-> zero-copy streams
-> zero-copy WALs, including WALs of WALs
-> zcvolume
-> zcsan
-> convenience zccsi
```

This directory contains the Kubernetes/SAN-facing material without changing the
Rust binary names, Cargo targets, container entrypoints, or in-cluster paths.

## Documentation

- `docs/zccusan.md`: canonical zccusan layering and the rule that `zccsi` is a
  convenience adapter above `zcsan`, not the storage authority.
- `docs/streaming-replication-shaping.md`: replication stream control,
  token-bucket shaping, snapshot transfer, WAL catch-up, and gateway direction.
- `deploy/zcblock-csi/README.md`: raw Kubernetes manifests, local multi-region
  test installs, snapshot API requirements, and operational examples.
- `charts/zcblock-csi/README.md`: no-hooks Helm chart, including the
  `helm template ... | kubectl apply -f -` stair-step upgrade idiom.
- `deploy/zcblock-csi/CRD-UPGRADES.md`: snapshot CRD and snapshot-controller
  versioning rules.

## Layout

- `deploy/zcblock-csi/` contains raw Kubernetes manifests and install/test
  scripts.
- `charts/zcblock-csi/` contains the no-hooks Helm chart.
- `docs/` contains zccusan architecture and replication/shaping docs.
