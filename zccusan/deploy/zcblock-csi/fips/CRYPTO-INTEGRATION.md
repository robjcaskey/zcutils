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
2^32 invocations per key). Current expiration/rotation is time-based; it does not
provide a distributed per-key invocation counter. Deployment review must establish
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
