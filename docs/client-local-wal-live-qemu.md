# Client-local WAL custody: live QEMU test

This is an **opt-in, single-writer, single-lane TCP implementation** of client-local
durable custody and live serial-mirror replacement. It uses the existing block
edge, fan-WAL volume protocol, client custody journal, persistent terminal writer,
and topology evolution state machine. Default direct TCP/RDMA sessions do not
enter this optional stage; their hardware performance still needs remeasurement
before making a no-regression claim.

Run both durability races locally:

```sh
CLIENT_WAL_LIVE_SLOW=middle bash scripts/client-wal-live-qemu.sh
CLIENT_WAL_LIVE_SLOW=tail bash scripts/client-wal-live-qemu.sh
```

The default frontend hands the block edge's shared-arena pages directly to the
co-located userspace custody stage. For a separate-process onramp regression test,
set `CLIENT_WAL_LIVE_FRONTEND=tcp-onramp`; that variant intentionally adds a local
TCP connection and its kernel copies.

The script prints the artifact directory under `/mnt/bulk_data/zcutils-qemu/`.
It needs KVM, QEMU, BusyBox, the running kernel's modules and a matching
`kmods/zcnblk_client_mod.ko`, plus passwordless permission to manage its own
temporary TAP interfaces and bridge. It uses the local resource coordinator.
It does not allocate cloud resources, load storage modules on the host, or write
to an existing host block device. New image files are terminal media only.

## What the test actually does

The initial three VMs are a client C, middle storage node M, and final storage
node S. M forwards the same receive-arena payload to S while a separate worker
appends it to M's persistent WAL. Neither durability receipt waits for the other
disk. A new replacement VM R is provisioned after M is lost.

```text
healthy:    /dev/zcnblk0 -> userspace client WAL -> M -> S
degraded:   /dev/zcnblk0 -> userspace client WAL ------> S
rebuilt:    /dev/zcnblk0 -> userspace client WAL -> R -> S
```

All placement, mirror membership, retained reads, fencing and reconstruction
live in userspace. `/dev/zcnblk0` remains only the client edge. No dm, md, loop,
RAM block module or other block-device mirror primitive is involved.

The first 16 writes warm up both copies. The selected slow node then deliberately
delays each terminal commit by 100 ms. The test requires an early winning receipt
**before** failure: client WAL + third node when M is slow, or client WAL + middle
node when S is slow. The lagging copy is genuinely still pending.

A continuously running workload uses one open `/dev/zcnblk0` descriptor, changes
32 different 4K blocks repeatedly, checks reads, calls fsync every eight writes,
and issues `RWF_DSYNC` writes every four iterations. The host harness kills M's
exact QEMU PID without a graceful shutdown. It never restarts the workload,
reconnects the block edge, reloads its module, or resets the volume.

The Rust client stage:

1. Persists its admitted local prefix and obtains a persisted new generation at
   S. S rejects old-generation requests and retires old sockets; closing the
   client's old socket alone is not treated as fencing.
2. Replays missing retained records to S, then continues foreground I/O using
   the policy-approved client-WAL + S pair.
3. Stages a replacement through `EvolutionController`. The QEMU harness acts
   only as the provisioning worker: it boots the preapproved replacement with
   empty media after observing that placement request.
4. Copies the full surviving image directly from S to R in the background while
   the client continues writing. All post-copy-start WAL records remain pinned.
5. Briefly fences new admissions, reduces the retained suffix to the last
   version of each changed 4K page, and sends those file extents with `sendfile`.
   R appends scatter-page batches to its existing persistent WAL and commits the
   exact fenced high-water mark before activation.
6. Activates the reconstructed replica and resumes serial C -> R -> S routing
   on the same block-edge session.

After further writes on the new route, the workload verifies the entire 1 MiB
volume, including unwritten ranges. Both remote services are stopped and their
persistent WAL/base files reopened independently. Each full-volume SHA-256 must
equal the workload's independently generated expected hash. The final local
journal release must follow both remote durable receipts, not an early ACK.

## Completion and retention contracts

