# Google Cloud cost model for Diskpipe trials

Start with the [general cost-control guide](../COST_CONTROL.md). Prefer existing
capacity and a single-region trial over a new cluster. Cross-region evaluation
and public internet storage traffic are excluded until the user explicitly
approves a costed plan.

## Cluster and storage

GKE pricing distinguishes cluster management from workload resources. Standard
and Autopilot use different resource billing models; identify the existing
cluster mode before estimating the incremental trial cost. Check eligibility
before assuming credits apply. Use [GKE pricing](https://cloud.google.com/kubernetes-engine/pricing).

Persistent Disk and Hyperdisk have different capacity and performance pricing.
Check the disk type, regional versus zonal configuration, provisioned capacity,
and any independently charged performance. Include snapshots and images when
used. Read [disk and image pricing](https://cloud.google.com/compute/disks-image-pricing).

Offer suitable user-provisioned disks only where the documented configuration
supports them. Record their project, zone or region, IDs, performance settings,
and ownership. Compare total cost and access patterns before using block storage
as a midline or farline tier.

## Network path

Google Cloud distinguishes same-zone, cross-zone, cross-region, and internet
transfer. The address type, VPC relationship, destination, and network service
can affect the applicable pricing. Do not infer a free path simply because both
endpoints are in Google Cloud. Consult [VPC network pricing](https://cloud.google.com/vpc/network-pricing).

For each proposed path, record source and destination locations, addresses,
expected bytes, and any NAT, load-balancing, or connectivity services. Research
their service-specific processing and runtime charges in addition to transfer.
Keep the default storage workload on a verified private path; present alternatives
if that requires additional paid infrastructure.

## Build the estimate

Separate existing committed capacity from new charges. Include incremental
workload resources, disk capacity and performance, snapshots, network bytes,
intermediaries, and logs. Estimate initial loading, replication, reads, recovery,
and tier retrieval. Record current price sources, assumptions, trial duration,
and the ongoing cost if resources remain.

## Cleanup evidence

Ask whether the user is ready to clean up at each natural stopping point. After
approved teardown, inspect created resource types in each project and location
used. Check retained disks, snapshots, images, addresses, forwarding rules,
load-balancing resources, routers or NAT configuration, and logs as applicable.
Preserve shared resources and pre-existing infrastructure. Report leftovers and
exact cleanup steps; successful Kubernetes or Terraform teardown is only part
of the evidence.
