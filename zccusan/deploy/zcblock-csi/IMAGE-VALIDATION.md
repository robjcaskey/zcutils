# Validate a zcblock-csi image

Use an immutable image digest and a public key your organization has approved.
Cosign can verify the image signature and its signed SBOMs without project-specific
software or access to AWS. Sigstore policy-controller can enforce the same trust
at Kubernetes admission. An independent rebuild adds a separate check that the
shipped executables can be produced again from the stated inputs.

These checks apply to signed `nonfips` and `fips-aspiring` images. Registry SBOM
publication with `--sign-image` was added in commit `4e4cbb9c` and is enabled in
the canonical FIPS workflow. The ordinary workflow currently exports detached
SBOM signatures for each architecture; its top-level multi-architecture manifest
is signed separately using the GitHub Actions identity. A tag or badge alone does not establish
that an image has passed these checks. A passing reproducibility badge covers the
unsigned executable bundle, not the entire OCI filesystem or FIPS conformance.

## Obtain the public key without Cosign

The release public key is published as
[`signing-keys/Rob-J-Caskey.pem`](signing-keys/Rob-J-Caskey.pem). The current image
build does not embed it in the image. Each signed release also exports a
`zcblock-csi-VARIANT.signing-public-key.pem` artifact. No private key is published
or needed for verification.

The published PEM file's SHA-256 is
`9536633d58eba6c6948bd8c2df891e7ce3ef336a9ba4db95f5991b1e8d872836`.
The SHA-256 fingerprint of its DER-encoded SubjectPublicKeyInfo is
`2561fb77ca382985c9b7daca2fe1bc61cb98564fb7ea75e2745f47b8d171a030`.
The DER fingerprint stays the same if PEM whitespace or line endings change.
These values identify the current Rob J. Caskey signing key. Your organization
still decides whether to trust it; obtaining a key and its fingerprint from the
same untrusted image would not establish that trust.

```sh
curl -fL https://raw.githubusercontent.com/robjcaskey/zcutils/main/zccusan/deploy/zcblock-csi/signing-keys/Rob-J-Caskey.pem \
  -o release-signing-public-key.pem
openssl pkey -pubin -in release-signing-public-key.pem -outform DER | sha256sum
```

Approve the fingerprint independently, then retain the key with the release.
The URL is a discovery location, not a replacement for fingerprint approval.
Key rotation must be approved separately; a new key at the same URL should not
silently replace the trusted key for an older release.

## 1. Standard tools: verify the release you will deploy

Use Cosign, `jq`, and `sha256sum`. The canonical FIPS pipeline uses Cosign 2.5.3;
the ordinary image pipeline uses 3.1.2. Preserve normal signature, claim, and
transparency-log checks. Obtain the image digest,
release artifacts, and approved public key through your release approval process.
The key included in a downloaded artifact is a convenience copy, not a trust
anchor. Compare its fingerprint with an independently approved fingerprint before
trusting it. A signature annotation naming a person does not authenticate that
person independently of the key.

The following examples use Bash. Replace the quoted example values:

```sh
set -euo pipefail
IMAGE='docker.io/robjcaskey/zcblock-csi@sha256:REPLACE_WITH_IMAGE_DIGEST'
KEY='/trust/release-signing-public-key.pem'
PREFIX='zcblock-csi-fips-aspiring'  # this registry example is the canonical FIPS image
ARTIFACTS='/archive/release/image-attestations'

sha256sum "$KEY"   # compare with the separately approved key-file fingerprint
cosign verify --key "$KEY" "$IMAGE" > verified-image-signatures.json
cosign verify-attestation --key "$KEY" \
  --type https://spdx.dev/Document --output json "$IMAGE" \
  > verified-spdx.dsse.json
cosign verify-attestation --key "$KEY" \
  --type https://cyclonedx.org/bom --output json "$IMAGE" \
  > verified-cyclonedx.dsse.json
```

The image signature authenticates a claim about that exact image digest under
the approved key. The digest binds the OCI manifest, which in turn references the
image configuration and filesystem layers by digest. The attestation signatures
bind their SBOM claims to the same image. Signatures protect the statements from
undetected alteration; they do not independently establish that the SBOM is
complete or that the software is safe.

Decode only the output of successful `verify-attestation` commands. Cosign emits
DSSE envelopes; decoding base64 by itself is not signature verification. These
commands accept either JSON-line envelopes or an envelope array:

