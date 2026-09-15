# FIPS crypto integration and migration

This is implementation evidence and a review guide, not a completed acceptance
record or a claim that the application/container has its own CMVP certificate.
The candidate module is AWS-LC FIPS 3.1.0, certificate 5314. Its Security Policy
and actual deployment/build evidence control the claim.

## Changed services

| Service | FIPS implementation | Evidence |
| --- | --- | --- |
| Transfer credentials and random session bindings | AWS-LC SystemRandom / module DRBG | Individually measured application RNG calls |
| Secret lifecycle credential generation | AWS-LC SystemRandom / module DRBG | Individually measured generation call; lifecycle tests |
| Sequential, parallel, native transport and RPC key derivation | AWS-LC SP 800-108 Counter HMAC-SHA256 | Each active derivation measured independently |
| AES-256-GCM encryption | `RandomizedNonceKey`, internal 96-bit IV | Encryption measured with a raw test key, outside the KDF interval |
| AES-256-GCM decryption | Same key/provider; received IV passed to open only | Isolated service indicators, plaintext equality, negative tests |
| Rustls TLS | Installed AWS-LC FIPS provider; RPC client/server configurations check `fips()` | Module/provider probes; complete TLS call-graph review still required |

The probe uses disposable public test values. Module negative controls deliberately
call non-approved APIs. Those diagnostic processes are not approved workloads.
A before/after counter proves only that at least one approved service ran inside
its interval. It cannot certify arbitrary composite call graphs.

Credentials must contain 32 random bytes encoded as 64 hexadecimal characters,
or the existing `zct1.issued.expiry.hex` transfer envelope. Hex shape checks do
not prove entropy: operators must generate/import keys through the approved
credential procedure, never substitute a password or predictable hex string.
The decoded 32-byte credential is the KDF input. Fixed info contains a versioned
domain, zero separator, length-prefixed credential representation and context,
and a 32-bit big-endian output length of 256 bits. Native contexts separate lanes
and directions. Temporary raw KDF input, output and fixed-info buffers are zeroized.

Internally generated random IVs do not remove GCM usage limits. Credential
rotation must bound aggregate encryptions under each derived key across all
processes/nodes/restarts to the applicable SP 800-38D random-IV limit (at most
2^32 invocations per key). The application reserves an attempt before each
application-frame GCM encryption through `approved_crypto::seal` and rejects
attempts above 2^32 under the same tracked key in one process. AWS-LC SHA-256
identifies the actual derived key, so reconstructing a key or sharing it among
threads does not reset the counter. Failures and authenticated empty frames
consume attempts too; decryption remains available after exhaustion. The registry
never evicts keys and rejects unseen encryption keys after 65,536 distinct keys
have encrypted in that process. Plan capacity and key lifecycle accordingly.
Do not restart merely to evade a budget or capacity failure: the lifetime
usage of any reused key must still be accounted for.

Current expiration/rotation is time-based; there is still no distributed or
restart-persistent per-key invocation counter. TLS record protection also has
its own provider/protocol limits and is outside this application-frame counter.
Deployment review must establish
a defensible workload/rate/lifetime bound before acceptance. This guide cannot
substitute for that evidence.

## Wire versions and migration

| Path | Existing non-FIPS build | FIPS build |
| --- | --- | --- |
| Sequential encrypted transfer | `ZC_AES256_GCM_FRAME_V1` | `ZC_AES256_GCM_FRAME_V2` |
| Native stream transport | `ZCNBAE01` | `ZCNBAE02` |
| Parallel transfer descriptor | `ZCTCPMUX_PARALLEL_V2` (reads V1 too) | `ZCTCPMUX_PARALLEL_V3` |
| Global RPC envelope | `ZCGRPC01` | `ZCGRPC02` |
| Legacy native payload-only AES | Available | Rejected before socket I/O; use stream encryption |

FIPS peers must be upgraded together. There is no automatic downgrade or silent
reinterpretation of old encrypted data. Non-FIPS builds retain the previous
cipher/KDF/framing behavior; retain an appropriate old-format tool to decrypt
existing V1 data, then re-encrypt with the new format in a separately controlled
migration workflow. The FIPS executable rejects legacy-format ciphertext.

Native FIPS frames encode a 12-byte module-generated IV, ciphertext, and 16-byte
tag. The old per-session/sequence nonce calculation is now an authenticated
session/sequence binding, never the encryption IV. Lane, offset and length AAD
remain authenticated. Sequential and encrypted parallel EOF markers carry an
empty authenticated frame, so truncation cannot forge successful completion.
A streaming receiver can already have emitted earlier authenticated chunks when
it detects truncation; callers must not publish the output as complete until the
entire transfer succeeds.

