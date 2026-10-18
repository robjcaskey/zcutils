# Federated identity and approval laboratory

This is an executable authorization specification and a loopback-only debug
fixture. It runs two independently keyed OAuth authorization servers, a
separate approval authority, and east/west HTTP adapters. The adapters invoke
the actual Rust authorization, bilateral placement, failover, and lane-limiter
implementations. Existing production management listeners still use their
current federation credentials: this change does not silently enable OAuth on
them or replace consensus/member authentication.

## Research and mechanism selection

These mechanisms solve related but different problems:

| Mechanism | Relevant behavior | Choice in this laboratory |
| --- | --- | --- |
| [ServiceNow Ask for Approval](https://www.servicenow.com/docs/r/build-workflows/workflow-studio/ask-approval-flow-designer.html) | Workflow records, approval/rejection rules, due dates, and approval state; timeout outcomes depend on workflow configuration | Emulate a pending request, independently authenticated approve/deny decision, ticket/reason, and deadline. Timeouts always deny; never auto-approve. |
| [Microsoft Entra PIM](https://learn.microsoft.com/en-us/entra/id-governance/privileged-identity-management/pim-approval-workflow) | Eligible privileged roles can require approval before activation | Separate eligibility from active, time-limited permission. A ticket identifier alone is never authority. |
| [Vault control groups](https://developer.hashicorp.com/vault/docs/enterprise/control-groups) | Additional authorization factors can protect particular capabilities, including joint approval | Require independent approval only for explicitly protected operations, in addition to ordinary resource authorization. |
| [Vault credential leases](https://developer.hashicorp.com/vault/docs/concepts/lease) | Explicit TTLs, renewal rules, and revocation | Hard expiry and revocation are enforced by the consuming verifier; identity refresh cannot extend a grant. |
| [AWS temporary role sessions](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_credentials_temp_request.html) | Trusted callers assume bounded sessions with restricted permissions | Bind the grant to one principal, federation, resource, tenant path, region, action and role. It cannot enlarge the locally permitted scope. |
| [Microsoft emergency access accounts](https://learn.microsoft.com/en-us/entra/identity/role-based-access-control/security-emergency-access) | Emergency credentials are independent of ordinary authentication dependencies; Microsoft recommends permanent active emergency administrator assignments | Distinguish true emergency recovery from approval-based JIT. Do not make a workflow outage silently bypass authorization. |

The special-purpose debug server implements **approval-based JIT elevation**,
often informally called break-glass. It is an authorization/approval authority,
not a third general OAuth login server and not an OAuth middleman replacing
the original principal. A real ServiceNow integration would authenticate to a
specific instance, verify the actual approved workflow record, and have this
broker mint a bounded entitlement. OAuth access to ServiceNow by itself is not
approval to administer zcutils.

True emergency recovery when both the IdP and workflow are unavailable needs a
separate design: independently held hardware credentials or recovery keys,
explicitly bounded recovery authority, out-of-band custody/dual control,
alerts and post-event review. This fixture does not implement an emergency
bypass, MFA, or hardware-backed approval. Its independent reviewer credential
is a generated debug-only secret, not a production authentication design.

## Trust and authorization contract

```text
OAuth issuer A ----\
                   +--> east/west gateway --> local grants --> existing budgets
OAuth issuer B ----/             ^
                                |
                 independent approval authority
                 request -> approve/deny -> bounded signed grant
```

Identities are `(configured issuer, subject)` pairs. Issuer A and B do not share
keys or trust each other. Each resource-owning region has explicit grants;
issuer roles, groups, email, tenant claims, and matching subject strings cannot
create local authority. Tenant scope uses complete path segments: `acme`
includes `acme/retail` when descendants are delegated, but never `acme-other`.

The fixture's single federation contains:

| Principal | East | West |
| --- | --- | --- |
| A / alpha-admin | Read/write, replication, tenant-label management and clean failover for acme and its subtenants; record deletion additionally needs approval | Read/write when authoritative, replication, tenant-label management and clean failover for the same subtree; no deletion privilege |
| B / alpha-operator | Ordinary acme operations; no eligible approval grant | Same narrowly delegated acme operations |
| A / retail-only | Exact retail-subtenant operations | Exact retail-subtenant operations; cannot move siblings through atomic-set expansion |
| B / west-admin | Explicit beta permissions only | Local beta tenant; cannot administer acme |

The atomic acme application comprises root, retail and wholesale volumes.
Failover expands the complete set and authorizes every participant at both
source and destination **before** observations, checkpoints, custody or workload
state can change. Source outbound trust and destination inbound admission both
remain necessary. Ciphertext placement does not authorize key escrow.

Tenant management really updates bounded display metadata. It cannot change
tenant ownership, grants, parentage, or canonical budget IDs. It is not a full
tenant provisioning or IAM administration API.

The existing lane limiter is keyed by canonical resource budget, never bearer
token, connection, issuer, or role activation. Two issuers and refreshed tokens
consume the same budget. Approval does not raise a ceiling or mint capacity.
The current consensus-lease fallback to guaranteed IOPS is distinct from
authentication expiry; guarantees never restore access to an expired identity.

## Approval lifecycle

1. A valid OAuth client requests an eligible role for an exact resource/region/
   action, with a ticket, justification and duration.
2. The request starts pending. It carries no privileged bearer credential.
3. A separate reviewer credential approves or denies it. Client credentials
   cannot self-approve; modifying the approved tuple or replaying the decision
   is rejected. Unanswered requests expire without approval.
4. An approved request returns an ES256-signed `zc-approval+jwt` entitlement.
   It binds principal, federation, tenant, region, resource, action, role,
   ticket, approver and hard expiry. Maximum lab lifetime is 30 seconds.
5. A protected operation supplies both its ordinary identity token and that
   entitlement. The gateway obtains independently signed current grant status;
   the Rust verifier checks both signature and every binding before mutation.
6. Revocation prevents further protected requests. Status proofs have at most
   two seconds of validity in this adapter (the library caps them at five).
   Each protected HTTP operation fetches current status without caching.
   Previously signed status proofs therefore have a bounded replay window;
   the design does not promise instantaneous distributed revocation.
7. Approval expiry and identity expiry are independent. Refreshing identity
   cannot extend an approval, and approval cannot extend identity. A fresh
   elevation requires a fresh request and independent decision.

Grants authorize repeated matching operations during their window, not a
single-use transaction. Single-use approval would additionally require an
atomically consumed grant ID at the execution authority. Normal data operations
do not contact the approval service. On workflow failure, protected operations
fail closed while otherwise authorized ordinary operations continue.

The crypto verifier accepts only configured ES256 keys and explicit token
types/audiences. Incoming `iss` is a registry lookup, never a discovery URL;
token-supplied key URLs and algorithm substitution are rejected. Verified
identities are opaque and bound to the actual configured signing key. See
[OAuth security BCP](https://www.rfc-editor.org/rfc/rfc9700.html) and
[JWT security BCP](https://www.rfc-editor.org/rfc/rfc8725.html).

## Run

```sh
python3 -m venv /tmp/zc-auth-debug-venv
/tmp/zc-auth-debug-venv/bin/pip install -r scripts/requirements-federation-auth-debug.txt
cargo test --locked --lib federation_auth::tests
/tmp/zc-auth-debug-venv/bin/python scripts/zc-auth-federation-e2e.py
```

On a constrained shared root filesystem, put Cargo outputs on a suitable
temporary filesystem and use the explicit driver path:

```sh
export CARGO_TARGET_DIR=/tmp/zc-auth-target
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0
cargo build --locked --bin zc-federation-auth-debug
/tmp/zc-auth-debug-venv/bin/python scripts/zc-auth-federation-e2e.py \
  --skip-build --driver /tmp/zc-auth-target/debug/zc-federation-auth-debug \
  --output /tmp/zc-auth-e2e-evidence
```

The harness starts and terminates only its own child PIDs and uses ephemeral
ports and a private temporary directory. Configurations and ready files are
mode 0600. It deletes transient credentials at exit and retains only a proof
summary, approval audit and logs; tokens, client/reviewer secrets and private
signing keys are not uploaded as evidence.

To keep a fresh lab running for manual API experiments, append `--serve` to
the harness command. It prints public endpoint URLs and the private temporary
configuration directory, then waits for Ctrl-C. The private `issuer-a.json`
and `issuer-b.json` files contain the generated client credentials;
`approval.json` holds the distinct reviewer/introspection credentials. No
requests are automatically approved and no E2E success is claimed in this mode.

`zc_auth_debug_server.py --mode oauth|approval|regions --config FILE
--ready-file FILE` also allows individual fixtures to be launched manually.
The checked-in harness constructs the exact configurations; the Rust driver's
debug JSON schema is declared in `src/bin/zc-federation-auth-debug.rs` and its
authorization schema in `src/federation_auth.rs`.

OAuth debug mode implements the client-credentials grant, authorization-server
metadata and JWKS. It has no browser authorization-code flow, OIDC login,
refresh tokens, dynamic registration, key rotation or production TLS. Approval
mode exposes `POST /requests`, `GET /requests/{id}`, reviewer-only
`POST /requests/{id}/approve|deny|revoke`, adapter-only `POST /status`, and
reviewer-only `GET /audit`. Region mode exposes `POST /operation` on two
separate loopback listeners and keeps the Rust driver's stdin private.

## Evidence boundary

The scenario performs two complete east -> west -> east cycles. Real HTTP
clients write versioned records, transfer AES-256-GCM ciphertext between the
two listeners, compare exact restored data, invoke the existing failover state
machine, check custody/session epoch fencing, and resume writes at the target.
Atomic scope, foreign-record preservation, shared quotas, identity substitution,
expiry, approval denial/revocation and workflow outage are negative checks.

This is a security/control workflow test, not a regional HA or performance
benchmark. Consensus is one in-process state-machine model; three-replica
observations, application quiescence and external workload acknowledgements
are modeled. A workload is an HTTP client, not a Kubernetes pod or live VM.
Ordinary files are terminal debug media, never mirror/stripe primitives.
Ephemeral encryption keys exist only in the driver process; this is not a
recoverable key-management or encrypted-witness implementation.

All listeners bind only 127.0.0.1. HTTP credentials and routing metadata are
visible to privileged local observers. Debug clients refuse non-loopback
destinations, ignore ambient HTTP proxies and reject redirects. Although record payloads are encrypted,
this HTTP envelope is **not** `public_envelope_v1` and must not be deployed on
an untrusted/cross-region network. It does not change the production transport
contract or assert wire conformance. There is no Kubernetes TokenReview adapter
or live ServiceNow integration in this fixture.
