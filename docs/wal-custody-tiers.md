# WAL custody tiers and geographic survivability

This is a proposed model for the next implementation, building on the existing
[topology change stream](dynamic-topology.md) and
[HA/PITR metadata](ha-pitr-consensus.md). Geographic custody receipts, scenario
evaluation, and the memory-holder lifecycle below are not yet integrated into
the live data path.

The central object is a **WAL holder**: a userspace stage that accepts custody
of a specified range of a log. It can retain ciphertext, forward it, transfer
it to another holder, persist it, or release it. It need not materialize a
volume, serve application reads, possess decryption keys, or vote in Raft.

A nearby holder can cheaply protect the recent tail while slower storage
establishes longer-term recovery coverage. Most segments may leave that
holder's memory without ever being written to its disk or read by an
application. Forwarding still consumes memory and network bandwidth.

## One model, independent properties

Tier numbers are a view of a volume's current topology. The same server may
be an early custody point for one volume and a late recovery store for another.
The underlying objects describe these independent properties:

| Property | Examples |
| --- | --- |
| Location and dependencies | Host, rack, power domain, AZ, region, operator, cloud account, control plane, network path |
| Payload representation | Original WAL, derived coalesced WAL, materialized base, read-through cache |
| Residency and evidence | Volatile RAM, qualified protected RAM, synchronized file, terminal device, object WAL |
| Custody | Exact retained ranges, retention obligations, pending handoffs, release authority |
| Forwarding | Destinations, raw or coalesced stream, freshness deadline, bandwidth budget |
| Persistence schedule | On admission, bounded delay, on pressure, or no local persistence |
| Overflow | Neighbor RAM, local sequential storage, object WAL, or bounded backpressure |
| Recovery service | Ciphertext export, WAL replay, indexed reads, full writable volume |

A RAM-only holder with no overflow is a valid configuration when its admission
limit and upstream retention make that safe. A disk-backed holder can defer
replay indefinitely while still retaining a complete recoverable WAL. Neither
requires a separate kind of region or a special Kubernetes implementation.

All placement, forwarding, replication, spill, and lane choices belong to
userspace stages. A block device can be terminal media behind a userspace
writer after placement is decided; it cannot implement the tier or mirror.

## Describe the failures that a copy survives

Avoid one unqualified `permanent` flag. Evaluate a copy, or a composition of
copies, against a named failure scenario and recovery deadline.

| Evidence at a cut | What it can contribute |
| --- | --- |
| RAM on another host in the same AZ | Surviving the source host's loss, subject to the other holder and its memory staying available |
| RAM in an independent AZ | Surviving loss of the source AZ, including its disks |
| RAM outside two source regions | Surviving loss of those two regions while the outside holder remains intact |
| Qualified protected RAM | The specific power interruption and restart behavior covered by its protection mechanism |
| Completed persistence to qualified media | Recovery after the covered volatile-memory/power loss, subject to media and site survival |

Multiple RAM copies can satisfy a declared host, AZ, or regional survival
contract without each performing a disk write. They do not cover simultaneous
loss of all those memories. The model must allow the useful composition and
report its actual failure coverage.

Frequent sequential flushes are a useful way to add protection for a small
tail. Their completed persistence receipts advance the power-loss recovery
cut. A scheduled flush, idle disk, or ability to flush quickly does not advance
that cut. A bounded flush interval gives a bounded persistence lag only while
the admitted queue, bandwidth, and flush-completion deadlines are being met.

The controller chooses enough persistence targets to maintain the requested
coverage; it need not flush the same segment at every RAM holder. Sequential
flushes can cover many volumes in one physical append stream, with lane and
volume identities preserved. Once other qualified coverage removes a holder's
local persistence obligation, it can cancel an unstarted flush and release
the segment. Logical obsolescence alone is insufficient if PITR or another
survival scenario still requires that version.

Protected RAM requires evidence beyond `bbu_present=true`: protected byte
capacity, battery health and hold-up time, an autonomous preservation path,
recoverable data and metadata after restart, and the dependencies of that
path. A software process that might copy memory after noticing a failure
cannot promise survival of that process's instantaneous loss.

If protection drains to another device, admission must cover:

```text
detection + queued protected bytes / reserved drain throughput
          + persistence completion + margin < usable hold-up time
```

Check that bound under simultaneous drains by every node sharing the target,
controller, link, or power supply. On degraded evidence, stop issuing new
protected-memory receipts and evacuate existing custody while protection still
holds. Report a contract violation if the promised coverage is actually lost;
changing a capability label does not retroactively preserve the data.

