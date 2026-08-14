# PostgreSQL TCP versus EFA RMA payload benchmark

Run: `zcutils-pgrma-adhoc-c8gn16-20260811T1055Z`  
Date: 2026-08-11  
Source commit: `1913baabf736da77d08e3d561036487486830cc6`  
Benchmark source-diff SHA-256:
`44119cbc389b9ca4326373e6b5329b26fc436cbd43a5d76b557f5355b052cc7b`

## Outcome

| Transport | Fresh databases | Samples | Mean TPS | TPS range | Mean latency | Latency range |
|---|---:|---:|---:|---:|---:|---:|
| TCP message payload | 2 | 6 | 94,961.2 | 92,766.0–97,067.4 | 1.3435 ms | 1.314–1.375 ms |
| EFA RMA payload | 2 | 6 | 96,661.6 | 95,220.9–98,780.5 | 1.3193 ms | 1.291–1.339 ms |

EFA improved aggregate TPS by 1.791% and reduced mean transaction latency by
1.799%. The independent fresh-run TPS deltas were 1.37% and 2.21%.

TCP passed strict topology preflight. The EFA measurements are intentionally
non-representative: the terminal 32 GiB `zcmem` leaf was explicit HugeTLB, but
the client RMA source is still the kernel module's
`vmalloc_user()`/`remap_vmalloc_range()` shared arena. A captured strict EFA
attempt rejected the run before creating the block edge or printing numbers.

## Matched setup

- Two `c8gn.16xlarge` ARM64 hosts in `us-east-2c`; private-IP bulk path.
- PostgreSQL 16.14, scale 300, 128 clients, 16 jobs, prepared TPC-B-like
  workload, synchronous commit, `fsync=on`, and full-page writes enabled.
- Two independent fresh database/leaf runs per transport. Each run had a
  five-second warmup and three 20-second samples.
- Two storage lanes. Ingress workers: CPUs 1 and 33; kernel lane threads: 2
  and 34; stable owners: 18 and 50; leaf workers: 3 and 35. PostgreSQL and
  pgbench had identical disjoint CPU sets for both transports. Network IRQs
  were isolated to CPUs 56–63.
- Unlimited memlock, 20,000 reserved 2 MiB HugeTLB pages, zero-sleep OFI CQ
  polling, EFA domain `efa_0-rdm`, and ENA adaptive moderation disabled.
- Ordinary PostgreSQL writes completed at local dirty-lease admission. Sync
  completed at the remote volatile leaf HWM. The leaf is not power-loss
  durable media.

## RMA evidence

- The live two-lane smoke passed same-sector read/write ordering and the
  all-lane sync drain.
- Smoke target write bytes and leaf RMA payload bytes were both 33,660,928;
  leaf payload receive/copy bytes were zero.
- Full EFA run 1: target write bytes and leaf RMA payload bytes were both
  29,436,796,928.
- Full EFA run 2: target write bytes and leaf RMA payload bytes were both
  29,538,689,024.
- Both full runs reported zero send, receive, RMA-write, TX-CQ, RX-CQ, and
  fatal provider errors, with no message-payload fallback.
- RMA payload completion was initiator delivery CQ before the metadata
  doorbell; remote acknowledgment was the doorbell result HWM.

## Raw fabric context

Three 4 KiB EFA `fi_pingpong` runs reported 6.43, 6.40, and 6.38 microseconds
per transfer. Three 100-packet private-IP ping runs averaged 43, 45, and 43
microseconds RTT. These are transport context only, not PostgreSQL completion
ceilings.

## Validation and remaining boundary

`cargo test --all-targets`, formatting, shell syntax checks, release builds,
the provider-backed out-of-order CQ tests, module load/unload, and diff checks
all passed. The next prerequisite for fully representative EFA numbers is an
external HugeTLB shared-arena allocation/ABI so the kernel block edge and
userspace target map the same explicit HugeTLB pages. The block edge still
performs one copy into the shared slot, so this RMA redesign removes the two
transport payload copies but is not end-to-end PostgreSQL zero copy.
