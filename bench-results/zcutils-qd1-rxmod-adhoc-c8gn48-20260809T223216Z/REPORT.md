# QD1 receive-moderation investigation

Run: `zcutils-qd1-rxmod-adhoc-c8gn48-20260809T223216Z`

## Outcome

The QD1 regression is ENA dynamic interrupt moderation, not userspace WAL placement, block ordering, or insufficient adaptive receive spinning. Fresh TCP flows hash onto ENA receive queues with different moderation state, creating a slow subset of lanes on the first repetition. Turning adaptive RX off and setting RX/TX coalescing to zero on both endpoints removed the lane bifurcation and reduced three-run spread from 45.78% to 0.77%.

The benchmark harness now rejects representative or topology-strict external low-QD WAL runs unless the caller explicitly confirms that both client and leaf NICs have been put into and verified in a low-latency configuration. It records that confirmation in `topology.log` and does not silently change shared-host NIC state.

## Topology and completion semantics

- Two `c8gn.48xlarge` spot instances in placement group `up-zcutils-adhoc-us-east-2c`
- 32 workers/lanes at per-worker QD1; aggregate outstanding depth 32
- Exact lane-to-client, target, kernel hctx, source NIC, destination NIC, and leaf-worker mappings are in each `topology.log`
- Dual EFA/ENA interfaces; 21,121 2 MiB hugepages and unlimited memlock
- Remote reads complete after the remote userspace leaf returns the requested payload
- `/dev/zcnblk0` remained only the client block edge; the userspace WAL stage retained all downstream decisions

## A/B result

| ENA condition on client and leaf | Repetition IOPS | Mean IOPS | Spread | Mean/theoretical efficiency |
| --- | --- | ---: | ---: | ---: |
| Adaptive RX on; RX 20 us; TX 64 us | 244,181 / 364,694 / 397,784 | 335,553 | 45.78% | 22.62% |
| Adaptive RX off; RX 0 us; TX 0 us | 413,131 / 415,412 / 412,225 | 413,589 | 0.77% | 27.89% |

Fixed moderation improved the first repetition by 69.19%, the three-run mean by 23.26%, and reduced spread by 98.32%.

## Raw transport ceiling

Three matching 4 KiB EFA remote-receive-and-reply probes had p50 RTTs of 21.543, 21.596, and 21.589 us (mean 21.576 us). The matching aggregate QD32 ceiling is therefore about 1,483,129 IOPS. This denominator applies to remote-read completion semantics only; it must not be reused for early local write acknowledgements or sync/FUA drains.

## Supporting evidence

- Earlier artifacts showed discrete slow-lane groups rather than whole-system slowdown. Slow lane identities changed when connections were recreated and generally healed across repetitions.
- Raising the userspace adaptive receive-spin minimum did not stabilize QD1; every lane reached the same maximum spin budget while only a subset remained slow.
- New target telemetry records each lane's target CPU, `SO_INCOMING_CPU`, `SO_INCOMING_NAPI_ID`, receive policy/budget, and adaptive-spin counters so future artifacts can correlate a slow flow with its NIC RX queue.
- Both adaptive snapshots show `Adaptive RX: on`, `rx-usecs: 20`, `tx-usecs: 64` on all four measured interfaces. Both fixed snapshots show `off`, `0`, and `0` respectively.

## Validation

- `bash -n scripts/zcnblk-shm-block-bench.sh`: PASS
- `cargo test --lib zcnblk_shm_target --no-fail-fast`: 38 passed, 0 failed
- `git diff --check`: PASS
- Both spot instances were terminated through the ad-hoc utility and both elastic IPs were released.
- Full client/leaf network snapshots, interrupts, topology, per-repetition latency, context deltas, target timing, and raw RTT records are stored beside this report.
