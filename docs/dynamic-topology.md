# Dynamic topology and durability ownership

Topology is a committed change stream, not a static placement DSL. The
userspace control plane applies atomic `CommittedTopologyBatch` transactions;
each transaction advances a topology epoch and is replayed from the shared
`ChangeLogStore`. The `/dev/zcnblk0` client edge never interprets these facts or
makes placement decisions.

## Characteristics

An entity or relationship carries arbitrary typed JSON characteristics. Common
facts include:

```text
cloud.provider
cloud.region
cloud.availability_zone
cloud.availability_zone_id
cloud.placement_group
failure.region
failure.az
failure.host
failure.rack
durability.role
```

`UpsertEntity` establishes a complete entity. `PatchEntityCharacteristics`
atomically sets and removes selected facts without discarding facts learned by
another source. Applications submit these commands through the metadata Raft
runtime; `TopologyStore` is the committed application-state-machine sink and
does not itself claim to be a Raft implementation.

## Permissionless EC2 discovery

`zctopology-detect` queries only the local EC2 IMDSv2 endpoint. It needs no IAM
role and makes no AWS API call:

```bash
zctopology-detect \
  --set durability.role=hop \
  --set failure.rack='"rack-7"'

ZC_TOPOLOGY_CHARACTERISTICS='durability.role=leaf,failure.foo="bar"' \
  zctopology-detect
```

Values are JSON when parseable and strings otherwise. A `null` override removes
a discovered fact. Region, AZ, stable AZ ID, placement group, instance identity,
instance type, and local IPv4 are detected when IMDS exposes them. Optional
facts such as placement group remain absent instead of making discovery fail.

## Policy and movement

A `DurabilityObligation` describes minimum copies, minimum role counts, and
minimum distinct values for any named characteristic. For example, two copies
may require one `hop`, one `leaf`, and two distinct `failure.host` values.

Custody is fenced by log identity, incarnation, term, and topology epoch. A
replacement begins `staged`, catches up to an explicit per-lane HWM, and
becomes `active` only after its userspace data stage reports that HWM durable.
Copy activation retains the source; move activation marks it
`pending_release`. A controller can also prepare a later release explicitly.
In every case source release is rejected unless remaining available witnesses,
excluding the source itself, satisfy the obligation at the requested HWM.
Released custody and its completed handoffs can then be retired before the
unused entity is removed.

`EvolutionController` provides these policy-neutral phases. It does not embed
a static topology DSL: higher-level controllers choose placements from the
current typed facts, then commit stage, activation, release, fact patch, and
retirement transactions. A node with `health.available=false` is excluded
from durability coverage. Failed controller transactions leave both the
in-memory state and durable change log unchanged.

Metadata voters do not count as payload copies merely because they know a HWM.
A small Raft tiebreaker can vote without owning the data WAL.

## QEMU topology-evolution proof

`scripts/zctopology-evolution-qemu.sh` builds a minimal initramfs and launches
six 192 MiB, one-vCPU VMs: five userspace replica/tier stages and one metadata
controller. Replica HWMs are exchanged over TCP; each target writes, syncs,
and atomically installs its local state before replying. The controller VM
commits activation only after receiving that reply. No block device is used as
a placement, mirror, stripe, or spill primitive.

The harness starts with a hot region-A replica and cold region-B replica. Its
three logical supervisor voters are separate from the two initial payload
witnesses. It physically lowers host TAPs, rather than changing an in-process
health flag. It performs and verifies:

1. isolated 2-of-3 supervisor quorum loss rejects both HA and topology commits
   without advancing either durable revision;
2. an application-consistent snapshot at sequence 4 on two data replicas;
3. a named recovery point at sequence 6, based on the snapshot, while retaining
   later destructive mutations at sequences 7 and 8;
4. region-A data loss causes durability coverage to fail closed;
5. overlapping loss of both data sources and supervisor quorum rejects recovery
   and all metadata/topology commits atomically;
6. after partial restoration, region C restores the sequence-4 snapshot, replays
   only WAL records 5 and 6, and verifies the resulting SHA-256 digest before
   custody activation; records 7 and 8 are proven absent;
