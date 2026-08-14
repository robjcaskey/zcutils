# Google Spot and H4D RDMA campaign report

Date: 2026-08-13

## Outcome

The small Google Compute Engine baseline completed successfully. The exact
two-node H4D Cloud RDMA launch was then attempted, but Google rejected it before
creating any VM because project `rob-adhoc-81326` has only 32 global vCPUs and
the pair requires 384. Consequently, this campaign has no H4D RDMA performance
number; reporting the Axion TCP result as an H4D or RDMA result would be false.

The live-mode launcher now performs this global quota check before it reaches
instance creation. Its real-project negative test reports:

```text
insufficient CPUS_ALL_REGIONS quota for the H4D request: need 384, available 32.0 (limit 32.0, usage 0.0); no instance request was sent
```

All VMs and all H4D campaign resources have been deleted. The final inventory
contains no instances, disks, Falcon networks/subnets, or placement policies.

## Completed small Google baseline

The representative block tests used two `c4a-standard-4` Spot VMs in
`us-central1-c`, Ubuntu 24.04 Arm64, stock `6.17.0-1022-gcp`, four physical
Neoverse V2 cores per VM, one NUMA node, and one gVNIC per VM. Benchmark payload
traffic used only the same-zone private addresses `10.128.0.5` and
`10.128.0.6`. Public addresses were limited to SSH, package installation, and
source synchronization.

The block topology was one logical `/dev/zcnblk0` edge, one userspace placement
lane, and one remote userspace `zcmem` leaf. CPU placement was explicit:

```text
lane 0: client CPU 0 -> userspace target CPU 1 -> zcnblk kernel CPU 2
        -> private gVNIC -> remote leaf worker CPU 0
stable write owner: CPU 3
hctx CPU map: 0-3
```

The runs had 1,024 x 2 MiB HugeTLB pages, unlimited memlock, pinned workers,
explicit hctx affinity, gVNIC `rx-usecs=0 tx-usecs=0`, and three repetitions per
point. Important results are shared-system measurements even on dedicated
ad-hoc VMs, so the repeat spread is retained.

### Private network

- One-outstanding TCP request/response: 27,047.67, 24,544.93, and 26,155.73
  transactions/s; mean 25,916.11/s, equivalent raw transport RTT 38.586 us.
- ICMP after low-latency coalescing: steady repeats averaged 31 us.
- TCP bulk after warmup: approximately 21.7-21.8 Gbit/s in both directions.
- One and four iperf streams reached the same link ceiling. All bulk route and
  NIC counter evidence identifies the same-zone private gVNIC path.

### Remote-read low-QD curve

Every point below has one worker, one lane, per-worker QD equal to the shown
QD, and aggregate outstanding depth equal to the shown QD. The theoretical
ceiling is `aggregate depth / 38.586 us`; completion means remote-read data is
visible at the client block completion. It is not a write or durability
denominator.

| Per-worker QD | Workers / lanes | Aggregate depth | Mean IOPS | Spread | Matching theoretical IOPS | Efficiency |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1 / 1 | 1 | 20,267 | 4.57% | 25,916 | 78.20% |
| 2 | 1 / 1 | 2 | 22,589 | 2.05% | 51,832 | 43.58% |
| 4 | 1 / 1 | 4 | 57,187 | 0.33% | 103,664 | 55.17% |
| 8 | 1 / 1 | 8 | 89,570 | 0.66% | 207,329 | 43.20% |
| 16 | 1 / 1 | 16 | 162,636 | 0.31% | 414,658 | 39.22% |

The QD2 dip reproduced in all three runs and has not been smoothed away.

### Remote-read saturation

The same one-worker/one-lane topology was swept beyond its peak:

| Per-worker QD / aggregate depth | Mean IOPS | Spread |
|---:|---:|---:|
| 32 | 179,194 | 0.45% |
| 64 | 247,837 | 0.64% |
| 128 | **316,320** | 0.79% |
| 256 | 226,845 | 0.97% |

QD128 is the observed remote-read saturation point on this four-core pair.
QD256 is materially slower, so continuing upward on this topology is not
justified.

### Writes and drains

Ordinary block writes used early local block acknowledgements while the
userspace owner drained to the remote leaf. They are therefore not comparable
to the TCP RTT ceiling and are not remote durability numbers:

| Per-worker QD / aggregate depth | Mean IOPS | Spread | Completion |
|---:|---:|---:|---|
| 32 | 241,538 | 1.95% | early local block acknowledgement |
| 128 | **316,369** | 1.21% | early local block acknowledgement |
| 256 | 311,974 | 0.92% | early local block acknowledgement |

FUA disabled early acknowledgement and forced a remote leaf drain for each
write: QD1 reached 12,759 IOPS (0.26% spread) and QD4 reached 47,366 IOPS
(0.30% spread). The terminal leaf was volatile remote memory, so this proves a
remote volatile drain, not persistence across process or host loss.

### PostgreSQL

PostgreSQL 16.14 ran on ext4 over the real `/dev/zcnblk0` client edge with
`fsync=on`, `synchronous_commit=on`, and `full_page_writes=on`. Scale 10,
16 clients, two pgbench jobs, and three 10-second repetitions produced:

- mean 6,262.735 TPS; min 6,146.784, max 6,333.631; spread 2.983%;
- mean latency 2.541 ms; min 2.511 ms, max 2.589 ms.

This application result is explicitly non-representative: the tiny four-core
VM could not isolate PostgreSQL from every transport role and the terminal
leaf was volatile memory. It is a functional application-on-block baseline,
not a durable-database claim.

