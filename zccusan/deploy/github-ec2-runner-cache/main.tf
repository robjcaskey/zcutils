# This optional module creates one persistent cache volume and attaches one
# narrow IAM policy to an explicitly named existing controller role. It creates
# no EC2 instances, launch templates, ENIs, or IPs.
locals {
  authority_tag = "Rob-J-Caskey"
  ec2_arn       = "arn:aws:ec2:${var.aws_region}:${var.aws_account_id}"
}

resource "aws_ebs_volume" "build_cache" {
  availability_zone    = var.availability_zone
  size                 = 20
  type                 = "gp3"
  iops                 = 3000
  throughput           = 125
  encrypted            = true
  kms_key_id           = var.kms_key_id
  multi_attach_enabled = false

  tags = {
    Name                       = "zcutils-build-cache-Rob-J-Caskey"
    RunnerPool                 = var.runner_pool
    ZcutilsBuildCacheAuthority = local.authority_tag
    ZcutilsSingleWriter        = "true"
    CleanupPolicy              = "terraform-destroy-only"
    ManagedBy                  = "Terraform"
  }

  lifecycle {
    prevent_destroy = true
  }
}

data "aws_iam_policy_document" "attach_cache" {
  statement {
    sid       = "ReadCacheAndRunnerState"
    effect    = "Allow"
    actions   = ["ec2:DescribeInstances", "ec2:DescribeVolumes"]
    resources = ["*"]
    condition {
      test     = "StringEquals"
      variable = "aws:RequestedRegion"
      values   = [var.aws_region]
    }
  }

  statement {
    sid       = "AttachDetachExactTaggedCacheVolume"
    effect    = "Allow"
    actions   = ["ec2:AttachVolume", "ec2:DetachVolume"]
    resources = [aws_ebs_volume.build_cache.arn]
    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/ZcutilsBuildCacheAuthority"
      values   = [local.authority_tag]
    }
    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/ZcutilsSingleWriter"
      values   = ["true"]
    }
  }

  statement {
    sid       = "AttachDetachOnlyPoolRunners"
    effect    = "Allow"
    actions   = ["ec2:AttachVolume", "ec2:DetachVolume"]
    resources = ["${local.ec2_arn}:instance/*"]
    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/RunnerPool"
      values   = [var.runner_pool]
    }
    condition {
      test     = "StringEquals"
      variable = "ec2:ResourceTag/adhocKeepaliveModeAction"
      values   = ["terminate"]
    }
    condition {
      test     = "StringLike"
      variable = "ec2:ResourceTag/ExpiresAt"
      values   = ["????-??-??T??:??:??Z"]
    }
  }
}

resource "aws_iam_policy" "attach_cache" {
  name        = "zcutils-build-cache-Rob-J-Caskey-attach"
  description = "Attach the exact single-writer zcutils cache only to tagged ephemeral runners"
  policy      = data.aws_iam_policy_document.attach_cache.json
  tags = {
    ManagedBy        = "Terraform"
    SigningAuthority = "Rob J. Caskey"
  }
}

data "aws_iam_role" "controller" {
  name = var.controller_role_name
}

resource "aws_iam_role_policy_attachment" "controller_cache" {
  role       = data.aws_iam_role.controller.name
  policy_arn = aws_iam_policy.attach_cache.arn
}
