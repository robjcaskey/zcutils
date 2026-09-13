# Optional build-attestation signing key

Deployed in AWS account `968134102381`, region `us-east-1`, on 2026-09-13:

* key `arn:aws:kms:us-east-1:968134102381:key/7d835db3-7134-4c20-a471-b4220951730d`
* alias `alias/zcutils-build-attestation-signing-authority-Rob-J-Caskey`
* SSM reference `/zcutils/build-attestation/signing-authority/Rob-J-Caskey/kms-key-arn`
* signer policy `arn:aws:iam::968134102381:policy/zcutils-build-attestation-signer-Rob-J-Caskey`
* GitHub signer role `arn:aws:iam::968134102381:role/zcutils-build-attestation-signer-Rob-J-Caskey`

The signer policy is attached only to the dedicated GitHub OIDC role. Its trust
policy accepts the exact `repo:robjcaskey/zcutils:ref:refs/heads/main` subject;
it is separate from the EC2 runner controller.

This isolated Terraform module defines one asymmetric AWS KMS `SIGN_VERIFY`
key whose signing authority is exactly `Rob J. Caskey`, its stable alias
`alias/zcutils-build-attestation-signing-authority-Rob-J-Caskey`, and the
standard SSM `String`
`/zcutils/build-attestation/signing-authority/Rob-J-Caskey/kms-key-arn` whose
value is only the key ARN. KMS performs signatures internally: this module
does not create, export, or escrow private key material.

The module also creates an IAM signer policy restricted to
`ssm:GetParameter` on that exact reference and `kms:DescribeKey`,
`kms:GetPublicKey`, `kms:Sign`, and `kms:Verify` on that exact key. The verify
grant supports cosign verification when the independently trusted identity is
an `awskms://` URI. Attach the
The policy is attached only to the dedicated `github_signer_role_arn`. The
`signer_policy_json` output supports independent policy review.

Do not apply the module merely to test it. `prevent_destroy` protects both the
key and reference. A customer-managed KMS key incurs a recurring monthly
charge plus signing requests; standard SSM parameters have no additional
storage charge at standard throughput. Review current AWS pricing before
changing this deployment.

Review a plan without saving it to the repository:

```sh
terraform init
terraform plan -var aws_account_id=968134102381
```

After an approved deployment and role attachment, the build wrapper resolves
the key ARN without printing it:

```sh
scripts/zc-image-attest.py generate --skip-build \
  --variant nonfips --image localhost/zcblock-csi:attested \
  --aws-profile slopmud-cicd --kms-key-parameter
```
