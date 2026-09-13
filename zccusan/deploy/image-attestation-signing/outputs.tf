output "kms_key_arn" {
  value       = aws_kms_key.attestation_signing.arn
  description = "Non-exportable asymmetric SIGN_VERIFY key ARN."
}

output "kms_alias" {
  value       = aws_kms_alias.attestation_signing.name
  description = "Stable cosign KMS identity naming Rob J. Caskey."
}

output "ssm_parameter_name" {
  value       = aws_ssm_parameter.attestation_signing_key_arn.name
  description = "Standard String containing only the KMS key ARN."
}

output "signer_policy_arn" {
  value       = aws_iam_policy.signer.arn
  description = "Least-privilege policy to attach to an approved signing role."
}

output "signer_policy_json" {
  value       = data.aws_iam_policy_document.signer.json
  description = "Reviewable least-privilege signer policy document."
}

output "github_signer_role_arn" {
  value       = aws_iam_role.github_signer.arn
  description = "Dedicated GitHub OIDC role permitted to use only the exact signing key/reference."
}
