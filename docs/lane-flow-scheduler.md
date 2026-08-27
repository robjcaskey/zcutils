# Lane/flow scheduler

`src/gang_scheduler.rs` is a deterministic control-plane scheduler. It reads an
indexed estate event stream and assigns flows to contention lanes. It does not
run a data-path thread, issue storage I/O, or make a per-I/O scheduling decision.
Production workers consume its prepare/commit plans and update lane-local HTB
targets outside the hot path.

## Flow, lane, gang, and arena

A **flow** is an end-to-end obligation, such as one volume's steady I/O or one
volume's recovery replay. A flow can traverse several **lanes**: local mux,
inter-region transport, targeted demux, and terminal userspace leaf lanes. A
lane is therefore a resource assigned to flows, not another name for a flow.

Gang and arena describe coordination over flow-to-lane assignments:

- `gang` admits a broad atomic unit. It minimizes control-plane transactions,
  but one failed participant aborts the entire unit.
- `arena` admits independent single-volume flows. A failed participant only
  unwinds its own assignment. Arena-only mode refuses multi-volume recovery or
  consistency work rather than silently weakening it.
- `hybrid` uses narrow arena admission for independent flows and gangs only for
  declared recovery groups, snapshot cuts, and similar atomic boundaries. It is
  the default.

All modes use the same idempotent prepare, commit, and abort protocol. “Arena”
does not mean partial commit of an operation that promised atomicity.

## Recovery scheduling

Recovery planning considers all flows converging on a destination together.
Each accepted recovery reserves:

- steady terminal bytes and IOPS on explicit host lanes;
- a place in the destination's aggregate recovery queue;
- every durable mux/demux path used to materialize the flow;
- pipeline fill latency, demux byte and operation rates, target copy/apply rate,
  and unavoidable payload copies.

The next flow sees those reservations, so ten flows do not each claim the whole
region's restore bandwidth. `RecoveryTiming` exposes queue start, demux time,
target materialization time, completion, deadline, whether the RTO is met,
transport kinds, and copy accounting. Work that misses its RTO can still be
restored; the missed objective remains explicit instead of making the data
permanently unschedulable.

`PreapprovedFailoverRule` selects a target or `hold_durably` from the current set
of unavailable regions. The most-specific rule wins. This supports, for
example, `us-east-1 -> us-east-2`, then `us-east-1 + us-east-2 -> eu-west-1` for
critical flows while lower-value flows remain in durable multiplexed backlog.
An in-flight destination loss aborts unfinished assignments; already committed
flows and unfinished flows are re-evaluated from their different current state.

`preview_disaster` evaluates single-loss, multiple-loss, and very-bad-day failure
sets without mutating authoritative state. It uses the same planner as a live
failover, not a separate capacity calculator.

## SLO and impact inputs

The scheduler does not accept a user-defined numeric recovery priority class.
It derives ordering from:

- RPO and RTO;
- current durable and applied HWMs;
- a solution-level estimate of downtime cost rate, one-time RTO breach cost,
  and cost per lost committed operation;
- the currently feasible lane/flow routes and their completion estimates.

`ScenarioBusinessImpactRule` can override both RTO and impact for a failure set.
This lets a same-country inter-region outage carry a tight, reputationally costly
objective while a true global disaster has a three-hour objective because the
business context is already radically different. Ambiguous equally specific
rules are deferred rather than resolved by an arbitrary tie-break.

## Zero copy and transport assumptions

Every mux path declares its transport and copy passes. Copy passes are charged
against a measured copy bandwidth and appear in recovery timing. A zero-copy
assignment means buffer ownership can pass to the next userspace stage without
a CPU payload copy; it does not claim that bytes bypass NIC DMA or terminal
media I/O.

Cross-region paths should normally be modeled as TCP. TCP may still use shared
registered arenas and zero-copy send/receive facilities where the actual stack
supports them. RDMA is an explicit option, never an assumed cross-region
property, and the principal multi-loss test uses TCP plus shared-arena demux.

## Federal-repatriation 4x design envelope

`FEDERAL_REPATRIATION_4X_DESIGN_ENVELOPE_V1` is the common scalability
baseline for design decisions. It is a deliberately conservative engineering
envelope, not a claim that an authoritative census of federal applications,
databases, cloud bytes, or objects exists. In particular, GAO says agencies do
not consistently track cloud costs and savings, so cloud byte counts inferred
from spending would provide false precision.

Public anchors for the estimate are:

