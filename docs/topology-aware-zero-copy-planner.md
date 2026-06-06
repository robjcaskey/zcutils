# Topology-Aware Zero-Copy Planner

## TLDR

The descriptor should become the fabric contract, not just a pointer to bytes.
Each stage advertises capabilities, the client declares workload intent, and a
planner compiles a `ZC_PLAN_V1` that fixes lane, CPU, queue, WAL shard,
branch, zipper, batch, and zero-copy requirements for one plan epoch. Data
frames then carry only compact descriptor references plus the plan id and lane
sequence. Runtime telemetry can renegotiate a new epoch, but silent remapping is
forbidden.

This removes hand-tuned benchmark knobs from the hot path. Operators choose a
policy such as "mirror writes, low latency" or "stripe reads, maximum
throughput"; the fabric chooses the concrete topology and reports whether the
host, NIC, kernel, memory pool, and leaf layout can honor it.

## Thesis

The common abstraction across tcpmux, zcnblk, zcraid, zcrx, WAL extents, and
terminal leaf writers is:

```text
descriptor lease + lane sequence + placement epoch + result-log contract
```

Everything else is a compiled plan:

```text
intent -> discovered topology -> fabric plan -> descriptor/result streams
```

`lane_id` remains the spine. It must survive ingress, mux, fanout, leaf submit,
result logs, zipper, and upstream completion unless an explicit remap record
changes it at a plan epoch boundary.

## Control Frames

Add these length-delimited descriptor control records. They can ride in the
existing descriptor stream and be mirrored into tcpmux/zcnblk fixed headers as
compact plan ids and lane ids.

```text
ZC_CAPS_V1
  role: client | mux | fan | zipper | leaf | terminal
  node_id, process_id, boot_id
  cpus, numa_nodes, cache_groups, smt_siblings
  nics: ifindex, card_id, rx_queues, tx_queues, numa_node, private_ip
  memory: zcrx pools, registered buffers, hugetlb, memlock, arena ids
  transport: tcp, send_zc, zcrx, liburing, libfabric, efa, fallback copy
  storage: wal shards, zcdevnull, zcbrd terminal leaves, raw allowlisted media
  limits: max_lanes, max_iov, ring_entries, cq_entries, batch_min/max
  costs: copy_cost, syscall_cost, ctx_switch_cost, branch_lag, queue_depth

ZC_INTENT_V1
  mode: stripe | mirror | raid10 | spill | passthrough
  operation_mix: read | write | mixed
  objective: low_latency | max_iops | max_gbit | balanced
  record_size, max_request_bytes, expected_qdepth
  durability: ack_on_admit | ack_on_leaf_commit | quorum | all_replicas
  sync_contract: global_high_water
  zero_copy_policy: required | auto | off
  failure_policy: fail_fast | degrade | retry

ZC_PLAN_V1
  plan_id, placement_epoch, required_features
  transport: tcp_mux | libfabric_sockets | libfabric_efa | libfabric_verbs
  lane_count, lane_groups, worker_count
  lane_map: lane -> node -> nic_queue or fabric_endpoint -> cq -> mr_region
            -> cpu -> worker -> wal_shard
  branch_map: lane -> branch set, replica set, stripe segment map
  parallel_raid.branch_topology:
    branch -> role -> fabric_domain/NIC -> lane set -> CPU slice
           -> ACK policy -> result-log contract
  coalescing: extent_bytes, record_count, age_budget_ns, batch_window
  zipper: owner lane, primary branch, secondary handoff, reorder_window
  backpressure: descriptor credits, byte credits, wal credits, branch credits
  ack: per-write ack rule, sync epoch rule, read freshness rule

ZC_STATS_V1
  plan_id, lane, worker_cpu, rx_queue, tx_queue
  bytes, records, acks, syncs, retries, copies
  send_zc_copied, zcrx_fallback, cqe_overflows
  voluntary_ctx, involuntary_ctx, migrations
  branch_lag, reorder_depth, pool_pressure

ZC_REPLAN_V1
  old_plan_id, new_plan_id, cutover_epoch
  explicit remap records for lanes whose worker, queue, branch, or shard changed
```

## Planning Algorithm

1. Collect `ZC_CAPS_V1` from every participant before opening hot data lanes.
2. Normalize hardware into locality groups: NIC card, RX/TX queue, CPU, NUMA
   node, cache group, memory pool, and leaf shard.
3. Expand the user intent into candidate graphs. Examples:
   `client -> mux -> mirror fan -> two leaves`, or
   `client -> mux -> stripe fan -> zipper -> leaves`.
4. Reject candidates that would implement mirror, stripe, tier, or spill inside
   a block device.
5. Reject candidates that cannot satisfy required zero copy, route, memlock,
   hugetlb, ring, or CPU pinning constraints.
6. For `libfabric_*` candidates, require both provider discovery and a real
   cross-host data smoke before marking the plan representative. `fi_info`
   alone is not enough; the smoke must move bytes through the selected provider,
   endpoint type, domain, and private fabric path.
