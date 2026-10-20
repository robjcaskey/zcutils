# AWS cost model for Diskpipe evaluations

Start with the [general cost-control guide](../COST_CONTROL.md). Reuse an existing
cluster or host, agree on the spending profile, and keep the default evaluation in one
region on verified private network paths. Cross-region evaluation and public
internet storage traffic require explicit approval of their estimated costs.

## Storage

EBS bills provisioned capacity until it is released. Depending on the volume
type, provisioned IOPS and throughput can add separate charges. For gp3, compare
the included performance with the additional performance requested; do not pay
for benchmark headroom the evaluation will not use. Snapshots have their own pricing.
Check the selected region and volume type in [EBS pricing](https://aws.amazon.com/ebs/pricing/).

Offer user-provisioned EBS capacity where the documented Diskpipe setup supports
it. Record the volume ID, availability zone, size, performance settings, owner,
and intended deletion policy. Compare the complete cost before choosing it as a
midline or farline tier; manual provisioning alone does not change the rate.

## Network path

Price each direction of the actual transfer. Same-region cross-zone traffic,
inter-region transfer, and internet egress have distinct rules. Service-specific
exceptions matter; do not apply one EC2 rate to every AWS service. Use
[EC2 data-transfer pricing](https://aws.amazon.com/ec2/pricing/on-demand/)
and the [billing guide to transfer charges](https://docs.aws.amazon.com/cur/latest/userguide/cur-data-transfers-charges.html).

NAT gateways have time and data-processing charges, and public IPv4 addresses
can add ongoing charges. Research these separately from bandwidth in
[VPC pricing](https://aws.amazon.com/vpc/pricing/). Inspect routes before assuming
that a private subnet provides a cheap private data path.

## Build the estimate

List incremental compute runtime, cluster charges if applicable, EBS capacity
and performance, snapshots, transfer bytes by path, network intermediaries, and
logs. Record the current rate source and estimate both evaluation-duration and
left-running costs. Review any proposed private CA, managed identity service,
load balancer, gateway, or endpoint separately before approving it.

## Cleanup evidence

Ask about cleanup at natural stopping points. After approved teardown, inspect
every resource type created, including retained EBS volumes, snapshots, load
balancers, public addresses, gateways, endpoints, and log groups. Check the
actual account and regions used. Preserve pre-existing resources. Report any
remaining resources, ongoing costs, and exact removal steps.
