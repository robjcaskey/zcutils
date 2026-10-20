# Cost control for Diskpipe evaluations

Use this guide before installing, expanding, benchmarking, or tearing down a
Diskpipe evaluation. It applies both to people following the documentation and to AI
agents working on their behalf. A request to try Diskpipe does not by itself
approve new infrastructure spending.

## Provider cost models

Use the relevant provider page alongside this guide. These pages explain what
to price and link to official sources; they are not fixed-price quotes.

- [AWS](costs/AWS.md): EBS capacity and performance, transfer paths, and VPC costs.
- [Google Cloud](costs/GCP.md): GKE billing modes, disk choices, and network paths.
- [Azure](costs/AZURE.md): AKS tiers, Managed Disks, and bandwidth rules.

## Start with what already exists

Look for existing Kubernetes contexts or suitable hosts first. Inspect their
capacity, permissions, storage classes, region, and availability zones without
changing resources. Propose a target and confirm it with the user, along with a
dedicated evaluation namespace or equivalent resource boundary.

For a cloud evaluation, recommend one region and a small disposable workload. Do not
create a cluster by default. Reusing capacity can reduce incremental cost, but
check whether the evaluation would trigger autoscaling, higher provisioned capacity,
or other additional charges.

## Agree on the spending profile

Before creating billable resources or increasing capacity, confirm:

- The evaluation budget, currency, and expected duration.
- Existing resources to reuse and new resources to create.
- Expected data volume, request rate, and transfer volume.
- Estimated incremental evaluation cost and ongoing cost if resources remain running.
- When to stop, which resources to clean up, and who owns cleanup.

Research current provider pricing for the actual region, resource configuration,
and network path. Record the pricing source, date, billing units, assumptions,
and unknowns. Do not assume free-tier eligibility, discounts, or unused quotas.
If a cost cannot be established, explain the uncertainty before asking the user
to approve it. Reconfirm the spending profile when the plan changes.

A budget alert is useful, but do not treat it as an enforced spending cap unless
the environment actually provides and configures that enforcement.

## Review the major cost drivers

| Driver | What to check |
| --- | --- |
| Compute and cluster services | Additional nodes, autoscaling, control-plane charges, instance size, and idle runtime. |
| Block storage | Allocated capacity, provisioned IOPS and throughput, minimum billing periods, and retained volumes. |
| Snapshots and object storage | Retention, request counts, retrieval, minimum storage duration, and restore costs. |
| Networking | Transfer direction, availability-zone and region boundaries, internet egress, NAT processing, private endpoints, transit services, and load balancers. |
| Observability | Log ingestion, retention, high-cardinality metrics, and tracing volume. |
| Supporting services | Fixed and usage-based charges for identity, certificate, DNS, and other services proposed by the setup. |

Avoid adding paid supporting services merely to run a small evaluation. For example,
do not provision a private certificate authority, managed identity service, NAT
gateway, or load balancer without explaining why it is needed and obtaining
approval for its costs.

## Research bandwidth before moving data

Map the actual data path, including both endpoints, their regions and zones, and
any gateways or endpoints in between. Research the provider's pricing for that
path. Private connectivity and same-region traffic must not be assumed free.

Estimate bytes and charges for initial loading, replicas, reads, benchmarks,
retries, recovery, tiering, and any transfers needed during cleanup. Include
per-request and processing charges where applicable. Record the calculation so
the user can adjust the workload size and duration.

Treat cross-region evaluation as too expensive for the default evaluation budget.
Exclude it unless the user explicitly opts in after reviewing the estimated
cost. Selecting an add-region workflow expresses interest; it does not approve
an unknown bill.

Likewise, exclude storage, replication, and benchmark traffic across the public
internet unless the user explicitly opts in after reviewing the bandwidth costs.
This restriction concerns the storage workload being evaluated; ordinary
documentation and package downloads are separate.

Use verified private network paths within one region by default. Prefer avoiding
cross-zone transfer charges for a basic evaluation where practical, and explain that
co-locating resources does not validate resilience to an availability-zone outage.
If a suitable private path is unavailable, present costed alternatives rather
than silently switching to public endpoints. Encrypted traffic still needs the
same routing and cost review.

## Offer economical storage choices

Where the documented configuration supports it, gently favor suitable
cloud-provided block volumes that the user can provision and supply themselves.
Offer existing unused capacity only after confirming ownership and intended use.
Explain the tradeoffs against dynamically provisioned storage; manual provisioning
does not inherently lower the provider's storage price.

Discuss whether cloud block storage is an appropriate midline or farline tier
for the workload. Compare capacity, performance, access frequency, retrieval,
network transfer, retention, and operational costs. Do not promise a storage tier
or integration that the implementation does not support.

Follow the [tiering guide](GETTING_STARTED_WITH_TIERING_ON_KUBERNETES.md) and the
actual installation prerequisites. The
[Kubernetes quickstart](GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md) uses a
volatile-memory example; distinguish that demo from a persistent-storage setup.
Never format or overwrite an existing volume without explicit confirmation.
Userspace storage stages retain placement and replication responsibilities;
block devices are terminal media, not mirror or stripe primitives.

## Keep cleanup in the conversation

Keep an inventory of resources created or changed, including provider IDs,
region, namespace, owner, purpose, ongoing charges, retention settings, and
cleanup procedure. Distinguish evaluation-owned resources from pre-existing resources.

At every natural stopping point, after validation, and before ending the session,
ask whether the user is done and ready to clean up. Name what would be removed,
what data would be lost, and what would continue costing money. If the evaluation
continues, ask again at the next stopping point. Respect an explicit decision to
keep it and agree on the expected duration and remaining spend.

Do not claim future reminders or automatic teardown are scheduled unless they
have actually been configured. An absent response is not permission to delete.

## Verify teardown, including retained resources

After cleanup is authorized, remove only the agreed evaluation resources. Preserve
pre-existing clusters, volumes, networks, and workloads unless their removal is
explicitly authorized.

Check PVC and PV reclaim policies before deleting claims. Review every resource
type the evaluation created: volumes, snapshots, nodes, load balancers, addresses,
gateways, endpoints, log groups, certificates, and any other supporting resources.
Use the inventory and provider reads to identify leftovers and their owners.

A successful Terraform destroy or namespace deletion is not sufficient evidence
that no billable resources remain. Inspect each created resource type afterward,
including retained or separately provisioned resources. Do not delete unrelated
resources found during an account-wide search.

Report what was removed, what remains, why it remains, its ongoing costs or
pricing uncertainty, and exact cleanup steps. Separate successful resource
removal from billing confirmation: usage already incurred may appear later.
