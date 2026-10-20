# Diskpipe with Kubernetes

Use Diskpipe through its CSI adapter, Helm chart and Kubernetes policy resources.
This path works on physical or virtual cluster nodes, including cloud clusters.

Read [cost control for evaluations](../COST_CONTROL.md) before provisioning or
moving evaluation data.

## Install and validate

- [Getting started](../GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md): the narrated volatile-memory happy path.
- [Detailed installation](../GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES_DETAILED.md): configuration, validation and troubleshooting.
- [Custom kernel build](../BUILD_KERNEL_MODULE.md): build the client edge for a kernel outside the supplied matrix.
- [Single-region HA checks](../VALIDATING_SINGLE_REGION_HA_ON_KUBERNETES.md): graded fault checks with the optional chaos toolbox.

The getting-started memory example is volatile. Follow the guide's durability
choices before using storage for data that must survive restart.

## Replication and failover

- [Cross-region replication](../GETTING_STARTED_WITH_CROSS_REGION_REPLICATION_ON_KUBERNETES.md).
- [Cross-region policy](../GETTING_STARTED_WITH_CROSS_REGION_POLICY_ON_KUBERNETES.md).
- [Cross-region failover](../GETTING_STARTED_WITH_CROSS_REGION_FAILOVER_ON_KUBERNETES.md).
- [Tiering](../GETTING_STARTED_WITH_TIERING_ON_KUBERNETES.md).
- [Untrusted storage](../GETTING_STARTED_WITH_UNTRUSTED_FEDERATION_STORAGE_ON_KUBERNETES.md) and [untrusted hosting](../GETTING_STARTED_WITH_UNTRUSTED_FEDERATION_HOSTING_ON_KUBERNETES.md).

## Day-two operations

[Identity and failover operations](../DAY_2_FEDERATED_IDENTITY_AND_FAILOVER.md)
explains native Kubernetes identity alongside OAuth and IAM, tenant-scoped
permissions, approval and teardown checks. Kubernetes credentials authenticate
a principal; storage authorization still follows explicit role bindings.

See the [Helm chart reference](../../charts/zcblock-csi/README.md) and
[raw deployment reference](../../deploy/zcblock-csi/README.md) for packaging.

## Other deployment choices

Start at [Diskpipe](index.md) or explore [Diskpipe without Kubernetes](standalone.md).
