# RAID Primitives TLDR
`/dev/zcnblk0` is the client block edge; it does not own RAID policy.
TCP mux carries lane-preserving block frames into userspace gateway code.
Mirroring is userspace fanout: copy one logical write to replica branches.
Striping is userspace mapping: route logical offsets to stripe members.
Never use a block device to implement mirroring or striping.
Stripe lanes should be dedicated so lane N stays close to shard/device N.
Spill is userspace tiering: write hot first, then queue bounded cold copies.
Backpressure belongs at the mirror, stripe, and spill queues, not in blk-mq.
Target block devices such as `/dev/zcbrdN` are terminal leaf media only.
fio should see one `/dev/zcnblk0`; topology is proven by target/gateway logs.
The fast path is: zcnblk -> TCP mux -> userspace RAID primitive -> leaf writer.

## Multi-Hop Shape

```text
fio/db/fs
  -> /dev/zcnblk0
  -> TCP mux ingress lanes
  -> userspace mirror primitive
       -> TCP mux replica-a lanes
       -> userspace stripe primitive
            -> TCP mux shard-a0 lane group -> userspace leaf writer -> /dev/zcbrd0
            -> TCP mux shard-a1 lane group -> userspace leaf writer -> /dev/zcbrd1
            -> TCP mux shard-a2 lane group -> userspace leaf writer -> /dev/zcbrd2
            -> TCP mux shard-a3 lane group -> userspace leaf writer -> /dev/zcbrd3
       -> TCP mux replica-b lanes
       -> userspace stripe+spill primitive
            -> TCP mux shard-b0 lane group -> userspace leaf writer -> hot /dev/zcbrd4 -> spill queue
            -> TCP mux shard-b1 lane group -> userspace leaf writer -> hot /dev/zcbrd5 -> spill queue
            -> TCP mux shard-b2 lane group -> userspace leaf writer -> hot /dev/zcbrd6 -> spill queue
            -> TCP mux shard-b3 lane group -> userspace leaf writer -> hot /dev/zcbrd7 -> spill queue
```

Each primitive consumes and emits lane-aware frames. A hop can be local loopback
for testing or a real host boundary later, but the rule is the same: TCP mux
preserves lanes between userspace RAID stages, and block devices only appear at
the leaves. `/dev/zcbrdN`, `/dev/nullbN`, `/dev/ramN`, dm, md, loop, and custom
block modules must not be used to perform mirror or stripe decisions.
