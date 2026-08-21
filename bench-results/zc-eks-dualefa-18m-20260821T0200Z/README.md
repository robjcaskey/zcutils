# EKS dual-EFA 18M IOPS reproduction

This run reproduced the prior dual-card EFA-direct result from two Kubernetes
pods on an Amazon EKS managed node group.

## Topology and completion semantics

- Region/AZ: `us-east-2` / `us-east-2c`
- Nodes: two Spot `c8gn.48xlarge` Arm64 instances in the same cluster placement
  group, each with two 300-Gbit network cards
- Kernel: `6.18.41-94.142.amzn2023.aarch64`
- Transport: direct userspace `efa-direct` FI_MSG; no block device or terminal
  media participates in this transport measurement
- Payload/wire size: 4096 / 4224 bytes
- Completion: remote application high-water-mark acknowledgement
- Per rail: 40 lanes, 40 workers, QD256 per worker, aggregate depth 10,240
- Dual rail: 80 lanes/workers, aggregate outstanding depth 20,480
- CPU maps:
  - client card 0: CPUs `0-39`; target card 0: CPUs `40-79`
  - client card 1: CPUs `96-135`; target card 1: CPUs `136-175`
- Hugepages: 21,121 x 2 MiB per node; memlock unlimited

## Results

Single card reached 8,966,542 remotely acknowledged logical 4K IOPS and
293.816 payload Gbit/s.

The synchronized dual-card rate divides the combined record/byte count by the
slower sender rail's elapsed time:

| Run | IOPS | Payload Gbit/s | Wire Gbit/s |
| --- | ---: | ---: | ---: |
| rep1 | 17,933,607 | 587.648 | 606.012 |
| rep2 | 17,928,502 | 587.481 | 605.840 |
| rep3 | 17,908,519 | 586.826 | 605.165 |
| published-image conformance | 17,913,877 | 587.002 | 605.346 |

All sender and receiver summaries report zero worker migrations. The three
source-tree repetitions span 25,088 IOPS (0.14%). The best repetition is 134
IOPS below the prior 17,933,741-I/O/s reproduction.

## Published image

`docker.io/robjcaskey/zcutils-efa-bench:eks-20260820-r1`

Digest:
`sha256:9ce27ed6eddff84b7ebde71ce4fe0fc0c3b4286066f0f284b0ce74174b77d9a0`

The embedded Arm64 `zcutils` binary SHA-256 is
`ddb02408dd08d4bfd99b92ad6c3c952893e5c66a59f83da2ec98ff31952105d6`.
The final conformance repetition was run after both StatefulSet pods restarted
from that immutable image digest.

Raw per-rail logs are under `dual-efa/`; the single-card baseline is under
`single/`; node/EFA/NUMA captures are under `bootstrap/`; raw Terraform, EKS,
EC2, ASG, and image evidence is under `cloud/`.

## Teardown

Terraform completed successfully with 34 resources destroyed and an empty
state. A separate raw-AWS-CLI audit at `2026-08-21T02:23:26Z` found no pending
or running EC2 instances in any enabled US/EU region, no matching autoscaling
groups or EKS clusters, and no residual project ENIs, volumes, launch
templates, VPCs, Elastic IPs, placement group, or IAM roles. Both tested
`c8gn.48xlarge` instances were explicitly confirmed `terminated`; the dedicated
NAT gateway was confirmed `deleted`. See `cloud/teardown-audit.json`.
