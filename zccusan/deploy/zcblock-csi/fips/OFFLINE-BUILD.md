# Offline FIPS compilation

The canonical build prepares its inputs online, compiles the native AWS-LC
provider and all application executables without network access, then assembles
and signs the release. Input acquisition, GitHub orchestration, and publication
still use the network. This is not an air-gapped end-to-end workflow.

## Compilation

`scripts/fips-reproducible-provider.py` invokes the prescribed AWS-LC procedure
inside a new network namespace containing only loopback. An unprivileged
runner creates it through a user namespace; passwordless sudo is unnecessary. The source archive is
verified before execution. The resulting provider package is the input to the
application build; Cargo does not recompile the native module.

The compiler consumes the provider headers, objects, and stable provider receipt.
That receipt binds source, tools, platform, procedure, and packaged module hashes;
its hash is embedded in the application. Archive member timestamps are zeroed
without changing any other bytes. The full `provider-build-record.json`
retains the timestamp, boot ID, and intermediate-output hashes and enters the
image during assembly. Those per-run observations do not change the executable.
The original archive is retained with that record. Acceptance verifies the
normalized linker input against the original, allowing only timestamp changes.

`Dockerfile.fips` prepares OS packages, the Rust toolchain, and locked source
dependencies in `build-inputs`. Cargo's download cache seeds a complete copy of
those dependencies in `/opt/cargo-inputs`; compilation does not mount that cache
or reuse compiled outputs. `offline-build` starts with an empty target directory
and runs Cargo with `--frozen` under `RUN --network=none`. Its manifest records
the source/compiler identity, provider hash, observed network interfaces, and
SHA-256 of every shipped executable.

The final image takes its executables from `offline-build`. Before publication,
the image tooling extracts the actual packaged executables and verifies their
hashes against `offline-build.json`. This guards selection of the offline
outputs; it does not perform an online rebuild.

Tests and acceptance-report collection happen after compilation. SBOM generation,
signing, and publication happen after image assembly. Workflow files and
acceptance collectors needed to document the build are copied into the assembly
stage, rather than into the compiler's input stage.

## Independent reproduction and authorized signing

Release checks have two separate scopes:

* **Independent payload reproduction:** an end-user organization verifies the
  published release signature using a trusted public key, extracts the payload,
  and rebuilds from the published inputs and effective build timestamp. The
  comparison covers executable bytes and explicitly identified deterministic
  provider artifacts. No private signing key or vendor participation is needed.
  Unwrapping an artifact must preserve the binding to the authenticated release;
  normalization may change only the documented archive metadata, never object
  contents, member names, ordering, or duplicates.
* **Authorized release signing:** the signing authority repeats the payload
  comparison, signs the same identified payload with its protected key, and
  verifies that the resulting signatures cover the expected content and key
  identity. Changed content and signatures from another key must be rejected.
  This exercises release authorization; it does not imply identical signature
  bytes or identical complete signed bundles.

The current AWS KMS key uses P-256 ECDSA. Fresh signatures must not be treated as
deterministic build outputs. Public verifiers can verify and retain existing
signatures, but creating a fresh authorized signature requires access to the
signing service. Payload comparison must report its exact scope separately from
signature verification; neither result extends a CMVP certificate.

The image tooling now emits `zcblock-csi-<variant>.unsigned-executable-bundle.tar`
and a separate Cosign signature bundle. The tar contains the shipped executables
and a stable input/hash manifest. Its scope excludes the complete OCI filesystem,
SBOMs, workflow records, and signatures. Cross-machine success must be demonstrated
by comparing actual bundles; the existence of this tooling is not that result.

Both build variants accept `--effective-build-timestamp` (Unix seconds). When
omitted, the build chooses it once at runtime, supplies it as `SOURCE_DATE_EPOCH`,
and exports it in the bundle and attestation manifests. Reuse that value for a
rebuild. Static-library member timestamps are always zero; they do not need the
effective timestamp. No new wall-clock start/end fields are added.

`--expected-unsigned-executable-bundle-sha256` optionally asserts the earlier
bundle hash. A mismatch stops before signing or publication. The FIPS workflow
exposes the corresponding `expected_unsigned_executable_bundle_sha256` and
`effective_build_timestamp` dispatch inputs and exports both values as job
outputs. Every build also writes `unsigned-executable-bundle.sha256`.

The bundle hash appears as `zcutils:unsigned-executable-bundle:sha256` in a
CycloneDX metadata property and an SPDX annotation. Both signed in-toto SBOM
statements also bind the bundle digest alongside the image digest. Standard
verification checks both SBOM signatures against an independently trusted key,
the detached bundle signature, and equality of the SBOM, exported, and actual
bundle hashes. The unsigned checksum file alone is not an authenticity check.

```sh
python3 scripts/zc-image-attest.py verify --variant fips-aspiring \
  --output-dir /path/to/release/image-attestations --require-signature \
  --cosign-verification-key /path/to/trusted-signing-public-key.pem
# The verified output prints the bundle SHA-256 and effective timestamp.
python3 scripts/test-github-ec2-runner.py --fips-build \
  --effective-build-timestamp EPOCH \
  --expected-unsigned-executable-bundle-sha256 SHA256
```

Independent rebuilds can use `scripts/zc-release-payload.py create`, `verify`,
and `compare`. `verify` authenticates an existing detached signature with the
public key; it does not need signing access. `compare` checks complete unsigned
bundle bytes and deliberately ignores the separate signature envelopes.

## Optional comparison experiment

Normal workflow dispatches set `compare_online=false`: they do not compile an
online reference or run the same-machine comparison. For an explicit experiment,
set `compare_online=true`, or run the local workflow helper with
`--fips-build --compare-online`.

In that experiment, the native runner rebuilds the provider from clean source
at the same absolute path with networking enabled and disabled, then compares
the module object, timestamp-normalized archive, tool, and identity probe. Original
archive hashes remain recorded separately. The application Dockerfile
selects the explicit `reproducibility-check` stage: two clean compiler branches
use the same prepared inputs, and every shipped executable must have matching
SHA-256 hashes. A mismatch stops the build. Only offline outputs are selected.

These experiments establish repeatability on one builder. Reproducibility across
machines must be assessed by rebuilding on separate machines with the same
source, tools, provider procedure, and input versions and comparing the actual
outputs. Preserve the release's offline manifest and native artifact hashes for
that comparison. Per-run receipts, signatures, and OCI metadata are not expected
to match byte for byte; matching executable hashes must not be described as
matching complete OCI image digests.

After downloading and verifying two releases and their launch records, compare
independent workers with:

```sh
python3 scripts/fips-compare-builds.py \
  --first /path/to/first/artifact --first-launch /path/to/first/launch.json \
  --second /path/to/second/artifact --second-launch /path/to/second/launch.json \
  --report independent-build-comparison.json
```

The tool requires distinct worker instance IDs, verifies the actual packaged
files against the offline manifests, and compares source/tool identity and
native-provider outputs. It does not authenticate the supplied launch records;
verify the downloaded artifacts and their origin before running it.
