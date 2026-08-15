# Cross-machine EFA lane-alignment A/B

Run ID: `zc-lane-align-c8gn48-20260814T221735Z`

## Outcome

The lane-alignment principle is validated for latency-sensitive operation. With
one lane and one worker, keeping the worker and its registered memory on the
EFA card's NUMA node improved both one-sided reads and delivery-complete writes
at every point on the QD1/QD2/QD4/QD8/QD16 curve.

Across both cards and six samples per mapping (three repeats on each card):

- read IOPS improved 1.83%, 1.93%, 1.91%, 2.25%, and 8.69% at QD1/2/4/8/16;
- delivery-complete write IOPS improved 1.95%, 2.03%, 2.18%, 2.25%, and 2.46%;
- the crossed-NUMA latency penalty was about 2% through QD8 and increased to
  6.37% for QD16 reads;
- all accepted runs had zero CPU migrations, EAGAIN, hot MR replacement,
  provider/CQ errors, and fatal provider return codes.

At 16 lanes and per-worker QD256, delivery-complete writes saturated each
physical rail at about 299 Gbit/s and 9.13 million 4 KiB operations/s with no
meaningful aligned/crossed difference. High-QD reads retained an 8.97% latency
benefit from alignment, but the crossed control produced higher aggregate IOPS.
That high-QD aggregate result does not validate alignment as a saturation-IOPS
optimization; concurrency, CQ batching, and worker finishing skew masked the
per-operation latency cost.

## Topology and controlled variable

Two `c8gn.48xlarge` Spot instances in `us-east-2c` were used. Each host had 192
physical CPUs, two NUMA nodes, and two EFA cards:

| Card | Domain | Linux NIC | NIC NUMA | Aligned CPUs | Crossed control CPUs |
|---|---|---|---:|---|---|
| 0 | `efa_0-rdm` | `ens68` | 0 | 0-15 (CPU 0 for low QD) | 96-111 (CPU 96) |
| 1 | `efa_1-rdm` | `ens146` | 1 | 96-111 (CPU 96) | 0-15 (CPU 0) |

The mapping was changed on both initiator and target. Provider, domain, private
address, access permutation, payload size, QD, completion semantics, build,
HugeTLB pool, and memlock limit remained fixed. Bulk traffic used private EFA
addresses. Crossed controls were explicitly marked non-representative because
strict topology admission correctly rejects them as production mappings;
aligned measurements ran with strict topology admission enabled.

The instances were shared-system cloud measurements. Runs used ABBA-style
ordering and three repeats per card/mapping point. Low-QD per-card spreads were
normally below 1.4%; the largest was 5.92% for card-1 crossed QD16 reads.

## Low-QD efficiency curves

These are means across both cards: six samples per mapping. There was one worker
and one lane, so per-worker QD equals aggregate outstanding depth. The ceiling
is matched to the reported completion latency and operation semantic, not to a
generic network RTT.

### RMA read: data visible at initiator local CQ

| QD | Aligned IOPS | Crossed IOPS | Gain | Aligned latency | Crossed latency | Aligned ceiling / efficiency | Crossed ceiling / efficiency |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 29,441 | 28,911 | 1.83% | 32.265 us | 32.896 us | 30,994 / 94.99% | 30,399 / 95.11% |
| 2 | 59,014 | 57,894 | 1.93% | 32.202 us | 32.844 us | 62,109 / 95.02% | 60,896 / 95.07% |
| 4 | 117,721 | 115,515 | 1.91% | 32.273 us | 32.916 us | 123,944 / 94.98% | 121,522 / 95.06% |
| 8 | 232,021 | 226,904 | 2.25% | 32.708 us | 33.447 us | 244,601 / 94.86% | 239,185 / 94.87% |
| 16 | 427,907 | 393,699 | 8.69% | 34.936 us | 37.163 us | 458,014 / 93.42% | 430,605 / 91.42% |

### RMA write: initiator delivery CQ, remotely visible but not durable

