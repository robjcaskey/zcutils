# Two paths to a FIPS deployment determination

zccusan is distributed with the material needed to evaluate a deployment that
uses the AWS-LC 3 Cryptographic Module (static), FIPS 140-3 certificate 5314.
The end-user organization may complete that work with its own security team and
assessor, or select an integration vendor with experience in zccusan to guide
the work.

Both paths start with the same signed image, software bills of materials,
provider and build receipts, acceptance profile, conformance checker, and
operating guidance. Both end with the end-user organization's assessor or
authorizing official deciding whether the deployed system meets the
organization's FIPS requirements. Neither path turns zccusan itself into a
CMVP-validated product, and neither expands the scope of Amazon's certificate.

## Path 1: end-user organization-directed evaluation

This path fits an end-user organization that already has FIPS engineering,
platform security, and assessment resources.

1. **Select a deployment profile.** The end-user organization chooses a listed
   operating system, architecture, hardware profile, Kubernetes configuration,
   and exact zccusan image digest. A configuration outside the published matrix
   requires a separately documented basis.
2. **Verify the release.** The end-user organization verifies the image
   signature, SPDX and CycloneDX attestations, file hashes, provider receipt,
   section 11.1 build receipt, and certificate status. Verification is performed
   against immutable digests rather than mutable tags.
3. **Map the cryptographic use.** The end-user organization reviews the service
   map, key and entropy flows, approved algorithms and parameters,
   service-indicator evidence, non-approved diagnostic operations, and excluded
   features against its own workload.
4. **Obtain the required determination.** The end-user organization gives the
   evaluation kit to its accredited FIPS laboratory, compliance assessor, or
   authorizing official. The reviewer determines whether the application
   remains outside the validated module boundary and whether certificate 5314
   may be relied on for the selected profile. A lab report is an applicability
   assessment; it is not a new CMVP certificate.
5. **Deploy and collect evidence.** The end-user organization enables the
   required host FIPS mode, enforces the selected image and configuration, runs
   the acceptance checker, and retains the resulting deployment conformance
   record with its system authorization evidence.
6. **Operate within the profile.** The end-user organization monitors
   certificate status, image and node drift, approved-mode failures, key
   lifecycle, security advisories, and profile expiration. An upgrade is
   evaluated before replacing a covered digest or platform component.

The end-user organization owns lab and assessor coordination, interpretation of
its regulatory requirements, environment preparation, remediation, deployment
evidence, and continuing authorization. The published kit is designed to avoid
routine vendor participation, but a reviewer may still identify a question or
gap that requires source-level investigation.

## Path 2: guided evaluation and deployment

This path fits an end-user organization that wants a shorter assessment cycle,
has a custom platform, or prefers a single technical coordinator. The
organization selects an integration vendor with demonstrated experience in
zccusan, FIPS 140 module reuse, container platforms, and the organization's
target infrastructure.

The end-user organization may purchase help at any of these checkpoints:

1. **Readiness and scope.** The integration vendor inventories the end-user
   organization's FIPS requirements, data paths, cryptographic services,
   deployment targets, sidecars, key systems, and authorization boundary. It
   recommends an existing profile or identifies the evidence needed for a new
   one.
2. **Lab-ready package.** The integration vendor verifies the release evidence,
   maps certificate 5314 and its Security Policy to the proposed deployment,
   prepares the module-boundary and approved-services rationale, and organizes
   the questions for the organization's chosen laboratory or assessor.
3. **Environment preparation.** The integration vendor helps configure the
   supported FIPS-mode nodes, admission controls, immutable image references,
   kernel artifacts, key and entropy procedures, logging, and failure policy.
   Credentials and authorization decisions remain under the end-user
   organization's control.
4. **Conformance rehearsal.** Before formal review, the integration vendor runs
   the acceptance suite, traces failures to their evidence source, remediates
   integration gaps, and produces a reviewable deployment conformance record.
   Failed or missing gates remain visible; they are not converted into passes by
   an advisory opinion.
5. **Reviewer coordination.** The integration vendor answers technical
   questions, demonstrates reproduction and runtime checks, maintains the
   evidence index, and supports resolution of findings. The laboratory,
   assessor, or authorizing official retains independent judgment.
6. **Handoff and continuing conformance.** The integration vendor delivers the
   final profile, runbooks, evidence bundle, exceptions, upgrade rules, and
   monitoring plan. Optional continuing services can evaluate new releases,
   add supported profiles, investigate drift, and prepare renewal evidence.

The guided path adds preparation, integration, remediation, and reviewer
coordination. It does not sell or issue a FIPS certificate. If a reviewer finds
that the proposed use falls outside certificate 5314, the integration vendor
can help the end-user organization choose among changing the deployment,
seeking participation from the module vendor, or sponsoring a new module
validation.

## Outcome-backed guided service

A guided engagement can be contracted around completed outcomes instead of
advice hours. The strongest defensible offer combines three milestones:

1. **Written determination.** The integration vendor engages or coordinates a
   named qualified reviewer, supplies the complete evidence package, answers
   requests for information within an agreed response time, and remains engaged
   until the reviewer issues a written applicability disposition for the agreed
   profiles. The disposition may approve reliance on certificate 5314, require
   changes, or determine that a new validation is necessary; the integration
   vendor cannot predetermine the reviewer's independent conclusion. Any
   laboratory deliverable must state its accredited scope and clearly identify
   opinions or interpretations outside that scope.
