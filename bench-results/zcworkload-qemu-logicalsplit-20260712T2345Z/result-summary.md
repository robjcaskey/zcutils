# zcworkload Two-VM Logical-Split Proof

Status: **I/O FUNCTIONAL PASS; PERFORMANCE NONREPRESENTATIVE; HARNESS EXIT FAILED AFTER I/O**

## Topology

- Client VM `/dev/zcnblk0` block edge, one 4 KiB SHM transport lane.
- Client userspace `zcnblk-shm-target` WAL-TCP onramp.
- Target VM userspace `zcnblk-wal-leaf` with a `zcmem:128M` leaf.
- One real guest-to-guest TCP stream over virtio-net.
- No mirror, stripe, tier, spill, or terminal block device.
- Logical sizes: 4, 8, 16, 32, and 64 KiB.
- Queue maximum: 4 KiB; at most 16 queue fragments per logical operation.

Host vCPU maps are in `host-thread-map.log`. The KVM claim covered CPUs 4-13,
memory bandwidth, port 36201, and KVM with `honored=true`. Guest lane mapping:

```text
workload worker 0 -> client vCPU 1 -> host CPU 9
zcnblk kthread 0 -> client vCPU 3 -> host CPU 11
WAL onramp requested client vCPU 2 -> host CPU 10
leaf worker 0 -> target vCPU 1 -> host CPU 5
```

The onramp owner reported `cpu=unpinned`, so topology preflight correctly marked
positive runs nonrepresentative. Small-page buffers and zero guest hugetlb pages
were additional performance warnings.

## Functional Evidence

- 100,000 generated requests: 39,606 reads and 60,394 writes.
- Invalid alignment: 0; out of bounds: 0.
- All three synchronous and all three io_uring repeats completed.
- 3,072 logical operations became 8,274 queue requests.
- Onramp totals: 5,208 writes, 3,066 reads, and zero pending writes at shutdown.
- Leaf totals: 7,033 remote records, including 1,825 remote-read records; dirty
  reads were served by the onramp cache and therefore did not all reach the leaf.
- Strict-negative topology and io_uring fast-path checks failed before result
  output as intended.

The guest exited with status 127 only after all I/O and summaries because the
minimal initramfs has BusyBox `rmmod` but no `/sbin/rmmod` applet link. QEMU and
the advisory lease were still cleaned up by verified PID and token. Attempt 1,
which exposed the incorrect module sysfs lookup, is retained under `attempt1/`.

## Diagnostic Performance

| Engine | Depth | Logical IOPS spread | Logical MiB/s spread |
|---|---:|---:|---:|
| synchronous | 1 | 63 / 69 / 83 | 0.67 / 0.72 / 0.88 |
| io_uring fixed | 4 | 114 / 124 / 125 | 1.19 / 1.31 / 1.31 |

These are not baselines. Synchronous reads averaged 30.6-40.3 ms while writes
were mostly tens of microseconds. The leaf's measured receive, decode, submit,
and result-write phases totaled about 0.13 seconds across a 54.95-second active
window, locating the dominant delay before or between leaf transactions rather
than in memory media work.

Workload-thread context switching:

- synchronous: exactly 1,000 voluntary switches per 1,000 logical operations;
- io_uring: 297-309 total switches per 1,000 logical operations;
- zcnblk transport kthread: +675 voluntary, +0 involuntary;
- WAL onramp process/thread: +1,527 total switches, including +467 involuntary.

Target leaf stream: 2,980 voluntary switches, zero involuntary, zero migrations.
The client and target QEMU processes recorded 35,937 and 32,183 voluntary
switches respectively over the full boot/workload/shutdown interval.

## Next Investigation

Isolate the low-depth read transaction into four timed stages: block edge to SHM
publication, onramp wake and batch formation, WAL-TCP request/result, and SHM
completion kick. The leaf phase accounting rules out backend memory copies as
the source of the roughly 40 ms median read stall. Do not spend an EC2 cycle
until that local stage timer identifies and removes the wait/wake delay.