- GAO counted about [6,700 federal IT investments in FY
  2024](https://www.gao.gov/products/gao-24-106693).
- Data.gov currently exposes hundreds of thousands of [federal public
  datasets](https://catalog.data.gov/dataset?organization_type=Federal+Government),
  which excludes most internal and classified state.
- OPM's official workforce source reports roughly [two million federal civilian
  employees](https://data.opm.gov/); it excludes important populations such as
  many contractors and parts of the national-security estate.
- Major agencies obligated about [$7 billion on cloud contracts in FY
  2022](https://www.gao.gov/assets/gao-23-106247.pdf), including about $3
  billion by DOD, but the source does not report stored bytes.
- NASA alone projected a [328.2 PB EOSDIS archive in
  2025](https://science.nasa.gov/wp-content/uploads/2023/04/5_Big_Data_Earth_Science_tagged.pdf?emrc=8c801c).

The version-1 estimate turns those anchors into simple, inspectable
assumptions:

1. One estimated federal estate is 100,000 logical applications: approximately
   15 deployable systems/components per reported IT investment, rounded.
2. Every application has dev, staging, and production. Twenty percent are
   treated as publicly available and additionally average 20 developer sandbox
   environments. That gives 700,000 deployed application environments before
   the 4x multiplier and 2.8 million after it.
3. Modern applications normally own one volume, while database clusters,
   object services, logs, and legacy pools raise the estate average to about
   4.3 volumes per deployed environment. The 4x target is 12 million managed
   volumes.
4. Developer clones count as distinct database and volume control-plane
   objects even when copy-on-write makes their physical byte cost initially
   small. The target is 8 million logical databases and 24 million explicit
   database-to-volume memberships.
5. Repatriated S3-like services count as deployed application environments and
   their backing pools count as ordinary managed volumes. Buckets and objects
   remain inside those services and are intentionally absent from the storage
   scheduler's model.
6. The estimated federal estate carries 25 EB of logical database, VM, object
   pool, log, and file state. This is an explicit capacity hypothesis rather
   than an inferred public-cloud byte count. The 4x target is 100 EB logical and
   350 EB physical after a planning factor of 3.5 for replicas/erasure coding,
   journals, snapshots, rebuild reserve, and stranded capacity. At 100 TB
   usable per storage host this is 3.5 million hosts.

The complete 4x envelope is therefore:

| Dimension | Target |
| --- | ---: |
| Business/administrative entities | 128 |
| Administrative regions | 512 |
| Failure-domain sites | 1,024 |
| Storage hosts | 3,500,000 |
| Logical applications | 400,000 |
| Deployed application environments | 2,800,000 |
| Managed volumes | 12,000,000 |
| Logical databases | 8,000,000 |
| Database-to-volume memberships | 24,000,000 |
| Authoritative relationship edges, including DB-volume memberships | 50,000,000 |
| Logical data | 100 EB |
| Planned physical media | 350 EB |

This envelope requires partitioned ownership. No production component may
load the global estate into one `BTreeMap`, run an O(global-estate) placement
pass, maintain a per-I/O global counter, or put every object through Raft.
Regions own sharded volume/application state and publish compact capacity and
policy summaries upward. Atomic snapshots use a hierarchy of immutable cuts
and bounded prepare groups instead of one 12-million-member transaction.
Disaster scheduling is proportional to the affected partition and consumes
precomputed placement alternatives; global policy reconciliation remains off
the lane-local data path.

The concrete ownership, partitioning, relationship-directory, scaled-model,
and cross-shard commit rules are specified in
[`estate-sharding-contract.md`](estate-sharding-contract.md).

## Tests

The module's deterministic tests cover:

- Cassandra JBOD, CockroachDB, Postgres data/WAL, a storage-durable sharded DB,
  MinIO, and Kafka in an evolving four-region estate;
- increasingly broad same-region and cross-region consistency snapshots;
- live business-impact changes, regional failover, capacity-short resize, and
  capacity growth;
- gang/arena/hybrid failure-isolation bakeoff;
- converged recovery into insufficient capacity, dry-run disaster previews,
  durable mux backlog, a destination loss during replay, targeted TCP demux,
  explicit hold behavior, and scenario-specific three-hour global RTOs;
- exact event/decision/state replay; and
- 1,000 interrelated applications and 1,199 physical volumes without worker
  threads or storage allocation: a low-impact legacy development cluster with
  2,000 logical databases spread over 200 pool volumes (10--50 databases per
  volume), 400 legacy consumers, four high-criticality database volumes, and
  595 newer one-volume services. Named workflow edges and an evolved
  estate-wide cut exercise transitive consistency while impact aggregation
  proves raw database count does not outrank dedicated high-criticality state.

These are control-plane model tests. They do not establish data-path IOPS or
latency and must not be reported as such.
