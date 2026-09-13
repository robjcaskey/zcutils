output "controller_role_arn" {
  description = "Set the GitHub repository variable AWS_FIPS_RUNNER_ROLE_ARN to this non-secret ARN."
  value       = aws_iam_role.controller.arn
}

output "termination_role_arn" {
  value = aws_iam_role.termination.arn
}

output "schedule_group_name" {
  value = aws_scheduler_schedule_group.runners.name
}

output "runner_configuration" {
  description = "Runtime launch contract. A job must create and verify its deadline schedule before starting work."
  value = {
    aws_account_id = var.aws_account_id
    aws_region     = var.aws_region
    # Compatibility hints for the workflow currently on main. The IAM policy
    # does not enforce these lists; reviewed workflows select and verify values.
    allowed_image_ids          = ["ami-070d7e2a04b25f972"]
    allowed_instance_types     = ["m6i.large"]
    allowed_subnet_ids         = local.subnet_ids
    allowed_security_group_ids = local.security_group_ids
    max_volume_gib             = var.max_volume_gib
    runner_pool                = var.name_prefix
    run_name_prefix            = "${var.name_prefix}-"
    required_tag_keys          = ["RunnerPool", "RunId", "ExpiresAt", "adhocKeepaliveModeAction", "adhocKeepalive"]
    required_tag_values = {
      RunnerPool               = var.name_prefix
      adhocKeepaliveModeAction = "terminate"
    }
    expiry_tag_keys             = ["ExpiresAt", "adhocKeepalive"]
    schedule_group_name         = local.group_name
    termination_role_arn        = local.termination_arn
    termination_target_arn      = "arn:aws:scheduler:::aws-sdk:ec2:terminateInstances"
    associate_public_ip_address = true
  }
}

output "vpc_id" {
  value = local.network_vpc_id
}

output "fallback_access_key_id" {
  value = try(aws_iam_access_key.bootstrap[0].id, null)
}

output "fallback_encrypted_secret" {
  description = "Base64 OpenPGP ciphertext; decrypt locally and store as a GitHub secret only if fallback is enabled."
  value       = try(aws_iam_access_key.bootstrap[0].encrypted_secret, null)
  sensitive   = true
}

output "fallback_key_fingerprint" {
  value = try(aws_iam_access_key.bootstrap[0].key_fingerprint, null)
}
