# Foundational secret providers

`zcglobal-policy-node` deliberately consumes only a local mode-0600 credential
bundle. It does not link an AWS SDK, Vault client, cloud TLS stack, or provider
credential chain into the storage/control binary. `zcsecret-materialize` is the
small provider boundary that validates and atomically publishes that file.

This separation has two useful properties:

- data-plane and Raft binaries receive no permission to call a secret store;
- AWS CLI, Vault Agent, Secrets Store CSI, and their dependency trees can live
  in an init container or sidecar image rather than every performance image.

Build the provider sidecar independently:

```console
cargo build --release --locked \
  --manifest-path tools/zcsecret-materialize/Cargo.toml
```

That package reuses the lifecycle source but has only `serde` and `serde_json`
as direct dependencies. It does not build or link the main `zcutils` library,
AWS SDK, Vault client, Reqwest, Rustls, Tokio, CSI, or storage data plane. The
AWS CLI and Vault Agent belong in provider-specific sidecars; a deployment that
uses only one provider does not need to ship the other provider's executable.

The provider value is the complete JSON bundle produced by
`zcglobal-policy-node credential-init` or `credential-rotate`. It contains the
new preferred credential and every unexpired predecessor needed for overlap.
Provider-side rotation must perform a compare-and-set on `generation`.
Materializers reject generation rollback and same-generation content changes.

The materializer's generic lifecycle bounds are `ZCSECRET_MAX_TTL`,
`ZCSECRET_ROTATE_BEFORE`, `ZCSECRET_ACTIVATION_CLOCK_SKEW`, and
`ZCSECRET_MAX_VERSIONS`. They fall back to the corresponding
`ZCGLOBAL_ADMIN_*` settings so an existing global-policy deployment has one
source of truth. The consuming process remains authoritative and validates the
same bounds again; materialization is not an authorization decision.

## AWS Systems Manager Parameter Store

```console
zcsecret-materialize sync \
  --provider aws-ssm \
  --name /zc/prod/global/admin-credentials \
  --region us-east-1 \
  --output /run/zcsecrets/global-admin.json \
  --interval 2s
```

The helper invokes `aws ssm get-parameter --with-decryption` and uses the AWS
CLI's ambient credential chain. Use an EC2 instance role, IRSA, EKS Pod
Identity, or web identity. It passes no access key, secret key, session token,
profile, or password. Static access-key environment variables and AWS profiles
are rejected by default, and the child CLI is isolated from the normal shared
credentials/config files. The narrowly scoped IAM
permission is `ssm:GetParameter` for the exact parameter ARN plus
`kms:Decrypt` for its customer-managed KMS key when applicable.

Parameter Store keeps versions, but it is not the rotation coordinator. A
controller creates the new bundle, writes a strictly higher generation, waits
for materializer/node acknowledgement, and lets predecessor timestamps expire.

## AWS Secrets Manager

```console
zcsecret-materialize sync \
  --provider aws-secrets-manager \
  --secret-id zc/prod/global/admin-credentials \
  --region us-east-1 \
  --output /run/zcsecrets/global-admin.json \
  --interval 2s
```

The secret must be a `SecretString` containing the canonical bundle. The
minimal permission is `secretsmanager:GetSecretValue` for the exact secret ARN,
plus `kms:Decrypt` when a customer-managed KMS key is used. Authentication is
the same passwordless ambient-IAM path as Parameter Store. A Secrets Manager
rotation Lambda may publish the bundle, but its stage transitions must preserve
the overlap encoded inside the document; a provider staging label alone is not
accepted as proof that both generations work.

## HashiCorp Vault

Vault Agent or the Vault Secrets Store CSI provider owns authentication and
renewal. Prefer the Kubernetes auth method using the pod's projected,
audience-bound service-account token, or Vault AWS auth using the workload's IAM
identity. Do not pass a Vault token or AppRole SecretID to the storage process.

Configure the Agent template destination with `perms = "0600"`, then validate
and republish it:

```console
zcsecret-materialize sync \
  --provider vault-agent \
  --source-file /vault/secrets/global-admin-source.json \
  --output /run/zcsecrets/global-admin.json \
  --interval 500ms
```

The source may be a Vault KV value or a response produced by a rotation plugin.
The template must render only the bundle JSON, not a surrounding Vault response.
The materializer never receives Vault login credentials and never calls Vault.
Vault Agent's cached client token remains in the Agent's own process/filesystem
boundary.

## Deployment and failure behavior

Use a memory-backed `emptyDir` (or private `/run` tmpfs on a VM) for the output.
The materializer writes through a new mode-0600 file, `fsync`s it, atomically
renames it, and syncs the directory. A transient provider failure retains the
last validated bundle but never extends its expiry. Once every retained version
expires, `zcglobal-policy-node` fails authentication closed even if the provider
is unavailable.

For a 30-second-expiry/10-second-rotation exercise, run the node and
materializer with:

```text
ZCGLOBAL_ADMIN_MAX_TTL=30s
ZCGLOBAL_ADMIN_ROTATE_BEFORE=10s
ZCGLOBAL_ADMIN_RELOAD_INTERVAL=100ms
```

Publish a generation every 10 seconds. Each new document should still contain
the prior unexpired generations. Provider conformance requires continuous
status reads, successful Raft mutation after every publication, rejection of
rollback, and rejection of the first generation at its exact expiry.

The global RPC can consume atomically replaced PEM CA, certificate, and private
key files for its optional mutual-TLS mode and reloads them on every new
connection. The current `zcsecret-materialize` document validator understands
credential bundles only; certificate issuance, SAN policy, expiry thresholds,
and overlapping CA-bundle assembly remain responsibilities of the external
issuer/controller or provider agent. A later kind-specific materializer should
validate those properties before publication. KMS-backed volume encryption
should normally store only key ARNs/URIs and wrapped DEKs locally, never export
the KMS root key. Data-plane TLS and key-envelope adapters remain fail-closed
until implemented.
