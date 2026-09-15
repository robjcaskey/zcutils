# zccusan FIPS overview

> **Future-state assumptions:** TLS call-graph review, dependency reachability review, and
> verification of encryption-key usage limits remain open, along with the other
> acceptance work listed in the [crypto integration guide](CRYPTO-INTEGRATION.md).
> The rest of this guide assumes that work has been resolved for the release
> being deployed. It does not describe the current build as accepted or validated.

zccusan can use validated cryptography through the AWS-LC 3 Cryptographic
Module (static), certificate 5314, within its permitted build and operating
conditions. The goal is to establish whether a particular zccusan release,
configuration, and set of cryptographic operations satisfy the criteria for
its use. Build records, configuration records, tests, and technical review
support that decision. The following process explains what each record proves
and how to obtain it.

For how releases are compiled and checked before publication, see
[Offline FIPS compilation](OFFLINE-BUILD.md).

## Deployment goals and how to establish them

Each applicable goal below needs a supported conclusion before it can be used
in the deployment assessment. The last column identifies standards or policies
for which organizations typically use that conclusion. Regulatory references
were reviewed on 2026-09-14; use the applicable version and assessment path.

| Goal for the zccusan deployment | Supporting records needed | How to obtain and verify them | Typical assessment use |
| --- | --- | --- | --- |
| Establish that the installed executable incorporates the module identified by certificate 5314. | Image digest, executable hashes, build and module receipts, AWS-LC version, certificate, and Security Policy. | Retrieve the release artifacts and receipts; verify signatures against independently trusted identities; compare installed digests and hashes with those records; review the module build and linkage against the permitted procedure. | Establishing validated module use under [FIPS 140-3](https://nvlpubs.nist.gov/nistpubs/FIPS/NIST.FIPS.140-3.pdf). |
| Establish that the actual node and container permit the claimed module use. | Recorded node, operating system, architecture, container userspace, acceptance profile, and approved-mode test results. | Inspect the running node and container, compare the recorded values with the Security Policy and selected profile, and run the acceptance checks on that installation. Resolve unsupported conditions before claiming coverage. | Checking operating-environment coverage under [CMVP verification guidance](https://csrc.nist.gov/Projects/cryptographic-module-validation-program/faqs). Successful installation alone establishes compatibility only. |
| Establish that zccusan encryption relied on for CUI confidentiality uses validated cryptography in approved mode. | Inventory of CUI-bearing volumes and connections; cryptographic service map; reviewed call paths; key-generation, rotation, and usage-limit records; service test results. | Trace each protected operation to its implementation, review the relevant call paths, inspect key configuration, and run service checks. Establish usage bounds across processes and restarts for the workload being accepted. | Assessing the zccusan portion of a system against [NIST SP 800-171 Rev. 2 / CMMC Level 2 SC.L2-3.13.11](https://dodcio.defense.gov/Portals/0/Documents/CMMC/AssessmentGuideL2v2.pdf). |
| Establish validated protection for covered data at rest and in transit, including protection outside zccusan. | Mapping of volumes, replicas, backups, and connections to their encryption implementations; module certificates; enabled encryption settings; relevant inherited service controls. | Trace storage and network paths, inspect where encryption occurs, retrieve the responsible component's module documentation, and verify the settings that activate the protection. Record any uncovered path. | Assessing zccusan within [FedRAMP Rev5 Moderate or higher](https://www.fedramp.gov/resources/documents/3PAO_Readiness_Assessment_Report_Guide.pdf). |
| Establish which cryptographic modules protect data in each deployed zccusan service. | Service/module inventory, validation or update-stream status, tenant configuration, and applicable assessment dates. | Inspect deployed services and their dependencies, consult module listings and supplier update documentation, and record the effective tenant settings and any departures from the applicable rules. | Following the [FedRAMP 2026 20x Class C rules](https://www.fedramp.gov/2026/reference/20x/c/cryptographic-module-use/), which mandate documentation and use SHOULD for active validated modules or their update streams, subject to transition dates. |
| Obtain the vendor's module-use assertions for the exact solution being verified. | Signed letter identifying the module and certificate and asserting that it supplies all cryptographic services in the specified solution. | Obtain the letter from the application/product vendor after it reviews the release, complete service inventory, and module use; compare its assertions with the CMVP entry. See [Vendor statement](#vendor-statement). | Following [CMVP FAQ SG-8](https://csrc.nist.gov/Projects/cryptographic-module-validation-program/faqs) or an assessment policy calling for that letter. |
| Decide whether the FIPS build and selected controls are appropriate for the workload. | Applicable policy or risk analysis, protected data paths, selected controls, and the verification results for any claimed validated protection. | Review the governing policy and workload risks; select controls; inspect and test their implementation; record why they meet the stated criteria. | Applying an internal cryptographic policy or evaluating safeguards under the [HIPAA Security Rule](https://www.hhs.gov/hipaa/for-professionals/security/laws-regulations/index.html). HIPAA alone does not mandate a FIPS build of zccusan. |

### Establishing the scope of protection

To claim coverage for the selected certificate-5314 profile, the installation
needs Amazon Linux 2023 userspace and the listed node conditions. Record the
actual node and container properties and compare them with the
[image and environment guidance](../FIPS.md). An EKS or GKE service name does
not identify those properties.

To establish encryption at rest for a zccusan volume, identify where its
persisted data is encrypted and which module performs that operation. Repeat
that check for replicas and backups. Obtain the responsible storage component's
module documentation and inspect its encryption settings. This connects the
stored data to its actual protection; transport-encryption tests cannot
establish that connection.

To establish protection for Kubernetes API connections, service-mesh traffic,
or external key operations, identify the component performing each operation,
obtain its module documentation, and verify its active configuration. For
hardware-protected keys or a particular module security level, also compare
the actual key handling and certificate with the policy being assessed.
zccusan's embedded AWS-LC module cannot establish these properties for other
components.

## Deployment process

1. **Define the decision.** Record the exact zccusan workload, protected data,
   applicable criteria, and the person or assessment process that decides
   whether the deployment satisfies them. Use this scope to select the goals
   in the [table above](#deployment-goals-and-how-to-establish-them).
2. **Identify the installed release.** Obtain and verify the release and module
   records described in the first row. Retain the installed image digest and
   executable hashes so subsequent checks apply to the same code.
3. **Establish permitted operation.** Capture the node and container properties,
   compare them with the selected profile and Security Policy, and run the
   installation's acceptance checks. Record differences and resolve those that
   prevent use under the profile.
4. **Establish coverage of the workload.** Use the service map, call-path review,
   key procedures, and tests to account for each covered cryptographic operation.
   Inspect separate storage and platform components as described under
   [scope](#establishing-the-scope-of-protection). Obtain the vendor letter when
   following SG-8 or an assessment policy calling for it. Each conclusion must
   cite the configuration, test, or review record that supports it.
5. **Decide whether the deployment meets the selected criteria.** The security
   team or assessor compares the conclusions from steps 2–4 with the criteria
   recorded in step 1. Resolve missing support or failed checks before recording
   a positive finding for the affected criterion. Where specialist interpretation
   is needed, have a laboratory review the specific module-use or environment
   question and retain its reasoning with that finding.
6. **Preserve the basis for the decision.** Before changing the image, node,
   dependencies, or configuration, compare the proposed change with the accepted
   installation. Recheck certificate status and repeat the affected inspections,
   tests, and reviews so the decision remains tied to the code and configuration
   actually in use.

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

## Recording the deployment decision

The end result of step 5 is a finding about whether the identified zccusan
installation satisfies each criterion selected in step 1. To make that finding
reviewable, record the image digest, deployment profile and revision, covered
services, criterion, assessment date, and unresolved issues. Link each finding
to the build, configuration, service-review, and test records gathered in steps
2–4; a missing record leaves the corresponding conclusion unsupported.

For example, after verifying the relevant records, the assessor could write:

> zccusan image [digest], deployed under [profile and revision], satisfies
> [specific cryptographic criterion] for [covered operations]. The build review
> [reference] identifies the embedded AWS-LC module covered by certificate 5314;
> the environment review [reference] establishes permitted operation; and the
> service review and deployment checks [references] establish approved use for
> those operations. This finding was made on [date].

This finding applies to the identified installation and services. Step 6
explains how to reassess it when they change. It relies on Amazon's existing
module validation and does not issue a CMVP certificate for zccusan. See
[CMVP FAQ P-17](https://csrc.nist.gov/Projects/cryptographic-module-validation-program/faqs).

To verify module identity and permitted conditions, consult the
[certificate record](https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5314)
and its [Security Policy](https://csrc.nist.gov/CSRC/media/projects/cryptographic-module-validation-program/documents/security-policies/140sp5314.pdf).
To verify how the embedded module was built, follow the receipt and procedure
checks in the [AWS-LC recompilation guide](AWS-LC-RECOMPILATION.md).
To verify which implementation handles each application operation, use the
service mapping and review instructions in the
[crypto integration guide](CRYPTO-INTEGRATION.md).