## Receipts describe retained recovery coverage

A proposed `CustodyReceipt` binds:

- holder identity and incarnation, log identity, writer term, topology epoch,
  and policy generation;
- lane and exact retained sequence range, with a contiguous prefix where one
  exists;
- the payload representation and source-cut manifest, including required base
  snapshots and consistency cuts;
- residency evidence and its capability generation;
- the retention obligation and authority needed to release it.

A highest-seen sequence is insufficient. A holder with a hole, an expired
retained range, or a coalesced replacement cannot advertise the same fact as
one holding a complete original prefix. Keep retained-range boundaries as well
as HWMs. Cuts spanning lanes or volumes need their consistency manifest.

Receipts are authenticated, batched, and bound to the correct stream. A relay's
TCP ACK or local send completion is not a custody receipt. Authentication alone
also does not prove that a malicious holder retained bytes: the existing
integrity/fault model must account for untrusted or corrupt witnesses.

For each named scenario, derive the newest acknowledged source cut for which
the surviving system has a reconstructible base, required WAL ranges,
manifests, and an authorized path to decryption keys. A WAL-only waypoint can
rely on a base in Europe; it cannot recover a volume if the only base was in a
destroyed source region. The waypoint itself does not need those keys.

Report two separate recovery properties for that scenario: which cut survives
and how long making it usable at the selected destination will take. An
isolated holder can still contain surviving data while providing no currently
bounded recovery time. Metadata quorum loss can prevent safe writable
activation even when all required payload is present.

RPO status includes missing committed sequences/bytes and the age of the
missing committed work, using source commit timestamps. Do not make an idle,
fully replicated volume look increasingly behind just because its last write
is old. Distinguish observed evidence, stale evidence, and an admitted RPO
bound. Clock uncertainty affects time estimates, not log ordering.

## Retain, forward, and usually discard

The ordinary lifecycle is:

```text
admit bounded frames into a lane's arena
    -> validate complete units and accept custody
    -> retain references, optionally forward immediately
    -> receive replacement coverage and release authorization
    -> recycle the original arena segments
```

Forward complete transport units as they arrive; do not wait for a large
storage segment to fill before beginning a geographic transfer. Units within
an open segment may have receipts once complete and covered by the advertised
protection. Segment sealing and receipt batching have explicit maximum delays
when they affect an RPO or acknowledgement deadline.

When a segment becomes obsolete for every obligation held locally, **discard**
it. There is no reason to flush that obsolete segment to disk. When it is
still needed and memory is under pressure, **spill or transfer custody** before
recycling it. These are different operations.

A successor receipt is necessary but not always sufficient for release.
Releasing a nearby holder must preserve every relevant scenario, required
retention window, PITR pin, and in-flight recovery. Transferring RAM to another
host in the same AZ relieves memory pressure but does not add AZ diversity.
Two holders simultaneously releasing in reliance on each other must be
prevented by ordered, fenced custody changes.

The receiver grants ingress credits only against admitted space, including
outstanding credits and bounded open-segment/manifest overhead. Elastic growth
can acquire capacity in useful increments; it need not preallocate all future
storage. Before free space is exhausted, the holder must secure the next
increment, transfer custody, spill, or withhold more credits. A predicted idle
disk or hoped-for free RAM on a neighbor is not reserved overflow capacity.

An ordinary relay should reclaim whole immutable segments from metadata.
Compaction that examines records is a separate optional consumer with an
explicit CPU, memory-bandwidth, and I/O budget. Required preservation work may
need reserved capacity; it cannot depend indefinitely on spare CPU appearing.

## Geographic progress: chasing the light

Model a directed custody graph with optional forwarding and fan-out. Support
one to ten application-level holders where useful, but optimize for covered
failures, RPO, RTO, and cost, rather than maximizing hop count.

```text
US-East writer -> same-AZ RAM holder -> other-AZ RAM holder
                       |
                       +-> independent nearby site -> European WAL store
                       |
                       +-> US-West recovery store
```

The European path is independently fed. It does not depend on US-West
remaining alive. The nearby site can be a small custody deployment without
application compute, provided its power, network, restart, control authority,
and retention dependencies support the advertised survival scenarios.

Illustrative recovery state, using one lane and no new writer after the loss:

| Location | Retained recovery coverage | After East and West fail |
| --- | --- | --- |
| US-East | Through 120 | Lost |
| US-West | Through 119 | Lost |
| Independent waypoint | WAL 116–118 | Available for transfer to Europe |
| Europe | Base at 100 plus WAL 101–115 | Recoverable through 115 locally |

