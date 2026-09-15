<!--
Copy this file to fips/reproduced/<12-character-source-revision>/README.md
after the comparison command has succeeded. Replace every {{PLACEHOLDER}},
resolve the conditional paragraphs, and remove this comment before publishing.
Never copy a previous release's passing hashes or signatures into a new result.
See REPRODUCIBILITY-ADMISSION.md for the badge update procedure.
-->

# Reproduced executable bundle: {{SOURCE_REVISION_SHORT}}

On {{VERIFICATION_DATE_UTC}}, two separate EC2 workers built revision
`{{SOURCE_REVISION_FULL}}` with network access disabled during native-provider
and application compilation. All {{EXECUTABLE_COUNT}} shipped executables,
`bcm.o`, normalized `libcrypto.a`, and the complete unsigned executable bundle
matched byte-for-byte. This is a project-run comparison signed by
{{SIGNING_AUTHORITY}}, not independent third-party verification or FIPS
certification.

| Build | GitHub run | Worker | Published image digest |
| --- | --- | --- | --- |
| First | [{{FIRST_RUN_ID}}]({{FIRST_RUN_URL}}) | `{{FIRST_WORKER_ID}}` | `{{FIRST_IMAGE_REPOSITORY}}@sha256:{{FIRST_IMAGE_SHA256}}` |
| Second | [{{SECOND_RUN_ID}}]({{SECOND_RUN_URL}}) | `{{SECOND_WORKER_ID}}` | `{{SECOND_IMAGE_REPOSITORY}}@sha256:{{SECOND_IMAGE_SHA256}}` |

Both builds used effective build timestamp `{{EFFECTIVE_BUILD_TIMESTAMP}}`.
The second build asserted this unsigned executable bundle SHA-256 before signing
or publishing:

```text
{{UNSIGNED_EXECUTABLE_BUNDLE_SHA256}}
```

{{ONLINE_COMPARISON_STATEMENT: state whether an optional online/offline comparison
ran, which run performed it, and whether it passed. Normal builds compile offline
only; do not imply every result includes an online comparison.}}

{{INPUT_PROVENANCE_STATEMENT: identify whether the workers shared a dependency
cache, how inputs were obtained and checked, and confirm fresh compilation
targets. State whether independently sourced archives were tested. Offline
compilation alone does not establish air-gapped preparation or signing.}}

The [comparison](independent-build-comparison.json) records each executable and
provider hash, compiler identity, source identity, and distinct worker IDs.
The [signed manifest](verification-manifest.json) identifies both images, runs,
timestamps, and hashes of the comparison, badge data, and
[admission policy](cluster-image-policy.json).

{{VERIFICATION_STATEMENT: identify the trusted key and fingerprint; record checks
of both downloaded artifact sets, detached bundle and SBOM signatures, actual
bundle hashes, registry image signatures, and registry SPDX/CycloneDX predicates
and image bindings. Describe only checks actually completed. State whether
transparency-log inclusion was checked.}}

To authenticate the manifest from the repository root, first approve the public
key through your own trust process, then run:

```sh
result=zccusan/deploy/zcblock-csi/fips/reproduced/{{SOURCE_REVISION_SHORT}}
cosign verify-blob \
  --key {{TRUSTED_PUBLIC_KEY_REPOSITORY_PATH}} \
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

The command above uses pinned-key verification without transparency-log checking.
For a new rebuild, follow
[Reproduced executables and image admission](../../REPRODUCIBILITY-ADMISSION.md).
For registry verification and verification without Cosign, follow the
[image validation guide](../../../IMAGE-VALIDATION.md).

{{ARCHIVE_STATEMENT: give the build and launch artifact names, retention period,
and any durable archive location. Explain which original artifacts must be
retained to repeat the comparison after GitHub artifacts expire.}}

The badge applies only to this revision and unsigned executable bundle.
SBOM bytes, OCI metadata, signature envelopes, later source revisions, and
deployment conformance are outside this comparison.
