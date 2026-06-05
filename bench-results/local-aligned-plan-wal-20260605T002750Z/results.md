# Local Aligned Plan WAL Fan Test

Topology:

`zcnblk-send` one client shard over 8 lanes -> `zcnblk-fan --engine wal --mode stripe` -> two terminal `zcdevnull` leaves.

No block device was used as a stripe or mirror primitive. Each run wrote 8 GiB total as 4 KiB records, with `URING_PLAY_ZCNBLK_BATCH_DEPTH=64` and `URING_PLAY_ZCNBLK_WAL_BATCH_WINDOW=64`.

CPU map for aligned runs:

- leaf0 workers: CPUs 0-7
- leaf1 workers: CPUs 8-15
- fan handlers: CPUs 16-23
- sender workers: CPUs 24-31

| case | fan Gbit/s | 4K frames/s | fan voluntary cs | fan involuntary cs | fan migrations | note |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| unpinned-no-plan | 43.873 | 1,338,892 | 97,102 | 738 | 793 | scheduler moved work heavily |
| pinned-no-plan | 61.949 | 1,890,544 | 26,672 | 192 | 8 | explicit CPU map |
| pinned-plan | 63.520 | 1,938,464 | 25,605 | 184 | 7 | same CPU map plus plan hash/epoch |

Takeaway:

Alignment clearly helped on this local fan path: about 41-45% more throughput and much lower context switching. The plan metadata itself did not introduce visible overhead; the small gain from pinned-no-plan to pinned-plan is within normal local benchmark noise. Its real value here is enforcing and logging the placement contract:

`plan_hash=0xe010360c8181275b placement_epoch=1001`

The run still emitted socket-buffer warnings because the kernel capped requested 16 MiB send/receive buffers at 8 MiB. Treat the numbers as a local topology signal, not a final network ceiling.
