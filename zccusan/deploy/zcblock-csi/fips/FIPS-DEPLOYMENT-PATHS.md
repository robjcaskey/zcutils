# FIPS deployment: independent use and vendor assistance

> **Future-state assumptions:** TLS call-graph review, dependency reachability review, and
> evidence for encryption-key usage limits remain open, along with the other
> acceptance work listed in the [crypto integration guide](CRYPTO-INTEGRATION.md).
> The rest of this guide assumes that work has been resolved for the release
> being deployed. It does not describe the current build as accepted or validated.

The intended approach is to use the AWS-LC 3 Cryptographic Module (static),
certificate 5314, within its permitted build and operating conditions. zccusan
is free software. An end-user organization can evaluate and operate it itself,
or use a commercial vendor experienced in zccusan to perform technical work.
Both paths use the same underlying module validation.

The open-source project does not attest to the identity or authority behind
release-signing keys, even when signatures are valid and the keys are properly
held. Signature verification establishes a relationship between an artifact
and a key; it does not by itself establish who controls that key or the authority of a statement signed with it. The end-user organization must establish that trust through
its own verification or a source it accepts, such as a commercial vendor.

## What different organizations need

Start with the rule that applies to the data and system, rather than the
organization's industry label. These are common cases, not complete compliance
checklists. Sources reviewed on 2026-09-14; use the applicable rule version and
assessment path when planning a deployment.

