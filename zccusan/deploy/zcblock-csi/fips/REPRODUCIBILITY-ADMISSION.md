# Reproduced executables and image admission

The release check compares the unsigned executable bundle from two distinct EC2
workers. A passing result establishes byte-for-byte equality of all shipped
application executables, their stable bundle manifest, and the native provider
objects. It does not establish reproducibility of the complete OCI filesystem,
SBOMs, signature envelopes, or FIPS deployment conformance.

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
