Local zcnblk WAL fan alignment smoke.
Topology: zcnblk-send one client shard over 8 lanes -> zcnblk-fan --engine wal stripe -> 2x zcdevnull terminal leaves.
No block device is used as a stripe/mirror primitive; leaves are terminal devnull media.
Variants:
  unpinned-no-plan: no CPU pinning, no plan hash/epoch.
  pinned-no-plan: leaf0 CPUs 0-7, leaf1 CPUs 8-15, fan CPUs 16-23, sender CPUs 24-31.
  pinned-plan: same CPU mapping plus URING_PLAY_ZC_PLAN_ID and URING_PLAY_ZC_PLACEMENT_EPOCH.
