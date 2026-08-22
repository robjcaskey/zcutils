# Global transport security and secret lifecycle

## Threat boundary

Every cross-region segment is untrusted. This is a hard invariant, not a
default that an administrator can relax. Same-AZ and same-region trust are
separate policy inputs and both default to `untrusted`. Marking a local segment
trusted may remove redundant local controls in a future adapter; it never
creates an identity, authorizes a federation, or makes trust transitive.

All user data crossing a network boundary must use authenticated encryption.
The supported policy targets are:

- `native-aead`: authenticated payload encryption with
  `public_envelope_v1`; this is the native performance mode.
- `native-aead+tls`: the same native protection inside mutually authenticated
  TLS; this is the check-the-box TLS mode.

TLS measurements are compliance/regression results. They must be labeled
`headline_performance_eligible=false` and must not replace native-AEAD results
in performance records. This does not permit a plaintext benchmark to be
published as representative.

`public_envelope_v1` permits only protocol magic/version, direction, an
unpredictable nonce, ciphertext length, and audited transport flags outside
the authenticated ciphertext. A lane-oriented data protocol may additionally
declare lane ordinal/count and frame sequence only after those fields have been
reviewed as non-sensitive for that protocol. Tenant, federation, node, cluster,
region, volume and object IDs; logical offsets; topology names; CPU/NUMA
placement; plan hashes; placement epochs; plaintext checksums; user bytes; and
error details are sensitive and must not occur in the public envelope. Adding
a field requires a new reviewed framing profile rather than silently extending
version 1.

The Raft policy API can declare either transport mode and independently set
same-AZ/same-region trust. Cross-region remains untrusted in code. A transport
adapter must fail closed if it cannot realize the committed mode. Global Raft
and management RPC now use `native-aead` by default: their complete JSON
document is encrypted with AES-256-GCM and the fixed 28-byte public envelope is
authenticated as additional data. Request and response keys are domain
separated. There is no plaintext compatibility fallback.

The existing TCP-mux data-plane topology header has not yet earned
`public_envelope_v1` conformance and must not be selected for an untrusted
segment. In particular, its current token preamble and topology fields are not
made safe merely because policy declares a secure mode. Policy declaration is
not evidence of wire conformance. Adapters must call the control-path
`LinkTransportSecurity::validate_realization` check before activation. The
legacy TCP mux capability descriptor fails that check for both modes.

## Global RPC TLS mode

Set `ZCGLOBAL_RPC_TRANSPORT=native-aead+tls` to wrap the native encrypted frame
in mutually authenticated TLS 1.3. TLS supplements native protection; it never
replaces it. All four identity settings are then required and startup fails
closed if any is absent:

| Setting | Meaning |
|---|---|
| `ZCGLOBAL_TLS_CA_FILE` | PEM trust bundle used for both server and client verification |
| `ZCGLOBAL_TLS_CERT_FILE` | PEM certificate chain presented by this node/client |
| `ZCGLOBAL_TLS_KEY_FILE` | PEM private key; must be a mode-0600 regular file |
| `ZCGLOBAL_TLS_SERVER_NAME` | DNS name or IP verified on every outbound connection |

The protocol is TLS 1.3 only, requires a client certificate, and negotiates the
`zcglobal-rpc/1` ALPN identifier. CA, certificate, and key files are rebuilt
for every new connection, so an atomic file replacement rotates identities and
overlapping CA bundles without restarting a node. Existing connections may
drain; this RPC currently uses one request per connection.

Node readiness and status expose `rpc_transport` and
`headline_performance_eligible`. The value is `false` whenever TLS is enabled.
Benchmark publication must preserve that field and must never promote a TLS
run to the native-AEAD headline series.

## Secret lifecycle invariant

Every credential has all of the following:

1. a non-secret version ID and monotonically increasing generation;
2. explicit `not_before` and hard `expires_at` timestamps;
3. a configured maximum TTL and rotate-before threshold;
4. an overlap mechanism where old and new credentials are independently valid;
5. atomic publication and reload without an I/O-hot-path lookup;
6. fail-closed behavior after the last accepted version expires;
7. redacted logs and status that reports IDs/times, never secret bytes; and
8. a tested replacement and revocation procedure.

