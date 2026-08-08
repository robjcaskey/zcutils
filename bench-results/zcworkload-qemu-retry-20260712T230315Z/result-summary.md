# zcworkload two-VM KVM retry

Status: **FUNCTIONAL FAILURE; no performance result accepted**

## Topology

- Client VM: `/dev/zcnblk0` client block edge only.
- Client VM userspace stage: `zcnblk-shm-target ... wal-tcp`, after the block edge, no placement.
- Target VM: one `zcnblk-wal-leaf` process with one `zcmem:128M` backend.
- WAL-TCP frame size: 4096 bytes end to end.
- Logical workload sizes sampled: 4096, 8192, 16384, 32768, and 65536 bytes.
- No RAID, mirror, stripe, tier, spill, dm, md, loop, terminal block media, `zcbrd`, `nullb`, or `ramN` stage was used.
- The initramfs explicitly omitted `null_blk.ko`, `zcbrd_mod.ko`, and stripe modules.

Host map:

- Target VM vCPU 0/1/2/3 -> host CPU 4/5/6/7; QEMU emulator and auxiliary threads -> host CPU 12.
- Client VM vCPU 0/1/2/3 -> host CPU 8/9/10/11; QEMU emulator and auxiliary threads -> host CPU 13.

Guest map:

- Lane 0 workload worker -> client guest CPU 1.
- Lane 0 `zcnblk-shm-0-0` kernel transport kthread -> configured client guest CPU 3.
- Client WAL onramp -> configured client guest CPU 2. The onramp startup log reports `cpu_list=2`, but its owner-worker line reports `cpu=unpinned`; this is a topology warning.
- Lane 0 target WAL leaf worker -> target guest CPU 1, `affinity_applied=true` in the leaf log.
- `/dev/zcnblk0` hctx 0 CPU list was `0, 1, 2, 3`; `hctx_affinity=Y` was configured.

## Coordination

Both attempts used a soft-exclusive `agent-coord` claim for
`cpu=4-13;memory-bandwidth=*;port=36200;kvm=*`. Both claims were acquired with
`honored=false` because a concurrent shared EC2 orchestration claim included
`memory-bandwidth=*`. These runs are functional shared-system measurements and
are not representative.

## Results

Attempt 1 reached the target leaf but the client module load failed because a
stale module had vermagic `7.0.8-io-slots` while the guest kernel was
`7.0.8-io-slots-03`. Cleanup completed using the verified target QEMU pidfile.

The module was then force-rebuilt against the exact kernel tree and gated on:

```
vermagic=7.0.8-io-slots-03 SMP preempt mod_unload modversions
```

Attempt 2 established the real guest-to-guest WAL-TCP connection and passed
sample validation:

- Requests: 100000
- Reads/writes: 39606 / 60394
- Invalid alignment / out of bounds: 0 / 0
- Logical-size counts, 4/8/16/32/64 KiB: 15243 / 70983 / 7578 / 3065 / 3131
- Raw guest-to-guest ICMP RTT min/avg/max: 0.198 / 0.269 / 0.583 ms
- RTT supplied to planned low-depth runs: 269000 ns

The first strict-negative command then failed before topology preflight:

```
chunk-bytes=65536 exceeds block queue max_hw_sectors_kb=4
```

Current immutable product source sets `lim.max_hw_sectors` from
`max_frame_bytes`. The mandated `max_frame_bytes=4096` therefore advertises a
4 KiB maximum block request, while `zcworkload` validates its fixed 64 KiB
logical maximum before preflight or I/O. No product source was edited.

Consequences:

- Sample validation: PASS.
- Strict-negative unmapped test: BLOCKED before topology preflight; no result printed.
- Strict-negative io_uring fast-path test: NOT RUN after the functional blocker.
- Sync mixed run: NOT RUN.
- io_uring mixed run: NOT RUN.
- Latency, completion-efficiency, and workload context-switch numbers: unavailable; no fabricated or unverified performance claim is made.

## Context Switches

Attempt 2 host QEMU totals from `/usr/bin/time -v`:

- Client QEMU: 11665 voluntary, 369 involuntary over 35.53 seconds.
- Target QEMU: 27236 voluntary, 490 involuntary over 96.20 seconds. This includes the post-failure wait and PID cleanup, so it is not a workload metric.

Guest pre-run snapshot:

- `zcnblk-shm-0-0`: 3 voluntary, 0 involuntary.
- `zcnblk-shm-targ`: 13 voluntary, 2 involuntary.

Per-QEMU-thread before/after snapshots are in
`host-thread-context-before.log` and `host-thread-context-after-client.log`.

## Cleanup

- Attempt 1 lease `l18c1ad3acce36094-1e1aa4`: released.
- Attempt 2 lease `l18c1add6dd37857c-1f09e2`: released.
- Client VMs powered off after their guest status.
- Target QEMU processes were inspected by pidfile, command line, executable name, and initramfs path before SIGTERM.
- No process-pattern cleanup command was used.
- QEMU socket port 36200 and all benchmark claims were released.

Exact executed launch commands are preserved in `host-run.sh`; exact guest
commands, including the unexecuted commands after the blocker, are preserved
in `rootfs/init`. Console, module build, cpio, SHA-256, timing, thread-map, and
cleanup evidence is retained in this directory.
