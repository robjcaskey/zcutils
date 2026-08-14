# `/tmp/grumps` implementation audit

The handoff file was last modified on 2026-08-10 at 21:31 EDT. The repository base used for the block/RDMA work is later commit `1913baabf736da77d08e3d561036487486830cc6`, committed on 2026-08-11 at 01:32 EDT with subject `transport: remove OFI queue serialization`. That commit is the implementation pass requested by the handoff: 6,359 additions and 1,079 deletions across the OFI shim, Rust queue users, mirror/relay paths, matrix harness, and documentation.

## Item-by-item status

| Grumps item | Status in `1913baab` and the current tree |
|---|---|
| 1. Batched, type-aware CQ dispatch | Implemented. `zc_ofi_op` embeds stable `fi_context2`; persistent SEND/RECV/READ/WRITE rings retain slots until CQ retirement. `zc_ofi_dispatch_cq` reads batches and dispatches by context across the shared TX CQ without assuming order. Provider errors retain full CQ error details and poison the endpoint. |
| 2. Real `send_nowait` depth | Implemented. The endpoint owns a configurable SEND ring (`URING_PLAY_OFI_TX_QUEUE_DEPTH`); `send_nowait`, `send_many_nowait`, `send_poll`, and `drain_send` allow multiple live sends. Configured and peak occupancy, posts, EAGAINs, retries, and errors are reported. |
| 3. Preposted receive ring | Implemented. `recv_queue_init`, `recv_post`, and batched `recv_poll` use stable slots and source/length/token validation. Mirror, relay, and WAL receive paths pre-register and prepost windows. `FI_MULTI_RECV` remains an optional provider-specific experiment, not the generic correctness path. |
| 4. Asynchronous RMA writes | Implemented. RMA READ and WRITE each have independently sized persistent rings, post APIs, and batched poll APIs. The current follow-on changes strengthen WAL writes to `FI_DELIVERY_COMPLETE` before publishing the metadata doorbell. |
| 5. Mixed operation types on one TX CQ | Implemented. The dispatcher recognizes SEND, READ, and WRITE contexts and lets each Rust caller reap only its own ring. The provider-backed `mixed_send_read_and_write_share_one_tx_cq_without_completion_theft` test covers completion ownership. |
| 6. Batch send completion handling | Implemented. Fixed send batches use the persistent ring; CQ dispatch and drain are batched and completion-order independent. Multiple batches can remain live up to ring credits. |
| 7. Hot context allocation | Implemented. Context arrays are allocated once with their rings; the batch path no longer allocates/frees an operation-context array per batch. |
| 8. Stable MR arenas | Implemented. Each operation class owns a table of registered arenas. Callers register stable arenas before posting and use contained-range descriptor lookup. Strict/fatal mode rejects any post-start registration attempt; final stats count registrations, closes, lookups/hits, and hot attempts. |
| 9. Serial userspace mirror/relay fanout | Implemented without moving placement into the kernel. The userspace RAID stage fills stable per-branch slots, posts a whole window with nonwaiting sends/inject, then fairly progresses every branch CQ. Branch-post skew is measured. Placement, mirror selection, lane selection, and backpressure remain userspace-owned. |
| 10. Slot-major blocking ACK collection | Implemented. Mirror and relay maintain branch x sequence masks, poll every branch nonblocking, validate branch/lane/sequence/range identity, accept out-of-order CQEs, and retire only contiguous all-branch-complete HWMs. Branch-mask wait and HOL wait are measured. |
| 11. Windowed ACK preposting | Implemented. Mirror defaults ACK preposting on and allocates a stable ACK arena/ring for the configured window on every branch. Relay does the same for every tail. Strict mode rejects a windowed mirror configuration that disables preposting. |
| 12. Workers owning multiple lanes | Implemented in both allowed forms. Raw RMA workers use event-driven round-robin progress across their lanes. Mirror/WAL representative strict mode rejects `workers < lanes`; non-strict runs warn and print every lane-to-worker-to-CPU mapping rather than claiming a representative shape. |
| 13. Sleeping CQ default in performance mode | Implemented. Low-CPU mode retains the 50 us default, but strict/fatal mode rejects nonzero `URING_PLAY_OFI_CQ_SLEEP_NS`; every command/profile prints the configured sleep and busy-poll controls. |
| 14. CQ sizing | Implemented. TX CQ requirement derives from SEND + READ + WRITE depths plus headroom; RX derives from receive depth plus headroom. Separate/shared sizing controls and CQ batch capacity are validated before open and reported with required sizes. |
| 15. Selective completion evaluation | Safely gated. `URING_PLAY_OFI_SELECTIVE_COMPLETION=1` binds selective completion as an A/B profile, but still requests every completion required to recycle a stable buffer or establish read/write semantics. Periodic suppression remains deliberately unimplemented because EFA-direct lacks the ordering guarantee needed to infer completion of earlier unsignaled operations, and RMA delivery/FUA paths require explicit completions. |
| 16. Correct `efa-direct` profile | Implemented and cross-host validated. Direct mode requests `FI_CONTEXT2` plus `FI_MR_LOCAL | FI_MR_VIRT_ADDR | FI_MR_ALLOCATED | FI_MR_PROV_KEY`, uses stable descriptors, checks returned mode/MR requirements, and exchanges peer profiles before timing. |
| 17. New API negotiation | Implemented. The shim requests libfabric 2.0 and falls back to 1.11 only when necessary; requested, query, and returned versions are reported. |
| 18. Device-RDMA and high-PPS verification | Implemented. EFA emulated READ/WRITE options are queried and become fatal in strict RMA mode when hardware operation cannot be proven. `FI_EFA_WR_HIGH_PPS` uses `fi_writemsg` only when advertised, reports requested/effective/verified/fallback state, and fails an unavailable strict request. The benchmark hosts advertised the flag as unavailable. |
| 19. Direct-profile semantic gates | Implemented. Maximum MSG/RMA sizes are queried and enforced, direct compact 4 KiB messages retain sequence headers, large extents use the separate RMA path, and versioned peer contracts reject provider/fabric/wire mismatches. Sequence, duplicate, range, HWM, branch, and lane checks remain enabled. |