Europe can recover through 118 by combining its retained base and WAL with the
waypoint's suffix. Without that waypoint, this example loses committed work
116–120 instead of 119–120. Recovery does not require the waypoint to have
previously applied any of those records to a volume.

Receipt 119 from US-West must not authorize the waypoint to drop 116–118 if
the contract includes losing both East and West. Europe or another acceptable
survivor must take that obligation first. Conversely, after Europe has the
required persisted coverage, this waypoint can release those segments without
ever persisting them locally.

For sequential disasters, evaluate an event timeline: East fails; surviving
holders continue forwarding; West fails during replay; some volumes switch to
Europe while others wait. Include the minimum interval between losses, or
explicitly model simultaneous losses. Data confined to lost sites cannot be
reconstructed by adding hops after the event. Failover writers start a fenced
new history whose relationship to the recovered cut is recorded.

Extra holders improve the time at which bytes are captured outside particular
failure sets. They do not inherently shorten the path to the final destination.
A serial detour adds link latency, queuing, processing, and possibly extra
transfer charges. Measure routes rather than inferring them from geography.
Compare direct, waypoint, and parallel paths; an intermediate holder is useful
when its earlier capture in an independent domain warrants that cost.

Actual survival can begin when a remote holder has the bytes. The source can
claim confirmed protection only after the corresponding receipt returns. A
foreground fsync that requires that protection waits for its evidence; a local
fsync with asynchronous geographic improvement does not acquire a WAN wait.

Repair is part of the contract. Approximate a repair budget as:

```text
repair time = detect + authorize + acquire capacity
            + missing bytes / available repair bandwidth + persist + validate
```

The sequential-loss guarantee depends on completing the necessary repair or
forwarding before the next allowed failure. Shared links and surviving nodes
must be evaluated under that failure load, not just normal average traffic.

## Keep raw survival and coalesced freshness independently configurable

A low-RPO geographic tail may stream raw WAL immediately. A low-bandwidth
buddy copy may consume the
[five-minute derived stream](../zccusan/docs/GETTING_STARTED_WITH_A_MACOS_FEDERATED_REGION.md)
instead. Both use holders and receipts, but promise different source cuts.

Coalescing can omit superseded extent versions only within the recovery cuts
that policy permits. It preserves required barriers, trim/discard state, base
dependencies, and consistency manifests. It cannot reclaim the original WAL
needed by another raw replica, PITR point, or ongoing recovery.

Content-aware reduction normally happens at an authorized sender. A holder
without decryption keys can forward and release opaque segments using
authenticated manifests. It must not need plaintext just to provide custody;
any metadata permitted for ciphertext-only reduction requires a separate
privacy and correctness contract.

The same stream need not cross an expensive boundary independently for every
downstream copy. Transfer once to an admitted remote holder, then fan out
locally when that satisfies path independence and failure requirements. Count
the shared path as a dependency. Raw and delayed copies with different cuts
cannot automatically share every byte of their transfer.

## Capacity and cost are inputs to placement

Size memory from the retained WAL arrival rate and residence time:

```text
memory needed = retained byte rate * retention time + bursts + pins + credits
```

For example, 1 GiB/s retained for five seconds needs about 5 GiB before
headroom; one hour needs about 3.52 TiB. A forwarding outage changes a cheap
short tail into a potentially large backlog. Compaction may reduce it only
when the representation and retention contract permit it.

The regional scheduler should receive measured RTT, jitter, sustained send and
drain capacity, path dependencies, and prices for each candidate edge. It
chooses among independent sites that satisfy the policy. A site in Canada,
Central America, or a small privately operated region is a candidate, not
automatically a shorter route or an independent failure domain. Include
parent-region, provider-account, operator, power, and key-authority failures.

An AWS cluster placement group stays within one AZ and is useful for network
locality. It does not constitute another AZ or prove that two holders have
independent rack/power dependencies. AWS documents separate partition and
spread strategies for hardware separation.
[AWS placement strategies](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/placement-strategies.html)