| Organization or workload | Typical requirement | What to establish for this deployment |
| --- | --- | --- |
| U.S. federal agency, or a system operated on its behalf, protecting sensitive unclassified information | FIPS applies to cryptographic modules used for that protection. | Identify the modules and certificates, permitted operating conditions, and approved services used by the system. Follow the agency's system assessment and authorization process. See [FIPS 140-3 applicability](https://nvlpubs.nist.gov/nistpubs/FIPS/NIST.FIPS.140-3.pdf). |
| Defense supplier or research organization handling controlled unclassified information (CUI) under applicable NIST SP 800-171 requirements | For example, CMMC Level 2 practice 3.13.11 requires validated cryptography when cryptography protects CUI confidentiality. | Trace CUI storage and transmission to the actual cryptographic implementations. Retain module certificates, configuration evidence, and tests for assessment. The applicable requirements depend on the governing agreement and rule version. See the [Level 2 assessment guide](https://dodcio.defense.gov/Portals/0/Documents/CMMC/AssessmentGuideL2v2.pdf). |
| Cloud service provider pursuing FedRAMP | Cryptographic requirements apply across the service's assessment boundary and depend on the FedRAMP path, class or baseline, and transition rules. | Inventory encryption in application traffic, storage, backups, and platform services; identify inherited controls and configuration responsibilities. Check the [Rev5 readiness guide](https://www.fedramp.gov/resources/documents/3PAO_Readiness_Assessment_Report_Guide.pdf) or the applicable [2026 cryptographic-module rules](https://www.fedramp.gov/2026/reference/20x/c/cryptographic-module-use/). |
| Healthcare organization subject to HIPAA | HIPAA's Security Rule is technology neutral and risk based; HIPAA alone does not impose a blanket requirement that every application use a FIPS-validated module. | Determine whether FIPS is required by an additional policy or obligation. If it is, identify the protected data paths and module evidence just as for other deployments. See the [HHS Security Rule summary](https://www.hhs.gov/hipaa/for-professionals/security/laws-regulations/index.html). |
| Enterprise, university, or nonprofit with an internal FIPS policy | The organization's policy defines which systems and cryptographic uses require validation. | Establish whether the policy requires validated modules, particular security levels, or specific operating environments. A requirement merely to run on FIPS-enabled nodes establishes compatibility, not certificate coverage. |

For FedRAMP in particular, avoid a single timeless rule. The Rev5 readiness
reference expects validated encryption for Moderate and higher data at rest and
in transit. The linked 2026 20x Class C rules require module documentation but
use SHOULD for active validated modules or their update streams, with stated
adoption and transition dates. Apply the rules governing the actual assessment.

A requirement for a higher module security level or hardware-protected keys
needs separate evaluation. It cannot be satisfied by relabeling this software
build. Likewise, the evidence for zccusan does not establish the status of
Kubernetes, a service mesh, disk encryption, backups, or an external key service.

## Path 1: independent use

The end-user organization's engineers review the published build procedure,
module receipt, image digest, signatures, software bills of materials, and
acceptance profile. They compare the actual node and container environment
with the module's Security Policy and the documented deployment conditions.
They also establish which signing identities they trust and the basis for
trusting them; a key supplied alongside an artifact is not sufficient by itself.

They then check which cryptographic services the workload actually uses,
configure keys and rotation, run the acceptance checks on that deployment,
and retain the evidence. Updates to the image, node, dependencies, or operating
configuration require a review of what changed and which checks must be rerun.

The organization's security team or assessor evaluates that evidence against
its requirements. A separate laboratory assessment is not an automatic step
for every installation using an existing validated module. It can be useful
when module reuse or an operating environment needs specialist interpretation,
or when the organization's assessment process requires it.

## Path 2: assistance from a commercial vendor

A vendor's useful contribution is engineering knowledge of zccusan, its
cryptographic integration, and the target platform. Concrete work can include:

- Reproducing the build and tracing the shipped executable to the module,
  source version, build procedure, and certificate being relied on.
- Reviewing TLS construction and other cryptographic call paths to establish
  which implementation handles each security operation, including less common
  error and recovery paths.
- Implementing and testing necessary application fixes, such as enforcing key
  usage limits or removing an unintended alternative cryptographic path.
- Testing a specific node and Kubernetes configuration, investigating failures,
  and determining whether a proposed platform fits the permitted conditions.
- Maintaining a release branch and assessing how dependency updates, security
  fixes, and platform changes affect the evidence for that release.
- Answering source-level questions from the organization's assessor and, where
  needed, working with a cryptographic testing laboratory or the module vendor
  to resolve uncertainty about module use.

For example, if a TLS client unexpectedly selects a different provider, a
vendor familiar with the code can locate the construction site, fix it, and
add a regression test. If a proposed operating environment is outside the
permitted conditions, a passing functional test does not resolve that issue.
The deployment needs an applicable policy basis or a different configuration.

### Vendor statement for validation verification

[NIST CMVP FAQ SG-8](https://csrc.nist.gov/Projects/cryptographic-module-validation-program/faqs)
instructs organizations verifying validation to request a signed letter from
the application or product vendor. For an application incorporating a module,
the letter states that it incorporates a validated module, identifies its
certificate number, and states that the module supplies all cryptographic
services in the solution. The organization checks those assertions against
the CMVP entry, including the version and operating environment.

An end-user organization following that guidance can select a vendor that
builds or verifies its zccusan distribution and can substantiate and sign that
letter for the specified configuration. The vendor must establish the scope
of the solution and account for every cryptographic service before making the
all-services assertion. A statement covering only selected calls is not the
full statement described in SG-8. The letter should identify the exact release
and configuration to which the assertion applies.

SG-8 is verification guidance. It does not establish a universal requirement
to obtain a commercial vendor's letter for every deployment. If an agency,
assessment process, or organizational policy requires that letter, cite that
specific requirement as the reason to obtain it. The letter is supplied by the
application or product vendor; it is not a new certificate from CMVP or an
attestation issued by the testing laboratory.

The same work can be done by an end-user organization with the necessary
expertise. Vendor participation does not change the FIPS requirements or grant
a different validation status.

## What either path establishes

The relevant result is evidence that the particular application release uses
the identified validated module correctly in the deployed configuration.
A passing checker supports that conclusion only for the checks it performs.
The end-user organization's assessment process decides whether the system
meets its requirements.

An independent lab can examine module integration and operating conditions.
Its assessment does not expand Amazon's certificate or create a new zccusan
validation. CMVP validates cryptographic modules; embedding one does not
validate the containing application. See [CMVP FAQ P-17](https://csrc.nist.gov/Projects/cryptographic-module-validation-program/faqs).

When supported by the release and deployment evidence, a statement can read:

> This deployment uses the AWS-LC 3 Cryptographic Module (static), FIPS 140-3
> certificate 5314, in approved mode under the identified deployment profile.
> zccusan is not independently CMVP validated.

The controlling module references are the [certificate record](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5314)
and its [Security Policy](https://csrc.nist.gov/CSRC/media/projects/cryptographic-module-validation-program/documents/security-policies/140sp5314.pdf).
For project-specific conditions, see the [AWS-LC recompilation guide](AWS-LC-RECOMPILATION.md)
and [crypto integration guide](CRYPTO-INTEGRATION.md).
