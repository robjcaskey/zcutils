# Diskpipe without Kubernetes

Use the storage services and their direct interfaces without a Kubernetes
control plane or CSI adapter. This describes an orchestration choice, whether
hosts are physical servers, virtual machines or cloud instances.

## Start with the service architecture

The [architecture reference](../zccusan.md) describes the local agent,
`zcblock-control`, its durable state log, replication policy endpoints, snapshot
exports and monitoring. The [replication and shaping reference](../streaming-replication-shaping.md)
explains stream budgets, snapshots and catch-up.

Storage placement, mirrors, stripes, tiers and spill remain userspace service
responsibilities. The Linux block client is an application-facing edge.
Kubernetes is an optional adapter above the same storage layers.

## Identity and operations

Use the [day-two identity and failover guide](../DAY_2_FEDERATED_IDENTITY_AND_FAILOVER.md)
for OAuth and IAM integration, supplementary approval and scoped failover.
Its native Kubernetes TokenReview section applies when a Kubernetes issuer is
part of your environment; it is not a requirement for every client.

The [federation debug laboratory](../../../docs/federated-identity-breakglass-debug.md)
provides local exercises for independent issuers, tenant boundaries and approval.
These exercises verify authorization behavior; they are not a complete
standalone production installer.

## Current standalone boundaries

There is no complete production installation walkthrough for a general Linux
service deployment in these docs yet. Use the architecture and interface
references to assess the implemented components and their limits.

The [Mac federated-region design](../GETTING_STARTED_WITH_A_MACOS_FEDERATED_REGION.md)
is explicitly a target-state preview. Its peer daemon, durability probe and
coalescing workflow are not available end to end in the current release.

## Other deployment choices

Start at [Diskpipe](index.md) or use [Diskpipe with Kubernetes](kubernetes.md) for
the documented CSI installation and cluster validation path.
