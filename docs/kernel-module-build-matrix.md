# Kernel-module build matrix

`zcnblk_client_mod.ko` is a kernel-ABI artifact, not an instance-type artifact.
The build matrix therefore keeps two independent locks:

- [`kmods/matrix/targets.json`](../kmods/matrix/targets.json) pins exact kernel
  releases, container image digests, header packages, upstream tool revisions,
  and the authoritative pages used to select them.
- [`kmods/matrix/machine-profiles.json`](../kmods/matrix/machine-profiles.json)
  maps representative ordinary, Arm, multi-NIC, and RDMA machine profiles to
  candidate ABI artifacts. The loader must still require exact architecture and
  `uname -r`; a machine profile is never sufficient evidence by itself.

The current lock was researched on 2026-08-27 UTC. It covers:

| Platform | Current pin | Architectures |
|---|---|---|
| EKS AL2023, newest minor | EKS `1.36.2-20260818`, release `v20260818`, Linux `6.18.41-94.142.amzn2023` | amd64, arm64 |
| EKS AL2023, maintained minor | EKS `1.35.6-20260818`, release `v20260818`, Linux `6.12.100-125.179.amzn2023` | amd64, arm64 |
| GKE Regular COS | GKE `1.35.6-gke.1641000`, COS `cos-125-19216-395-138`, Linux `6.12.85+` | amd64, arm64 |
| AKS default Ubuntu | node image `202607.29.0`, Ubuntu 24.04 Azure kernel `6.8.0-1063-azure` | amd64 |
| Debian stable | Debian `13.6`, Linux `6.12.105+deb13` generic and cloud-image kernels | amd64, arm64 |
| UBI 10.2 / EL10 | UBI 10.2 toolchain, CentOS Stream 10 Linux `6.12.0-264.el10` | amd64, arm64 |
| NixOS stable | NixOS `26.05.8477.062346a6d85b`, Linux `6.18.46` | amd64 |

The source URLs in the lock are part of the review record. Do not update a pin
from memory: consult the current provider release table, select the desired new
cluster default or stable channel, record the exact digest/package NVR, and run
the complete build before merging the lock change.

## Local builds without cloud resources

List or build exact targets with Docker or Podman:

```console
scripts/zccusan-build-kmod-matrix.sh --list

scripts/zccusan-build-kmod-matrix.sh \
  --target eks-al2023-1.36-amd64 \
  --engine podman \
  --cache /mnt/bulk_data/zc-kmod-matrix/cache \
  --output /mnt/bulk_data/zc-kmod-matrix/dist
```

`--all` builds the full lock. Arm Debian, EKS, and UBI images require native
Arm or local binfmt/QEMU support. GKE Arm uses the exact cross-toolchain and
headers published with the Arm COS release, so its development container runs
on amd64. No command in the builder creates a VM, Kubernetes cluster, or other
cloud resource.

Each successful directory contains the module, its SHA-256, vermagic, exact
target fragment, source hashes, toolchain/package evidence, and a small
`metadata.env`. The build fails if the module name or vermagic does not match
the lock.

`scripts/zccusan-package-daemonset-kmods.sh` re-audits those artifacts and
creates one architecture-local image root. CI copies that root into the normal
`zcblock-csi` image at `/opt/zcutils/kmods`; it does not publish or require a
second kernel-module image.

The source bundle carries both a convenience `Makefile` and a declarative
`Kbuild`. Linux 6.18 and later can generate an output `Makefile` while setting
up an external-module build; keeping `obj-m` in `Kbuild` prevents that generated
file from erasing the module declarations.

## Platform details

GKE's official `cos_kernel_devenv` helper is pinned by Git revision. The public
Arm release directory uses `toolchain.tar.xz.gcs`, while the helper's custom
bucket path expects `toolchain_path`. The reviewed patch in
[`cos-kernel-devenv-arm-bucket.patch`](../kmods/matrix/cos-kernel-devenv-arm-bucket.patch)
accepts both official descriptor layouts; it does not select an unpinned
toolchain.

UBI is a userspace container base and does not define the host kernel that will
load a module. The UBI targets intentionally pair the exact current UBI 10.2
toolchain image with an explicit EL10-compatible CentOS Stream 10 host ABI.
They do not claim that every UBI workload runs that kernel. Production module
selection remains an exact node-kernel match.

RDMA does not change the `zcnblk_client_mod` ABI. EFA, Cloud RDMA/iRDMA, and
InfiniBand examples live in the machine-profile lock because transport
libraries and NIC setup are userspace/node concerns. They only reuse a module
artifact after the exact kernel check succeeds.

The current EKS standard, NVIDIA, and Neuron AMI types are recorded on each
target. Where several AMI types use the same architecture and exact kernel ABI,
they intentionally consume one module artifact. EKS 1.35 and 1.36 have distinct
kernel ABIs, so both are independent build targets for amd64 and arm64.

## CI and pin refreshes

[`zcnblk-kmod-matrix.yml`](../.github/workflows/zcnblk-kmod-matrix.yml) builds
all twelve targets on changes to the module, lock, recipe, or Helm copy. The CI
job list is derived directly from `targets.json`, so a target cannot be added to
the lock without joining the build. It also runs weekly so removed packages or
changed provider publication layouts fail visibly. amd64 COS cross-compiles the
GKE Arm target; the other Arm jobs use native GitHub Arm runners.

[`zcblock-csi-images.yml`](../.github/workflows/zcblock-csi-images.yml) runs the
same locked builds before publishing. Its amd64 and arm64 jobs download all
matrix outputs, reject missing, stale-source, wrong-ELF, wrong-vermagic, or
wrong-lock artifacts, and bundle only the matching architecture into the same
multi-architecture DaemonSet image that contains the CSI and Rust node loader.

To refresh the lock:

1. Read the current provider/stable-channel sources and update
   `researchedAt`, release identifiers, digests, package versions, kernel
   releases, and URLs together.
2. Confirm every machine profile still points at a compatible candidate ABI.
3. Build all targets locally or dispatch the complete CI matrix.
4. Inspect every `vermagic.txt` and `target-lock.json`; do not accept a nearby
   kernel release.
5. On a real node, the loader must independently compare the artifact with
   `uname -m` and `uname -r` before loading it.
