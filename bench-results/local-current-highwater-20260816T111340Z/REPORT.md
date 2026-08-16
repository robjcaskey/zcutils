# Current-HEAD scalable high-water mirror investigation

Date: 2026-08-16.  Revision under test: `bac96223`.  This is a shared-host
diagnostic, not a representative cloud result.

## Conclusion

The recent 48,487-operation result is not evidence that the scalable mirror
path fell from millions of IOPS.  It measured `zcracing-mirror`, a deliberately
single-lane, blocking recovery/conformance prototype, with 32 KiB frames across
three small TCP hosts.  The production-oriented `zcraid-mirror` path is a
different implementation: it has lane-local workers, vectored extent batches,
TCP/OFI/RDMA transports, two userspace branches, and terminal-sync high-water
ACK windows.

The immediately preceding C8gn isolation run already exercised that scalable
path.  Sixteen lanes on two `c8gn.16xlarge` hosts reached 4.210M logical 4 KiB
records/s with two volatile userspace terminal branches and all-branch HWM ACKs
every 64 64-KiB extents.  Native EFA delivery-complete reached 177.1 Gbit/s.
Removing the RAM terminals and syncing two journals on root gp3 reduced the
same architecture to tens of thousands of records/s, isolating the persistent
terminal rather than transport or mirror coordination as the bottleneck.  See
`../zc-mirror-isolate-c8gn16-20260815T1623Z/REPORT.md`.

The larger block-edge run on two `c8gn.48xlarge` hosts remains a distinct read
result: 5.064M remote 4 KiB random-read IOPS through `/dev/zcnblk0` and EFA RMA.
It does not supply mirrored-write durability semantics.  See
`../zcutils-rmadirect-dualcard-adhoc-c8gn48-20260813T0059Z/REPORT.md`.

## Current-HEAD gate

Current `main` was rebuilt in release mode and run three times through the
scalable TCP mirror on a 16-physical-core shared workstation.  Four sender
workers used CPUs 0-3, branch-0 terminal workers used CPUs 4-7, and branch-1
terminal workers used CPUs 8-11.  Placement remained in the separate userspace
RAID stage.  Each run sent 65,536 logical 4 KiB records as 4,096 64-KiB extents,
with per-lane ACK window 64, and committed only after both volatile `zcmem`
terminal HWMs covered the window.

| repeat | logical 4K records/s | logical payload | aggregate branch wire |
| ---: | ---: | ---: | ---: |
| 1 | 1,414,062 | 46.336 Gbit/s | 92.853 Gbit/s |
| 2 | 1,430,645 | 46.879 Gbit/s | 93.942 Gbit/s |
| 3 | 1,447,168 | 47.421 Gbit/s | 95.027 Gbit/s |

Mean was 1,430,625 logical 4K records/s; min-max spread was 2.31%.  Every sender
and receiver exited zero.  Each branch reported 4,096 terminal writes, 64
terminal sync windows, and 4,096 HWM ACKs.  The host clamped the requested
16-MiB TCP buffers to 8 MiB, so these numbers are diagnostic only.

Targeted tests also passed:

- four `zcraid_mirror` topology/RDMA protocol tests;
- four `racing_mirror` framing, HWM, and recovery-scanner tests.

## Revision audit

The scalable TCP path changed after the first durable C8gn qualification only
to fix the discovered IOV limit deadlock: a durability window larger than
`IOV_MAX / 2` is emitted as multiple vectored transport chunks, but ACK waiting
still begins only after the full durability window is sent.  The later tier
change adds admission/drain telemetry and does not alter `zcraid-mirror` data
or completion handling.  The current-HEAD run above exercises the fixed path.

## Architectural implication

Use `zcraid-mirror` as the high-rate data plane.  Port the independently proven
restart scan, missing-suffix replay, and lag recovery rules from
`zcracing-mirror` into lane-local extent/HWM state; do not scale the single-lane
prototype itself.  Keep the following measurements separate:

1. early local WAL admission;
2. remote volatile all-branch HWM;
3. remote persistent journal HWM;
4. base-image replay/application.

No new C8gn instances were rented for this audit because the semantics-matched
C8gn evidence was already present from the preceding run and the current-HEAD
shared-host gate reproduced multi-million-scale execution.