7. recovered region-C custody is certified with region B in a higher metadata
   term and configuration epoch;
8. hot promotion, online replacement, and cost-driven contraction; and
9. exact topology and PITR metadata replay after reopening both committed logs.

The latest 2026-08-16 run completed 19 atomic topology epochs with two active
copies in two live regions. The snapshot cut was 4, recovery cut was 6, and the
recovered payload digest was
`sha256:ff599e97ef880f6d3bb103e258988eec1513c389762bbee13fc3732bd02fdd66`.
Logs are in `bench-results/zctopology-evolution-qemu-20260816T142626Z/`. This is a
correctness/failure-injection proof, not an IOPS benchmark; the topology
manifest marks it non-representative and reports no performance number.

## EC2 architecture measurements, 2026-08-16

These are TCP metadata transport/load measurements with 64-byte payloads and
one ACK per record. They are not block IOPS, persistent-media results, or proof
of elections. All nodes were in `us-east-2c`; each result is three repeated runs.

| Layout | Majority records/s, mean | Range | Raw RTT |
|---|---:|---:|---:|
| One c8gn.48xlarge tier leader + one c8gn.48xlarge peer | 246,246 | 211,411–302,498 | 45 us |
| Two c8gn.48xlarge tier voters + one t4g.nano tiebreaker | 516,374 | 447,442–587,161 | tier 45 us, tiny 222 us |
| Three c8g.large voters | 1,095,444 | 1,084,535–1,102,947 | 118–208 us |
| Three c8g.4xlarge voters | 1,086,722 | 1,083,217–1,093,024 | 84–159 us |

The asymmetric quorum reports the first required follower ACK: with three
voters, the leader plus either follower is a majority. It still drains and
reports the slower peer. The test exposed and fixed a leader deadlock in which
the append stream was written completely before reverse ACKs were drained.
ACKs are now consumed concurrently with sends.

The t4g.nano is burstable and these short runs do not establish indefinitely
sustained CPU-credit performance. `raft-leader` remains a transport benchmark.

The persistent runs use `raft-durable-leader` and
`raft-durable-follower`. Every voter writes a sequential file WAL and emits a
prefix ACK only after `fdatasync`. Majority completion is the second durable
completion among three voters, including the leader's local WAL.

| Persistent layout | Flush grouping | Majority records/s, mean | Range |
|---|---:|---:|---:|
| Two large tier voters + t4g.nano | 1 record | 374 | 373–374 |
| Two large tier voters + t4g.nano | 64 records | 21,598 | 21,038–22,398 |
| Three c8g.large voters | 1 record | 379 | 379–379 |
| Three c8g.large voters | 64 records | 22,573 | 22,572–22,576 |
| Three c8g.4xlarge voters | 64 records | 22,557 | 22,538–22,576 |
| Large leader + t4g.nano, other large voter unavailable | 64 records | 20,999 | 20,977–21,025 |
| Large tier leader + two c8g.large voters | 64 records | 22,573 | 22,570–22,577 |

`raft-durable-inspect` reopened and structurally validated all nine selected
leader/follower WALs: each contained 100,000 contiguous frames through index
100,000 and exactly 9,600,000 bytes. A unit test also proves a truncated final
frame is rejected. With one configured large follower connection refused, the
durable leader plus t4g.nano continued as two available voters in a three-voter
configuration. With both followers unavailable, it returned `NotConnected`
instead of publishing a result. Elections and automated leader promotion remain
a separate gate; these runs prove persistent replicated-prefix, majority-ACK,
one-peer-loss availability, and no-quorum fail-closed behavior.

The c8g.large and c8g.4xlarge groups are effectively tied in this workload.
Adding cores does not improve the single-stream metadata path, so c8g.large is
the better price/performance voter here. With persistent ACKs, the ceiling is
the identical gp3 flush cadence rather than voter CPU size.

Raw logs and discovered facts are under
`bench-results/zcutils-raft-topologies-20260816/`.