```sh
jq -s '[.[] | if type == "array" then .[] else . end |
  .payload | @base64d | fromjson]' verified-spdx.dsse.json > verified-spdx.json
jq -s '[.[] | if type == "array" then .[] else . end |
  .payload | @base64d | fromjson]' verified-cyclonedx.dsse.json > verified-cyclonedx.json

SPDX_HASH=$(jq -er '
  [.[] | .predicate.annotations[]? |
   select(.comment | startswith("zcutils:unsigned-executable-bundle:sha256=")) |
   .comment | split("=")[1]] | unique |
  if length == 1 and (.[0] | test("^[0-9a-f]{64}$")) then .[0] else error("ambiguous bundle hash") end
' verified-spdx.json)
CDX_HASH=$(jq -er '
  [.[] | .predicate.metadata.properties[]? |
   select(.name == "zcutils:unsigned-executable-bundle:sha256") | .value] | unique |
  if length == 1 and (.[0] | test("^[0-9a-f]{64}$")) then .[0] else error("ambiguous bundle hash") end
' verified-cyclonedx.json)
test "$SPDX_HASH" = "$CDX_HASH"

BUNDLE="$ARTIFACTS/$PREFIX.unsigned-executable-bundle.tar"
cosign verify-blob --key "$KEY" --bundle "$BUNDLE.cosign.bundle" "$BUNDLE"
test "$SPDX_HASH" = "$(cat "$ARTIFACTS/$PREFIX.unsigned-executable-bundle.sha256")"
printf '%s  %s\n' "$SPDX_HASH" "$BUNDLE" | sha256sum -c -
```

This establishes agreement between two authenticated SBOM claims, the actual
signed bundle bytes, and the exported checksum. The checksum file alone provides
no signer authentication. Deploy using the verified `@sha256:…` reference so a
later tag update cannot select a different image.

To check reproduction, also compare this hash with the bundle hash from an
approved independent-build comparison. Matching a signed SBOM does not itself
mean anyone rebuilt the software.

### Ordinary multi-architecture images

The ordinary workflow's top-level manifest uses a keyless GitHub Actions
signature. Verify that manifest with its exact workflow identity:

```sh
cosign verify \
  --certificate-identity 'https://github.com/robjcaskey/zcutils/.github/workflows/zcblock-csi-images.yml@refs/heads/main' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  "$IMAGE"
```

Its architecture-specific SBOM artifacts use the published Rob J. Caskey KMS
public key. Download the artifact for the chosen architecture, set
`PREFIX=zcblock-csi-nonfips`, and verify the detached statements and bundle using
`cosign verify-blob` or the OpenSSL procedure below. Compare their image subject
with the architecture image digest, not the multi-architecture index digest.
If deploying the index, inspect its descriptors and confirm that the verified
architecture digest is its selected child. A keyless signature on the index does
not substitute for that child/SBOM comparison.

The generated SBOM admission policy targets images with registry-published
attestations, such as the canonical FIPS image. The ordinary workflow's detached
SBOM files alone cannot satisfy that policy. Its keyless index signature needs
an identity-based policy if used as an admission control.

## 2. Kubernetes admission: enforce the decision