RPC retains its 28-byte header and 16-byte tag overhead. The V2 header's IV field
is zeroed when constructing AAD, since the actual IV is returned by AWS-LC after
encryption. GCM authenticates the actual IV; all other header bytes are AAD.
Tests flip every header/ciphertext/tag byte, check direction and key separation,
verify limits, and exercise overlapping credential rotation.

## Remaining acceptance work

The current UBI builder/runtime on this local non-FIPS host is a functional lab
configuration. It does not meet the certificate-5314 tested-environment profile.
Acceptance continues to reject an unsupported builder/node/userspace, missing
approved-mode/deployment evidence, and incomplete service-map/build review.
QEMU checks provide functional evidence, not a blanket extension of a certificate.
A permitted port needs its own reviewed policy basis; do not edit the profile
solely to turn a local run green.

The full source inventory still requires dispositions for non-security checksums,
legacy code excluded by `cfg(not(feature = "fips"))`, ring, old Rustls brought in
by k8s-csi/tonic, and every TLS construction site. The CSI gRPC endpoint is a Unix
socket, but dependency presence alone cannot establish that alternate crypto is
unreachable. No dependency finding has been silently waived here.

Review `build_procedure`, `crypto_service_map`, `operating_policy` and
`deployment_scope` against the exact image and receipt. In particular, assess
CMake symbol prefixing/options against the approved source-build instructions,
external entropy assurance, key generation/import/rotation and aggregate GCM
limits, TLS certificates/algorithms, module self-test failure handling, and all
Security Policy operational restrictions. No paid independent lab is automatically
required merely to embed the upstream module; a reviewer must still supply actual
evidence for the deployment and the scope of the vendor's claim.