## Acceptance evidence

- The historical handoff implementation run is retained under `bench-results/zcutils-grumps-efa-adhoc-c8gn16-20260811T0355Z/`. It includes three-repeat raw RMA curves, a successful strict cross-host `efa-direct` smoke, large-extent direct RMA, two-branch mirror ACK/HOL runs, two-tail relay runs, a negative peer-contract test, high-PPS-unavailable strict failure, and teardown evidence.
- The current run revalidated normal EFA after the delivery-complete payload redesign: single-lane low-QD and saturation RMA curves completed, EFA block reads/writes/mixed/FUA completed, and fixed mixed traffic stayed stable through aggregate depth 512.
- Current local validation: 214 library tests passed; 8 live-libfabric tests are ignored on a host without the needed provider. Kernel module build, formatting, shell syntax, and diff checks pass.
- The ignored provider tests cover vectored messages, preposted receive recycling, queued RMA reads/writes, mixed SEND/READ/WRITE CQ ownership, timeout slot retention, and fatal CQ-error slot retention. Their cross-host EFA equivalents have run in the retained artifacts.

## Deliberately deferred experiments

Two handoff suggestions remain experiments rather than missing correctness work:

1. `FI_MULTI_RECV`: keep it as a separately measured normal-`efa` optimization. The stable generic receive ring is required for `efa-direct` and is the current default.
2. Periodic selective-completion suppression: do not suppress CQEs until a provider/profile-specific buffer-lifetime proof exists. SEND buffers, RMA read destinations, RMA write delivery barriers, and error attribution currently need their CQEs. Binding selective completion while explicitly requesting required CQEs permits controlled A/B work without weakening semantics.

The current high-priority RDMA gaps are therefore above this transport-queue layer: the zcnblk block arena is still `vmalloc_user`, RMA reads still copy from a registered lane bounce buffer into the shared result slot, and mixed EFA traffic still uses RMA reads with framed writes rather than simultaneous RMA READ/WRITE queues. Those are listed in the main block report.
