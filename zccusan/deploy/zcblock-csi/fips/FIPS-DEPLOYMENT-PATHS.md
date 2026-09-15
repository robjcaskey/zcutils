# FIPS deployment requirements and verification

> **Future-state assumptions:** TLS call-graph review, dependency reachability review, and
> evidence for encryption-key usage limits remain open, along with the other
> acceptance work listed in the [crypto integration guide](CRYPTO-INTEGRATION.md).
> The rest of this guide assumes that work has been resolved for the release
> being deployed. It does not describe the current build as accepted or validated.

zccusan can use validated cryptography through the AWS-LC 3 Cryptographic
Module (static), certificate 5314, within its permitted build and operating
conditions. Deployment verification connects the application release and its
cryptographic services to that module, its certificate, and the environment
in which it runs.

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

## Deployment process

1. **Identify the requirements.** Establish which data and cryptographic
   operations are in scope, which rules apply, and what evidence the
   organization's assessment process requires.
2. **Verify the release.** Review the build procedure, module receipt, image
   digest, signatures, software bills of materials, and acceptance profile.
   Establish which signing identities are trusted and the basis for trusting
   them; a key supplied alongside an artifact is not sufficient by itself.
3. **Check the operating conditions.** Compare the actual node and container
   environment with the module's Security Policy and the documented deployment
   conditions. A configuration outside those conditions needs an applicable
   policy basis or a different environment; functional tests alone do not
   establish certificate coverage.
4. **Verify cryptographic use.** Identify the implementation handling each
   security operation used by the workload. Configure keys, rotation, and
   usage limits, run the deployment acceptance checks, and resolve failures.
   Retain the evidence for the exact release and configuration. Obtain the
   vendor statement described below when following SG-8 or when required by
   the organization's assessment process.
5. **Assess the deployment.** The organization's security team or assessor
   evaluates the evidence against its requirements. A separate laboratory
   assessment can address uncertainty about module reuse or operating
   conditions, or satisfy an explicit assessment requirement; it is not an
   automatic step for every installation using an existing validated module.
6. **Review changes.** Assess updates to the image, node, dependencies, and
   operating configuration before deployment. Determine which evidence and
   checks need updating, including certificate status and cryptographic use.

## Vendor statement

[NIST CMVP FAQ SG-8](https://csrc.nist.gov/Projects/cryptographic-module-validation-program/faqs)
instructs organizations verifying validation to request a signed letter from
the application or product vendor. For an application incorporating a module,
the letter states that it incorporates a validated module, identifies its
certificate number, and states that the module supplies all cryptographic
services in the solution. The organization checks these assertions against
the CMVP entry, including the version and operating environment.

The open-source project does not issue that letter or attest to the identity
or authority of release-signing key holders. An end-user organization following
SG-8 can obtain the letter from a vendor that builds or verifies its zccusan
distribution and can substantiate the assertions for the exact release and
configuration. The vendor must account for every cryptographic service in the
specified solution before making the all-services assertion.

SG-8 is verification guidance, not a universal requirement to obtain a
commercial vendor's letter for every deployment. Where an agency or
organizational policy requires the letter, identify that requirement. The
letter supports verification of an existing module validation; it does not
issue a new certificate or authorize the deployed system.

## What the evidence establishes

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