## Exact H4D launch that was attempted

- two `h4d-standard-192` Spot VMs in `us-central1-a`;
- HPC Rocky Linux 8 image family from `cloud-hpc-image-public`;
- one ordinary gVNIC for SSH/control and exactly one IRDMA interface with no
  external address per VM;
- a Falcon-profile VPC with MTU 8896 and a non-overlapping private subnet;
- a COLLOCATED compact placement policy with `maxDistance=3` and `vmCount=2`;
- Hyperdisk Balanced boot disks and `TERMINATE` host-maintenance policy;
- 50-minute maximum runtime and forced `DELETE` termination;
- metadata policy `bulk_traffic_policy=irdma-same-zone-only`;
- current Spot price guard of $3.29832 per node-hour, projected pair compute
  cost $5.497200, and a hard launch ceiling of $6.

Google rejected the first VM insert at the global quota gate:

```text
Quota 'CPUS_ALL_REGIONS' exceeded. Limit: 32.0 globally.
```

No VM or boot disk was created. Before retrying, the project needs global CPU
headroom for 384 vCPUs and regional H4D quota for 384 vCPUs. H4D uses the
`CPUS_PER_VM_FAMILY` pool with `vm_family=H4D`. If Google grants a separate
Preemptible CPUs pool for `us-central1`, that pool also needs 384 vCPUs; without
such a grant, Spot consumes the standard H4D family pool.

## H4D qualification gates now available

- `scripts/gce-h4d-pair-manifest.py` reads both live VM definitions and refuses
  a pair unless machine type, zone, compact policy, Falcon profile, MTU,
  private/no-external IRDMA addresses, runtime deletion, cost metadata, and
  same-zone/no-internet bulk policy all agree.
- `scripts/gce-h4d-rdma-preflight.sh` runs on each VM and fails strict runs for
  the wrong machine, anything other than 192 physical cores/no SMT, missing
  gve/IDPF/iRDMA devices, inactive verbs ports, an incorrect route or default
  route on Falcon, missing HugeTLB/memlock, unpinned or cross-NUMA workers,
  ambiguous GIDs, or unverified MSI-X affinity. It emits every lane-to-worker,
  CPU, QP/CQ, NUMA, NIC, and peer mapping.
- `scripts/gce-h4d-rdma-qualify.sh` binds Google's documented `ib_send_bw -aF`
  health test to the selected IRDMA device, private Falcon peer, and one
  explicit CPU. It rejects results unless a message size over 4 KiB reaches
  11,000 MB/s. The payload is verbs RDMA; the peer/control address is the
  private Falcon address, never a public or cross-region address.
- `scripts/zcofi-rdma-topology-preflight.sh` now understands the IDPF/iRDMA
  hardware path as well as mlx5 and RXE, allowing the existing repeated
  QD1/2/4/8/16 plus high-QD RMA matrix to run without relabeling TCP as RDMA.
- `scripts/gce-h4d-kernel-build.sh`, `gce-h4d-kernel-deploy.sh`, and
  `gce-h4d-kernel-probe.sh` provide the custom x86_64 kernel path. The build
  starts from the exact HPC-image vendor config and retains gve, IDPF, iRDMA,
  NVMe/Hyperdisk, NUMA, HugeTLB, and io_uring. Deployment preserves the vendor
  kernel and stages a custom kernel only as an explicit one-shot boot.

The H4D kernel config-only rehearsal succeeded for
`7.0.8-zc-h4d-io-slots`. Its config, diff, manifest, and checksums are under
`kernel-config-rehearsal/`. A custom kernel must not replace the stock HPC
kernel baseline until the strict IRDMA health test passes before and after the
one-shot boot.

Final local verification passed: `cargo check --locked`, the complete
`cargo test --locked` suite (no failures; eight hardware/libfabric tests
explicitly ignored), 11 Spot-launcher safety tests, two H4D manifest-policy
tests, shell syntax checks, Python byte compilation, and the config-only H4D
kernel rehearsal. The live H4D RDMA and custom-kernel boot probes remain
intentionally unexecuted because no H4D VM could be created.

## Cost and cleanup

The H4D attempt incurred zero VM compute cost. The completed C4A pair ran for
about 30 minutes; current C4A Spot resource prices imply roughly $0.054 of
pair compute, with only small boot-disk and public-control-IP additions. The
15-minute e2 smoke nodes add pennies at most. The guarded worst-case compute
exposure for the completed basic campaign was below $0.25, and the planned H4D
pair would have kept total campaign compute below $6, safely under the $10
daily authorization. Final invoiced totals should still be taken from Cloud
Billing because Spot prices and disk/IP rounding are external to the harness.

The final cleanup verification is `cleanup-verification.json`; every campaign
array is empty, including all project instances.

## Primary evidence

- H4D dry run: `launch-dry-run.json`
- Original quota rejection: `launch.stderr`
- Hardened quota preflight: `quota-preflight-after-hardening.log`
- Cleanup proof: `cleanup-verification.json`
- Small Google run root:
  `../zcutils-gce-axion-small-20260813t112542z/`
- Full small-run topology and repetition artifacts:
  `../zcutils-gce-axion-small-20260813t112542z/node0-artifacts/`

Relevant Google documentation:

- https://docs.cloud.google.com/compute/docs/instances/create-vm-with-rdma
- https://docs.cloud.google.com/compute/docs/networking/using-irdma
- https://docs.cloud.google.com/compute/resource-usage
- https://docs.cloud.google.com/compute/docs/instances/spot
- https://cloud.google.com/spot-vms/pricing
- https://cloud.google.com/vpc/pricing
