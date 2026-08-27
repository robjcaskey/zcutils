# Estate sharding contract

This contract makes the federal-repatriation 4x envelope a concrete scheduler
model without putting the whole estate in one process. Its scope ends at
managed volumes. An agency-run object service is an application with managed
backing volumes; its individual buckets and objects are not scheduler records.

## Concrete universe

The version-1 scheduler target contains 128 business entities, 512
administrative regions, 1,024 failure-domain sites, 3.5 million storage hosts,
400,000 logical applications, 2.8 million deployed environments, 12 million
managed volumes, 8 million logical databases, and 50 million authoritative
relationship edges. The directory divides those edges as follows:

| Relationship | Concrete count |
| --- | ---: |
| Database backed by volume | 24,000,000 |
| Application environment uses database | 8,000,000 |
| Volume consistency relationship | 10,000,000 |
| Application dependency | 8,000,000 |
| **Total** | **50,000,000** |

Database-to-volume memberships are included in the 50 million total. They are
not another 24 million edges added on top.

## Ownership hierarchy

```text
federation boundary state
├── 512 regional export summaries
├── relationship-directory checkpoint vector
└── explicit cross-region operation manifests

one logical relationship directory
└── 4,096 authoritative partitions
    └── exactly one authoritative copy of every relationship edge

each region
├── cached adjacency projection at a directory checkpoint epoch
└── 16 regional state/scheduling shards
    ├── applications, databases, and volumes homed in the region
    ├── host, worker, lane, leaf, and local capacity assignments
    └── lane-local HTB targets and private scheduling revisions
```

“One relationship directory” means one schema, namespace, consistency
contract, and query surface. It does not mean one lock, one process, one Raft
group, or one machine. An authoritative edge hashes to exactly one of 4,096
partitions. Endpoint regions receive derived adjacency projections at a named
directory high-water mark; those projections cannot author or commit an edge.

Regional state shards own placement. The client block edge never chooses a
mirror, stripe, tier, spill target, locality, or lane. Those remain userspace
placement decisions and terminal block devices remain leaf media.

## What stays inside a region

A normal lane/flow scheduling change updates only its regional state shard:

- volume-to-host and volume-to-lane assignments;
- worker and CPU affinity;
- mux, demux, terminal-leaf, and local recovery queue choices;
- lane-local HTB targets and borrowing;
- local capacity consumption that remains inside the current exported bucket;
- private scheduling generation and placement digest.

No other region observes or acknowledges those changes. They do not update a
federation log and cannot contend on a federation lock or atomic.

A region publishes upstream only when a boundary value changes:

- online, fenced, or lease state;
- topology generation visible to placement outside the region;
- exported free-capacity or protected-IOPS bucket;
- failover-reserved bytes or IOPS;
- relationship-directory checkpoint epoch the region has consumed;
- a cross-region durability, recovery, or consistency commitment.

Publishing an identical summary is a no-op. Consumers compare monotonically
increasing export generations, not region-private schedule revisions.

## Cross-shard atomic work

A snapshot, failover, or migration that genuinely spans shards first chooses
one immutable relationship-directory checkpoint. Its compact epoch and root
digest name an exact vector of the 4,096 partition HWMs. The directory resolves
the relationship closure into participant shards. The coordinator persists a
`CrossShardCutManifest` containing one bounded record per participant shard:
member count and selector digest, not millions of volume IDs.

Each participant resolves its selector locally at the declared checkpoint,
prepares its immutable cut, and returns a token tied to its local revision. The
coordinator commits only after all declared participants prepare. A region or
shard absent from the manifest has no work and need not hear about the
operation. A directory partition outage permits unaffected local scheduling to
continue from cached state, but new relationship mutations and new cross-shard
cuts requiring an unavailable checkpoint fail closed.

Large operations therefore have hierarchical atomicity: one global manifest,
one prepare record per participant shard, and shard-local bounded member sets.
They never become one 12-million-entry Raft command.

## Deterministic smaller-scale model

`FederalEstateProjection::build(1_000)` constructs a deterministic 1:1,000
model:

| Population | Representative records | Exact concrete weight |
| --- | ---: | ---: |
| Storage hosts | 3,500 | 3,500,000 |
| Logical applications | 400 | 400,000 |
| Deployed environments | 2,800 | 2,800,000 |
| Managed volumes | 12,000 | 12,000,000 |
| Logical databases | 8,000 | 8,000,000 |
| Relationship edges | 50,000 | 50,000,000 |

Every representative owns a contiguous concrete ordinal range. The ranges
cover each population exactly, with no gaps, overlaps, rounding loss, or
uncounted tail. All 128 entities, 512 regions, 1,024 failure-domain sites, and
relationship categories remain explicit rather than being scaled away.

The projection is exact for cardinalities, weights, ownership boundaries,
relationship categories, local-versus-cross-region classification, and
capacity accounting. It is not proof that one representative graph has every
pathological topology possible in 50 million individually materialized edges.
Tests must therefore add adversarial graph profiles: giant components, hot
endpoints, hub-and-spoke legacy clusters, dense consistency groups, acquisition
overlays, and multi-region failure chains. Full materialization can be run as a
separate distributed soak without changing the contract.

## Current executable checks

The module tests establish that:

- 50,000 authoritative representative records cover exactly 50 million edges;
- every relationship has exactly one authoritative partition;
- regional adjacency is a read-only projection at one directory checkpoint;
- 10,000 downstream schedule changes produce no federation-state change;
- one changed regional capacity boundary produces exactly one export update;
- cross-shard cuts reject duplicate participants and use one exact directory
  checkpoint reference.