Sources: [certificate 5314](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5314),
[Security Policy](https://csrc.nist.gov/CSRC/media/projects/cryptographic-module-validation-program/documents/security-policies/140sp5314.pdf),
[AWS-LC-Rs 1.15.3 release](https://github.com/aws/aws-lc-rs/releases/tag/v1.15.3).

## Frozen-version security advisories

Pinning the certificate's source also pins code with upstream security advisories.
The inventory adds mandatory review findings for the pinned sys packages:
[CRL scope checking](https://github.com/aws/aws-lc-rs/security/advisories/GHSA-9f94-5g5w-gf6r),
[PKCS7 chain verification](https://github.com/aws/aws-lc-rs/security/advisories/GHSA-vw5v-4f2q-w9xf),
[PKCS7 signature verification](https://github.com/aws/aws-lc-rs/security/advisories/GHSA-hfpc-8r3f-gw53),
and [AES-CCM tag verification](https://github.com/aws/aws-lc-rs/security/advisories/GHSA-65p9-r9h6-22vj).
The non-FIPS `aws-lc-sys` 0.36.0 also needs the
[X.509 name constraints](https://github.com/aws/aws-lc-rs/security/advisories/GHSA-394x-vwmw-crm3)
assessment. The changed application services use AES-GCM, not CCM, and Rustls
uses WebPKI certificate verification. These are starting points for reachability
analysis, not a blanket waiver for every shipped binary. No PKCS7/X509/CCM API
may be added without revisiting this assessment. If an affected operation is
reachable, use a patched module and a corresponding policy/validation basis;
do not patch the frozen module and retain an unchanged-source claim.

Cargo resolves WebPKI 0.103.13 with this wrapper. Upstream's subsequent
0.103.14 release adds ML-DSA support; 0.103.13 includes the earlier CRL parsing
and URI name-constraint fixes. See [upstream release notes](https://github.com/rustls/webpki/releases).


## Structural service-assurance checks

`scripts/fips-service-assurances.py` collects review inputs from the complete
first-party Rust source hashes, Cargo inputs and the target-filtered resolved
Cargo graph. Collection rejects a root without the FIPS feature, a reachable
AWS-LC wrapper or Rustls 0.23 without its FIPS feature, missing graph nodes, or
an absent FIPS native provider. It records the paths that bring alternate
providers into the graph and inventories TLS construction, alternate crypto,
external RNG and restricted native APIs. The scan includes inactive and test
code deliberately; it is a review index, not a Rust call-graph analysis.

Offline image assembly runs the Rust budget/telemetry tests and Python
assurance tests, then retains `cargo-metadata.json` and `service-inventory.json`
under `/usr/share/zcutils/fips/`. The build receipt binds both. Deployment
acceptance recomputes the source/graph inventory and rejects disagreement.
It writes an incomplete `*.service-review-template.json`; missing semantic and
operational records leave the services and operation gates BLOCKED.

Pass the completed assessment with `fips-acceptance.py check --service-review
review/service-review.json`, alongside the existing `--review` deployment record.
The records must describe the same exact image. For separate inspection of
these inputs, the lower-level commands are:

```sh
cargo metadata --offline --locked --features fips --filter-platform "$TARGET" \
  --format-version 1 > cargo-metadata.json
python3 scripts/fips-service-assurances.py collect --source-root . \
  --cargo-metadata cargo-metadata.json --out service-inventory.json
python3 scripts/fips-service-assurances.py check --inventory service-inventory.json \
  --review review/service-review.json --image-digest "$IMAGE_DIGEST"
```

`TARGET` must match the actual compiled target. To establish coverage of the
exact release, a named reviewer supplies a schema-1 JSON record binding the
`inventory_sha256`, `image_digest`, `reviewed_at` and `expires_at`. Assessment
must occur within those dates; the assessor sets expiry under the applicable
policy. No arbitrary maximum duration is imposed by this checker.
Its `findings` map must cover exactly every inventory finding ID. Each entry
contains a disposition (`approved-service`, `non-security-use`,
`unreachable-in-release` or `diagnostic-only`), a rationale and a `record` with
bundle-relative `path` and `sha256`. Full source hashes mean changed control flow
invalidates the review even when the scanner's matches stay the same.

To establish permitted operation, `sections` must bind supporting records for
`tls_service_coverage`, `dependency_reachability`, `entropy_and_credentials`,
`self_test_failure_handling` and `operating_conditions`, each with `path` and
`sha256`. Reviewers gather these through source tracing, protocol/configuration
inspection, fault tests and observation of the actual operating environment.
The script verifies record presence and integrity, not their semantic truth.

To establish an aggregate GCM usage bound, `gcm_key_budgets` must enumerate
`key_scope`, `max_encrypting_instances`,
`max_attempts_per_second_per_instance`, `max_key_lifetime_seconds`,
`prior_attempts`, and an `enforcement_record` with `path` and `sha256`.
The checker rejects non-integer bounds, duplicate scopes, missing justification,
and totals above 2^32:

```
prior_attempts + max_encrypting_instances
              * max_attempts_per_second_per_instance
              * max_key_lifetime_seconds <= 2^32
```

These must be enforced maximums over every process/node/restart using the same
actual key, including retries, failures and empty frames; measured averages are
insufficient. The reviewer must establish that scopes cannot double-count a
shared key as independent budgets and that no encryption path is omitted. The
script cannot discover undeclared key reuse or enforce distributed rates.
Authenticate the reviewer's identity and records independently and compare the
image digest with the installed image. Passing these structural checks is not
a semantic approval, a vendor letter or a CMVP certificate.


## Observing encryption budgets alongside performance

The existing control and telemetry-server `/metrics` endpoints now publish
label-free `zccusan_fips_application_frame_gcm_*` gauges for **application-frame
GCM through `approved_crypto::seal` in their own process**. These do not observe
Rustls/AWS-LC TLS record encryption or independent calls by dependencies.
Performance telemetry emitted by an encrypting process carries the corresponding
`fips_application_frame_gcm_*` integer fields, retained through the existing
non-identifying telemetry allowlist:

| Suffix | Operator use |
| --- | --- |
| `process_max_key_consumed_attempts` | Consumption of the most-used application-frame key in this process; recreated key objects share its counter. |
| `process_min_key_remaining_attempts` | Smallest remaining tracked application-frame key budget in this process. Zero means at least one key has exhausted its process ceiling. With no tracked keys, this reports the per-key ceiling. |
| `per_key_attempt_limit` | Fixed 4,294,967,296 process ceiling for random-IV GCM attempts. |
| `process_tracked_keys`, `process_registry_key_capacity` | Registry occupancy and its fixed capacity; previously unseen encryption keys fail at capacity. |
| `process_exhausted_keys` | Number of tracked application-frame keys that have reached the ceiling. |
| `enabled`, `process_snapshot_available` | Whether this build enables the counter and whether the registry snapshot was obtained. Missing detail gauges must not be interpreted as zero consumption. |
| `scope_application_frames_only` | Always one: the observations concern application-frame GCM, not every cryptographic service in the process. |
| `tls_record_accounting_complete` | Always zero: TLS record usage and its provider/protocol limits must be assessed separately. |
| `cross_process_accounting_complete` | Currently always zero: these gauges cannot establish a complete distributed key budget. |

Inspect low remaining budget together with the accepted workload/rate/lifetime
bound and rotate keys before its aggregate ceiling. Do not add remaining budgets
across processes, reset alerts on process restart, or assume the telemetry-server
process's counters describe the encryption workers it receives events from.
Worker performance events preserve their own process scope. No raw key, key hash,
credential, or per-key label is published. Worst-case per-key consumption and
remaining-budget gauges avoid both sensitive identifiers and unbounded series.

A fleet-wide consumed/remaining value for a key reused across nodes or restarts
still needs coordinated, durable per-key accounting or a reviewed collection
scheme that preserves private key identity and accounts for missing writers.
That work is not implemented; the explicit completeness gauge prevents these
local measurements from being represented as that assurance. The steady-state
encryption path uses an atomic reservation after the first registry lookup;
metric snapshots scan the bounded registry outside the encryption operation.
