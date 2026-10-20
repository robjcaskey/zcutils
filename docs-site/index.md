# Identity with explicit boundaries

Operating guides for federated access, scoped regional failover and revocation
in zcutils. Start with a local laboratory, follow the permissions through a real
request, and inspect the evidence before choosing a production integration.

## Choose your exercise

| Guide | What you will demonstrate |
| --- | --- |
| [SAML and real-time revocation](../docs/saml-realtime-revocation-debug.md) | An OAuth-backed SAML IdP, two nginx auth proxies, group changes on existing sessions and signed logout. |
| [Day-two identity operations](../zccusan/docs/DAY_2_FEDERATED_IDENTITY_AND_FAILOVER.md) | OAuth, AWS IAM and native Kubernetes identities; tenant boundaries, approval, failover and cleanup. |
| [The federation laboratory](../docs/federated-identity-breakglass-debug.md) | Two independent OAuth issuers, scoped replication, workload handoffs and a supplementary approval workflow. |

## The permission model

An identity is qualified by its issuer. The same name at another issuer does
not inherit access. An administrator grants explicit actions on regions,
tenants and resources; switching login method does not reset a resource budget.

Sensitive operations can require a second, time-limited approval. That approval
is bound to the requesting principal and exact operation. Workload failover
checks the complete consistency group at both endpoints.

## Revocation on the next request

The SAML exercise checks current authorization on each protected request.
Removing an operator group changes what an existing session can do. Signed
Global Token Revocation requests remove sessions and deny otherwise unexpired
OAuth access tokens for the selected issuer and subject.

Group snapshots use a separate debug protocol. The Global Token Revocation
profile follows [Okta's documented Universal Logout flow](https://developer.okta.com/docs/guides/oin-universal-logout-overview/);
the underlying GTR specification remains an evolving draft.

## Know what the laboratory proves

These are executable debug integrations. Local logical regions and ordinary
worker processes exercise the real Rust authorization, limiter and failover
code. The SAML lab uses local SQLite authorization logs and loopback HTTP.

Production Raft event delivery, production SAML hosting and live Okta
interoperability remain separate work. The guides distinguish local IAM fixtures
from opt-in real AWS verification, and explain resource teardown explicitly.

## Run without cloud resources

Start with the [SAML local run](../docs/saml-realtime-revocation-debug.md#run-locally)
or the [native Kubernetes exercise](../zccusan/docs/DAY_2_FEDERATED_IDENTITY_AND_FAILOVER.md#native-kubernetes-identity-without-aws-or-vault).
The default local exercises create no AWS resources. Real EKS verification is
an explicit, separate option with temporary charges and a cleanup audit.
