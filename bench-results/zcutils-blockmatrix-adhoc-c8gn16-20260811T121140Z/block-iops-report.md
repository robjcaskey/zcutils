# Raw block IOPS: EFA/RDMA, userspace TCP, and NVMe/TCP

Run: `zcutils-blockmatrix-adhoc-c8gn16-20260811T121140Z`

## Bottom line

At exactly one worker, one lane/queue, per-worker QD 1, and aggregate outstanding depth 1:

| Path | Operation and completion boundary | Mean IOPS | Repeat spread | Mean / p50 / p99 latency |
|---|---|---:|---:|---:|
| zcnblk over EFA | Remote read completion | 44,864 | 0.252% | 22.24 / 23 / 26 us |
| zcnblk over TCP | Remote read completion | 27,571 | 0.243% | 36.22 / 36.33 / 54 us |
| NVMe/TCP | Remote read, target page-cache admission | 28,015 | 0.364% | 35.64 / 32 / 48.33 us |
| zcnblk over EFA | Early local dirty-lease write acknowledgement | 40,123 | 0.107% | 24.87 / 5 / 110 us |
| zcnblk over TCP | Early local dirty-lease write acknowledgement | 77,302 | 0.079% | 12.88 / 5 / 64 us |
| NVMe/TCP | Remote write, target page-cache admission | 28,532 | 0.512% | 34.99 / 32 / 46 us |
| zcnblk over EFA | 50/50 remote reads and early local writes | 72,550 | 0.154% | 13.73 / 5 / 43 us |
| zcnblk over TCP | 50/50 remote reads and early local writes | 52,227 | 0.155% | 19.09 / 5 / 68 us |
| NVMe/TCP | 50/50 remote commands, target page-cache admission | 28,078 | 0.534% | 35.56 / 32 / 45.67 us |
| zcnblk over EFA | Per-write FUA, remote drain | 16,101 | 0.137% | 62.06 / 61 / 67.67 us |
| zcnblk over TCP | Per-write FUA, remote drain | 17,434 | 0.235% | 57.31 / 57 / 74.33 us |
| NVMe/TCP | FUA target sync on volatile tmpfs media | 28,563 | 0.886% | 34.96 / 32 / 46 us |

For matching remote-read semantics, EFA is 62.7% faster than userspace TCP at aggregate QD 1. EFA's ordinary write result is 48.1% below TCP, but both of those are early local acknowledgements and therefore must not be compared with a network-RTT ceiling. The EFA mixed result is 38.9% above TCP. For remote FUA drains EFA is 7.6% below TCP.

The NVMe/TCP target used a buffered regular file on a dedicated tmpfs and is volatile. Its ordinary and FUA write numbers are remote NVMe command completions, not zcnblk's early local write completion. FUA is nearly free on this target and says nothing about durable-media latency.

## Raw transport ceilings

The raw one-worker, one-lane, aggregate-QD1 remote-application-ack control was:

| Transport | Completion | IOPS | RTT | Matching ceiling | Efficiency | Spread |
|---|---|---:|---:|---:|---:|---:|
| EFA | Remote application acknowledgement | 69,405 | 14.058 us | 71,132 | 97.57% | 0.082% |
| TCP | Remote application acknowledgement | 35,415 | 28.185 us | 35,480 | 99.82% | 2.759% |

The one-sided EFA RMA controls used a single worker and lane at every point. Reads complete when data is visible at the initiator's local CQ. Writes use `FI_DELIVERY_COMPLETE` and complete when the initiator receives the delivery CQ event indicating remote visibility; remote admission and durability are separate.

