# Ad-hoc regression benchmark report

Run: `zcutils-regressions-adhoc-c8gn48-20260809T220433Z`

## Outcome

- Fixed the read/mixed regression caused by applying stable-owner completion routing globally. Representative read and mixed workloads now select lane-inline completion routing; representative pure-write workloads retain stable-owner routing. Explicit environment overrides remain supported.
- Added timed remote-global-sync-drain and native `RWF_DSYNC`/FUA validation to the benchmark harness, including fatal checks that the target observed sync and FUA requests.
- Read saturation peaks near aggregate QD 2048 (32 workers at QD64) and rolls over at QD128 and QD256. This is an over-queuing effect rather than a transport ceiling.
- The cold QD1 run still has a pronounced adaptive-receive warm-up effect. It is reported as a shared-system measurement with spread and is not claimed as fixed or representative from a single repetition.

## Test topology

- Two `c8gn.48xlarge` spot instances in placement group `up-zcutils-adhoc-us-east-2c`
- 32 lanes/workers; client CPUs `0-15,96-111`; leaf CPUs `0-15,96-111`
- Dual EFA; 21,121 free/total 2 MiB hugepages; unlimited memlock
- External userspace WAL/RAID stage; `/dev/zcnblk0` remained the client block edge

## Raw transport RTT

Three 4 KiB EFA repetitions:

- Remote receive plus explicit reply: p50 21.553, 21.541, 21.523 us; sequential QD1 ceiling about 46.4 kIOPS per lane
- Remote RDMA-write CQ plus explicit reply: p50 23.968, 23.983, 23.938 us; sequential QD1 ceiling about 41.7 kIOPS per lane

The mean receive/reply RTT used for the remote-read efficiency curve is 21.539 us.

## Routing regression validation

All rows use 32 workers at per-worker QD128, aggregate QD4096, with three repetitions.

| Workload | Routing | Min IOPS | Mean IOPS | Max IOPS | Spread |
| --- | --- | ---: | ---: | ---: | ---: |
| Mixed | lane-inline | 1,809,403 | 1,815,348 | 1,822,369 | 0.71% |
| Read | lane-inline | 1,606,314 | 1,614,588 | 1,620,951 | 0.91% |
| Write, early local acknowledgement | stable-owner | 1,717,098 | 1,914,432 | 2,038,587 | 16.79% |

The broken stable-owner read/mixed baselines were approximately 1.00 MIOPS and 0.995 MIOPS respectively.

## Remote-read latency-efficiency curve

Theoretical ceiling is aggregate outstanding depth divided by the measured 21.539 us raw receive/reply RTT. Each row contains three shared-system repetitions.

| Per-worker QD | Aggregate QD | Min IOPS | Mean IOPS | Max IOPS | Mean efficiency | p50 latency |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 32 | 122,527 | 266,270 | 408,196 | 17.9% | 71-72 us |
| 2 | 64 | 365,694 | 409,777 | 433,427 | 13.8% | 133-134 us |
| 4 | 128 | 573,290 | 587,760 | 597,437 | 9.9% | 167-169 us |
| 8 | 256 | 950,474 | 982,257 | 1,006,074 | 8.3% | 185-187 us |
| 16 | 512 | 1,238,447 | 1,297,190 | 1,336,375 | 5.46% | 259-273 us |

QD1 spread is 107.29%, so its mean is not accepted as a stable representative number. Increasing adaptive receive spin improved the first-run average latency but did not sufficiently stabilize the result and was not made the default.

## Mixed low-QD validation

Mixed completion semantics are deliberately not assigned one RTT efficiency: reads wait for a remote response while writes receive an early local acknowledgement.

| Per-worker QD | Aggregate QD | Mean IOPS | Spread | Read p50 | Early-write p50 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 32 | 587,180 | 32.05% | about 74 us | about 9 us |
| 16 | 512 | 1,456,700 | 4.67% | 327-333 us | 184-187 us |

These improve over the broken stable-owner baselines of about 91.7 kIOPS and 697 kIOPS.

## Saturation curves

Remote reads:

| Per-worker QD | Aggregate QD | Mean IOPS | Spread |
| ---: | ---: | ---: | ---: |
| 64 | 2048 | 1,966,338 | 2.81% |
| 128 | 4096 | 1,614,588 | 0.91% |
| 256 | 8192 | 1,323,592 | 0.12% |

Early-local-ack writes:

| Per-worker QD | Aggregate QD | Mean IOPS | Spread |
| ---: | ---: | ---: | ---: |
| 64 | 2048 | 1,806,764 | 18.54% |
| 128 | 4096 | 1,914,432 | 16.79% |
| 256 | 8192 | 2,023,476 | 2.00% |

The early-local-ack write curve is not compared with a network-RTT ceiling. Sync/FUA drains are measured separately.

## Ordering and durability gates

- Same-sector and cross-lane ordering smoke: PASS; remote-global-sync-drain terminal state observed; 5 target syncs; combined elapsed 13.917 ms
- Native FUA smoke: PASS; one target FUA request observed; `RWF_DSYNC`, I/O priority, write lifetime, and readback validated; elapsed 8.363 ms
- The in-memory external leaf required the explicit volatile-sync test opt-in. This is a benchmark-only durability caveat, not a persistent-media result.

## Validation and teardown

- `bash -n scripts/zcnblk-shm-block-bench.sh`: PASS
- `git diff --check`: PASS
- `cargo test --lib`: 196 passed, 0 failed
- Both spot instances terminated through the ad-hoc utility; both elastic IPs released
- Inventory and 463 collected client/leaf artifacts are stored beside this report
