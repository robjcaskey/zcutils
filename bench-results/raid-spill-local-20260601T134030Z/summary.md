# RAID/Spill Local Smoke

Local-only smoke run. No block device was used as a mirror, stripe, tier, or
spill primitive; terminal files were under `/dev/shm` and `/dev/null`.

## WAL Matrix

| segment | fanin | verify | fanout 4K IOPS | fanin 4K IOPS | effective 4K IOPS |
| --- | --- | --- | ---: | ---: | ---: |
| 64K | tree | none | 1,842,727 | 47,658,109 | 1,773,078 |
| 64K | primary | checksum | 1,647,482 | 5,828,666 | 1,283,752 |
| 384K | tree | none | 1,293,788 | 187,743,503 | 1,284,490 |
| 384K | primary | checksum | 1,337,176 | 4,839,742 | 1,047,276 |
| 1M | tree | none | 1,409,982 | 314,848,355 | 1,403,170 |
| 1M | primary | checksum | 1,318,751 | 4,830,791 | 1,035,404 |

## Direct Split/Merge

`zcraid-split --shape stripe(8,mirror(2))` over 512MiB and 1MiB chunks wrote
16 userspace leaves at 3335.22 MiB/s. `zcraid-merge` reconstructed the primary
lanes to `/dev/null` at 11538.41 MiB/s.

The run emitted branch/source CPU, voluntary/involuntary context switch, and
migration counters. Branch workers mostly stayed below five involuntary context
switches each; migration counts show this smoke was not CPU-pinned and is not a
representative topology result.

## Spill

`zctier` hot-only wrote 512MiB at 3635.51 MiB/s. Hot plus spill admission wrote
512MiB hot and 512MiB spill at 4079.44 MiB/s with a 3MiB queued high-water mark.
The high voluntary context switch count came from the `dd | zctier` pipe shape,
so the result is useful for correctness and counters, not final topology.
