# FIPS images and test environments

This is a **FIPS-aspiring application build**, not a validated CSI product or a
claim that all cryptographic services are approved. A passing runtime probe
does not establish certificate coverage. Never promote a lab report into a
CMVP validation statement.

For organizational requirements, deployment verification, and the SG-8 vendor
statement, see [zccusan FIPS overview](fips/FIPS-OVERVIEW.md).

## Image variants

| `FIPS_DISTRO` | Build and runtime | Scope |
| --- | --- | --- |
| `amzn2023` (default) | Amazon Linux 2023 userspace | Certificate-5314 candidate userspace; acceptance still requires a listed bare-metal node and approved operation |
| `ubuntu` | Ubuntu 22.04 userspace | Compatibility lab outside the selected certificate profile |
| `ubi9` | UBI9 userspace | Public-repository lab: provider checks and TCP/block paths only; no filesystem provisioning tools or RDMA |
| `rhel9` | UBI9 expanded with entitled RHEL9 packages | Full CSI tool set; entitled repositories required |

All variants compile with `--locked --features fips`, select AWS-LC for the
process-default rustls provider, and include `zc-fips-check`. They do not switch
to the OS OpenSSL implementation. Builder and runtime use the same distribution
libraries; the UBI variant no longer overlays Debian libraries onto UBI.
`FIPS_DISTRO` selects the final runtime. `FIPS_BUILD_DISTRO` defaults to the
same value; `rhel9` building for `ubi9` is the additional supported combination.

The Rust toolchain is copied from the `RUST_IMAGE` stage. The native crypto
module is supplied by `FIPS_PROVIDER_ROOT`; the container build never compiles
it. Create that directory with the exact procedure in the
[AWS-LC recompilation guide](fips/AWS-LC-RECOMPILATION.md). The build fails if
the package or its certificate-profile receipt is missing.

```sh
FIPS_DISTRO=amzn2023 FIPS_PROVIDER_ROOT=zccusan/deploy/zcblock-csi/fips/provider \
  zccusan/deploy/zcblock-csi/build-fips-image.sh
FIPS_DISTRO=ubuntu zccusan/deploy/zcblock-csi/build-fips-image.sh
FIPS_DISTRO=ubi9 zccusan/deploy/zcblock-csi/build-fips-image.sh
# On an entitled RHEL build host, using supported subscription mounts:
FIPS_DISTRO=rhel9 zccusan/deploy/zcblock-csi/build-fips-image.sh
```

Override `UBI_IMAGE`, `RHEL_IMAGE`, `UBUNTU_IMAGE`, and `RUST_IMAGE` with immutable
references for a recorded candidate. Use `KMOD_BUNDLE_ROOT` to supply the
architecture/kernel-specific client module bundle. Empty bundles are suitable
for provider probes only; full CSI node setup requires matching artifacts.
`CONTAINER_ENGINE` selects Podman or Docker. `FIPS_PODMAN_STORAGE=/tmp/zc-fips-podman`
keeps build storage off a full workspace filesystem. Scripts do not publish images.

### Thin UBI runtime with a separate builder

The UBI variant starts its final stage from a fresh UBI runtime and copies only
our executables, build evidence, and the supplied client-module bundle. Compiler
tools and entitled RHEL packages remain in the builder. Do not publish the
builder image or export its build cache to a public registry. Deleting packages
at container startup, or in a later image layer, does not remove their bytes
from earlier downloadable layers.

```sh
# Entirely public UBI package sources, including the compiler stage:
FIPS_DISTRO=ubi9 zccusan/deploy/zcblock-csi/build-fips-image.sh
# Optional entitled RHEL compiler stage, with the same thin UBI runtime:
FIPS_BUILD_DISTRO=rhel9 FIPS_DISTRO=ubi9 \
  zccusan/deploy/zcblock-csi/build-fips-image.sh
```

Both forms explicitly disable libfabric at compile time. Filesystem-formatting
tools and RDMA are absent from this TCP/block lab image; deleting `libfabric`
from a binary that links it would prevent that binary from loading. Full storage
functionality still needs redistributable runtime dependencies or separately
licensed deployment packaging. No crypto-provider stripping or startup package
deletion is used.

