# Federated identity and approval laboratory

This is an executable authorization specification and a loopback-only debug
fixture. It runs two independently keyed OAuth authorization servers, an AWS
IAM identity broker, a supplementary approval backend, and east/west HTTP adapters. The adapters invoke
the actual Rust authorization, bilateral placement, failover, and lane-limiter
implementations. Existing production management listeners still use their
current federation credentials: this change does not silently enable OAuth on
them or replace consensus/member authentication.

The [day-two guide](../zccusan/docs/DAY_2_FEDERATED_IDENTITY_AND_FAILOVER.md)
also covers native Kubernetes TokenReview identity without AWS/Vault. For
browser sessions, see the separate
[OAuth-backed SAML, nginx and real-time revocation lab](saml-realtime-revocation-debug.md).

## Research and mechanism selection

These mechanisms solve related but different problems:

| Mechanism | Relevant behavior | Choice in this laboratory |
| --- | --- | --- |
| [ServiceNow Ask for Approval](https://www.servicenow.com/docs/r/build-workflows/workflow-studio/ask-approval-flow-designer.html) | Workflow records, approval/rejection rules, due dates, and approval state; timeout outcomes depend on workflow configuration | Implement a generic pending request, independently authenticated approve/deny decision, ticket/reason, and deadline. Timeouts always deny; never auto-approve. |
| [Microsoft Entra PIM](https://learn.microsoft.com/en-us/entra/id-governance/privileged-identity-management/pim-approval-workflow) | Eligible privileged roles can require approval before activation | Separate eligibility from active, time-limited permission. A ticket identifier alone is never authority. |
| [Vault control groups](https://developer.hashicorp.com/vault/docs/enterprise/control-groups) | Additional authorization factors can protect particular capabilities, including joint approval | Require independent approval only for explicitly protected operations, in addition to ordinary resource authorization. |
| [Vault credential leases](https://developer.hashicorp.com/vault/docs/concepts/lease) | Explicit TTLs, renewal rules, and revocation | Hard expiry and revocation are enforced by the consuming verifier; identity refresh cannot extend a grant. |
| [AWS temporary role sessions](https://docs.aws.amazon.com/IAM/latest/UserGuide/id_credentials_temp_request.html) | Trusted callers assume bounded sessions with restricted permissions | Bind the grant to one principal, federation, resource, tenant path, region, action and role. It cannot enlarge the locally permitted scope. |
| [Microsoft emergency access accounts](https://learn.microsoft.com/en-us/entra/identity/role-based-access-control/security-emergency-access) | Emergency credentials are independent of ordinary authentication dependencies; Microsoft recommends permanent active emergency administrator assignments | Distinguish true emergency recovery from approval-based JIT. Do not make a workflow outage silently bypass authorization. |

The special-purpose debug server is a **supplementary approval backend** for
approval-based JIT elevation, often informally called break-glass. It is an authorization/approval authority,
not a third general OAuth login server and not an OAuth middleman replacing
the original principal. The product references above document research inputs;
the fixture implements its own debug protocol and makes no product compatibility
claim. A future external workflow adapter would authenticate to its backend,
verify the actual approved workflow record, and have this broker mint a bounded
entitlement. OAuth access to a workflow system by itself is not approval to
administer zcutils.

True emergency recovery when both the IdP and workflow are unavailable needs a
separate design: independently held hardware credentials or recovery keys,
explicitly bounded recovery authority, out-of-band custody/dual control,
alerts and post-event review. This fixture does not implement an emergency
bypass, MFA, or hardware-backed approval. Its independent reviewer credential
is a generated debug-only secret, not a production authentication design.

## AWS IAM identity without Vault

The supplementary IAM broker accepts temporary AWS assumed-role credentials
through a signed STS identity proof. It does not receive the AWS secret access
key, assume roles on the caller's behalf, or import AWS permissions into zcutils.
There is no Vault, Cognito, Private CA, EKS cluster or other AWS provisioning
dependency. The default E2E harness performs no AWS API calls and creates no AWS
resources. It explicitly substitutes a local, signature-verifying STS fixture.

1. An administrator registers an exact IAM role ARN and unique `RoleId` with the
   broker, then grants the mapped principal explicit zcutils tenant/resource,
   action and regional scopes. Account membership alone grants nothing.
2. The broker issues a one-use, 60-second challenge bound to its issuer,
   federation and audience. The client uses botocore to sign a POST
   `GetCallerIdentity` request, including the server binding, nonce and temporary
   session token in the signed headers.
3. The client exchanges the proof at `/token` using the OAuth token-exchange
   grant type and the private subject-token type
   `urn:zcutils:params:oauth:token-type:aws-sigv4`. This is a limited token-exchange
   profile, not a general OAuth/OIDC login provider or a product compatibility
   claim. Scopes, actor tokens and refresh tokens are not supported.
4. The broker validates the fixed action/body, signed headers, timestamp and
   challenge, then forwards only that request to the pinned commercial regional
   STS HTTPS endpoint. Caller-selected URLs, proxies and redirects are forbidden.
   The debug fixture transport is explicitly configured by the harness and can
   only connect to loopback. STS errors fail closed and consume the challenge.
5. A verified account, assumed-role ARN and `UserId` must match the registered
   role name and unique role ID. A deleted/recreated role with the same name is
   denied. IAM users, root and federated-user identities are not accepted.
6. The broker issues a signed access token with a maximum 60-second lifetime.
   Its stable subject is `aws:aws:<account>:role-id:<RoleId>`; the existing Rust
   verifier and explicit grants authorize every operation. Identity exchange
   creates no new volume budget. Group failover still authorizes the entire
   atomic set at both endpoints, and protected actions still require independent
   supplementary approval.

The AWS SDK credential chain used by `scripts/zc_aws_iam_login.py` supports
temporary role credentials, including credentials acquired through IRSA and
EKS Pod Identity. IRSA uses `AssumeRoleWithWebIdentity`; EKS Pod Identity uses its
agent/EKS Auth flow. The local test covers the resulting temporary credentials,
not those live acquisition flows. It also executes the login CLI using the SDK
environment credential provider with generated test credentials.

`GetCallerIdentity` returns account, ARN and user ID, not Kubernetes identity,
MFA state, session tags, IAM policy evaluation or source-session expiration.
Role-session names and caller-supplied namespace/service-account claims confer
no authority. Service accounts sharing an IAM role share the mapped principal;
use distinct IAM roles for distinct zcutils privileges or independently verify
Kubernetes identity. The broker's access token has its own short lifetime:
expiration/revocation of the source AWS session does not immediately revoke an
already-issued zcutils token. New exchanges always require a fresh STS proof.
Removing local grants can deny otherwise valid tokens. AWS policies governing
who may assume a registered role remain the AWS account owner's responsibility.

References: [STS GetCallerIdentity](https://docs.aws.amazon.com/STS/latest/APIReference/API_GetCallerIdentity.html),
[IAM role identifiers](https://docs.aws.amazon.com/IAM/latest/APIReference/API_Role.html),
[AWS SigV4 signing](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html),
[EKS workload credentials](https://docs.aws.amazon.com/eks/latest/userguide/service-accounts.html).
The server-binding pattern is also documented by
[Vault's AWS auth method](https://developer.hashicorp.com/vault/docs/auth/aws)
and [AWS IAM Authenticator](https://github.com/kubernetes-sigs/aws-iam-authenticator);
neither is a runtime dependency here.

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
An optional explicit resource set further restricts a grant. An absent set
uses the tenant scope; an explicit empty set grants no resources.

The fixture's single federation contains:

| Principal | East | West |
| --- | --- | --- |
| A / alpha-admin | Read/write, replication, tenant-label management and clean failover for acme and its subtenants; record deletion additionally needs approval | Read/write when authoritative, replication, tenant-label management and clean failover for the same subtree; no deletion privilege |
| B / alpha-operator | Ordinary acme operations; no eligible approval grant | Same narrowly delegated acme operations |
| A / retail-only | Exact retail-subtenant operations | Exact retail-subtenant operations; cannot move siblings through atomic-set expansion |
| B / west-admin | Explicit beta permissions only | Local beta tenant; cannot administer acme |
| Dedicated A / root and retail workers, B / wholesale worker | Read/write to each worker's exact resource only | Same resource restriction when authoritative here; no management, failover or elevation |

The atomic acme application comprises root, retail and wholesale volumes.
Failover expands the complete set and authorizes every participant at both
source and destination **before** observations, checkpoints, custody or workload
state can change. Source outbound trust and destination inbound admission both
remain necessary. Ciphertext placement does not authorize key escrow.

The workload adapter uses real OS child processes. Each has its own scoped
OAuth client credentials, obtains and renews its own access token, and writes
a persistent counter while pinned to one region and resource. A clean move:

1. Authorizes the complete atomic scope at both regions before stopping any
   process. A subtenant cannot use this path to stop a sibling workload.
2. Stops and reaps all managed source workers, then installs an admission
   barrier and waits for already-admitted source writes to finish.
3. Replicates the final encrypted cut and lets the existing failover state
   machine commit destination custody and its new placement epoch.
4. Starts destination worker processes. Each must authenticate and successfully
   write a counter continuing from the replicated value before it is ready.
5. Acknowledges the existing workload actions only after those write witnesses
   exist; only then can the operation become `completed`.

HTTP clients cannot submit adapter acknowledgements or assert that source
workers are quiesced. A separate fault scenario gives one destination worker
an invalid client credential: partial target activation is stopped, custody
remains fenced at the new epoch, actions stay unacknowledged and the operation
remains `reconciling_workloads`. The adapter must never roll custody back after
target writes or claim a successful move when target authentication failed.

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

The default command below is entirely local and uses the STS fixture. For a
real EKS identity run, use the separate opt-in runner described under
"Real EKS identity verification" below.

```sh
python3 -m venv /tmp/zc-auth-debug-venv
/tmp/zc-auth-debug-venv/bin/pip install -r scripts/requirements-federation-auth-debug.txt
/tmp/zc-auth-debug-venv/bin/python scripts/test_aws_iam_auth.py
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
`workload-transitions.json` records source process exits, destination process
incarnations and their first successful write witnesses for all 12 handoffs.
`workload-events.log` records real starts and reaping; `activation-failure/`
contains the independent failure-scenario logs.

### Real EKS identity verification

`scripts/zc-auth-federation-live-eks.py` reuses the existing disposable EKS
Terraform stack by copying only its `.tf` sources and provider lock file into a
new private run directory. Source state and plans are never copied or modified.
It assigns a unique name, creates a dedicated VPC, uses one small Spot worker,
disables performance nodes, and rejects plans containing NAT gateways,
Cognito, Private CA, EIPs or performance launch templates. This opt-in run
incurs temporary EKS, worker, root-disk and public IPv4 charges.

```sh
python3 scripts/zc-auth-federation-live-eks.py run \
  --stack /home/rob/cloud-sweeper/aws/test-cluster \
  --run-dir /tmp/zc-auth-live-UNIQUE-RUN \
  --driver /tmp/zc-auth-target/debug/zc-federation-auth-debug \
  --profile tf --region eu-west-1 --operator-cidr YOUR_PUBLIC_IPV4/32
```

The runner installs an IRSA role, a distinct EKS Pod Identity role/association,
and the Pod Identity agent. Each role has only its identity trust policy; neither
needs an application AWS permission policy for `GetCallerIdentity`. Kubernetes
service-account trust is restricted to the test namespace/account and cluster.
Two real pods acquire their own temporary credentials without static secrets,
Vault, Cognito or Private CA. The test rejects an unexpected SDK credential
provider or role ID, preventing node-role fallback from passing the test.

Each pod runs the actual OAuth/approval/regional adapters and Rust debug driver.
The IAM broker uses the real regional STS HTTPS endpoint, with no fixture
transport. The login CLI, successful identity exchange, AWS rejection of a
forged signature, scoped operations, shared volume bucket, supplementary
approval and twelve workload-process handoffs are exercised for each provider.
AWS request IDs, pod UIDs, source hashes, exact image digest and secret-free
scenario evidence are retained in the run directory. Projected tokens, AWS
credentials and issued bearer tokens are not collected.

The `run` action destroys the isolated stack in a `finally` block after testing,
including on a test failure. For interrupted/manual runs, retain the run
directory and explicitly finish teardown with:

```sh
python3 scripts/zc-auth-federation-live-eks.py destroy \
  --run-dir /tmp/zc-auth-live-UNIQUE-RUN
```

Terraform state must be empty after teardown. Also verify the run's cluster,
worker instances, VPC, temporary IAM roles and OIDC provider are absent through
AWS APIs. The run validates real EKS credential acquisition and live STS
federation. East and West remain logical loopback regions inside each pod;
workload handoffs are OS processes, not Kubernetes deployment relocation. It
does not establish physical cross-region storage or production regional HA.

The 2026-10-18 live run in `eu-west-1` passed 106 checks for IRSA and 106 for
EKS Pod Identity, with 24 workload-process handoffs in total. Terraform destroyed
all 28 managed resources across 13 resource types. Forty-four subsequent AWS
checks covered those resources and the automatically created worker, root disk,
instance profile, launch template and network objects; no run-owned resources
remained. Additional checks found no EKS access entries or cluster/container log
groups. Shared service-linked roles predated the run and were preserved. Local
evidence is under `bench-results/federation-auth-live-eks-20261018/`, including
`summary.json`, `cleanup-verification.json` and `service-managed-inventory.json`.

To keep a fresh lab running for manual API experiments, append `--serve` to
the harness command. It prints public endpoint URLs and the private temporary
configuration directory, then waits for Ctrl-C. The private `issuer-a.json`
and `issuer-b.json` files contain the generated client credentials;
`approval.json` holds the distinct reviewer/introspection credentials. No
requests are automatically approved and no E2E success is claimed in this mode.
Manual mode allows five minutes for a reviewer decision; automated tests use
a two-second pending-request deadline to exercise timeout denial quickly.

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

The acceptance matrix maps the requested workflow to executable evidence:

| Requirement | Evidence produced by the scenario |
| --- | --- |
| Two independent OAuth servers accepted in east | Both discovery/JWKS responses, distinct public keys, and successful east requests with both issuers' signed tokens |
| Vault-free IAM identity and scoped failover | SDK-signed temporary credentials, signature-verifying local STS, login CLI, existing Rust verification, and the first East/West/East handoff pair initiated by the mapped IAM principal |
| IAM identity cannot expand scope | Wrong accounts, recreated roles, forged signatures/session tokens, replay, wrong bindings, endpoint/action substitution, sibling/group access and approval bypass denied; the same volume limiter applies |
| IAM exchange fails closed on STS outage | New exchanges fail while an already-issued identity retains only its bounded local token lifetime |
| Scoped west replication and management | Exact ciphertext roundtrips and real root/subtenant metadata changes; foreign CRUD, management, replication and failover all denied without media changes |
| Tenant/subtenant workloads fail over and back | Four clean cuts with three actual worker handoffs each; source exits, fresh target incarnations, continuing counters, unchanged non-counter records and completed actions |
| Subtenant permissions cannot move or stop siblings | Full atomic-scope denials before either media changes or stopping any running sibling worker |
| East privileges require independent time-limited approval | A valid east administrator cannot delete without a separately approved exact grant; requester credentials cannot approve it |
| Approval lifecycle is enforced | Eligibility/role/duration limits, pending timeout, deny, decision replay, binding substitutions, expiry, revocation and approval-service outage cases |
| Identity refresh cannot extend privilege or quota | Reissued user tokens share one limiter and do not extend approval; running workers renew short identity tokens without changing process identity or resource scope |
| Workload completion cannot be fabricated | Public acknowledgement/quiescence fields rejected; a real HTTP 401 at target activation produces exit 77 and leaves actions unacknowledged at the new custody epoch |
| Debug server remains usable and bounded | `--serve` exposes loopback endpoints with an independent reviewer window; PID-owned shutdown removes credentials and workers |

Inspect `summary.json` for `passed=true` and 12 `workload_handoffs`, then inspect
`workload-transitions.json` for source `running=false`/`exit_code=0` and a fresh,
running destination with larger counter and sequence. The failure scenario's
checks require `reconciling_workloads`, retained pending actions and the
authentication-specific worker exit code. CI runs the same harness after a
fresh build; the authorization unit tests additionally cover validly signed
but misbound grants, key/algorithm substitution and empty resource caps.

This is a security/control workflow test, not a regional HA or performance
benchmark. Consensus is one in-process state-machine model; three-replica
HA observations are modeled. The default local harness uses a signature-verifying
STS fixture; the separate opt-in EKS runner exercises live STS with real IRSA
and EKS Pod Identity acquisition. Source quiescence, destination activation and
workload acknowledgements are backed by real, separately authenticated child
processes making HTTP writes. The workloads are not Kubernetes pods or VMs.
The fault case proves an activation error cannot be replaced by a synthetic
acknowledgement. This is not a durable regional consensus runtime or a
production workload recovery supervisor.
Ordinary files are terminal debug media, never mirror/stripe primitives.
Ephemeral encryption keys exist only in the driver process; this is not a
recoverable key-management or encrypted-witness implementation.

All listeners bind only 127.0.0.1. HTTP credentials and routing metadata are
visible to privileged local observers. Debug clients refuse non-loopback
destinations, ignore ambient HTTP proxies and reject redirects. Although record payloads are encrypted,
this HTTP envelope is **not** `public_envelope_v1` and must not be deployed on
an untrusted/cross-region network. It does not change the production transport
contract or assert wire conformance. There is no Kubernetes TokenReview adapter
or live external approval-workflow integration in this fixture.
