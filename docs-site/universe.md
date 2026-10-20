# Zero Copy Cinematic Universe

Related projects share a zero-copy foundation. Each has its own documentation
and responsibilities, even while their source lives in one repository.

## Choose a project

| Project | Responsibility | Start here |
| --- | --- | --- |
| zcutils | Descriptor and stream tools, TCP transport, encryption and low-level experiments. | [zcutils documentation](index.md) |
| zccusan | Storage area network, volumes, snapshots, replication, tenancy and failover. | [zccusan documentation](../zccusan/docs/site/index.md) |

## How the layers fit

```text
descriptor primitives -> streams -> WALs -> volumes -> SAN
                                                         -> Kubernetes CSI adapter
                                                         -> clients without Kubernetes
```

The CSI adapter translates Kubernetes requests. Storage policy and data movement
belong to the underlying userspace storage services. The
[zccusan architecture](../zccusan/docs/zccusan.md) describes the full layering.

## Find your deployment path

[zccusan with Kubernetes](../zccusan/docs/site/kubernetes.md) covers CSI, Helm,
cluster policy and workload storage. [zccusan without Kubernetes](../zccusan/docs/site/standalone.md)
covers the underlying services, direct interfaces and current standalone limits.
Both paths may use physical servers, virtual machines or cloud instances.
