# Two-node zcnblk shared-WAL control

Date: 2026-07-11

Topology:

`/dev/zcnblk0 -> shared payload slots -> userspace WAL fan -> lane-local TCP -> userspace zcmem leaf`

Both nodes were `c8gn.16xlarge` Spot instances in `us-east-2c`. Bulk traffic
used private ENA addresses. No block device performed placement, mirroring, or
striping. The client, target, kernel kthread, and leaf CPU maps are captured in
each run's `topology.log`.

## Accepted evidence

- `client/client-order16lane-v14`: 64 concurrent same-sector ordering pairs
  passed read-before-write, write-before-read, and remote terminal sync checks.
- `client/client-mixed16lane-4m8-v15-repeat`: three 50/50 random 4K runs
  produced 875,599-887,854 acknowledged IOPS, 1.39% spread. The target drained
  600,413 descriptors/s and 18.32 Gbit/s to the real remote userspace leaf.
- `leaf/mixed16lane-4m8-v15-repeat`: 2,275,038 cold reads were returned in
  49,122 result batches (46.3 records/batch). Leaf workers incurred 333,567
  voluntary context switches.
- `raw-tcp32-portlane-v19`: 32 pinned port-lane TCP streams produced
  144.983 Gbit/s at the sender and 137.099 Gbit/s at the receiver. This is the
  transport control for the same hosts, not a storage result.

## Negative and invalid evidence

- `client/client-mixed16lane-fill50-min512-v16`: bounded coordinator-side fill
  reduced drain to 377,801 descriptors/s. Waiting there prevents completions
  that clients need before publishing more requests; do not retain this policy.
- `client/client-write16lane-4m-v2`: **invalid performance result**. The
  4,147,273 acknowledged IOPS used the generic target path and did not dispatch
  WAL payloads to the leaf. It has no remote-leaf summary and must never appear
  in an end-to-end leaderboard.
- `client/client-mixed16lane-160k-v11-globalwindow` and
  `client/client-mixed16lane-160k-v12-windowkick`: failed/deadlocked development
  runs superseded by the snapshot-window implementation.
- `raw-tcp16-v17` and `raw-tcp32-pinned-v18`: topology controls with unpinned or
  observed receive assignment. Their 39.6 and 74.7 Gbit/s receiver results show
  the cost of collapsing several flows onto the same workers.

## Architectural conclusion

The 18.32 Gbit/s WAL ceiling is not an ENA limit. The current fan serializes
admission through one global coordinator and permits only one synchronous
request/result batch in flight on each lane. The next implementation must let
lane-local ingress publish completions while persistent asynchronous transport
workers form and drain large extents. Same-sector dependencies remain local to
an ownership shard; only sync requires a global admitted/durable HWM barrier.
