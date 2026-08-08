# c8gn Dummy WAL Stream Target Ceiling

Run ID: `zc-sync-adhoc-c8gn48-20260606T162227Z`

Topology:
- sender node0: `ens68` `172.31.36.180`, `ens146` `172.31.34.52`
- receiver node1: `ens68` `172.31.38.28`, `ens146` `172.31.34.119`
- branch-local source binding used `URING_PLAY_SOURCE_IP` per sender process
- no block device was used as a mirror/stripe primitive
- this is a synthetic target-drain ceiling: premanufactured WAL payloads to dummy remote WAL receivers

Results use 384 KiB WAL extents, ACK enabled, 384 MiB per lane.

| variant | lanes per NIC | framing/path | ens68 drain | ens146 drain | aggregate drain | aggregate 4K IOPS | note |
| --- | ---: | --- | ---: | ---: | ---: | ---: | --- |
| `wal-ack` | 32 | extent/blocking | 144.0 Gbit/s | 182.9 Gbit/s | 326.9 Gbit/s | 10.0M | validates much more headroom than coupled mirror sender |
| `stream-uring` | 32 | stream/io_uring | 133.7 Gbit/s | 138.9 Gbit/s | 272.5 Gbit/s | 8.3M | lower sender context switches, lower throughput |
| `wal64-ack` | 64 | extent/blocking | 245.7 Gbit/s | 254.9 Gbit/s | 500.6 Gbit/s | 15.28M | best observed dummy target-drain ceiling |
| `wal96-ack` | 96 | extent/blocking | 240.8 Gbit/s | 233.7 Gbit/s | 474.5 Gbit/s | 14.48M | too many lanes for this topology |

Takeaway:

The remote target-drain ceiling is not the 3.0-3.3M IOPS seen in the coupled mirror sender. With branch-local premanufactured WAL streams, the same two c8gn nodes reached about 500 Gbit/s and 15.3M 4K-equivalent IOPS. The current mirror implementation loses the headroom in the coupled sender/HWM structure, not in basic remote receive capacity.

The next architecture step should keep the 64-lane branch-local stream shape, then add a small RAID/HWM zipper beside it: branch-local sender workers, target-drain ACK/result logs, and one coordinator that computes mirror commit high-water marks without serializing payload sends branch0-then-branch1 inside each lane worker.
