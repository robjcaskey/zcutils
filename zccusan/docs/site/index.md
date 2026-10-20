# Diskpipe

Storage services for volumes,
snapshots, replication, tenant policy and regional failover, with Kubernetes as
one deployment and integration choice.

The implementation currently lives under `zccusan/` in the zcutils repository.
Binary names, API paths and installation commands retain their existing names.

## Before you start

Read the [cost-control guide](../COST_CONTROL.md) to agree on a trial budget,
review bandwidth costs, and plan cleanup.

## Choose your deployment

| Documentation | Use it when |
| --- | --- |
| [With Kubernetes](kubernetes.md) | You operate clusters and want CSI volumes, Helm installation and Kubernetes policy resources. |
| [Without Kubernetes](standalone.md) | You need the underlying storage services and direct interfaces without a Kubernetes control plane. |

These choices describe orchestration. Either can run on physical servers,
virtual machines or cloud instances.

## Operate the storage system

[Day-two identity and failover](../DAY_2_FEDERATED_IDENTITY_AND_FAILOVER.md)
covers scoped OAuth, IAM and native Kubernetes identity, supplementary approval,
workload failover and cleanup. Each deployment page points to the relevant
installation and architecture material.

The [architecture reference](../zccusan.md) defines the shared storage layers.
The Kubernetes CSI adapter translates requests; the storage services own
placement, replication and tenancy.

## Related projects

Explore the [Zero Copy Cinematic Universe](../../../docs-site/universe.md) for
project relationships, or visit [zcutils](../../../docs-site/index.md) for the
underlying stream and transport tools.
