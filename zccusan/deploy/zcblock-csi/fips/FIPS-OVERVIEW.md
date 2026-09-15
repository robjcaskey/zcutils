# zccusan FIPS overview

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

## Requirements when using zccusan

For a zccusan deployment, identify which volumes contain protected data and
which zccusan transfer, native transport, RPC, and TLS connections carry that
data or protect access to it. For each operation whose cryptography is relied
on to meet a FIPS requirement, establish that the deployed executable uses the
validated module in approved mode under the permitted operating conditions.
The [crypto integration guide](CRYPTO-INTEGRATION.md) maps zccusan's services
to their cryptographic implementations.

The table identifies the inputs needed to assess zccusan against each
requirement and the resulting evidence. It does not prescribe new official
forms or imply that each assessment issues a certificate. Regulatory references
were reviewed on 2026-09-14; use the applicable version and assessment path.

| Standard or requirement | Requirement as applied to zccusan | Inputs needed | Resulting evidence or decision |
| --- | --- | --- | --- |
| [FIPS 140-3 module use](https://nvlpubs.nist.gov/nistpubs/FIPS/NIST.FIPS.140-3.pdf) | Cryptographic operations relied on for required protection must use a validated module under its permitted conditions. | Exact zccusan image and executable identification; AWS-LC version and certificate 5314; Security Policy; build receipt; actual node/container configuration; cryptographic service map; approved-mode and deployment checks. | A traceable basis for the module-use claim for that release and configuration, with unresolved gaps identified. Embedding AWS-LC does not issue a separate zccusan certificate. See [CMVP FAQ P-17](https://csrc.nist.gov/Projects/cryptographic-module-validation-program/faqs). |
| [CMVP FAQ SG-8 verification guidance](https://csrc.nist.gov/Projects/cryptographic-module-validation-program/faqs) | Request the application/product vendor's signed letter identifying the validated module and certificate and asserting that it supplies all cryptographic services in the solution; compare the assertions with the CMVP entry. | Defined zccusan release and solution boundary; complete cryptographic service inventory; module-use evidence; a vendor able to substantiate and sign the assertions. | Signed vendor letter and verification that its assertions agree with the certificate entry. SG-8 is guidance; a mandatory letter requirement must come from the applicable assessment or policy. |
| [CMMC Level 2 SC.L2-3.13.11 / NIST SP 800-171 Rev. 2](https://dodcio.defense.gov/Portals/0/Documents/CMMC/AssessmentGuideL2v2.pdf) | Use validated cryptography when zccusan cryptography protects CUI confidentiality. | CUI-bearing volumes and transfer paths; identification of encryption at each relevant boundary; module certificates; configuration and system security plan entries; test evidence. | Evidence supporting assessment of 3.13.11 for the zccusan portion of the system. A zccusan acceptance report alone does not establish compliance for the entire system. |
| [FedRAMP Rev5 readiness expectations](https://www.fedramp.gov/resources/documents/3PAO_Readiness_Assessment_Report_Guide.pdf) | For Moderate and higher, establish validated encryption for data at rest and in transit, including the relevant zccusan storage and communication paths. | Assessment boundary and data-flow inventory; zccusan module-use evidence; evidence for the implementations encrypting stored data and backups; inherited service controls and enabled configuration. | Assessment evidence mapping each relevant zccusan data path to its cryptographic protection and identifying coverage gaps. This contributes to the service assessment; it is not a separate authorization of zccusan. |
| [FedRAMP 2026 20x Class C cryptographic-module rules](https://www.fedramp.gov/2026/reference/20x/c/cryptographic-module-use/) | Document modules protecting federal data. The linked rules use SHOULD for active validated modules or their update streams and for their default use where available. Apply the stated adoption and transition dates. | Applicable assessment path and dates; inventory of zccusan services and modules; validation or update-stream status; configuration used for agency tenants; basis for applicable departures. | Module documentation and configuration evidence for the zccusan services within the assessed service. Do not substitute these rules for a different applicable FedRAMP path. |
| [HIPAA Security Rule](https://www.hhs.gov/hipaa/for-professionals/security/laws-regulations/index.html) | Assess appropriate safeguards for electronic protected health information stored or transported through zccusan. HIPAA alone does not impose a blanket FIPS-build requirement. | Risk analysis covering the relevant zccusan volumes and connections; chosen encryption and key controls; any additional policy requiring validated cryptography. | Documented safeguard decisions and implementation evidence. Include FIPS module-use evidence when an additional applicable requirement calls for it. |
| Deployment-specific FIPS policy | Apply the policy's stated scope: validated cryptographic use, a required module security level, or compatibility with FIPS-enabled nodes. | Exact policy text; zccusan workload and configuration; module evidence or compatibility tests appropriate to that requirement. | A finding against the specific policy. Successful installation on a FIPS-enabled node establishes compatibility only, unless module coverage and approved use are also established. |

### Scope of the zccusan evidence

The selected certificate-5314 profile uses Amazon Linux 2023 userspace and
requires the listed node conditions and approved operation. Installing the
image on EKS, GKE, or another Kubernetes service does not by itself establish
those conditions. Check the actual node and container against the
[image and environment guidance](../FIPS.md).

Transport encryption does not establish encryption at rest. For protected
zccusan volumes, identify where encryption of persisted data occurs and which
module performs it. Assess stored copies, replicas, and backups at their
actual encryption boundaries. If protection is provided by an underlying
storage service, retain that service's evidence and the configuration enabling
it; the zccusan AWS-LC certificate is not evidence for a separate implementation.

Likewise, zccusan's evidence covers the application services identified in its
review. Kubernetes API connections, service-mesh proxies, and external key
services need their own evidence where they provide required protection.
A requirement for hardware-protected keys or a higher module security level
must be checked against the actual key handling and module certificate; the
zccusan FIPS image alone does not establish either property.

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

## What the evidence establishes

The evidence collected in the [deployment process](#deployment-process) connects
four facts about the zccusan installation being assessed:

1. **Which code is running.** The image digest and executable hashes identify
   the installed release. The verified build and module receipts connect that
   executable to the AWS-LC module version being claimed. Signatures are checked
   against the signing identities accepted in step 2.
2. **Which validation applies.** The module version and build procedure are
   compared with certificate 5314 and its Security Policy. The node and
   container records from step 3 establish whether the actual environment
   meets the applicable operating conditions.
3. **Which operations use that module.** The service map and integration review
   from step 4 identify the implementation used by each in-scope zccusan
   cryptographic operation. Approved-mode checks and the key-generation,
   rotation, and usage-limit evidence support the claim for those operations.
   Evidence for encryption supplied by other components is assessed separately,
   as described under [scope](#scope-of-the-zccusan-evidence).
4. **What was checked on the deployment.** The acceptance results record which
   checks passed or failed for the identified release and configuration. Review
   findings account for conditions that the automated checks cannot establish.
   The [vendor statement](#vendor-statement), when obtained, records the
   vendor's assertions about module use; those assertions must agree with the
   release, service map, and certificate evidence above.

In step 5, the assessor uses these records to determine whether zccusan meets
the applicable requirement in the [requirements table](#requirements-when-using-zccusan).
The assessment should identify the image digest, deployment profile, covered
services, requirement assessed, date, and any unresolved findings. For example,
a conclusion supported by the records could read:

> For zccusan image [digest] deployed under profile [profile and revision],
> the reviewed cryptographic operations [service-map reference] use the AWS-LC 3
> Cryptographic Module (static), certificate 5314, in approved mode. Evidence
> [record references], assessed on [date], supports satisfaction of [specific
> cryptographic requirement] for those operations in that configuration.

This conclusion applies to the identified installation and services. Step 6
requires reviewing changes before applying it to a different release or
configuration. It relies on Amazon's existing module validation; it does not
issue a CMVP certificate for zccusan. See [CMVP FAQ P-17](https://csrc.nist.gov/Projects/cryptographic-module-validation-program/faqs).

Use the [certificate record](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5314)
and its [Security Policy](https://csrc.nist.gov/CSRC/media/projects/cryptographic-module-validation-program/documents/security-policies/140sp5314.pdf)
for the module identity and permitted conditions, the
[AWS-LC recompilation guide](AWS-LC-RECOMPILATION.md) for build evidence, and the
[crypto integration guide](CRYPTO-INTEGRATION.md) for application service review.