The UBI stages allow only named public UBI repositories, including when built
on an entitled RHEL host. The final UBI build rejects compiler tools and the
omitted storage packages and checks every packaged executable for missing shared
libraries. `image-stages.txt`, `runtime-packages.tsv`, and `runtime-linkage.txt`
under `/usr/share/zcutils/fips` record the stage selection and runtime contents.
These checks establish packaging properties, not a complete license audit:
copied executables, statically linked dependencies, and supplied kernel modules
must themselves be redistributable, with their notices/source obligations met.
Retain UBI's EULA and licenses. Container thinning does not establish FIPS
certificate coverage.

See [Docker multi-stage builds](https://docs.docker.com/build/building/multi-stage/),
[Red Hat's UBI package-repository guidance](https://docs.redhat.com/en/documentation/red_hat_enterprise_linux/9/html/building_running_and_managing_containers/assembly_adding-software-to-a-ubi-container_building-running-and-managing-containers),
and the [UBI redistribution FAQ](https://developers.redhat.com/articles/ubi-faq).

The image records Cargo.lock, dependency tree, compiler versions, binary hashes,
and its build-provider probe in `/usr/share/zcutils/fips`. Verify executable hashes
with `cd /usr/local/bin && sha256sum -c /usr/share/zcutils/fips/binaries.sha256`. Normal first-party
entrypoints require the guest/node kernel FIPS flag through
`ZC_REQUIRE_HOST_FIPS=1`. Setting it to `0` is an explicit provider-only lab
override; it is never accepted by the strict guest test.

```sh
podman run --rm --network none --entrypoint /usr/local/bin/zc-fips-check \
  localhost/zcblock-csi-fips-ubuntu:dev --require-fips
# On a genuine FIPS guest/node, also require the kernel mode:
podman run --rm --network none --entrypoint /usr/local/bin/zc-fips-check \
  localhost/zcblock-csi-fips-ubuntu:dev --require-fips --require-host-fips
```

Run the repeatable image smoke suite (provider mode, host-mode enforcement,
binary hashes, and isolated CSI listener startup):

```sh
python3 scripts/fips-image-smoke.py \
  --image localhost/zcblock-csi-fips-ubuntu:dev \
  --image localhost/zcblock-csi-fips-ubi9:dev --report /tmp/fips-images.json
# Add --storage /tmp/zc-fips-podman if built with FIPS_PODMAN_STORAGE there.
python3 scripts/test_fips_qemu_lab.py
```

The smoke suite's listener check explicitly overrides host enforcement inside
a container with no host mounts/devices/network. Its report marks actual FIPS
guest, OpenShift, and full storage acceptance as unrun.

## Debian outside, genuine FIPS guests inside

Debian can run the QEMU/KVM host and tooling. Its own FIPS status is not inherited
by guests. Whether the outer virtualization environment is covered by a chosen
certificate is a separate review. The harness does not certify Debian.

```sh
python3 scripts/fips-qemu-lab.py preflight --target ubuntu --report /tmp/fips-preflight.json
```

Prerequisites for a standalone guest:

* Writable `/dev/kvm`, QEMU, `qemu-img`, SSH/SCP, and adequate free storage/RAM.
* A standalone BIOS-bootable installed qcow2 Ubuntu or RHEL guest, with vendor FIPS packages
  configured, rebooted, and `/proc/sys/crypto/fips_enabled` equal to `1`.
* Guest Podman and an SSH account with noninteractive sudo for the lab commands.
  Use an RSA SSH key of at least 3072 bits for broad FIPS guest compatibility.
* A guest provisioned according to its vendor's procedure. Do not fake the
  kernel flag, use a host-built replacement kernel, or treat generic Ubuntu
  cloud images as already FIPS enabled.

Ubuntu requires a Pro account; RHEL requires appropriate subscription access.
Tokens and pull secrets belong in vendor provisioning workflows, never in this
repository, container build arguments, committed cloud-init, or reports.

```sh
# Keep this running in its own terminal. Base is never modified.
python3 scripts/fips-qemu-lab.py boot --target ubuntu \
  --base /path/ubuntu-fips.qcow2 --sha256 <base-sha256> \
  --work-dir /path/fips-ubuntu-run --ssh-port 2244

podman save --format oci-archive -o /tmp/zc-fips-ubuntu.tar localhost/zcblock-csi-fips-ubuntu:dev
podman image inspect localhost/zcblock-csi-fips-ubuntu:dev --format '{{.Id}}'
python3 scripts/fips-qemu-lab.py guest --target ubuntu \
  --work-dir /path/fips-ubuntu-run --ssh-port 2244 --user lab \
  --ssh-key /path/lab-rsa --archive /tmp/zc-fips-ubuntu.tar \
  --image-id sha256:<image-id> --report /tmp/fips-ubuntu.json
```

Repeat with `--target rhel` and the matching image archive. UBI is a container,
not a QEMU guest. The guest probe checks image content identity, the real kernel
flag, AWS-LC mode, and TLS provider mode. It does **not** exercise provisioning,
replication, CSI sidecars, or every cryptographic service.

## Temporary RHEL runners on EC2

An official **subscription-included RHEL AMI** provides RHEL and regional RHUI
package access through the AWS bill. A separately purchased Red Hat subscription
or Red Hat login is not required for this option. Choose the subscription-included
image rather than a BYOL/Cloud Access image. EC2 RHEL usage is billed per second
with a 60-second minimum; instance time, storage, public IPv4, and applicable
data transfer still cost money. This subscription does not supply an OpenShift
subscription or pull secret, or entitlement to run the EC2 image outside AWS.

For a short host test, use `scripts/ec2_perf_spot.py launch` with an explicit
Red Hat-owned RHEL 9 AMI, one small x86_64 instance, an existing SSH security
group/subnet, a temporary RSA key, one network interface, `--no-enable-efa`,
an absolute `--drop-dead-utc`, `--max-spot-price`, and `--max-total-cost`.
Keep the helper's termination tags and root-volume `DeleteOnTermination`.
Verify the subnet's internet route is active as well as checking SSH ingress;
automatic public-IP assignment alone does not establish connectivity. Spot
capacity can be unavailable in an individual availability zone. The existing
helper launches Spot only. An On-Demand fallback must preserve its run/deadline
tags, root-volume deletion, and spending checks, using an independently checked
On-Demand price. The discovery test used the helper's reviewed request as that
template; it did not add an On-Demand option to the shared Spot helper.

Create an independent one-time EventBridge Scheduler target immediately after
the instance ID is known and verify it before bootstrapping the workload:

```json
{
  "Arn": "arn:aws:scheduler:::aws-sdk:ec2:terminateInstances",
  "RoleArn": "arn:aws:iam::ACCOUNT:role/temporary-run-termination",
  "Input": "{\"InstanceIds\":[\"i-EXACT_INSTANCE_ID\"]}"
}
```

Use `at(UTC_TIMESTAMP)`, timezone `UTC`, flexible window `OFF`, bounded retries,
and `ActionAfterCompletion=DELETE`. The execution role may terminate only this
run's instance, using its ARN and/or a unique run-tag condition. Restrict the
role's trust to `scheduler.amazonaws.com`, the account, and the **schedule group**
ARN; AWS does not accept a specific schedule ARN for this trust condition.
If the guard cannot be created, terminate the new instance before continuing.
Scheduler operates at minute precision and API retries can delay termination;
retain normal job cleanup and the existing independent ad hoc sweeper as well.
Remove the temporary key, schedule, and role only after termination is verified.

An ordinary RHEL AMI initially boots without FIPS mode. For the disposable probe,
enable it with `sudo fips-mode-setup --enable`, reboot, and verify both
`fips-mode-setup --check` and `/proc/sys/crypto/fips_enabled` before building:

```sh
# Inside the disposable RHEL 9 guest, after the FIPS reboot:
sudo bash scripts/rhel-fips-ec2-probe.sh /var/tmp/zc-fips-probe
```

The x86_64 probe installs a pinned vendor package set from Red Hat's public UBI
repository, checking download hashes and requiring RPM signature verification.
It uses OpenSSL 3.2.2-6.el9_5.1 with provider/provider-so 3.0.7-6.el9_5, prevents
the compiler/runtime installation from replacing that OpenSSL set, and verifies
both provider packages, including the package containing the actual module.
It builds a small C program and tests SHA-256, random generation, and rejection
of an MD5 fetch with `fips=yes`. The active module must report version
`3.0.7-395c1a240fbfffd8`, which Red Hat maps to certificate 4857.

The probe repeats the program inside a UBI 9.6 container pinned by digest and
checks Podman's automatic FIPS policy. The output contains the executable,
source, hashes, vendor RPMs, package/module versions, container identity, and a
JSON report. It does not build zcutils or exercise a GitHub Actions runner.
This package set is a reproducible module experiment; selecting maintained
packages for a production release remains a vendor-support and certificate
mapping decision.

The initial EC2 test found that installing the current default repository
packages selected OpenSSL 3.5.5 and provider 3.0.7-11.el9_8, whose active module
reported `3.0.7-cda111b5812c30d4`. The probe rejected that version instead of
treating its FIPS mode as certificate evidence. Newer OpenSSL libraries also
require the newer provider packaging, so pinning only the provider is insufficient
for this particular experiment. Algorithm-test listings for another module
version do not establish a CMVP module-certificate mapping.

The 2026-09-13 EC2 smoke run passed on a subscription-included RHEL 9.6
`m6i.large` (2 vCPUs, 8 GiB). Both host and container reported the required
provider version and passed the program's checks. The artifact was downloaded
and its hashes verified. After moving the independent schedule forward, AWS
terminated the instance at the requested time; the temporary instance, disk,
network, SSH key, schedule, and role were cleaned up. Local evidence and the
one-off orchestration scripts are under `target/fips-ec2-smoke/`, with the
combined result in `report.json`. GitHub runner registration and a full CSI
build were not part of this smoke run.

For a production runner image, provision according to Red Hat's FIPS guidance
and ensure workload keys are created after FIPS is active. Switching the lab AMI
after first boot does not retroactively establish how pre-existing keys were
generated. A tested provider version and a kernel flag alone do not establish
approved use for every operation or coverage of an EC2 operational environment.

The [Terraform bootstrap](../github-ec2-runner/README.md) provisions the OIDC
controller role, restricted Scheduler role/group, and a separate nondefault
public VPC for these workers. It creates no EC2 instances or per-run schedules.
Its example allows the pinned RHEL AMI and `m6i.large` in `us-east-1`; it does
not change the default or ad hoc HPC networks. The empty stack has no standing
hourly charge; workers allocate public IPs and disks only at runtime.

The GitHub Actions integration can use three jobs:

1. A controller job assumes a restricted AWS IAM role using GitHub OIDC, launches
   the RHEL instance, establishes its independent termination schedule, verifies
   host mode, and registers a runner with a unique label for this workflow run.
2. The build job targets that label, runs the provider and application checks,
   and uploads its artifact, logs, package inventory, image digest, and evidence
   using `actions/upload-artifact` before the runner exits.
3. A controller cleanup job with `if: always()` terminates the exact instance and
   removes its runner registration and temporary resources. The AWS schedule
   remains the fallback when GitHub is canceled or unavailable.

Register the worker as an ephemeral or just-in-time runner so it accepts one job.
Runner deregistration does not destroy EC2. Use a GitHub App or other suitably
scoped registration credential; AWS OIDC credentials alone cannot register a
GitHub runner. Keep the App private key on the controller and deliver only the
short-lived registration material to the worker. Restrict AWS trust to the
actual repository/ref or protected environment OIDC subject, and restrict
`iam:PassRole` to the intended worker/termination roles. Allow only trusted
workflows onto this runner; do not expose its credentials to arbitrary fork jobs.
The worker does not need the controller's EC2 launch or IAM administration rights.

This gives us a disposable FIPS-enabled build/test host. The application still
needs the module mapping and approved-service work listed below; running a Rust
build on RHEL does not redirect its embedded AWS-LC crypto to Red Hat OpenSSL.

References:

* [AWS RHEL subscription and billing FAQ](https://aws.amazon.com/partners/redhat/faqs/)
* [Red Hat FIPS activation and container guidance](https://docs.redhat.com/en/documentation/red_hat_enterprise_linux/9/html/security_hardening/switching-rhel-to-fips-mode_security-hardening)
* [GitHub OIDC with AWS](https://docs.github.com/en/actions/how-tos/secure-your-work/security-harden-deployments/oidc-in-aws)
* [GitHub ephemeral runner lifecycle](https://docs.github.com/en/actions/reference/runners/self-hosted-runners)
* [Scheduler universal API targets](https://docs.aws.amazon.com/scheduler/latest/UserGuide/managing-targets-universal.html)
* [Scheduler execution-role trust conditions](https://docs.aws.amazon.com/scheduler/latest/UserGuide/cross-service-confused-deputy-prevention.html)

## OpenShift

Actual FIPS-enabled OpenShift needs an OpenShift pull secret, an appropriate
RHEL FIPS installer environment, `fips: true` **before installation**, and
RHCOS/RHEL nodes. An ordinary OKD or CRC cluster is not substituted. The existing
`scripts/okd-sno-qemu.sh` remains an independent non-FIPS compatibility lab.

Provision the OpenShift cluster using its version's supported installation
procedure and cluster networking; the standalone guest runner's SSH-only user
network is not an OpenShift cluster installer. An outer Debian host can run the
VMs; run the OpenShift installation program inside the required RHEL environment.

Install the candidate CSI chart by digest with the correct kernel bundle and
the cluster's required SCC permissions. This document does not approve ordinary
upstream CSI sidecar images for a FIPS release. Then collect read-only evidence:

```sh
python3 scripts/fips-qemu-lab.py openshift --context local-fips-openshift \
  --namespace zcblock-csi --image-digest sha256:<manifest-digest> \
  --report /tmp/fips-openshift.json
```

Every node must have one running selected CSI pod. Mixed architectures require
separate per-architecture checks/digests or an extension to the report format;
the present command expects one platform manifest digest. Missing pods, wrong
digests, non-RHEL nodes, and failed provider checks fail the command.

## Acceptance check suite

`scripts/fips-acceptance.py` implements three release gates. Run `check` on the
actual deployment node (or inside the guest being assessed), with local Podman
and Python 3.9 or newer. It never starts EC2 instances or changes host FIPS mode.
It inspects the image once and uses its immutable ID for every subsequent run.
FAIL and missing evidence (BLOCKED) both exit **1**. Only a complete acceptance
record exits **0**; that is a project acceptance decision, not a CMVP certificate.

| Gate | Automated evidence | Required reviewed evidence |
| --- | --- | --- |
| Module | Current CMVP status and sunset; exact runtime module identity in all 16 executables; original vendor archive hash and source-tree comparison; binary hashes and static identity symbols; source/build-recipe binding; native FIPS options and tool versions | Security Policy build/link procedure, including any generated source, symbol-prefix changes, compiler options and module boundary considerations |
| Services | Module self-tests and integrity test in each executable; approved-service counters for SHA-256, HMAC, RNG and AES-GCM; rejection of modified ciphertext; negative controls for external-nonce GCM and standalone SHA-256; real application RNG, key derivation and native/RPC frame operations; dependency/feature and source inventory | Complete security-service map, including actual TLS configurations, legacy CSI/tonic TLS reachability, key/nonce/entropy rules, signatures, optional features, and non-security checksum dispositions |
| Operation | Actual node OS, container OS, kernel FIPS mode, boot identity, virtualization, hardware identity and the selected platform list | Operating procedures and deployed workload/sidecar scope, including entropy, key lifecycle, failure handling and restrictions in the Security Policy |

The initial profile is
[`fips/acceptance-5314.json`](fips/acceptance-5314.json), a **candidate target**
based on [CMVP certificate #5314](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5314)
and its [Security Policy](https://csrc.nist.gov/CSRC/media/projects/cryptographic-module-validation-program/documents/security-policies/140sp5314.pdf).
It expects AWS-LC FIPS 3.1.0, Amazon Linux 2023 userspace, and one of the listed
bare-metal platforms. It does not silently extend that coverage to UBI/RHEL,
Ubuntu, OpenShift, virtual EC2 instances or QEMU. Requiring the node FIPS flag is
an additional project deployment rule. Portability or a different vendor module
needs a separately reviewed profile and policy basis.

The FIPS build pins `aws-lc-rs = 1.15.3` and the FIPS 3.1 ABI at
`aws-lc-fips-sys = 0.13.11`. A local provider adapter replaces that sys crate's
native build while preserving its Rust API. It contains no AWS-LC source and
requires the unmodified `libcrypto.a` produced by the certificate 5314 section
11.1 commands. Ordinary Cargo builds fail closed unless
`AWS_LC_FIPS_SYS_SYSTEM_DIR` points at a valid provider package.

The concrete [AWS-LC recompilation guide](fips/AWS-LC-RECOMPILATION.md) provides
a checked runner for the literal section 11.1 native build and packages its
output without rebuilding it. The adapter verifies the provider receipt and
artifact hashes, uses the exact upstream 0.13.11 bindings with unprefixed
symbols, and statically links the recorded library. Acceptance rejects Cargo
CMake output, prefixed AWS-LC symbols, dynamic crypto libraries, receipt/hash
mismatches, and provider builds outside a certificate-listed environment.
The adapter and boundary still require qualified review; passing the mechanical
checks does not itself grant certificate coverage.

FIPS builds now use module RNG, SP 800-108 Counter HMAC-SHA256 key derivation,
and the module's internally generated GCM IV service. The application probe
isolates encryption from key derivation to prevent a successful KDF from
masking a non-approved encryption call. See [the crypto integration guide](fips/CRYPTO-INTEGRATION.md)
for wire versions, migration, coverage and outstanding deployment evidence.

For a local assessment, including an explicit BLOCKED result for the skipped
online certificate check:

```sh
python3 scripts/fips-acceptance.py check \
  --image localhost/zcblock-csi-fips-ubi9:dev \
  --offline \
  --report target/fips-acceptance/report.json
```

Add `--storage /tmp/zc-fips-podman` when using the existing isolated local image
store. The normal online check only contacts the CMVP certificate page. Supply
the original archive identified by section 11.1 with
`--validated-source /path/to/AWS-LC-FIPS-3.1.0.zip`; hashing and source comparison
do not extract or execute its contents. Missing archives block acceptance.

Each run writes a JSON report and a neighboring `*.review-template.json`. The
template is intentionally incomplete. A qualified reviewer must supply their
name, review/expiry dates, dispositions and rationales for every inventory
finding, and four actual review documents under the review bundle directory:
`build_procedure`, `crypto_service_map`, `operating_policy`, `deployment_scope`.
Each document entry has a relative `path` and its `sha256`. The record binds the
image ID/digest, source, build receipt, profile and node environment. Changed
inputs, omitted findings, altered documents, or expired reviews fail. The
90-day maximum review lifetime is a project rule, not a NIST requirement.

Pass the completed record with `--review /path/to/review.json`. Review records
and policy profiles must be controlled by trusted repository/CI review: a name
in JSON is **not** signature verification or independent accreditation. The
checker does not validate a review document's reasoning. Known code failures,
bad module versions, unsupported environments and failed tests cannot be
overridden by a review record. The source inventory is conservative; an empty
scan is not proof that all cryptography has been found.

The image build now embeds `/usr/share/zcutils/fips/build-receipt.json`. It
contains hashes of the actual source and executables, the resolved dependency
features, application toolchain, provider receipt and final static-link
evidence. The provider receipt separately binds the literal module build,
native toolchain, complete source manifest, `libcrypto.a`, and `bcm.o`.
Missing or mismatched receipts require rebuilding.

For a build that must pass acceptance before the wrapper returns success:

```sh
FIPS_ACCEPTANCE=1 \
FIPS_VALIDATED_SOURCE=/path/to/AWS-LC-FIPS-3.1.0.zip \
FIPS_ACCEPTANCE_REVIEW=/path/to/review.json \
FIPS_DISTRO=amzn2023 \
FIPS_PROVIDER_ROOT=zccusan/deploy/zcblock-csi/fips/provider \
  zccusan/deploy/zcblock-csi/build-fips-image.sh
```

UBI and Ubuntu compatibility builds exit nonzero under the selected
certificate-5314 environment profile. A release/publish job must require the
acceptance exit status and retain its report with the exact image digest.

`<shipped-executable> --fips-evidence` runs the bounded module controls before
CLI/service startup. `zcutils --fips-application-evidence` calls the real shared
library's native and RPC crypto functions with disposable, in-memory data. The
controls deliberately include non-approved operations and belong in a separate
diagnostic process. A changed service counter only proves that an approved
service ran during that call; mixed operations still need the reviewed service
map. The application checks isolate native encryption from key derivation to
prevent an approved KDF masking a non-approved encrypt operation.

These checks use no application secrets, network, devices or writable mounts.
Containers are read-only, have resource/time limits and are removed by their
own container IDs, including on timeout. Functional storage and TLS handshake
acceptance, cluster sidecars and operating procedures remain separate evidence
requirements; a provider test does not establish those facts.

Run the regression tests with:

```sh
python3 -m unittest discover -s scripts -p 'test_fips*.py' -v
```

`.github/workflows/fips-acceptance-tests.yml` runs those regressions on pull
requests and manual dispatch. Its successful fixtures are synthetic and test
the decision logic only. GitHub-hosted CI passing these tests is not acceptance
of a FIPS deployment. The existing QEMU and OpenShift lab checks continue to
report runtime compatibility and mode; this stricter suite is the acceptance
gate.

The acceptance implementation, certificate profile, and AWS-LC recompilation
guide have a combined SHA-256 review stamp in the header of
`scripts/test_fips_acceptance.py`. That header also contains the normalized
test-file SHA-256 and review date. The test normalizes only those three comment
values to avoid a self-referential digest, verifies both hashes, and requires a
matching latest entry in `fips/acceptance-review-history.json`. When criteria
or tests change, append a review entry with a later UTC timestamp and update all three
header values after reviewing that every changed criterion has a corresponding
test. Do not rewrite prior review entries.

## Remaining validation gates

* Obtain qualified review of the custom Rust FFI adapter, static link, module
  boundary, and exact provider/build receipt against certificate 5314 and its
  current Security Policy.
* Audit approved services: native AES-GCM nonce management, standalone `sha2`
  key derivation/hashing, direct OS randomness, signing, and older TLS dependencies.
  The current feature does not replace those paths or establish their approval.
* Audit all sidecars and enabled transports, then run full storage/replication
  acceptance on each intended image/node combination.
* Record package/module versions, image digest, node OS/kernel/architecture,
  hypervisor conditions, module certificate, and approved configuration together.

Sources checked 2026-09-12:

* [rustls FIPS integration](https://docs.rs/rustls/0.23.43/rustls/manual/_06_fips/index.html)
* [AWS-LC build and validation information](https://github.com/aws/aws-lc/blob/main/crypto/fipsmodule/FIPS.md)
* [Ubuntu FIPS installation](https://documentation.ubuntu.com/security/compliance/fips/how-to-install-ubuntu-with-fips/)
* [OpenShift installation requirements](https://docs.redhat.com/en/documentation/openshift_container_platform/4.21/html-single/installation_configuration/installation_configuration)
* [Red Hat validation inventory](https://access.redhat.com/compliance/fips)