| QD | Aligned IOPS | Crossed IOPS | Gain | Aligned latency | Crossed latency | Aligned ceiling / efficiency | Crossed ceiling / efficiency |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 32,707 | 32,082 | 1.95% | 30.515 us | 31.112 us | 32,771 / 99.80% | 32,143 / 99.81% |
| 2 | 65,456 | 64,150 | 2.03% | 30.494 us | 31.115 us | 65,586 / 99.80% | 64,278 / 99.80% |
| 4 | 130,810 | 128,019 | 2.18% | 30.516 us | 31.177 us | 131,083 / 99.79% | 128,301 / 99.78% |
| 8 | 259,805 | 254,094 | 2.25% | 30.721 us | 31.396 us | 260,415 / 99.77% | 254,813 / 99.72% |
| 16 | 513,218 | 500,873 | 2.46% | 31.075 us | 31.782 us | 514,887 / 99.68% | 503,445 / 99.49% |

These writes establish remote visibility at the EFA delivery-complete CQ. They
do not establish remote WAL admission, FUA completion, `fsync`, or durability.

## Very-high-QD saturation control

The saturation workload used 16 workers/lanes, per-worker QD256, aggregate
depth 4,096, 2 GiB per lane, and a 32 GiB random-permutation working set. Means
below combine both cards, with three repeats per card and mapping.

| Operation | Mapping | IOPS | Completion latency | Payload rate | Interpretation |
|---|---|---:|---:|---:|---|
| Read | aligned | 4,958,204 | 481.653 us | about 162.5 Gbit/s | lower latency, lower aggregate IOPS |
| Read | crossed | 5,791,798 | 524.863 us | about 189.8 Gbit/s | 8.97% worse latency; concurrency masked it |
| Delivery-complete write | aligned | 9,131,909 | 447.684 us | about 299.1 Gbit/s | physical-rail limited |
| Delivery-complete write | crossed | 9,134,252 | 447.337 us | about 299.3 Gbit/s | physically indistinguishable |

The read result is why the low-QD curve is the cleaner test of the principle.
It would be incorrect to summarize this run as “alignment always increases
IOPS.” It reduces locality latency; whether that becomes aggregate IOPS depends
on which other limit is active.

## Zero-copy audit

Every accepted endpoint reported `efa_direct=1`,
`efa_emulated_read=0`, and `efa_emulated_write=0`. RMA reads used fixed
registered initiator buffers and landed directly from the remote registered
arena. RMA writes used fixed registered source buffers and completed at the
delivery CQ. Target payload validation passed for all 144 accepted points.
There was no per-operation MR registration (`*_mr_hot=0`) and no message-path
payload fallback in this raw RMA workload.

This proves the transport-only registered-memory path; it does not prove that
the complete `/dev/zcnblk0` path is end-to-end copy-free. The current block edge
still reports one kernel payload copy per direction. The principal remaining
opportunity is to let safe, pinned request-owned pages or registered io-slot
leases become the transport buffer directly, while preserving request lifetime,
write ordering, dirty-read ownership, and flush/FUA semantics. That requires a
separate block-edge A/B and must not move placement into the kernel client.

The persistent file WAL already uses `pwritev` to avoid gathering header and
payload into another userspace buffer, but buffered file I/O still copies into
the page cache. A genuinely direct persistent variant would need aligned
direct-I/O or fixed-buffer io_uring leases plus crash-ordering validation; it is
not demonstrated by this run.

## Validation and artifacts

- 24 saturation points and 120 low-QD points accepted.
- All 144 target payload validations passed.
- All logs contain EFA-direct, non-emulated endpoint profiles.
- No nonzero CQ/provider error, EAGAIN, hot-MR, fatal-return, or migration
  counter was found.
- Build source was local commit `72018afe31e28a446524417d880dd041431c80c7`
  plus the recorded dirty working tree synchronized identically to both hosts.
- Exact runner: `run-point.sh` in this directory.
- Full client/target output: `card*-*.console.log` and per-point directories.

This run exercised raw userspace RMA only. It did not use a block device as a
mirror or stripe primitive, and it did not make placement decisions in the
`zcnblk` kernel client.

## Cost and teardown

Instances `i-0872ac52c15d06b87` and `i-05fa027d14a82d48f` launched at
22:17:37Z and were confirmed terminated shortly after 23:06Z. Both public EIP
allocations were released and subsequently returned `InvalidAllocationID.NotFound`.
At the observed $1.9188/hour Spot price, pair compute was approximately $3.1;
including the short-lived 512 GiB total gp3 roots and public IPv4 time keeps the
estimated run around $3.2, well below the $10 cap.