Ordinary block writeback completion is not an fsync promise. The existing block
edge may acknowledge volatile admission. Its fsync/FUA drain reaches the custody
stage, which requires its **actually synced local WAL and an eligible remote
durable prefix**, or both remote durable prefixes. A TCP send or receive, an
RDMA CQ completion, and an unsynced memory buffer are not durable receipts.

The client journal is a bounded retained suffix, **not a full-volume replica**.
Early ACK does not permit reclamation: both remote copies must attest custody
before the local suffix is released. While degraded, previously reclaimed base
data may have only the surviving remote copy, just as during normal mirror
resilvering. The client cannot reconstruct that base if both full remote copies
are lost. New acknowledged writes have the eligible local-plus-remote custody
pair; full-volume remote redundancy is restored only after reconstruction.

Reads use the retained local WAL overlay where the remote image can be behind.
After reclamation, reads can use the surviving full image. The final catch-up
coalesces **current state**, not historical versions: this is not a PITR-history
replication implementation. A partially patched replacement remains staged and
uncountable, including after restart.

Buffers are bounded. If a lagging replica/rebuild cannot keep up and retained
custody fills, writes backpressure rather than evict acknowledged data. The
bounded middle-node forwarding/terminal queues can also eventually backpressure
a sustained slow leg; early ACK is not infinite buffering.

## Evidence and limits

`CLIENT_WAL_LIVE_QEMU_PASS` requires stable-descriptor continuity, fsync/FUA
activity, the requested pre-failure race winner, foreground writes during
replacement, and matching independently reopened remote images. Guest serial
logs record high-water marks, retained reads, replacement activation and catch-up
time. The harness checks kernel diagnostics and cleans up its owned VMs and
network interfaces on success or failure. Disk images and logs are retained for
inspection.

These are shared-host, debug-build correctness tests with intentionally delayed
disks, a three-second provisioning delay, and a throttled base copy. They are
**not representative IOPS or latency benchmarks**. VM failure domains emulate
separate hosts; they do not provide independent physical-host/power-loss domains.
The same-descriptor guarantee means no client reconnect or I/O error, not zero
pause: failure detection defaults to 1.5 seconds in the fixture, survivor catch-up
can add time, and final replica activation has a measured short admission fence.

The default shared-arena frontend borrows the block edge's existing payload
lease for local journal `pwritev` and remote TX. Reads fill the final shared page
directly from retained WAL or the surviving remote. There is no local TCP socket,
intermediate userspace payload buffer, or kernel placement logic. Only the small
control/completion metadata is copied. Replay uses file extents, and base-copy
and remote-read buffers are reused. **Remote TCP kernel copies remain**: this is
not an end-to-end zero-copy/RDMA performance claim. Multi-lane/group-commit
performance and live RDMA failover remain to be validated.

The explicit opt-in for `zcnblk-shm-target ... wal-tcp ...` is:

```sh
export URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=custody-tcp
export URING_PLAY_ZCNBLK_SHM_CLIENT_WAL_CONFIG=/path/to/client.json
export URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE=blocking
```

The JSON is the same policy/terminal configuration as the socket adapter. Its
`listen` address is unused in shared-arena mode. This initial implementation
accepts one lane and does not negotiate RMA or atomic multi-request semantics.
Custody processing, including replica activation, advances on I/O or the final
drain; idle control-plane reconciliation is not yet a background service.

The adapter refuses operation unless `allow_plaintext_private_network` is
explicitly enabled. Its identity/scope checks are fencing, **not authentication**;
do not expose it to untrusted networks. The local topology store exercises the
existing committed state machine, not a distributed Raft quorum. The fixture
provides one preapproved replacement. Restarting the client stage with existing
custody intentionally refuses to reset/adopt it without recovery reconciliation;
terminal restart and stale-generation rejection are covered by unit tests.

Entry points are `zcutils zcnblk-wal-custody client|peer|inspect CONFIG.json`.
See [live client configuration](../tests/fixtures/client-wal/live-client.json),
[client stage](../src/wal_custody.rs), [terminal/forwarder](../src/wal_custody_peer.rs),
and [QEMU harness](../scripts/client-wal-live-qemu.sh). The paused multi-region
tutorial drafts are not part of this implementation.
