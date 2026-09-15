# Reproduced executable bundle: 75c89bd43418

On 2026-09-15, two separate EC2 workers built revision
`75c89bd4341809db9a2955149e092b8e3f01c8dd` with network access disabled during
native-provider and application compilation. All 16 shipped executables,
`bcm.o`, normalized `libcrypto.a`, and the complete unsigned executable bundle
matched byte-for-byte. This is a project-run comparison signed by Rob J. Caskey,
not independent third-party verification or FIPS certification.

The first [build](https://github.com/robjcaskey/zcutils/actions/runs/34957392295)
also compared online and offline compilation and selected the offline output.
The second [build](https://github.com/robjcaskey/zcutils/actions/runs/34960674743)
compiled offline only, reusing effective build timestamp `1789468959` and
asserting the first bundle's hash before signing or publishing:

```text
afcd58e2676990271f2077540c78403857a220b086dc7cbc79a9150e417bc4fc
```

The workers shared a retained dependency cache, but compilation used fresh
targets with network access disabled. This establishes reproduction on separate
machines with the recorded inputs; it does not test independently sourced
dependency archives or an entirely air-gapped preparation and signing process.

The [comparison](independent-build-comparison.json) records each executable and
provider hash, compiler identity, source identity, and distinct worker IDs.
The [signed manifest](verification-manifest.json) identifies both image digests,
GitHub runs, effective timestamps, and hashes of the comparison, badge data, and
[admission policy](cluster-image-policy.json). Both builds' detached bundle and
SBOM signatures were verified against the published signing key. The published
image signatures and registry SBOM attestations were also checked; their bundle
hashes agree with the actual downloaded bundles. These checks use a pinned key;
they do not claim transparency-log inclusion.

To authenticate the manifest from the repository root, first approve the public
key through your own trust process, then run:

```sh
result=zccusan/deploy/zcblock-csi/fips/reproduced/75c89bd43418
cosign verify-blob \
  --key zccusan/deploy/zcblock-csi/signing-keys/Rob-J-Caskey.pem \
  --insecure-ignore-tlog \
  --bundle "$result/verification-manifest.cosign.bundle" \
  "$result/verification-manifest.json"
python3 - "$result" <<'PY'
import hashlib, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
manifest = json.loads((root / 'verification-manifest.json').read_text())
for name, expected in manifest['files'].items():
    assert hashlib.sha256((root / name).read_bytes()).hexdigest() == expected, name
print('Manifest-bound comparison, badge, and policy hashes verified')
PY
```

For a new rebuild, download both runs' named build and launch artifacts and use
the command in [Reproduced executables and image admission](../../REPRODUCIBILITY-ADMISSION.md).
GitHub build artifacts have a 14-day retention period; archive them before expiry
if you need the original provider objects, build records, and detached bundles.
This repository retains the small signed comparison, not those complete archives.
The [image validation guide](../../../IMAGE-VALIDATION.md) covers registry
verification and verification without Cosign.

The badge applies only to this revision and unsigned executable bundle. The two
OCI image digests differ. SBOM bytes, OCI metadata, signature envelopes, later
source revisions, and deployment conformance are outside this comparison.