AWS EC2 transfer charges depend on the actual path: same-AZ traffic can avoid
transfer charges, while cross-AZ and cross-region transfers and intervening
network services can add charges. Record those as edge costs rather than
assuming every geographic hop has the same price.
[AWS transfer-cost overview](https://aws.amazon.com/blogs/architecture/overview-of-data-transfer-costs-for-common-architectures/)

For scale, AWS's published Virginia-to-Oregon Transit Gateway example includes
$0.02 per GB in inter-region transfer plus $0.02 per GB in Transit Gateway
processing. These are distinct costs; the example is not a universal quote
for every route. [AWS Transit Gateway pricing](https://aws.amazon.com/transit-gateway/pricing/)

Using an illustrative **$0.02 per billable GB per charged leg**, one billable
GB every second for 30 days costs **$51,840 per leg** before compute, storage,
processing, retries, or discounts. Ten equally priced legs carrying the same
stream cost $518,400. This is arithmetic at that assumed rate, not a forecast
for a specific deployment. Saving disk writes does not remove those network
bytes. Price actual emitted WAL bytes, not read IOPS or peak NIC throughput.

Policy therefore needs separate budgets for steady replication, catch-up,
repair after declared failures, and long outages. HTB can borrow unused
capacity normally, but an RPO/RTO commitment needs enough admitted capacity
under its failure scenarios. A spend ceiling may stop an optional copy and
increase its reported lag; it must not silently weaken a required durability
contract. For a required path, use another qualified path or backpressure
before acknowledging work that cannot meet the contract.

## Control changes and the fast path

Continue using committed topology changes rather than a new static topology
DSL. Kubernetes, the native CLI, and other clients submit the same desired
obligations and topology operations. A regional controller chooses holders and
lanes; global policy sees summarized cross-region obligations and exceptions.
Local changes preserving those external obligations stay local.

Use ordered control events for creating obligations, admitting holders,
preparing transfers, activating replacement custody, releasing ranges, and
retiring holders. Atomic changes fence policy, witness-set, and topology
generations. Administrative lease expiry or a disconnected controller cannot
by itself permit deletion of required payload. A separate retention contract
defines any agreed expiry and what the sender must retain before it expires.

Raft commits authority, policy changes, and batched custody transitions or
release frontiers. It does not order every logical write or every receipt.
Under a committed plan, holders emit batched coverage receipts; workers use
precompiled witness rules and authorized release frontiers. Stronger future
obligations begin only after their evidence exists. A plan change cannot reuse
receipts from a stale incarnation or make already-released ranges reappear.

The data implementation should retain lane-owned arena references, preserve
NUMA locality, forward immutable payloads, and update progress in batches.
Keep descriptor lifetimes pinned until all network/DMA consumers and custody
obligations release them. Use TCP across regions; RDMA is an optional local
transport. Record actual platform-specific copies, especially encryption and
TCP receive boundaries; no universal zero-physical-copy claim follows from
this model.

Geographic placement, scenario evaluation, record compaction, pricing, and
Raft calls stay outside per-I/O processing. Even asynchronous workers consume
CPU, memory bandwidth, and NIC capacity, so isolation and admission must be
tested. Unchanged source code on the fast path is not proof of unchanged IOPS.

## Existing foundations and implementation gaps

| Existing source | Reusable foundation | Extension required |
| --- | --- | --- |
| [`src/topology.rs`](../src/topology.rs): `DurabilityObligation`, `CustodyLease`, `TopologyState::verify_coverage` | Characteristic-based copy coverage, fenced staged handoff, release checks | Scenario-specific evidence, retained ranges, dependencies and sequential-loss timelines |
| [`src/ha_metadata.rs`](../src/ha_metadata.rs): `ReplicaHwm`, `certify_durable_hwm`, `HaState::retention_floor` | Per-lane certificates outside per-fsync Raft, named recovery pins | Distinguish memory/protected/persisted evidence and derived cuts without upgrading old reports |
| [`src/integrity_contract.rs`](../src/integrity_contract.rs): `admit_topology` | Admission against declared erasures/corruptions and a real correction operator | Compose qualified memory custody with media evidence; existing admission requires durable flush and must not be bypassed with a false capability |
| [`src/persistent_wal.rs`](../src/persistent_wal.rs): `PersistentWal::sync`, `reduce`, `PersistentWalRetention` | Persistence, deferred materialization, pinned replay ranges | A holder that can retire or export an opaque WAL without requiring a full local base image |

Version these extensions explicitly. An older durable-HWM peer must not
interpret a volatile-memory receipt as power-safe, or a coalesced source cut
as possession of the omitted original log. Unknown required evidence is
unsupported, not an implicit downgrade.

The first empirical tests should exercise memory-only receive/forward/release
with zero terminal payload writes; missing/reordered/stale receipts; two
holders attempting circular release; pressure transfer to a neighbor; loss of
BBU evidence; bounded flush and overflow failures; and East/West loss during
replay with recovery from the independent holder plus Europe's base. A small
deterministic event model can explore one to ten holders and timing variations
before a QEMU payload-recovery test. QEMU can test preservation protocol state;
real BBU survival requires hardware qualification.