7. Score the rest by estimated copies, context switches, queue crossings,
   branch lag, result zipper locality, and lane balance.
8. Compile the winning graph into `ZC_PLAN_V1`.
9. Run a short probe with the exact lane map and check observed stats against
   the plan. If it misses badly, replan before claiming representative numbers.

The score should be biased toward boring locality:

```text
same lane -> same NIC card -> same queue group -> same CPU/cache group
          -> same WAL shard -> same result zipper owner
```

More lanes are not automatically better. The planner should increase lanes only
when it has CPUs, queues, memory credits, and result zipper capacity to keep
them runnable without scheduler churn.

## Data Plane Shape

The hot data plane should not parse policy. It should validate `plan_id`,
`placement_epoch`, `lane_id`, and sequence, then execute precompiled decisions:

```text
client block edge
  -> request descriptor lease
  -> lane-local WAL extent coalescer
  -> transport lane selected by plan
     tcp_mux: source-bound TCP 5-tuple
     libfabric: endpoint + polled CQ + registered MR region
  -> userspace fan branch descriptors
  -> terminal leaf writer
  -> leaf result log
  -> lane-local zipper
  -> upstream completion log
```

For mirror writes, branch records reference the same payload lease. They do not
copy payload per branch. For striped writes, branch records point at explicit
payload slices chosen by the userspace placement plan. For reads, result records
return descriptors and the zipper produces the ordered upstream view.

For parallel EFA or multi-NIC TCP, the plan must name branch ownership rather
than relying on a process-local default. Example: a two-leg mirror can map
branch 0 to `efa_0-rdm` and CPUs `0-31`, branch 1 to `efa_1-rdm` and CPUs
`96-127`, while both branches reference the same payload lease. A two-shard
stripe can use the same branch topology but each branch owns only its planned
lane set and payload slices. In both cases the emitted branch record must say
`block_device_raid_primitive=false`.

## Correctness Contract

Normal writes can be acknowledged individually once the selected placement
policy is committed. A sync or FUA request observes a global high-water mark:

```text
sync_epoch E is complete when every lane has emitted all writes <= E
according to the active placement policy.
```

Same-sector ordering is lane-local where possible and explicit where it crosses
lanes. Reads must consult the placement log for the active epoch; they must not
guess freshness from leaf socket order. Zippers merge monotonic result logs by
`(lane_id, sequence, segment_index)` with bounded O(1) windows, not global sort.

Blocking and io_uring leaf adapters share the same result-log contract. The
adapter may change the terminal submit mechanism, but it may not change
placement, ack, sync, read freshness, or backpressure semantics.

## Autotuning And Replanning

The planner should run in two loops:

- Cold plan: compile the initial `ZC_PLAN_V1` from discovered hardware and
  operator intent.
- Hot telemetry: consume `ZC_STATS_V1` and produce a new plan only at an epoch
  boundary.

Replan triggers:

- `send_zc_copied` is nonzero when zero copy was required.
- route source or interface does not match the planned private NIC.
- voluntary or involuntary context switches exceed the plan budget.
- a branch result log stays behind long enough to pressure payload credits.
- a lane group has idle CPUs while another group is saturated.
- a coalescer misses its byte target because age budget is too low.
- queue or ring overflow appears.

The first correction should change only one dimension: batch window, extent
bytes, lane count, zipper owner, or CPU map. Multi-variable retuning should be a
new plan epoch with a clear reason in the benchmark output.

## Benchmark Contract

Every representative run should print a plan digest:

```text
plan_id=...
intent=mirror-write/max_iops/zero_copy=required
lane_count=...
lane_map=lane:cpu:nicq:worker:wal_shard,...
branch_map=lane:leafs/replicas/stripe_segments,...
extent_bytes=... batch_window=... result_window=...
sync_contract=global_high_water ack_contract=...
zero_copy_observed=... copied_fallbacks=...
ctx_switches=vol/invol migrations=...
```

If hugetlb, memlock headroom, worker pinning, hctx affinity, batching, route
checks, or io_uring fast-path knobs are missing, the benchmark should warn
loudly and mark the result as a smoke run.

## Implementation Milestones

1. Add serializable `ZC_CAPS_V1`, `ZC_INTENT_V1`, `ZC_PLAN_V1`, `ZC_STATS_V1`,
   and `ZC_REPLAN_V1` structs with stable text and binary encodings.
2. Teach tcpmux and zcnblk WAL frames to carry `plan_id` and
   `placement_epoch` while preserving the existing lane header fields.
3. Build a local `zcplan` command that reads node caps JSON and emits a plan
   without launching data traffic.
4. Make `zcnblk-fan` consume a plan instead of ad hoc lane, batch, and CPU env
   knobs.
5. Move benchmark warnings to plan validation so bad topology fails before a
   misleading IOPS number is printed.
6. Add telemetry-driven replanning after the fixed-plan path is correct.