| Operation | Per-worker QD = aggregate depth | IOPS | Raw completion RTT | Matching ceiling | Efficiency | Spread |
|---|---:|---:|---:|---:|---:|---:|
| RMA read | 1 | 52,252 | 17.496 us | 57,156 | 91.42% | 0.07% |
| RMA read | 2 | 104,211 | 17.554 us | 113,936 | 91.46% | 0.25% |
| RMA read | 4 | 200,523 | 18.262 us | 219,042 | 91.54% | 1.67% |
| RMA read | 8 | 352,098 | 20.554 us | 389,233 | 90.46% | 1.89% |
| RMA read | 16 | 403,055 | 32.512 us | 493,095 | 81.66% | 15.11% |
| RMA read, random | 32 | 417,379 | 59.959 us | 535,774 | 77.86% | 14.85% |
| RMA read, random | 64 | 409,867 | 115.230 us | 555,785 | 73.75% | 5.85% |
| RMA read, random | 128 | 365,408 | 261.628 us | 489,392 | 74.67% | 3.73% |
| RMA read, random | 256 | 306,741 | 637.737 us | 401,423 | 76.41% | 0.55% |
| RMA write | 1 | 64,886 | 15.359 us | 65,110 | 99.65% | 0.32% |
| RMA write | 2 | 129,362 | 15.406 us | 129,820 | 99.65% | 0.47% |
| RMA write | 4 | 254,344 | 15.661 us | 255,413 | 99.58% | 0.55% |
| RMA write | 8 | 484,482 | 16.401 us | 487,783 | 99.32% | 0.96% |
| RMA write | 16 | 856,659 | 18.375 us | 870,761 | 98.38% | 1.41% |
| RMA write, random | 32 | 1,315,648 | 23.316 us | 1,372,430 | 95.86% | 0.20% |
| RMA write, random | 64 | 1,301,454 | 36.912 us | 1,733,854 | 75.06% | 0.19% |
| RMA write, random | 128 | 965,080 | 92.850 us | 1,378,577 | 70.01% | 0.24% |
| RMA write, random | 256 | 643,425 | 261.098 us | 980,476 | 65.62% | 0.44% |

## Low-depth block curve

Each cell is mean IOPS followed by the analyzed-repeat spread. Runs used four repetitions with repetition 1 treated as warm-up and repetitions 2-4 analyzed. QD is per worker and, because these rows all use one worker and one lane/queue, it is also aggregate outstanding depth.

| zcnblk over EFA | QD1 | QD2 | QD4 | QD8 | QD16 |
|---|---:|---:|---:|---:|---:|
| Remote read | 44,864 (0.252%) | 54,057 (0.052%) | 98,835 (0.072%) | 134,406 (0.095%) | 245,827 (0.112%) |
| Early local write | 40,123 (0.107%) | 71,597 (0.380%) | 120,176 (0.493%) | 129,976 (0.124%) | 206,401 (0.746%) |
| 50/50 remote-read/local-write mix | 72,550 (0.154%) | 96,143 (0.233%) | 139,188 (0.085%) | 127,951 (0.345%) | 204,958 (0.117%) |

| zcnblk over TCP | QD1 | QD2 | QD4 | QD8 | QD16 |
|---|---:|---:|---:|---:|---:|
| Remote read | 27,571 (0.243%) | 36,685 (0.352%) | 89,472 (0.342%) | 79,482 (0.155%) | 141,994 (0.067%) |
| Early local write | 77,302 (0.079%) | 118,390 (0.611%) | 164,666 (0.624%) | 127,951 (0.078%) | 188,902 (0.121%) |
| 50/50 remote-read/local-write mix | 52,227 (0.155%) | 68,352 (0.108%) | 110,060 (0.688%) | 117,038 (0.302%) | 182,178 (0.024%) |

| Kernel NVMe/TCP | QD1 | QD2 | QD4 | QD8 | QD16 |
|---|---:|---:|---:|---:|---:|
| Remote read | 28,015 (0.364%) | 37,984 (1.340%) | 65,590 (2.263%) | 64,047 (1.296%) | 82,295 (1.949%) |
| Remote write | 28,532 (0.512%) | 37,187 (3.880%) | 31,573 (115.266%) | 50,890 (61.977%) | 53,254 (103.194%) |
| 50/50 remote mix | 28,078 (0.534%) | 43,099 (4.144%) | 45,121 (36.005%) | 56,107 (38.945%) | 56,553 (54.979%) |

