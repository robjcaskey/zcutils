# Same-placement-group EFA latency

- Run ID: `zcutils-efa-latency-20260809T1618Z`
- Instances: `i-05bc4b48e5accf6ee`, `i-0a6b3964f0e0c7c93`
- Shape/AZ: two `c8gn.48xlarge`, `us-east-2c`
- Placement group: `up-zcutils-adhoc-us-east-2c` (cluster strategy)
- EFA 0: `efa_0-rdm`, NUMA 0, both peers pinned to CPU 0
- EFA 1: `efa_1-rdm`, NUMA 1, both peers pinned to CPU 96
- Per-worker QD: 1; workers/lanes: 1; aggregate outstanding depth: 1
- Warmup: 10,000; measured iterations: 100,000 per run
- Memlock soft limit: 49,811,976,192 bytes

| Semantics | Payload | Domain | Repeats | RTT min | RTT p50 | RTT p95 | RTT p99 | QD1 ceiling from p50 |
|---|---:|---|---:|---:|---:|---:|---:|---:|
| remote receive + reply | 64 B | efa_0-rdm | 3 | 11.045 us | 12.477-12.512 us | 13.434-13.457 us | 14.166-14.412 us | 79.9-80.1 kIOPS |
| remote receive + reply | 64 B | efa_1-rdm | 3 | 11.127 us | 12.466-12.471 us | 13.426-13.429 us | 14.168-14.220 us | 80.2 kIOPS |
| remote write CQ + explicit reply | 64 B | efa_1-rdm | 3 | 11.118 us | 12.468-12.551 us | 13.421-13.631 us | 14.100-14.720 us | 79.7-80.2 kIOPS |
| remote receive + reply | 4096 B | efa_1-rdm | 1 | 12.416 us | 13.607 us | 14.461 us | 15.298 us | 73.492 kIOPS |
| remote write CQ + explicit reply | 4096 B | efa_1-rdm | 1 | 12.396 us | 13.609 us | 14.447 us | 15.302 us | 73.481 kIOPS |

The best observed transport RTT was 11.045 us, making 5.522 us the lowest
half-RTT estimate. Half-RTT is a physical-path lower bound only; it is not a
measured one-way latency.

The WAL saturation run is not directly comparable to these remote-ack ceilings:
it used 32 workers at per-worker QD 128 (aggregate QD 4096) and explicitly
reported early local write acknowledgements. Its 489 kIOPS baseline therefore
is not constrained by one remote RTT per operation. The same-placement-group
transport is healthy and stable, so the earlier regression remains in the
software/batching path rather than being explained by raw network latency.
