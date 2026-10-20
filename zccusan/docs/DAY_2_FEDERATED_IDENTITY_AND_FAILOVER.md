# Day-two operations: federated identity and failover verification

Use this guide after the [Kubernetes getting-started exercise](GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md)
to rehearse delegated access, tenant isolation, supplementary approval and scoped
failover. The executable laboratory and its real EKS identity test are available
today. Production management listeners still use the existing federation
credentials; the laboratory does not enable OAuth or IAM login on those listeners.

## Choose the verification environment

| Environment | What it verifies | Resources and limits |
| --- | --- | --- |
| Local lab | Two OAuth issuers, IAM proof exchange, tenant/subtenant authorization, encrypted replication, approval and workload-process handoff | Local processes and a signature-verifying STS fixture; no AWS calls or resources |
| Native Kubernetes lab | Real projected ServiceAccount tokens, TokenReview, UID binding and scoped operations | Two disposable local k3s containers; no AWS or Vault |
| SAML/revocation lab | OAuth-backed SAML, two nginx/auth-proxy pairs, live group updates and signed logout | Loopback services and local authorization logs; no cloud resources |
| Disposable EKS lab | Real IRSA and EKS Pod Identity acquisition, live STS verification, and the same scoped operations | One temporary cluster and small Spot worker; East/West remain logical regions inside pods |

The [implementation and run guide](../../docs/federated-identity-breakglass-debug.md)
contains the complete configuration schema, prerequisites and commands. Debug
listeners bind to loopback and do not implement the public cross-region security
envelope. Keep them inside the test environment.

## Review access before rehearsing a failover

Bind each trusted identity to explicit local permissions. OAuth subjects are
qualified by issuer. AWS identities are qualified by account and unique role ID;
register exact role ARNs and role IDs rather than trusting an entire account.
AWS administrator permissions do not automatically confer zcutils privileges.

For each grant, check the region, tenant path, descendant inclusion, resource IDs
and permitted actions. A retail-subtenant operator must not gain access to its
wholesale sibling. Replication, management and failover permissions are separate
from ordinary read/write access. New credentials and refreshed tokens must use
the existing resource budget rather than create a new allowance.

A failover request expands to the complete atomic consistency set. The debug
adapter requires permission for every expanded member at both endpoints before
side effects. Independent subtenant failover requires independently movable
consistency sets. Changing group membership must not be a way to expand an
operator's authority.

The underlying state machine distinguishes a clean cut from explicit acceptance
of bounded data loss. The federated identity lab exercises clean failover;
OAuth-authorized declared-loss operation is not implemented by this adapter.
See the [failover contract](../../docs/global-volume-failover.md) for checkpoint,
fencing, custody and workload-acknowledgement requirements.

## Rehearse approval and AWS identity

Protected operations require an independently approved, time-limited grant from
the supplementary approval backend, in addition to ordinary local permission.
Verify that expiry, revocation, a different principal/resource/region and an
approval-service outage deny the protected action. Identity refresh must not
extend approval or reset quota. This backend uses its own debug protocol.

The Vault-free AWS path is temporary credentials → SDK-signed STS identity proof
→ explicit local mapping → short-lived access token. IRSA and EKS Pod Identity
use the standard SDK credential chain. The broker pins its STS endpoint and
requires a fresh, one-use proof bound to this federation and broker.

`GetCallerIdentity` does not establish Kubernetes namespace/service-account
identity, MFA state or session tags. Service accounts sharing a role share its
mapped permissions; use distinct roles where permissions differ. Existing
zcutils tokens have their own bounded lifetime after AWS session revocation.

## Run the local checks first

