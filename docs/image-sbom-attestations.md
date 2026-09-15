# Image SBOM attestations

`scripts/zc-image-attest.py` builds either zcblock-csi image track, scans the
exact local image with Syft, and writes SPDX 2.x JSON and CycloneDX JSON. Each
SBOM is embedded as the predicate in an in-toto Statement v1 whose subject is
the image's registry digest when available, or its content-addressed local
image ID. The output manifest records SHA-256 hashes for all evidence.

The signing authority in both SBOM formats and in the evidence manifest is
exactly `Rob J. Caskey`.

The main-branch non-FIPS image workflow scans each pushed architecture image,
assumes the dedicated `AWS_ATTESTATION_SIGNER_ROLE_ARN` through GitHub OIDC,
signs both statements with the KMS key, verifies the bundles immediately, and
uploads the evidence. Manual local generation supports both `nonfips` and
`fips-aspiring`; the certificate-profile FIPS workflow must first produce its
external provider package before the latter can build.

These are SBOM attestations. They do not claim SLSA build provenance and do not
replace a SLSA provenance predicate containing the builder identity, invocation,
materials, and build configuration.

Install Docker, Syft, and optionally cosign, then build the normal image:

```sh
scripts/zc-image-attest.py generate \
  --variant nonfips \
  --image localhost/zcblock-csi:attested
```

Build the FIPS-aspiring image after preparing the local provider as described
in `zccusan/deploy/zcblock-csi/FIPS.md`:

```sh
FIPS_DISTRO=amzn2023 \
FIPS_PROVIDER_ROOT=zccusan/deploy/zcblock-csi/fips/provider \
scripts/zc-image-attest.py generate \
  --variant fips-aspiring \
  --image localhost/zcblock-csi-fips-amzn2023:attested
```

Use `--engine podman` on a Podman host. Use `--skip-build` to attest an image
already built by CI. Set `SOURCE_DATE_EPOCH` through `--source-date-epoch` when
the build system supplies a reproducible timestamp. Re-running `verify` checks
the recorded hashes, in-toto subjects, and embedded predicates:

```sh
scripts/zc-image-attest.py verify --variant nonfips
```

To sign the statements locally, pass a temporary cosign key path:

```sh
COSIGN_PASSWORD='' cosign generate-key-pair --output-key-prefix /tmp/zcutils-test
COSIGN_PASSWORD='' scripts/zc-image-attest.py generate --skip-build \
  --variant nonfips --image localhost/zcblock-csi:attested \
  --cosign-key /tmp/zcutils-test.key
```

The key files in this example are local test material. Keep them outside the
repository and remove them after testing.

When signing is enabled, the script exports the public key, verifies both
cosign bundles cryptographically immediately after signing, and records the
public key and bundles in the evidence manifest. Offline verification can be
repeated with:

```sh
scripts/zc-image-attest.py verify --variant nonfips \
  --require-signature \
  --cosign-verification-key /secure/trust/zcutils-attestation.pub
```

The verifier must supply an independently trusted public key or KMS URI. The
public key packaged beside the evidence is useful for distribution, but it is
not its own trust root. If policy distributes a SHA-256 key pin instead, pass
the packaged key path with `--trusted-public-key-sha256 HEX`; the verifier
checks the pin before cosign. Supplying either `--require-signature` or a
verification key rejects a manifest changed to `signed=false`.

KMS verification can use the trusted alias identity directly:

```sh
scripts/zc-image-attest.py verify --variant nonfips --require-signature \
  --cosign-verification-key \
  awskms:///alias/zcutils-build-attestation-signing-authority-Rob-J-Caskey
```

## Verified developer build cache

Registry cache import is available only through Docker Buildx. A mutable cache
tag is never passed to BuildKit. The wrapper resolves it with `docker buildx
imagetools inspect`, requires a SHA-256 manifest digest, verifies the
digest-pinned object with an independently trusted cosign key, and requires the
signature annotations `signingAuthority=Rob J. Caskey` and the exact trusted
builder identity. It then passes only
`repository@sha256:...` to `--cache-from`.

```sh
scripts/zc-image-attest.py generate \
  --variant nonfips --image localhost/zcblock-csi:attested \
  --cache-from registry.example/zcutils/zcblock-csi-cache:dev \
  --cache-verification-key \
  awskms:///alias/zcutils-build-attestation-signing-authority-Rob-J-Caskey \
  --cache-builder-identity \
  https://github.com/robjcaskey/zcutils/.github/workflows/zcblock-csi-images.yml@refs/heads/main
```

For a local public key, add `--cache-trusted-public-key-sha256 HEX` to pin the
independently distributed key. Missing signatures, mutable-only resolution,
wrong authority, and digest disagreement are fatal before the build starts.
The resolved cache reference, digest, builder identity, verification method,
cosign output hash, and authority are recorded in the SPDX creation comment,
CycloneDX properties, and attestation manifest. The complete cosign
verification output is a hashed evidence file.

