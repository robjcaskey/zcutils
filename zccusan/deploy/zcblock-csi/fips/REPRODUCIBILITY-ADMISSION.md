# Reproduced executables and image admission

The release check compares the unsigned executable bundle from two distinct EC2
workers. A passing result establishes byte-for-byte equality of all shipped
application executables, their stable bundle manifest, and the native provider
objects. It does not establish reproducibility of the complete OCI filesystem,
SBOMs, signature envelopes, or FIPS deployment conformance.

Published result: [revision 75c89bd43418](reproduced/75c89bd43418/README.md)
matched on two distinct workers, including the complete unsigned executable
bundle. The linked signed report identifies the exact images and inputs.

Build the first release normally. Verify its signed SBOM with an independently
trusted public key and retain its exported unsigned executable bundle SHA-256 and
effective build timestamp. Pass both values to the next build on a fresh worker:
`--expected-unsigned-executable-bundle-sha256` and `--effective-build-timestamp`.
The next build refuses to sign or publish if its bundle differs. Both builds
compile offline; the optional online comparison is a separate experiment.

After downloading and verifying both GitHub runs, generate the comparison report,
badge data, and Sigstore admission policy:

```sh
python3 scripts/zc-reproducibility-admission.py \
  --first /archive/first/build-artifact \
  --second /archive/second/build-artifact \
  --first-launch /archive/first/launch.json \
  --second-launch /archive/second/launch.json \
  --trusted-public-key /trust/release-signing-public-key.pem \
  --image-repository registry.example.org/zcblock-csi \
  --output-dir /archive/reproduced-release
```

This command verifies both sets of detached signatures before comparing the
actual executable files and bundles. Supply launch records from authenticated
GitHub downloads: they identify the two workers but are not hardware attestations.
The command creates a new output directory and emits no passing badge if signature
verification or comparison fails. Archive the report and badge with that release;
a badge for one release says nothing about later releases. Its label is
**executable bundle: reproduced on 2 workers**. Do not label it “reproducible OCI
image” or use it as a FIPS certification badge.

To publish a new result and update the front-page reproduction badge:

1. Finish the two-worker comparison above. Check the full source revisions and
   effective timestamps against the authenticated run records, and check the
   second run passed the expected-bundle-hash gate. Verify both published image
   signatures and registry SPDX/CycloneDX bindings using the
   [image validation guide](../IMAGE-VALIDATION.md).
2. Create `fips/reproduced/<12-character-source-revision>/` and copy the three
   generated JSON files into it. Preserve older results. Copy
   [REPRODUCIBILITY-RESULT.template.md](REPRODUCIBILITY-RESULT.template.md) into
   that directory as `README.md`; fill every placeholder from the verified
   records, resolve the conditional paragraphs, and remove its author comment.
3. Create `verification-manifest.json` using the
   [published example](reproduced/75c89bd43418/verification-manifest.json) as its
   schema. Populate the new revision, date, authority, runs, image digests,
   timestamps, artifact names, and SHA-256 values calculated from the new
   comparison, badge JSON, and policy. Sign this exact file with the release
   signing authority's key, producing `verification-manifest.cosign.bundle`.
   Never reuse an earlier result's signature. Run the template's verification
   commands against the new manifest and files.
4. Replace only the front-page **executables** badge's revision and result link
   with the newly verified revision and directory. Keep the visible revision in
   its label. The live **Image build** and **FIPS gate tests** badges update from
   GitHub automatically and need no release-specific edit. Add the new result
   link to this guide.
5. Check that no template placeholders remain, all relative links resolve from
   the result directory, and the advertised hash agrees with the signed manifest,
   comparison, registry SBOMs, and admission policy. Commit and publish the result
   and badge together. A failed or incomplete comparison leaves the previous
   revision's badge unchanged.

The result template records what was verified; filling it in does not perform
the checks or authorize a passing badge.

The normal image build with `--sign-image` publishes signed SPDX and CycloneDX
attestations to the image registry using Cosign, then fetches and verifies their
exact contents and image digest. An admission controller can discover these
registry attestations; detached files in a GitHub artifact alone are insufficient.
The registry must be reachable by the cluster. The canonical workflow publishes its final image to Docker Hub; its loopback
registry stores build cache only. A copied image must carry attestations bound
to its actual destination digest.

The generated `cluster-image-policy.json` is a Sigstore `ClusterImagePolicy`.
It requires an SPDX attestation signed by the supplied trusted public key and
exactly one bundle-hash annotation matching the independently compared bundle.
It does not trust a self-declared `reproducible: true` field. The controller checks
the signature and image binding; the release comparison performs the rebuild.
End-user organizations select the trusted signing authority and image repository.

Install [Sigstore policy-controller](https://docs.sigstore.dev/policy-controller/installation/),
review and apply the generated policy, and enable enforcement for the deployment
namespace as described in the
[controller documentation](https://docs.sigstore.dev/policy-controller/overview/).
Before using this as a deployment gate, exercise a matching image, a mismatched
bundle hash, a wrong signing key, and a missing attestation in an isolated test
namespace. Confirm only the matching image is admitted. Unit tests of the policy
generator do not substitute for that cluster test.

Local integration check (2026-09-15): the generated policy was exercised through
server-side dry-run admission in disposable K3s 1.36.4 with policy-controller
Helm chart 0.10.8 (app 0.13.1). A real KMS-signed smoke-image SBOM passed; an
unexpected bundle hash, an absent attestation, and a different trusted key were
denied. This tests the admission mechanism with a fixture, not reproduction of a
release. Release-specific comparison and signature checks still precede its badge.
