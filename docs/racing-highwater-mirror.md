# Racing high-water userspace mirror

`zcracing-mirror` is a three-role correctness implementation of a userspace
mirror stage. It does not use a block device, device mapper, MD, loop, or the
zcnblk client edge as a mirror primitive. Block devices appear only as terminal
media, behind a userspace leaf writer, after placement has been decided.

The tested topology is:

```text
client VM
  TCP framed log
    -> first-hop userspace mirror VM
         -> splice -> local append log -> fdatasync -> terminal ext4 device
         -> tee/splice -> TCP -> remote userspace leaf VM
                                      -> splice -> append log -> fdatasync
                                      -> terminal ext4 device
```

## Completion and recovery contract

Each leg reports a contiguous durable high-water mark. The client may submit a
bounded window of records; the final record marks the batch barrier, so both
terminal `fdatasync` operations cover the whole prefix with one drain per leg.
The mirror high-water mark is
`min(local_contiguous_hwm, remote_contiguous_hwm)`. Local `fdatasync`
and the remote durable ACK race concurrently, but a fast leg never permits an
early client ACK. A missing sequence holds the relevant leg at the hole.

The terminal format is an append-only sequence of fixed 64-byte metadata
headers and payloads. On restart, each leaf scans the framed prefix, truncates a
torn header or payload tail, and reports the recovered next sequence in a
handshake. If the remote leg is behind, the first hop splices only the missing
local-log suffix to it before admitting a client. It fails closed if the remote
leg is ahead because this topology has no reverse read path. A restarted client
resumes at the repaired sequence.

There is deliberately no payload CRC in this hot path. Framing and sequence
numbers detect structural/torn-tail errors; protection against undetected
complete-record corruption must be supplied by the topology's declared memory,
transport, and terminal-media integrity contract. The `verify` command's
deterministic payload grading is a test oracle, not a general-data checksum.

## Copy ledger

- The synthetic source creates each payload once, then `vmsplice`s it to the
  client socket.
- The first hop reads only metadata headers. Payload moves socket -> pipe,
  `tee`s the pipe-buffer references, then splices the two legs to the local log
  and remote socket. It has zero userspace payload copies.
- The remote leaf splices socket -> pipe -> terminal log. It has zero userspace
  payload copies.
- The unavoidable boundaries remain source creation, small protocol metadata,
  network/device DMA, and any internal copying performed by the kernel or
  device. The claim is specifically zero userspace payload copies at the
  mirror and remote leaf, not zero physical copies everywhere.

A syscall trace of 1,024 32 KiB records in 32-record windows exercised one
source pipe, two first-hop pipes, and one remote-leaf pipe for the entire run.
The source made 1,024 `vmsplice` calls; the first hop used 1,035 `tee` and 3,105
`splice` calls; the remote leaf used 2,866 `splice` calls. Both resulting logs
were independently graded. The small `read` and `write` calls in the trace are
protocol metadata, not payload.

Pipe capacity is only an upper bound on each transfer. Linux can exhaust pipe
buffer slots before the nominal byte capacity, so each receive-side splice
accepts the progress the kernel returns and drains it immediately. Conversely,
the final pipe-to-socket splice deliberately omits `SPLICE_F_MORE`; incorrectly
promising another send caused a severe low-depth TCP latency regression on the
tested ARM64 kernel.

## Three-machine QEMU proof

Run:

```bash
scripts/zcracing-mirror-qemu-smoke.sh
```

The harness boots three concurrent KVM guests on a private TCP network. The
first-hop and remote guests each receive their own raw image as a terminal
virtio-blk device and mount ext4; the client receives no terminal device. It:

1. appends eight 4 KiB records in two four-record windows;
2. delays every remote durable ACK by 50 ms and checks that mirror completion
   cannot outrun that leg;
3. truncates the remote test log to the previous committed batch at HWM 4;
4. restarts all three userspace roles, zero-copy replays sequences 4–7 from the
   local log, and recovers mirror HWM 8;
5. appends four more records; and
6. independently grades all 12 records on both terminal devices.

Every guest prints the lane -> worker -> vCPU mapping. The harness loudly marks
the run as a functional QEMU correctness test, not a representative benchmark:
hugetlb, memlock, kthread/hctx affinity, raw RTT, and io_uring fast paths are
not qualified. The four-record durability batching is exercised but is not a
performance result.

The representative three-host single-lane performance run is recorded in
[`racing-highwater-mirror-adhoc-20260815.md`](racing-highwater-mirror-adhoc-20260815.md).
