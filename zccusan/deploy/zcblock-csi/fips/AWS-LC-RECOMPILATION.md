# AWS-LC certificate 5314 recompilation guide

This guide separates reproduction of the validated module build from compiling
the zcutils Rust application. It applies to the **static AWS-LC FIPS 3.1.0
module on CMVP certificate 5314**. It does not extend the certificate to another
module version or operational environment.

The controlling document is the certificate's
[Security Policy](https://csrc.nist.gov/CSRC/media/projects/cryptographic-module-validation-program/documents/security-policies/140sp5314.pdf),
especially sections 2.2, 2.4, 6, 10, and 11.1. Check the live
[certificate record](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5314)
for status and policy revisions before every release.

## What section 11.1 requires

1. Build the complete, unchanged `AWS-LC-FIPS-3.1.0.zip` source archive. Its
   SHA-256 must be
   `fe408fa438850786396faf79eba9ea4116c3802e60f3a95865f0dd2adb64c9f1`.
2. On Amazon Linux 2023, install the `Development Tools` group, `cmake3`, and
   `golang` as directed by the policy.
3. From a newly extracted source tree, run these commands without adding
   options:

   ```sh
   mkdir build
   cd build
   cmake3 -DFIPS=1 ..
   make
   ```

4. Require `./tool/bssl isfips` to print `1`.
5. Require `awslc_version_string()` to return `AWS-LC FIPS 3.1.0` and confirm
   that a statically linked application defines one `T awslc_version_string`
   symbol.
6. Run on one of the tested environment pairs when claiming the certificate's
   tested profile:

   | OS | Architecture | Hardware |
   | --- | --- | --- |
   | Amazon Linux 2023 | x86_64 | `c6i.metal`, Intel Xeon Platinum 8375C |
   | Amazon Linux 2023 | aarch64 | `r8g.metal-24xl`, Graviton4 |

The policy lists both PAA/PAI states for each platform. It lists no
vendor-affirmed operational environments. Building elsewhere can be useful as
a rehearsal, but it is not the tested certificate profile.

After installing the prerequisites exactly as the policy directs, the checked
runner performs steps 1 and 3 through 5 and writes hashes and environment
evidence:

```sh
python3 scripts/fips-recompile-aws-lc.py \
  --archive /path/to/AWS-LC-FIPS-3.1.0.zip \
  --work-dir /var/tmp/aws-lc-5314-build-001 \
  --provider-dir "$PWD/zccusan/deploy/zcblock-csi/fips/provider" \
  --report /var/tmp/aws-lc-5314-build-001.json
```

Both the work and provider directories must be new. The provider directory is
a package of the already-built headers, `libcrypto.a`, `bcm.o`, and a receipt;
creating it does not rebuild or change the native module. Keep it inside the
container build context but out of source control. The script refuses an
unlisted environment by default. `--allow-untested-environment` permits a local
rehearsal and records `certificate_profile_environment: false`; the Rust adapter
and acceptance collector reject that package for a tested-profile build.

## Why the ordinary Cargo build is not used

The pinned `aws-lc-rs 1.15.3` resolves `aws-lc-fips-sys 0.13.11`, which contains
AWS-LC FIPS 3.1.0. Its ordinary Cargo build does not execute the section 11.1
procedure literally. It enters through the sys crate's wrapper CMake project
and changes the native configuration:

| Section 11.1 build | Current `aws-lc-fips-sys` build |
| --- | --- |
| CMake source directory is the unchanged AWS-LC root | CMake source directory is the Rust sys-crate wrapper |
| No symbol prefix option | Adds `BORINGSSL_PREFIX=aws_lc_fips_0_13_11_` |
| No build type option | Adds `CMAKE_BUILD_TYPE=release` for a release Cargo build |
| AWS-LC defaults build the tool, libssl, and tests | Sets `BUILD_TOOL=OFF`, `BUILD_LIBSSL=OFF`, and `BUILD_TESTING=OFF` |
| Compiler flags come from the policy command and platform defaults | Cargo/cmake-rs adds section, PIC, architecture, and source-path flags |

Restoring the complete archive inside the crate proves source equivalence. It
does not make these command and configuration differences disappear. A working
`FIPS_mode()`, integrity self-test, version string, or service indicator also
does not prove that the section 11.1 installation procedure was followed.

The acceptance collector records that bundled Cargo path as
`not-section-11.1` and treats it as a module-gate failure. A reviewer cannot
waive that known procedural difference merely by signing the review JSON.

## Path to a Rust application using the exact module build

The repository now supplies `vendor/aws-lc-fips-sys-provider`, a small FFI
adapter outside the module boundary. Cargo patches `aws-lc-fips-sys 0.13.11` to
this adapter so `aws-lc-rs 1.15.3` keeps its matching FIPS 3.1 API while the
native implementation comes only from the provider package. The adapter:

1. has no AWS-LC C source and no bundled-build fallback;
2. requires an absolute `AWS_LC_FIPS_SYS_SYSTEM_DIR` and a valid provider
   receipt for certificate 5314, FIPS 3.1.0, the exact archive, exact two policy
   commands, and a certificate-profile environment;
3. verifies the packaged `libcrypto.a` and `bcm.o` hashes before linking;
4. derives unprefixed declarations from the exact upstream 0.13.11 generated
   bindings without renaming the native module;
5. links the recorded `libcrypto.a` statically and forces a pre-`main` integrity
   and FIPS-mode check into each executable; and
6. exposes the provider receipt, library, and `bcm.o` hashes in runtime evidence.

The image build refuses to run without this package. It copies the package into
the isolated builder, sets `AWS_LC_FIPS_SYS_SYSTEM_DIR`, and never invokes the
old source-restoration/CMake route. Build it with:

```sh
FIPS_DISTRO=amzn2023 \
FIPS_PROVIDER_ROOT=zccusan/deploy/zcblock-csi/fips/provider \
  zccusan/deploy/zcblock-csi/build-fips-image.sh
```

The build receipt binds the provider receipt, adapter and application sources,
tool versions, final executable hashes, and runtime provider hashes. Acceptance
requires every executable to define the unprefixed `awslc_version_string`, to
contain no 0.13.11-prefixed AWS-LC identity, and to have no dynamic OpenSSL or
libcrypto dependency. It then repeats module identity, integrity, self-test,
approved-service, and application call-path checks against every executable.

Upstream `aws-lc-fips-sys 0.13.11` has no supported external-library mode. The
newer upstream system-library interface targets the FIPS 4 module generation,
so it is not substituted for certificate 5314. This local adapter is custom
integration code outside the module boundary. A qualified reviewer or CMVP lab
must confirm the boundary and linking rationale; the automated checks establish
artifact identity and fail-closed behavior, not that legal conclusion. The
frozen 3.1.0 source also has published security advisories that require a
release disposition.

Until that review exists and all service/operation gates pass on the deployed
node, the image remains a FIPS evaluation candidate and must not be labeled a
certificate-covered build.

## Ephemeral certificate-profile build

`.github/workflows/fips-aws-lc-5314.yml` launches one `c6i.metal` worker from a
pinned AWS-owned Amazon Linux 2023 AMI. The launch job verifies the live AMI
owner, name, architecture and state. The worker then verifies its IMDS AMI and
instance IDs, DMI product/vendor, processor, and OS release before doing any
build work. It downloads the policy URL, verifies the policy hash, runs the
checked native reproduction, builds `zc-fips-check` against the resulting
provider, and uploads the provider, linked binary and receipts.

Terraform only limits which capacity the GitHub role may launch. It is not part
of the FIPS evidence. The deployed role requires the shared ad hoc cleanup tags
on the instance, volume and network interface at creation, including
`adhocKeepaliveModeAction=terminate` and `adhocKeepalive=ExpiresAt`. The launcher
also verifies a per-run termination schedule before the self-hosted job starts,
and the guest has an independent shutdown timer.

From an administrative GitHub CLI session, mint the one-job JIT registration,
dispatch the workflow, download its artifacts and verify their hashes with:

```sh
python3 scripts/test-github-ec2-runner.py --fips-build
```

This incurs charges only while the worker, EBS volume and automatic public IPv4
address exist. It does not perform the qualified boundary/service review or the
deployment-node acceptance gate.

Retain the source archive, report, complete build logs, module and final-binary
hashes, package inventory, image digest, and deployment-environment evidence
for each release. Re-run the procedure separately for each processor/OS
combination; an artifact from one combination is not evidence for another.
