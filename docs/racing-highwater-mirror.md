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

Each leg reports a contiguous durable high-water mark. The mirror high-water
mark is `min(local_contiguous_hwm, remote_contiguous_hwm)`. Local `fdatasync`
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

A syscall trace of two 32 KiB records exercised 2 `vmsplice` calls at the
source, 16 `tee` plus 48 `splice` calls at the first hop, and 32 `splice` calls
at the remote leaf. Both resulting 65,664-byte logs verified.

## Three-machine QEMU proof

Run:

```bash
scripts/zcracing-mirror-qemu-smoke.sh
```

The harness boots three concurrent KVM guests on a private TCP network. The
first-hop and remote guests each receive their own raw image as a terminal
virtio-blk device and mount ext4; the client receives no terminal device. It:

1. appends eight 4 KiB records;
2. delays every remote durable ACK by 50 ms and checks that mirror completion
   cannot outrun that leg;
3. truncates the remote test log to HWM 7 to model a lagging leg;
4. restarts all three userspace roles, zero-copy replays sequence 7 from the
   local log, and recovers mirror HWM 8;
5. appends four more records; and
6. independently grades all 12 records on both terminal devices.

Every guest prints the lane -> worker -> vCPU mapping. The harness loudly marks
the run as a functional QEMU correctness test, not a representative benchmark:
hugetlb, memlock, kthread/hctx affinity, raw RTT, batching, and io_uring fast
paths are not qualified.