The NVMe/TCP QD1 mixed point was rerun in separate target setups. Two internally stable analyzed means were 23,378 IOPS (0.222% within-run spread) and 28,078 IOPS (0.534%), a 20.1% setup-to-setup difference. The table uses the latest cohort. At QD4-QD16, isolated 0.5-5.6 second completions materially reduced whole-run throughput even when p99 remained much lower; these points are measurements of a noisy shared system, not stable saturation claims.

### Matching low-depth ceilings

For remote reads, the matching transport ceilings and actual/theoretical efficiencies are:

| Path | Raw RTT source | QD1 | QD2 | QD4 | QD8 | QD16 |
|---|---|---:|---:|---:|---:|---:|
| zcnblk/EFA read | Per-QD RMA-read RTT above | 78.49% | 47.45% | 45.12% | 34.53% | 49.85% |
| zcnblk/TCP read | 28.185 us remote-app-ack RTT; ceilings 35,480/70,960/141,920/283,840/567,680 | 77.71% | 51.70% | 63.04% | 28.00% | 25.01% |
| NVMe/TCP read | Same raw TCP RTT and ceilings | 78.96% | 53.53% | 46.22% | 22.56% | 14.50% |
| NVMe/TCP write | Same raw TCP RTT and ceilings | 80.42% | 52.41% | 22.25% | 17.93% | 9.38% |
| NVMe/TCP mixed | Same raw TCP RTT and ceilings | 79.14% | 60.74% | 31.79% | 19.77% | 9.96% |

There is deliberately no network-RTT efficiency for ordinary zcnblk writes: they complete at an early local dirty-lease acknowledgement. There is also no single matching ceiling for a zcnblk mixed run because half its operations are remote reads and half are early local writes. EFA's QD-specific raw RMA RTTs and TCP's 28.185 us raw RTT are context only for those rows. FUA adds a remote leaf drain, so its matching transport-only ceiling and efficiency are also `N/A`; treating 1/RTT as its ceiling would omit the required sync work.

## High-depth saturation curve

These rows use two workers and two lanes/queues. Thus per-worker QD 32/64/128/256 means aggregate outstanding depth 64/128/256/512. Every reported EFA and userspace-TCP point states lane 0 -> client CPU 0 / target CPU 1 / kernel CPU 2 / leaf CPU 3 and lane 1 -> client CPU 32 / target CPU 33 / kernel CPU 34 / leaf CPU 35. Each cell is analyzed mean IOPS and spread.

| Path and operation | 2xQD32 = agg64 | 2xQD64 = agg128 | 2xQD128 = agg256 | 2xQD256 = agg512 |
|---|---:|---:|---:|---:|
| zcnblk/EFA read | 595,222 (0.154%) | 600,659 (0.068%) | 613,731 (0.215%) | 607,886 (0.062%) |
| zcnblk/EFA early-local write | 589,840 (0.199%) | 589,047 (0.277%) | 603,223 (0.063%) | 607,465 (0.064%) |
| zcnblk/EFA 50/50 mix | 594,854 (0.058%) | 597,566 (0.081%) | 609,686 (0.116%) | 607,508 (0.054%) |
| zcnblk/TCP read | 421,370 (0.042%) | 541,171 (0.035%) | 563,943 (0.002%) | 564,034 (0.001%) |
| zcnblk/TCP early-local write | 571,955 (0.112%) | 572,343 (0.294%) | 569,956 (0.679%) | 572,390 (0.177%) |
| zcnblk/TCP 50/50 mix | 471,858 (0.303%) | 593,673 (0.054%) | 602,926 (0.077%) | 601,303 (0.215%) |
| NVMe/TCP read | 288,122 (0.497%) | 165,263 (0.170%) | 312,658 (0.984%) | 285,184 (1.682%) |
| NVMe/TCP write | 235,231 (7.126%) | 152,102 (3.362%) | 243,678 (0.288%) | 102,752 (3.122%) |
| NVMe/TCP 50/50 mix | 152,488 (4.366%) | 155,731 (0.222%) | 263,723 (10.350%) | 270,309 (2.833%) |