A dev build may export a separate registry cache:

```sh
scripts/zc-image-attest.py generate \
  --variant nonfips --image localhost/zcblock-csi:attested \
  --cache-export-ref registry.example/zcutils/zcblock-csi-cache:dev-next \
  --cache-builder-identity \
  https://github.com/robjcaskey/zcutils/.github/workflows/zcblock-csi-images.yml@refs/heads/main
```

This pushes BuildKit cache data, so registry authentication and explicit write
authorization are required. The exported tag and its newly resolved digest are
recorded as untrusted. The generated `cache-export-signing-receipt.json`
contains a concrete `cosign sign` command for the digest-pinned object. A later
build will not import that cache until the signature and exact authority pass
normal verification. Exporting to a tag and importing that same mutable tag in
one command does not establish trust.

For the ephemeral FIPS builder, the registry is bound to `127.0.0.1:5000` and
its data directory lives on the single-writer 20 GiB cache volume. The workflow
imports only a cache digest whose KMS signature binds both the signing authority
and its exact GitHub workflow identity. It overwrites the local `trusted` tag
only as a new export, signs the resulting immutable digest, and verifies it
immediately. `--allow-insecure-loopback-registry` permits plain HTTP and skips
transparency-log lookup only for loopback references; it cannot authorize a
remote registry. The cache never leaves the build machine, while the final
runtime image is the only image pushed to Docker Hub.

`--push-image --sign-image` pushes the final image, signs its immutable registry
digest with the same KMS authority and builder-identity annotations, verifies
that signature, and records the verification in the attestation manifest.

The builder identity is a claim cryptographically bound to the cache manifest
by the independently trusted signing key. It tells a verifier which reviewed
builder was authorized to produce the layers. This cache signature is not a
substitute for independently verified SLSA provenance.

The ordinary Dockerfile also uses variant-specific BuildKit cache mounts for
APT metadata and Cargo registry/git downloads. Cargo still runs with `--locked`,
and package-manager signatures and Cargo checksums remain authoritative. The
FIPS image keeps its acceptance evidence independent of cache hits and receives
the same verified external BuildKit cache handling through this wrapper.

For ephemeral AL2023 runners, the optional persistent EBS and loopback Squid
configuration is documented in
`zccusan/deploy/github-ec2-runner-cache/README.md`. The disk caches native
Cargo/DNF dependencies. Squid does not participate in the Docker cache trust
decision and cannot content-cache opaque HTTPS CONNECT traffic.

## AWS signing design

Production signing should use an asymmetric AWS KMS `SIGN_VERIFY` key. KMS
performs the signature without exporting private key bytes, which is safer and
more auditable than placing an encrypted cosign private key in Parameter Store.
The optional, unapplied Terraform definition is in
`zccusan/deploy/image-attestation-signing/`.
Use this clear resource identity:

* KMS alias: `alias/zcutils-build-attestation-signing-authority-Rob-J-Caskey`
* KMS description/tag `SigningAuthority`: `Rob J. Caskey`
* SSM reference: `/zcutils/build-attestation/signing-authority/Rob-J-Caskey/kms-key-arn`

The SSM value is the KMS key ARN and is configuration, so it should be a
versioned `String`; it is not private-key escrow. Once provisioned, retrieve it
without printing its value and sign with KMS using:

```sh
scripts/zc-image-attest.py generate --skip-build \
  --variant nonfips --image localhost/zcblock-csi:attested \
  --aws-profile slopmud-cicd --kms-key-parameter
```

The flag's default parameter name is the SSM reference above. A KMS URI can
also be passed directly, for example
`--cosign-key awskms:///arn:aws:kms:us-east-1:ACCOUNT:key/KEY-ID`.

The current profiles were checked read-only on 2026-09-13. The base `slopmud`
IAM user can call STS, but cannot assume `slopmud-cicd`, run IAM policy
simulation, list KMS aliases, or read the proposed SSM parameter. The
`slopmud-breakglass` profile does assume an administrator role. The exact KMS
alias and SSM parameter do not exist, and policy simulation reports that the
break-glass role may create and use the asymmetric KMS key and may create/read
the SSM reference. No AWS resource or key material was created.

Creating the asymmetric KMS key managed by the organization would incur AWS
charges. At the time of this check, AWS lists $1 per key per month, prorated
hourly, plus
asymmetric signing requests (the AWS example prices ECC signing at $0.15 per
10,000 requests). A standard SSM parameter has no additional storage charge at
standard throughput. Verify current prices on the [AWS KMS pricing page](https://aws.amazon.com/kms/pricing/)
and [Systems Manager pricing page](https://aws.amazon.com/systems-manager/pricing/)
before provisioning.
