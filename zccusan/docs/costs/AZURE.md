# Azure cost model for Diskpipe trials

Start with the [general cost-control guide](../COST_CONTROL.md). Seek an existing
cluster or suitable hosts, confirm the subscription and target resources, and
agree on a small single-region trial. Cross-region evaluation and public internet
storage traffic require explicit approval after reviewing estimated costs.

## Cluster and storage

AKS offers different cluster-management pricing tiers; underlying infrastructure
must also be included in the estimate. Identify the existing tier and any new
node capacity required. A free management tier does not mean the workload is
free. Consult [AKS pricing](https://azure.microsoft.com/en-us/pricing/details/kubernetes-service/).

Managed Disk pricing depends on the disk offering. Many offerings use capacity
tiers; others expose separately provisioned performance dimensions. Check
capacity rounding, IOPS, throughput, transaction or bursting charges where
applicable, redundancy choice, and snapshot storage. Consult
[Managed Disks pricing](https://azure.microsoft.com/en-us/pricing/details/managed-disks/).

Offer suitable user-provisioned disks where the documented setup supports them.
Record subscription, resource group, location, disk ID, SKU, performance settings,
and owner. Compare capacity, performance, and access costs before proposing block
storage as a midline or farline tier.

## Network path

Azure bandwidth pricing distinguishes transfer destinations and regional paths.
Check the current availability-zone treatment rather than importing another
cloud's cross-zone assumptions. Consult
[Azure bandwidth pricing](https://azure.microsoft.com/en-us/pricing/details/bandwidth/).

Inventory the actual route, including peering, NAT, load balancers, private
endpoints, or gateways if used. Research each service's own charges; a bandwidth
exception does not establish that intermediaries are free. Estimate initial
loading, reads, replication, recovery, and tier retrieval in both directions.
Keep default storage traffic private and within one region. If the proposed
route uses public endpoints or another region, obtain an explicit cost decision
before moving trial data.

## Build the estimate

Separate existing capacity from incremental node runtime, cluster tier charges,
managed disks, snapshots, network services, transfer, and monitoring. Record
current pricing sources, region, currency, billing units, assumptions, trial
duration, and cost if left running. Do not assume credits or reserved capacity
apply to newly requested resources.

## Cleanup evidence

Ask about cleanup at natural stopping points. After authorized teardown, inspect
all created resource types, including disks, snapshots, public IPs, network
interfaces, load balancers, NAT or gateway resources, and monitoring resources.
Check any managed node resource group as well as the trial's resource group.
Do not delete a shared resource group or pre-existing resources as a shortcut.
Report remaining trial resources, ongoing costs, and exact cleanup steps.