EFA plateaus at roughly 590-614k block IOPS, independent of operation mix after the fixes. TCP reaches roughly 564k reads, 572k early-ack writes, and 601-603k mixed. NVMe/TCP is substantially lower and non-monotonic because of the target stalls; its best analyzed means are 313k reads, 244k writes, and 270k mixed.

## Same-instance refactor controls

The old and current trees were run on the same two instances with matched topology. Positive means the current tree is faster.

| Transport | Operation | Workers x per-worker QD = aggregate | Current IOPS | Base IOPS | Change |
|---|---|---:|---:|---:|---:|
| EFA | Remote read | 1x1 = 1 | 44,864 | 44,305 | +1.262% |
| EFA | Remote read | 1x16 = 16 | 245,827 | 245,230 | +0.243% |
| EFA | Remote read | 2x128 = 256 | 613,731 | 609,308 | +0.726% |
| TCP | Remote read | 1x1 = 1 | 27,571 | 27,806 | -0.845% |
| TCP | Remote read | 1x16 = 16 | 141,994 | 144,148 | -1.494% |
| TCP | Remote read | 2x128 = 256 | 563,943 | 563,943 | 0.000% |
| TCP | 50/50 mixed semantics | 1x1 = 1 | 52,227 | 51,926 | +0.580% |
| TCP | 50/50 mixed semantics | 1x16 = 16 | 182,178 | 181,932 | +0.135% |
| TCP | 50/50 mixed semantics | 2x128 = 256 | 602,926 | 606,052 | -0.516% |
| TCP | Early local write | 1x1 = 1 | 77,302 | 77,640 | -0.435% |
| TCP | Early local write | 1x16 = 16 | 188,902 | 190,718 | -0.952% |
| TCP | Early local write | 2x128 = 256 | 569,956 | 571,652 | -0.297% |

There is no broad refactor regression in the matched controls: all changes are within -1.494% to +1.262%. There is no old-tree matched control for the new EFA RMA-write path or for NVMe/TCP.

## Bugs found and fixed before accepting results

1. A transport-independent zcnblk kernel connection worker could sleep indefinitely after a missed wakeup while descriptors remained pending and rings appeared empty. This stranded high-QD writes on both EFA and TCP. The worker now performs a bounded idle recheck using `wait_event_interruptible_timeout`; the topology artifact records `kernel_idle_recheck_us=1000`.
2. Large framed EFA mixed requests could deadlock in libfabric rendezvous: the initiator attempted the next request send before posting the prior result receive while the leaf attempted that prior result send. OFI now permits only one outstanding framed request per lane, while one-sided RMA read batches retain the configured queue window. Fixed EFA mixed reruns are stable through aggregate depth 512.
3. A transient module-holder race between matrix points could make `rmmod` fail. Cleanup now retries an exact module unload for up to five seconds and fails the point if cleanup remains incomplete.
4. The RMA write payload path was redesigned so registered shared-slot runs are written directly into a negotiated remote leaf memory window with delivery-complete CQ retirement, followed by a metadata-only doorbell. Source leases remain owned until delivery completion, destination reuse waits for the result high-water mark, and FUA waits for the leaf drain. This removes the userspace payload gather and leaf message receive copy for eligible pure-write batches.

## Topology and caveats