The management credential file is a mode-0600 JSON bundle. It has one preferred
credential and retains unexpired predecessors during rollout. Rotation prunes
expired versions, adds a new active version, and never extends a predecessor's
expiry. Servers periodically reload the file and retain their last valid copy
through a transient file error, but that retained copy still expires normally.
Clients try the preferred credential and can fall back to an unexpired
predecessor while publication reaches all servers.

AWS Parameter Store, AWS Secrets Manager, and HashiCorp Vault materialization
are described in `FOUNDATIONAL_SECRET_PROVIDERS.md`. They use workload identity
and a separate lightweight helper/agent so provider SDKs and provider access do
not enter the storage binaries.

Configuration:

| Setting | Default | Meaning |
|---|---:|---|
| `ZCGLOBAL_ADMIN_MAX_TTL` | `90d` | Hard maximum validity of one management credential |
| `ZCGLOBAL_ADMIN_ROTATE_BEFORE` | `7d` | Status threshold indicating rotation is due |
| `ZCGLOBAL_ADMIN_RELOAD_INTERVAL` | `250ms` | Control-plane file reload cadence |
| `ZCGLOBAL_ADMIN_ACTIVATION_CLOCK_SKEW` | `2s` | Early activation allowance; never extends expiry |
| `ZCGLOBAL_ADMIN_MAX_VERSIONS` | `16` | Bound on overlapping credentials |

Durations accept `ms`, `s`, `m`, `h`, or `d`. Initialize and rotate atomically:

```console
zcglobal-policy-node credential-init /run/secrets/zcglobal-admin.json 30s
zcglobal-policy-node credential-rotate /run/secrets/zcglobal-admin.json 30s
zcglobal-policy-node credential-status /run/secrets/zcglobal-admin.json
```

For a deliberately aggressive continuous-rotation test:

```console
scripts/zcglobal-credential-rotation-smoke.sh
```

That test uses a 30-second TTL, rotates every 10 seconds three times, maintains
a sub-second status subscription, and commits a Raft policy mutation after each
rotation.

The TLS conformance variant generates an ephemeral CA/node identity, repeats
the rotation workload through mutual TLS 1.3, verifies that a client without a
certificate is rejected, and asserts the result is headline-ineligible:

```console
scripts/zcglobal-transport-security-smoke.sh
```

One-use TCP-mux/replication authentication tokens are generated as expiring
`zct1` credentials. `ZC_TRANSFER_TOKEN_TTL_SECS` defaults to 900 and may not
exceed `ZC_TRANSFER_TOKEN_MAX_TTL_SECS` (default 3600). Expiry is checked when a
new connection authenticates; an already authenticated transfer is not killed
mid-frame. Legacy non-expiring authentication tokens fail closed unless the
temporary migration escape hatch `ZC_ALLOW_LEGACY_NONEXPIRING_TOKENS=1` is set.
The escape hatch is not an acceptable steady-state configuration.

Volume data-encryption keys are not bearer credentials. Their rotation story is
cryptographic erasure plus re-wrapping: volume DEKs remain random and stable for
existing ciphertext extents, while versioned KMS/HSM KEKs rotate and re-wrap
DEKs without bulk data rewriting. New writes may advance to a new DEK epoch.
Only wrapped DEKs and key IDs may be referenced by policy; plaintext keys never
enter Raft. This key-envelope implementation remains outstanding.

TLS certificate/private-key rotation uses overlapping trust bundles and
new-connection config reload. Certificate validity is checked by the TLS peer
on every handshake. The external certificate issuer/controller remains
responsible for configurable expiry thresholds and overlap timing; the global
RPC does not extend X.509 validity. TLS support for data-plane adapters remains
outstanding, so selecting `native-aead+tls` must fail closed in any adapter that
does not explicitly advertise that capability.