2. **Conforming deployment.** For a profile that the reviewer accepts, the
   integration vendor delivers the agreed environment with the exact signed
   image digest, required policy controls, and an acceptance report containing
   no `FAIL` or `BLOCKED` gates. The evidence bundle must pass its hash,
   signature, provenance, and profile checks and be ready for the end-user
   organization's authorization record.
3. **Supported handoff.** The integration vendor delivers operating and upgrade
   runbooks, open findings and exclusions, evidence-retention instructions, and
   a dated support window. It stays available through the first assessor review
   and closes in-scope technical findings or documents the third-party decision
   that prevents closure.

The engagement should also define an early suitability gate. Before substantial
implementation work begins, the integration vendor must state whether the
requested profile appears eligible to rely on certificate 5314, identify every
known gap, and give the end-user organization a written go, change, or stop
recommendation. If the profile is not eligible, the result is a documented
decision and migration plan rather than an open-ended consulting effort.

A complete result includes reproducible configuration as well as reports. The
handoff should contain pinned image digests, configuration or infrastructure as
code, admission and runtime policy, acceptance output, an evidence index, and a
clean-room reproduction procedure. A second operator should be able to verify
the package without access to the original build host or the integration
vendor's private systems.

The statement of work can make these commitments enforceable through:

- fixed milestone pricing with a final-payment holdback until the deliverables
  meet their acceptance criteria;
- included remediation of product or integration defects found within the
  agreed profile;
- response-time objectives for laboratory and assessor questions;
- a dated implementation schedule, dependency register, and escalation path for
  delays within the integration vendor's control;
- re-performance, service credits, or a limited refund when a
  vendor-controlled acceptance criterion is missed;
- a stabilization period in which drift alarms, restart behavior, evidence
  collection, and upgrade blocking are exercised in the deployed environment;
- a release-continuity period covering certificate-status monitoring,
  vulnerability disposition, replacement evidence, and migration guidance; and
- an optional profile-extension price and schedule when the end-user
  organization needs additional operating environments.

This creates a result-oriented service without making an impossible promise.
CMVP alone issues a new module validation, the qualified reviewer controls its
applicability opinion, and the authorizing official controls system acceptance.
No integration vendor can guarantee those independent decisions. It can
guarantee the completeness and integrity of its evidence, successful execution
of the agreed conformance gates, timely remediation of defects it controls, and
continued support through a written disposition and deployable result.

## Responsibility and deliverables

| Activity | End-user organization-directed path | Guided path |
| --- | --- | --- |
| Choose regulatory and authorization requirements | End-user organization | End-user organization, advised by integration vendor |
| Select or extend a deployment profile | End-user organization | Integration vendor prepares; end-user organization approves |
| Verify image, SBOMs, receipts, and signatures | End-user organization | Integration vendor performs and demonstrates |
| Prepare module-reuse rationale | End-user organization or its reviewer | Integration vendor prepares for reviewer |
| Configure and operate the deployment | End-user organization | End-user organization, with implementation assistance |
| Run acceptance and remediate failures | End-user organization | Integration vendor assists and documents |
| Decide certificate applicability | End-user organization's qualified reviewer | End-user organization's qualified reviewer |
| Authorize the deployed system | End-user organization's authorizing official | End-user organization's authorizing official |
| Monitor drift, advisories, and certificate status | End-user organization | End-user organization or continuing integration service |

The normal outputs are a module-reuse applicability report from the qualified
reviewer and a deployment conformance record from the end-user organization's
environment. These documents support the organization's system authorization.
They do not create a new certificate. An end-user organization seeking its own
CMVP certificate must act as, or contract with, the validation sponsor; define
the cryptographic module boundary; engage an accredited laboratory; complete
the CMVP submission; and own ongoing certificate maintenance.

## Claim language

For a deployment accepted under the module-reuse path, use language similar to:

> This deployment uses the AWS-LC 3 Cryptographic Module (static), FIPS 140-3
> certificate 5314, in approved mode under the identified deployment profile.
> zccusan is not independently CMVP validated.

Do not describe a passing checker, lab applicability report, FIPS-enabled host,
algorithm certificate, or system authorization as a new CMVP validation of
zccusan.

The controlling references are the live
[certificate 5314 record](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5314),
its [Security Policy](https://csrc.nist.gov/CSRC/media/projects/cryptographic-module-validation-program/documents/security-policies/140sp5314.pdf),
the [CMVP Management Manual](https://csrc.nist.gov/projects/cryptographic-module-validation-program/cmvp-fips-140-3-management-manual),
the [CMVP frequently asked questions](https://csrc.nist.gov/projects/cryptographic-module-validation-program/faqs),
the [NVLAP cryptographic testing handbook](https://doi.org/10.6028/NIST.HB.150-17-2021),
the [AWS-LC recompilation guide](AWS-LC-RECOMPILATION.md), and the
[crypto integration guide](CRYPTO-INTEGRATION.md).
