# Real-World Speed Design Goals

This project is aiming for a Raft transport and WAL path that is fast on real
machines, not only on loopback or a synthetic simulator. The useful benchmark is
the full commit path: receive client work, place or persist it, replicate to
followers, observe quorum, and send the reply with predictable latency.

## Goals

- Sustain high aggregate bandwidth across 100G, 200G, 400G, and later wider
  NICs by spreading traffic across many independent flows, queues, rings, and
  cores.
- Keep the baseline deployable in plain userspace with TCP multiplexing, then
  add faster paths for libfabric/EFA/RDMA and Linux 7 io_uring features where
  they are actually available.
- Preserve locality from flow to RX queue to worker CPU to io_uring ring to WAL
  region. Steering should be negotiated and measured, not assumed.
- Use zero-copy receive and transmit only when counters prove it is active.
  ZCRX, registered buffers, fixed files, io slots, and send-zc are wins only
  when they reduce real memory traffic and do not destabilize the host.
- Make batching explicit. Raft should batch entries, quorum acknowledgements,
  and WAL work without hiding backpressure or producing tail-latency cliffs.
- Keep fallback paths first-class. TCP mux over public IP must remain viable for
  inter-region, non-RDMA, and restricted container deployments.

## Transport Shape

The default production path is TCP mux: one logical peer connection is striped
over many ports and source ports to create enough distinct 5-tuples for NIC,
cloud, and kernel steering. The target is to keep each flow modest while the
aggregate saturates the node.

The fabric path should start with libfabric rather than direct driver APIs.
Providers such as `efa`, `verbs`, `tcp`, `sockets`, and `shm` let us test the
same transport abstraction across AWS EFA, ConnectX, and fallback environments.
Direct verbs or mlx5-specific APIs can be considered later only if libfabric is
measurably blocking the target.

## Kernel And io_uring Use

Linux 7-era io_uring APIs are part of the design:

- ZCRX for receive-side zero copy where NIC or simulator support exists.
- send-zc for transmit-side copy reduction, guarded on kernels where host
  crashes were observed.
- registered buffers, fixed files, and io slots for WAL and network buffers.
- NAPI registration and busy poll to reduce scheduling noise.
- ring resize, buffer cloning, and future slot APIs when they are stable enough
  to justify the operational risk.

Current TCP WAL defaults:

- `tcp-wal-mux-server` uses fixed-file receives by default
  (`URING_PLAY_TCP_WAL_FIXED_RECV=1`) so accepted socket fds are registered in
  each worker ring and `IORING_OP_RECV` skips per-SQE fd table lookup.
- The WAL write default remains `WRITE_FIXED` plus registered file
  (`URING_PLAY_TCP_WAL_WRITE_MODE=fixed-file`) for small 4K chunks. The current
  slot-RW prototype is useful for persistent DMA experiments, but it is slower
  for this 4K TCP WAL shape.
- `tcp-bench-uring-mux-send` has fixed-file send support
  (`URING_PLAY_TCP_SEND_FIXED_FILE=1`), but it defaults off because the local
  loopback traffic generator did not improve with registered socket fds.

eBPF is useful for filtering, steering, telemetry, and possibly cheap protocol
classification. It should not own Raft consensus state. The state machine stays
in userspace or a deliberate kernel module boundary where it can be tested,
versioned, and recovered cleanly.

## What netdevsim Proves

The patched netdevsim module is a lab tool. It can prove that:

- RX queues are actually being exercised.
- 5-tuple or destination-port steering maps lanes to queues.
- ZCRX counters move when receive zero copy is active.
- The NSRD RDMA simulator parses write packets and accounts bytes/drops.

It does not prove that a real NIC DMA engine, PCIe, firmware, interrupt
moderation, NUMA topology, or cloud fabric behaves the same. Passing netdevsim
means the software plumbing is ready for real NIC tests.

## Benchmark Rules

Every serious result should report:

- client and server throughput;
- batch rate and commit latency distribution when Raft is in the path;
- CPU affinity, NUMA node, worker migrations, and context switches;
- RX/TX queue usage and active queue count;
- zero-copy counters, not just command-line mode flags;
- drops, retransmits, time-squeeze, and softirq distribution;
- exact kernel, module, and io_uring feature set.

Benchmarks that only win by using loopback, discarding correctness, or hiding
copies are not production evidence.

## Current Bare-Metal Smoke Results

On May 23, 2026, the plain userspace loopback-to-raw-WAL smoke on the approved
test partition showed:

- Same-core TCP mux, 16 lanes, 16 workers, 4K chunks, fixed-file WAL writes,
  fixed-file receives, 4 GiB total: 80.96 Gbit/s server-side and about 2.47M
  chunks/sec.
- The same 16-lane shape with a local sender pinned to SMT siblings was
  slightly worse, so the default local generator remains unpinned.
- 32 lanes/32 workers was much worse on this 16-core/32-thread host because it
  spills onto SMT siblings and contends with itself.
- Split RX/WAL workers worked with fixed-file receive, but was slower than the
  same-core path for this small 4K chunk test.

These are loopback and raw-block WAL numbers. They prove the userspace
io_uring/WAL plumbing can exceed 80 Gbit/s locally, not that a real NIC path is
already at that rate.
