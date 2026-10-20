# Diskpipe

Diskpipe is the storage project formerly presented as zccusan. Source paths,
binary names and deployment commands retain their current names during the transition.

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

zccusan has its own [documentation home](docs/site/index.md).
Choose [with Kubernetes](docs/site/kubernetes.md) or
[without Kubernetes](docs/site/standalone.md). Either can run on physical
servers, virtual machines or cloud instances.

The [Zero Copy Cinematic Universe](../docs-site/universe.md) connects this
project with the underlying zcutils tools.

## Reference library

- `docs/GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md`: narrated,
  volatile-RAM Kubernetes happy path.
- `docs/GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES_DETAILED.md`: the complete
  second-step install, configuration, validation, and troubleshooting guide.
- `docs/VALIDATING_SINGLE_REGION_HA_ON_KUBERNETES.md`: approachable, graded
  single-region fault check using the separately installed chaos toolbox.
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
- `charts/zccusan-chaos-toolbox/` contains the separate, opt-in fault toolbox.
- `docs/` contains zccusan architecture and replication/shaping docs.
