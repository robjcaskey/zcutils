# Racing high-water mirror: three-host performance run

This note records the August 15, 2026 performance and regression run for the
userspace racing high-water mirror. It is a sequential WAL append benchmark,
not a block-device mirror benchmark. Userspace chooses both destinations before
the first hop writes to local terminal media or forwards to the remote
userspace leaf. `/dev/zcnblk0`, device mapper, MD, loop, and kernel placement
code are not involved.

## Topology and qualification

- Run id: `zc-rhwm-adhoc-i8g48-20260815T145118Z`
- Region/AZ: `us-east-2/us-east-2c`
- Hosts: three `i8g.48xlarge` ARM64 instances, 192 vCPUs and 1.5 TiB RAM each,
  in one cluster placement group
- Path: client -> first-hop userspace mirror -> remote userspace leaf, using
  private VPC TCP addresses
- Terminal media: the first hop and remote leaf each use their own local
  `Amazon EC2 NVMe Instance Storage` device, mounted as ext4; the client has no
  terminal device
- Lane mapping: lane 0 -> worker 0 -> CPU 100 on every host
- Depth: one lane and one worker; per-worker QD and aggregate outstanding depth
  both equal the reported window
- Locality: `ens142` and terminal NVMe are NUMA node 1. The worker is CPU 100,
  NIC IRQs are CPU 101, and the terminal NVMe managed IRQ serving the selected
  hctx is effective on CPU 99.
- Host preparation: 1,024 huge pages reserved, benchmark-shell memlock
  unlimited, irqbalance stopped, explicit process/IRQ placement, and idle-host
  noise snapshots captured
- Fast path: persistent pipes plus `vmsplice`/`tee`/`splice`; one persistent
  local sync worker; no io_uring in this prototype

This is representative of this explicit **single-lane** topology only. It does
not establish multi-lane scaling, random-read behavior, or saturation of the
instances' advertised network bandwidth. The hosts are ad hoc cloud instances,
so three repeats and their spread are reported rather than treating one sample
as canonical.

## Completion semantics and ceiling

Every result below is a remotely acknowledged write: the client completes a
window only after both the first-hop and remote terminal `fdatasync` operations
cover it. There is no early-local write acknowledgement and no read path in
this prototype, so neither is compared with the write ceiling. A window is one
durability batch, not a claim that individual records completed earlier.

Raw one-byte TCP echo RTT, pinned to CPU 100 with `TCP_NODELAY`, was measured
for 20,000 samples on each hop:

| hop | minimum | median | p99 | maximum |
| --- | ---: | ---: | ---: | ---: |
| client -> first hop | 33.904 us | 68.802 us | 80.429 us | 163.371 us |
| first hop -> remote leaf | 38.536 us | 40.282 us | 68.340 us | 129.722 us |

The conservative transport term is the sum of medians, 109.084 us. Matching
terminal baselines used fio with 32 KiB records, synchronous file writes, and
one `fdatasync` per reported window on both leaves. The lower of the two leaf
rates is used. The matching ceiling is:

```text
durable write ceiling = window /
    (window / slower_terminal_record_rate + two_hop_raw_TCP_RTT)
```

This deliberately models both-media durable acknowledgement rather than using
one network RTT denominator for every completion type.

## QD1 through QD128 curve

Each point appends 8,192 32 KiB records. Values are three-run mean and observed
range. Efficiency is actual IOPS divided by the matching durable-write ceiling.

| worker QD / aggregate depth | mean IOPS | observed range | mean MiB/s | durable ceiling IOPS | mean efficiency |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 2,902 | 2,606–3,144 | 90.7 | 4,892 | 59.3% |
| 2 | 3,807 | 3,787–3,826 | 119.0 | 11,806 | 32.2% |
| 4 | 6,205 | 6,078–6,358 | 193.9 | 22,133 | 28.0% |
| 8 | 9,142 | 8,947–9,263 | 285.7 | 35,398 | 25.8% |
| 16 | 12,511 | 12,044–13,103 | 390.9 | 49,137 | 25.5% |
| 32 | 15,593 | 15,384–15,716 | 487.3 | 63,059 | 24.7% |
| 64 | 17,647 | 17,474–17,924 | 551.5 | 69,546 | 25.4% |
| 128 | 19,801 | 19,183–20,134 | 618.8 | 75,842 | 26.1% |

The initial QD1 implementation incorrectly used `SPLICE_F_MORE` on the final
pipe-to-socket send before waiting for the ACK. That produced only 78–127 IOPS
because Linux delayed a send the process had promised to continue. Removing
the false hint restored 2.6–3.1k IOPS, a roughly 23–37x regression fix depending
on the paired sample. The post-fix table is the authoritative curve.

## High-depth saturation curve

The longer high-depth points append 65,536 32 KiB records so even the largest
window contains multiple durability barriers.

| worker QD / aggregate depth | mean IOPS | observed range | mean MiB/s | durable ceiling IOPS | mean efficiency |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 256 | 21,656 | 21,574–21,806 | 676.7 | 82,128 | 26.4% |
| 512 | 22,833 | 22,795–22,862 | 713.5 | 87,388 | 26.1% |
| 1,024 | 23,357 | 23,315–23,427 | 729.9 | 87,800 | 26.6% |
| 2,048 | 23,260 | 23,179–23,341 | 726.9 | 88,124 | 26.4% |
| 4,096 | 22,578 | 22,259–23,171 | 705.6 | 87,693 | 25.7% |

This single lane saturates around window 1,024 at about 23.4k durable 32 KiB
records/s or 730 MiB/s. Increasing the window to 2,048 does not improve the
mean, and 4,096 regresses slightly with more spread. The terminal-only baseline
plateaus around 88–89k records/s, so the current bottleneck is the serial
single-lane userspace/TCP pipeline rather than terminal durability alone.

## Copy and recovery evidence

A `strace -f -c` run of 1,024 records at window 32 counted exactly one `pipe2`
at the client, two at the first hop, and one at the remote leaf. The client made
1,024 `vmsplice` calls; the first hop made 1,035 `tee` and 3,105 `splice` calls;
the remote leaf made 2,866 `splice` calls. The log reports zero userspace
payload-copy bytes at both relay stages, and both terminal logs passed the
payload grader.

The final three-VM KVM regression used two independent virtio-blk/ext4 terminal
images behind userspace placement. It delayed the remote leg, truncated it to
the previous committed batch, restarted every userspace role, replayed the
missing suffix with splice, and independently graded both 12-record logs. It
passed with mirror HWM 12 and zero relay userspace payload-copy bytes. This is a
functional crash/recovery proof, not a performance number.

Raw evidence is under
`bench-results/zc-rhwm-adhoc-i8g48-20260815T145118Z/`, including per-repeat
logs, fio JSON, RTT summaries, topology/noise captures, syscall summaries, and
the pre- and post-fix curves.