From the repository root, follow the laboratory's
[local run instructions](../../docs/federated-identity-breakglass-debug.md#run).
They build the private Rust driver and run the focused IAM security tests and
full HTTP workflow. Inspect `summary.json`, `workload-transitions.json` and
`approval-audit.json` in the selected output directory.

Require successful source process exits, new target write witnesses, continuing
counters and unchanged foreign-tenant data. A target authentication failure must
leave workload actions unacknowledged rather than fabricate completion. These
are correctness checks, not performance measurements.

## Browser sessions and real-time revocation

For the separate browser/session exercise, see the
[SAML and real-time revocation guide](../../docs/saml-realtime-revocation-debug.md).
It covers authenticated logout, changing group membership without replacing a
session cookie, partial regional delivery, durable local enforcement and the
remaining production log integration.

## Native Kubernetes identity without AWS or Vault

OAuth, AWS IAM and native Kubernetes login can coexist. They produce distinct
issuer-qualified principals; matching names never link identities automatically.
An administrator can grant them the same resources and budget IDs. Refresh or
switching login method does not reset those budgets. A grant from the independent
approval backend remains bound to the particular principal that requested it.

The native broker calls the registered Kubernetes API's `TokenReview` over TLS.
It pins the API endpoint, CA, cluster ID and enrolled ServiceAccount UID. It
requires Pod-bound tokens with audience `urn:zcutils:FEDERATION:kubernetes`,
checks the returned audience and Pod binding, and issues an access token lasting
at most 60 seconds, capped by source expiry. Kubernetes RBAC is not imported as
zcutils authorization. Human OIDC users and ServiceAccounts remain different
identity types.

Give the reviewer only this ClusterRole rule, bound to its ServiceAccount:

```yaml
apiGroups: [authentication.k8s.io]
resources: [tokenreviews]
verbs: [create]
```

Mount the workload identity separately from the reviewer's API credential:

```yaml
automountServiceAccountToken: false
volumes:
  - name: zc-identity
    projected:
      sources:
        - serviceAccountToken:
            audience: urn:zcutils:auth-lab:kubernetes
            expirationSeconds: 600
            path: token
```

Mount that volume read-only in the workload. `KubernetesTokenClient` reopens the
projected path on every refresh, so kubelet's symlink replacement is observed.
The broker also reopens its reviewer's credential each request. Register a
ServiceAccount's current UID, not just namespace/name; deletion and recreation
requires explicit reenrollment. Account for the API's bound-object invalidation
timing and the already-issued zcutils token's bounded remaining lifetime.
Kubernetes documents a 60-second allowance for objects pending deletion;
the local forced-deletion probe observed TokenReview rejection after roughly
10 seconds. The test polls within a 70-second observation window rather than
assuming instant rejection. This observation is not a production latency SLA.
See [bound-token validation](https://kubernetes.io/docs/reference/access-authn-authz/service-accounts-admin/#bound-service-account-tokens).

The debug `kubernetes` mode config contains `federation`, output `audience`,
`token_ttl_seconds` (1–60), and `clusters`. Each cluster has `id`, HTTPS `server`,
`ca_file`, `reviewer_token_file` and `service_accounts` entries with `namespace`,
`name`, `uid`. `scripts/zc_kubernetes_login.py` exchanges a projected token file
through the local broker. Production listener hosting and automatic Kubernetes
Deployment migration are separate work; this enables the executable debug path.

After building the private Rust driver and installing the debug requirements:

```bash
# Requires an available sudo -n docker and the pinned k3s image cached locally.
python scripts/zc-auth-kubernetes-e2e.py \
  --driver /tmp/zc-auth-target/debug/zc-federation-auth-debug \
  --evidence /tmp/zc-native-kubernetes-evidence
```

The image is pinned in the script. Pre-pull it deliberately if needed; the runner
does not silently pull it. Two privileged local k3s containers are constrained to
two CPUs/two GiB each, with loopback API ports and private `/tmp` bind data.
The default kubeconfig and existing clusters are untouched. This is a local
development test, not an unprivileged or production Kubernetes deployment.

Tests use real API TokenReview, Pod projections, actual kubelet token rotation,
wrong-cluster/audience/UID failures, insufficient reviewer RBAC, reviewer rotation,
TLS rejection, API outage and deleted/recreated subjects. The suite runs the
existing Rust policy/HTB/failover code with OAuth, local IAM and native identities
together. It waits several minutes for a real 600-second projected token to
rotate; it does not simulate rotation by rewriting claims. Teardown removes
only the recorded container IDs, verifies container and anonymous-volume absence,
and removes their bind data.
Temporary credential/config files are private under `/tmp`; evidence is secret-free.
For a development follow-up after separately observing rotation,
`--skip-rotation-wait` omits that long wait and records
`real_projected_token_rotation: false`. It is not a full rotation test; CI uses
the default full-duration path. The outage check stops an owned API container,
because pausing privileged k3s does not reliably interrupt its API.

## Optional real EKS run and teardown

Use the [real EKS instructions](../../docs/federated-identity-breakglass-debug.md#real-eks-identity-verification)
and `scripts/zc-auth-federation-live-eks.py run`. The runner needs the existing
disposable Terraform stack supplied through `--stack` and a fresh private run
directory. It copies sources and the provider lock file, uses isolated state
and a dedicated VPC, and destroys the stack in a `finally` block after testing.

This opt-in run incurs temporary AWS charges. It uses one small Spot worker and
a standard-support EKS version. It rejects plans containing expensive performance
nodes, NAT gateways, Cognito or Private CA. Normal CI runs only the local lab.

Retain the run directory until cleanup is verified. For an interrupted run, use
the documented `destroy --run-dir ...` command. Require empty Terraform state
and the AWS cleanup audit: all managed resource types plus automatically created
instances, disks, instance profiles, launch templates and network resources.
Inspect the unique run-tag scan for dangling resources. Preserve shared,
preexisting AWS service-linked roles.

The 2026-10-18 EKS run passed 106 checks per credential provider and 24 workload
handoffs in total. All 28 Terraform resources across 13 types were destroyed,
and 44 subsequent AWS checks passed. Local evidence is under
`bench-results/federation-auth-live-eks-20261018/`. This validates AWS identity in
real pods; physical cross-region HA and Kubernetes deployment relocation were
not tested.