- Two `c8gn.16xlarge` instances in `us-east-2c`, one EFA device per host, provider/domain `efa` / `efa_0-rdm`.
- Single-lane runs map block worker CPU 0, userspace target CPU 1, kernel queue CPU 2, and remote leaf CPU 3. Two-lane runs add the sibling mapping 32/33/34/35. ENA/EFA IRQs were moved to CPUs 56-63.
- 8,192 x 2 MiB huge pages per host and unlimited memlock were present. Raw RMA and leaf transport windows used registered huge pages.
- Block buffers used `uring-fixed` with HugeTLB buffers. Kernel hctx CPU masks remained broad, and the CPU governor was not changed; the preflight artifacts warn about the governor. Results are topology-explicit dedicated-host measurements, but local variability still applies.
- The zcnblk shared request arena is `vmalloc_user` exposed through `remap_vmalloc_range`, not HugeTLB. It was registered with EFA, but this is not end-to-end zero copy. The kernel still copies once per direction at the block edge.
- Pure EFA writes used one-sided RMA payloads. EFA mixed runs used RMA reads plus framed-message writes (`shm_ofi_rma_writes=0` for mixed); they do not prove simultaneous RMA reads and writes on one lane.
- EFA high-PPS RMA-write capability reported unavailable/effective=0 on these hosts.
- NVMe/TCP used one client I/O queue, client CPU 0, target worker CPUs 3 and 35, IRQ CPUs 56-63, and volatile `tmpfs` regular-file backing with `buffered_io=1`. It is a protocol/control comparison, not a durable-device comparison.
- Every matrix point recorded per-worker QD, worker/lane count, aggregate depth, CPU mapping, huge-page/memlock state, completion semantics, and repeat spread. Interrupted pre-fix invocations were separated into cohorts; only the latest complete cohort was summarized.

## Remaining RDMA/zero-copy work

1. Replace or supplement the `vmalloc_user` zcnblk payload arena with a long-lived, DMA-friendly registered allocation while preserving the kernel-client rule that placement remains in userspace. This is the largest missing piece for true block-edge zero copy.
2. RMA reads currently land in a lane-local registered bounce area and are copied into the shared completion slot. Registering stable completion slots or a safe scatter destination would remove that userspace copy.
3. Enable one-sided RMA writes in mixed read/write batches and test simultaneous RMA read/write CQ arbitration. The current framed-window fix is correct but intentionally serializes message-framed requests; it does not solve dual-RMA mixed scheduling.
4. Pipeline multiple metadata doorbells/result windows without permitting remote-window aliasing or violating sector order. QD1 pure EFA writes currently pay delivery CQ plus doorbell/result bookkeeping and achieve only 40.1k early-ack block IOPS despite a 64.9k raw delivery-complete RMA control.
5. Investigate why EFA high-PPS writes were unavailable, including libfabric/provider version and instance capability, before treating 1.3M raw RMA-write IOPS as the hardware limit.
6. Tighten benchmark qualification further: explicitly set/record CPU governor, narrow hctx affinity instead of broad masks, isolate kthreads/worker CPUs, and rerun NVMe/TCP on real terminal media or a target without buffered-tmpfs stalls.

## Validation and provenance

- Source base commit: `1913baabf736da77d08e3d561036487486830cc6`
- Final benchmarked working-tree diff SHA-256: `49ad9677684f67ecb719b8df244ff8b35e12f49aebb95459798e0ab7dc573dc1`
- `src/zcnblk_shm_target.rs` SHA-256: `2b84c23538b0948072cf9fdb7c15a21ea0baa9c113799609a138b8723079c657`
- Full Rust library suite: 214 passed, 0 failed, 8 ignored because they require a local libfabric provider.
- `cargo fmt --all -- --check`, `make -C kmods`, shell syntax checks, and `git diff --check`: passed.
- Both AWS instances were confirmed `terminated`, and the coordination lease was released.

Machine-readable results are in [analysis/block-summary.csv](analysis/block-summary.csv), [analysis/block-repeats.csv](analysis/block-repeats.csv), [analysis/raw-rma-summary.csv](analysis/raw-rma-summary.csv), [analysis/raw-ack-summary.csv](analysis/raw-ack-summary.csv), and [analysis/baseline-comparison.csv](analysis/baseline-comparison.csv). Raw client and target artifacts are retained under `client-node/` and `leaf-node/`.

The deferred handoff is mapped item by item in [grumps-implementation-audit.md](grumps-implementation-audit.md).