[Sigstore policy-controller](https://docs.sigstore.dev/policy-controller/overview/)
can verify registry attestations and evaluate their contents before admitting
workloads. It needs registry access and an organization-approved public key; it
does not need the private signing key or KMS access.

After two independently verified builds match, the project helper can emit
`cluster-image-policy.json` and release-scoped badge data. See
[reproduced executables and admission](fips/REPRODUCIBILITY-ADMISSION.md) for that
comparison command. The emitted policy checks the SPDX signature and demands
exactly one bundle-hash annotation with the compared value. Review the repository,
key, and hash before applying it. The JSON file is an ordinary Kubernetes manifest;
it can also be maintained directly without the helper.

For a basic signature-and-SBOM policy, use the following standard manifest and
replace the repository and public key. This version establishes signed SBOM
presence; it does not check reproduction:

```yaml
apiVersion: policy.sigstore.dev/v1beta1
kind: ClusterImagePolicy
metadata:
  name: zcblock-csi-signed-sbom
spec:
  images:
    - glob: "docker.io/robjcaskey/zcblock-csi:*"
    - glob: "docker.io/robjcaskey/zcblock-csi@sha256:*"
  authorities:
    - key:
        data: |
          -----BEGIN PUBLIC KEY-----
          REPLACE_WITH_APPROVED_PUBLIC_KEY
          -----END PUBLIC KEY-----
      attestations:
        - name: signed-spdx
          predicateType: https://spdx.dev/Document
```

Install the controller using its
[official instructions](https://docs.sigstore.dev/policy-controller/installation/).
Apply the selected policy and opt the workload namespace into enforcement:

```sh
kubectl apply -f cluster-image-policy.json
kubectl label namespace YOUR_NAMESPACE policy.sigstore.dev/include=true
```

The controller's namespace selection, matching image patterns, and webhook failure
policy are part of this deployment control. Confirm enforcement in the namespace
where zcblock-csi will run. A policy matching one repository is not a universal
allowlist for all workload images. With the generated reproduction policy, test
that the correct image is admitted and that a different bundle hash, a wrong key,
and an absent attestation are denied. Server-side dry-run requests exercise the
webhook without starting the image. Client-side dry-run does not.

Admission validates the signed claim about the requested image digest. It does
not rebuild the image, scan the running process, or establish the host's FIPS
configuration. The generated policy relies on the signed SBOM for the relationship
between the image and its executable bundle; the next procedure checks the actual
executable bytes too.

## 3. Detailed inspection and independent rebuild

This route exposes each link in the trust chain. Keep the original image digest,
verified statements, bundles, input archives, build logs, and comparison results
together so another reviewer can repeat the checks.

### Verify detached artifacts with OpenSSL, without Cosign

The canonical FIPS release uses Cosign 2.5.3 legacy bundles, with a signature in
`base64Signature`. The ordinary architecture builds use Cosign 3.1.2 Sigstore v0.3
bundles, with a signature in `messageSignature.signature`. Both contain an ECDSA
P-256 signature that OpenSSL can verify over the **original file bytes**. No
reformatting, unpacking, or JSON reserialization is allowed before verification.
The commands below explicitly recognize those two formats and reject others.

Start with downloaded release artifacts, the independently approved PEM key, and
the same `IMAGE`, `KEY`, `PREFIX`, and `ARTIFACTS` variables used above. Downloading
the artifacts does not need Cosign. Run this in a fresh working directory:

```sh
set -euo pipefail
for FILE in \
  "$ARTIFACTS/$PREFIX.spdx.json.intoto.json" \
  "$ARTIFACTS/$PREFIX.cyclonedx.json.intoto.json" \
  "$ARTIFACTS/$PREFIX.unsigned-executable-bundle.tar"
do
  jq -er '
    if has("base64Signature") and (has("messageSignature") | not) then .base64Signature
    elif .mediaType == "application/vnd.dev.sigstore.bundle.v0.3+json"
         and (has("base64Signature") | not)
         and .messageSignature.messageDigest.algorithm == "SHA2_256"
    then .messageSignature.signature
    else error("unsupported or ambiguous signature bundle") end |
    select(type == "string" and length > 0)
  ' "$FILE.cosign.bundle" | base64 --decode > signature.der
  openssl dgst -sha256 -verify "$KEY" -signature signature.der "$FILE"
  if jq -e 'has("messageSignature")' "$FILE.cosign.bundle" >/dev/null; then
    test "$(jq -er '.messageSignature.messageDigest.digest' "$FILE.cosign.bundle")" = \
      "$(openssl dgst -sha256 -binary "$FILE" | base64 | tr -d '\n')"
  fi
done
rm signature.der

SPDX_STATEMENT="$ARTIFACTS/$PREFIX.spdx.json.intoto.json"
CDX_STATEMENT="$ARTIFACTS/$PREFIX.cyclonedx.json.intoto.json"
IMAGE_DIGEST="${IMAGE##*@sha256:}"
for FILE in "$SPDX_STATEMENT" "$CDX_STATEMENT"; do
  jq -e --arg digest "$IMAGE_DIGEST" \
    'any(.subject[]; .digest.sha256 == $digest)' "$FILE" >/dev/null
done
jq -e '.predicateType == "https://spdx.dev/Document"' "$SPDX_STATEMENT" >/dev/null
jq -e '.predicateType == "https://cyclonedx.org/bom"' "$CDX_STATEMENT" >/dev/null

jq -S '.predicate' "$SPDX_STATEMENT" > authenticated-spdx.json
jq -S . "$ARTIFACTS/$PREFIX.spdx.json" > supplied-spdx.json
cmp authenticated-spdx.json supplied-spdx.json
jq -S '.predicate' "$CDX_STATEMENT" > authenticated-cyclonedx.json
jq -S . "$ARTIFACTS/$PREFIX.cyclonedx.json" > supplied-cyclonedx.json
cmp authenticated-cyclonedx.json supplied-cyclonedx.json
```

Every OpenSSL invocation must report `Verified OK` and exit successfully. An
unsupported envelope format must be inspected rather than selecting an arbitrary
signature field.
These commands verify the publisher's signature, **not** the Rekor transparency
log's signature, inclusion proof, or trusted time. Thus they establish authenticity
under the approved long-lived key, but do not replace Cosign's additional
transparency-log checks. If log inclusion or trusted signing time is part of the
organization's policy, retain a full Sigstore verifier for those checks.

Check the authenticated statements against the bundle and exported hash:

```sh
SPDX_HASH=$(jq -er '
  [.annotations[]? | select(.comment | startswith("zcutils:unsigned-executable-bundle:sha256=")) |
   .comment | split("=")[1]] |
  if length == 1 and (.[0] | test("^[0-9a-f]{64}$")) then .[0] else error("invalid bundle hash") end
' authenticated-spdx.json)
CDX_HASH=$(jq -er '
  [.metadata.properties[]? | select(.name == "zcutils:unsigned-executable-bundle:sha256") | .value] |
  if length == 1 then .[0] else error("invalid bundle hash") end
' authenticated-cyclonedx.json)
test "$SPDX_HASH" = "$CDX_HASH"
BUNDLE="$ARTIFACTS/$PREFIX.unsigned-executable-bundle.tar"
for FILE in "$SPDX_STATEMENT" "$CDX_STATEMENT"; do
  jq -e --arg name "${BUNDLE##*/}" --arg hash "$SPDX_HASH" \
    'any(.subject[]; .name == $name and .digest.sha256 == $hash)' "$FILE" >/dev/null
done
test "$SPDX_HASH" = "$(cat "$ARTIFACTS/$PREFIX.unsigned-executable-bundle.sha256")"
printf '%s  %s\n' "$SPDX_HASH" "$BUNDLE" | sha256sum -c -
```

A valid signature on a statement about a different image or bundle must not pass
these comparisons. Continue with the file inspection and rebuild steps below.

### If you have a registry DSSE envelope instead

A registry attestation signs the DSSE pre-authentication encoding, not just the
JSON statement. With an already downloaded `attestation.dsse.json`, use the
[DSSE specification](https://github.com/secure-systems-lab/dsse/blob/master/protocol.md)
to reconstruct the exact signed bytes. The following uses Python's standard
library only and deliberately accepts one signature:

```sh
python3 - <<'PYVERIFY'
import base64, json
from pathlib import Path
value = json.loads(Path('attestation.dsse.json').read_text())
assert value['payloadType'] == 'application/vnd.in-toto+json'
assert len(value['signatures']) == 1
kind = value['payloadType'].encode('utf-8')
payload = base64.b64decode(value['payload'], validate=True)
pae = (b'DSSEv1 ' + str(len(kind)).encode() + b' ' + kind + b' '
       + str(len(payload)).encode() + b' ' + payload)
Path('attestation.pae').write_bytes(pae)
Path('attestation.signature.der').write_bytes(
    base64.b64decode(value['signatures'][0]['sig'], validate=True))
Path('unverified-statement.json').write_bytes(payload)
PYVERIFY
openssl dgst -sha256 -verify "$KEY" \
  -signature attestation.signature.der attestation.pae
```

Only after OpenSSL succeeds may `unverified-statement.json` be treated as an
authenticated statement. Check its image subject, predicate type, and bundle hash
as above. A downloaded envelope is not trusted merely because it came from the
registry. This procedure also omits transparency-log verification.

### Inspect the contents and reproduce them

1. **Authenticate the signer and image.** Use either the standard Cosign checks
   or the OpenSSL procedure above, with its stated transparency-log limitation,
   and the separately approved key. If using detached SBOM artifacts, run
   `cosign verify-blob --key "$KEY" --bundle FILE.intoto.json.cosign.bundle FILE.intoto.json`
   for both SPDX and CycloneDX statements. Check each verified statement's
   `subject[].digest.sha256` against the exact image and bundle hashes, and compare
   its `predicate` with the corresponding SBOM JSON. Registry DSSE attestations and
   detached signed JSON statements use different envelopes; each needs its own
   Cosign verification command.

2. **Check archive contents against their signed hash.** Verify the bundle
   signature and whole-file SHA-256 before reading it. Its `manifest.json` lists
   each `bin/NAME` and SHA-256, along with the effective build timestamp and stable
   compiler input identity. Inspect the tar member list; reject duplicate names,
   links, special files, absolute paths, and traversal paths. Hash the regular file
   bytes and compare every entry with the manifest. Work in a fresh directory and
   avoid extracting an unchecked archive. The project's optional
   `scripts/zc-release-payload.py verify` performs these structural and hash checks
   after Cosign verification.

3. **Compare with the files inside the image.** Pull the digest-pinned image and
   copy `/usr/local/bin` from a created, unstarted container into a fresh directory.
   For example, `docker create "$IMAGE"` followed by `docker cp` inspects the
   filesystem without executing the image. Compare the complete filename set and
   each executable's SHA-256 with the bundle manifest, then remove that container.
   This checks the SBOM's bundle relationship against actual image contents.
   The canonical build already does this before producing its release artifacts.

4. **Check the native provider for a FIPS-aspiring image.** Inspect its retained
   `provider-receipt.json`, `provider-build-record.json`, original `libcrypto.original.a`,
   packaged `libcrypto.a`, and `bcm.o`. Compare the source archive with the pinned
   hash and follow [AWS-LC recompilation](fips/AWS-LC-RECOMPILATION.md). The packaging
   transformation normalizes archive timestamps, owner/group IDs, and object modes;
   object contents, member order, and symbol-index bytes must remain unchanged.
   The original archive makes that transformation independently inspectable.
   Runtime provider identification and static symbol checks add integration checks;
   they are not themselves a CMVP validation.

5. **Rebuild on another machine.** Acquire and hash the recorded source and
   dependency inputs, use the specified toolchain and provider procedure, start
   with clean compilation outputs, and compile with networking disabled. Reuse
   the effective build timestamp from the authenticated bundle manifest. Follow
   [offline build instructions](fips/OFFLINE-BUILD.md) for the exact inputs and
   build commands. A populated cache is an input source only after its artifacts
   have been authenticated or checked against trusted hashes; it is not proof of
   reproducibility. Record the second worker's identity and input hashes.

6. **Compare before signing.** Compare all shipped executables and native objects,
   then assemble the unsigned executable bundle deterministically and compare its
   full SHA-256. The optional pipeline parameter
   `expected_unsigned_executable_bundle_sha256` refuses signing and publication if
   this value differs; `effective_build_timestamp` selects the recorded timestamp.
   The equivalent CLI options use hyphens. A byte-for-byte match across distinct
   workers establishes reproduction for those inputs and outputs. It does not
   establish compiler correctness or eliminate a compromise shared by both builds.
   EC2 launch records identify workers operationally; they are not hardware-signed
   proof of how each instruction executed.

7. **Keep signatures outside the comparison.** Public rebuilders need no signing
   key to reproduce the unsigned bundle. The current signer uses non-exportable
   AWS KMS ECDSA P-256 signing keys; KMS signs online and the private key does not
   travel with the build. Fresh valid signatures can differ bytewise. Verify each
   signature normally and compare the signed content, not the signature envelope.

| Check | What a successful check establishes | Trust that remains |
| --- | --- | --- |
| Approved key plus valid signature | The matching private key authorized these exact signed bytes | Key identity, custody, and signing policy |
| Image digest and OCI layer digests | Retrieved image content matches the authenticated digest | Registry availability and correct verification/runtime implementation |
| Signed SBOM and matching bundle hash | The signer associated this image with this executable bundle | Accuracy and completeness of the SBOM claim |
| Direct image-to-bundle file comparison | The shipped executable bytes match the authenticated bundle | Other image files and runtime configuration remain outside this comparison |
| Independent matching rebuild | These inputs produced the same executable bundle on distinct workers | Input provenance, toolchain trust, and independence of the workers |
| Admission enforcement | The submitted workload satisfies the configured signature/hash policy | Cluster administration, webhook configuration, and runtime controls |

A transparency-log entry adds a verifiable publication record for a signature;
it does not certify source quality, signer identity beyond the selected trust
model, or FIPS status. FIPS deployment assessment remains separate: see the
[FIPS overview](fips/FIPS-OVERVIEW.md). Image authenticity and reproducibility are
useful inputs to that assessment, not substitutes for it.
