locals {
  signing_authority = "Rob J. Caskey"
  key_alias         = "alias/zcutils-build-attestation-signing-authority-Rob-J-Caskey"
  parameter_name    = "/zcutils/build-attestation/signing-authority/Rob-J-Caskey/kms-key-arn"
  github_oidc_arn   = "arn:aws:iam::${var.aws_account_id}:oidc-provider/token.actions.githubusercontent.com"
}

# This policy delegates key use to IAM in this account. The separate signer
# policy below grants only the operations needed by the build signer.
data "aws_iam_policy_document" "key" {
  statement {
    sid       = "EnableAccountIAMPolicies"
    effect    = "Allow"
    actions   = ["kms:*"]
    resources = ["*"]
    principals {
      type        = "AWS"
      identifiers = ["arn:aws:iam::${var.aws_account_id}:root"]
    }
  }
}

resource "aws_kms_key" "attestation_signing" {
  description              = "zcutils build attestation signing authority: Rob J. Caskey"
  key_usage                = "SIGN_VERIFY"
  customer_master_key_spec = "ECC_NIST_P256"
  deletion_window_in_days  = 30
  policy                   = data.aws_iam_policy_document.key.json

  tags = {
    Name             = "zcutils-build-attestation-signing-authority-Rob-J-Caskey"
    SigningAuthority = local.signing_authority
    ManagedBy        = "Terraform"
  }

  lifecycle {
    prevent_destroy = true
  }
}

resource "aws_kms_alias" "attestation_signing" {
  name          = local.key_alias
  target_key_id = aws_kms_key.attestation_signing.key_id
}

# The parameter is an ordinary configuration reference. Private key bytes
# never leave KMS and are never stored in Parameter Store.
resource "aws_ssm_parameter" "attestation_signing_key_arn" {
  name        = local.parameter_name
  description = "KMS key ARN for zcutils signing authority Rob J. Caskey"
  type        = "String"
  tier        = "Standard"
  value       = aws_kms_key.attestation_signing.arn

  tags = {
    SigningAuthority = local.signing_authority
    ManagedBy        = "Terraform"
  }

  lifecycle {
    prevent_destroy = true
  }
}

data "aws_iam_policy_document" "signer" {
  statement {
    sid       = "ResolveExactAttestationSigningKey"
    effect    = "Allow"
    actions   = ["ssm:GetParameter"]
    resources = [aws_ssm_parameter.attestation_signing_key_arn.arn]
  }

  statement {
    sid       = "SignWithExactAttestationKey"
    effect    = "Allow"
    actions   = ["kms:DescribeKey", "kms:GetPublicKey", "kms:Sign", "kms:Verify"]
    resources = [aws_kms_key.attestation_signing.arn]
  }
}

resource "aws_iam_policy" "signer" {
  name        = "zcutils-build-attestation-signer-Rob-J-Caskey"
  description = "Sign zcutils build attestations as Rob J. Caskey with the exact non-exportable KMS key"
  policy      = data.aws_iam_policy_document.signer.json

  tags = {
    SigningAuthority = local.signing_authority
    ManagedBy        = "Terraform"
  }
}

resource "aws_iam_role" "github_signer" {
  name                 = "zcutils-build-attestation-signer-Rob-J-Caskey"
  description          = "GitHub OIDC signer for zcutils attestations under Rob J. Caskey's signing authority"
  max_session_duration = 3600
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid       = "GitHubMainBranchOIDC"
      Effect    = "Allow"
      Principal = { Federated = local.github_oidc_arn }
      Action    = "sts:AssumeRoleWithWebIdentity"
      Condition = {
        StringEquals = {
          "token.actions.githubusercontent.com:aud" = "sts.amazonaws.com"
          "token.actions.githubusercontent.com:sub" = var.github_oidc_subjects
        }
      }
    }]
  })

  tags = {
    SigningAuthority = local.signing_authority
    ManagedBy        = "Terraform"
  }
}

resource "aws_iam_role_policy_attachment" "github_signer" {
  role       = aws_iam_role.github_signer.name
  policy_arn = aws_iam_policy.signer.arn
}
