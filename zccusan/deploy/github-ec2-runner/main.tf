# No workers or per-job schedules: jobs create those dynamically.
locals {
  iam_arn            = "arn:aws:iam::${var.aws_account_id}"
  ec2_arn            = "arn:aws:ec2:${var.aws_region}:${var.aws_account_id}"
  scheduler_arn      = "arn:aws:scheduler:${var.aws_region}:${var.aws_account_id}"
  group_name         = "${var.name_prefix}s"
  group_arn          = "${local.scheduler_arn}:schedule-group/${local.group_name}"
  controller_arn     = "${local.iam_arn}:role/${var.name_prefix}-controller"
  termination_arn    = "${local.iam_arn}:role/${var.name_prefix}-termination"
  oidc_arn           = coalesce(var.existing_github_oidc_provider_arn, "${local.iam_arn}:oidc-provider/token.actions.githubusercontent.com")
  subnet_ids         = var.create_public_network ? [aws_subnet.runners[0].id] : sort(tolist(var.allowed_subnet_ids))
  security_group_ids = var.create_public_network ? [aws_security_group.runners[0].id] : sort(tolist(var.allowed_security_group_ids))
  subnet_arns        = [for id in local.subnet_ids : "${local.ec2_arn}:subnet/${id}"]
  group_arns         = [for id in local.security_group_ids : "${local.ec2_arn}:security-group/${id}"]
  pool_condition     = { "ec2:ResourceTag/RunnerPool" = var.name_prefix }
  create_tag_conditions = {
    StringEquals = {
      "aws:RequestTag/RunnerPool"               = var.name_prefix
      "aws:RequestTag/adhocKeepaliveModeAction" = "terminate"
      # Preserve this as an IAM policy variable, evaluated against the request.
      "aws:RequestTag/adhocKeepalive" = "$${aws:RequestTag/ExpiresAt}"
    }
    StringLike = {
      "aws:RequestTag/RunId"     = "${var.name_prefix}-*"
      "aws:RequestTag/ExpiresAt" = "????-??-??T??:??:??Z"
    }
  }
}

resource "aws_iam_openid_connect_provider" "github" {
  count          = var.existing_github_oidc_provider_arn == null ? 1 : 0
  url            = "https://token.actions.githubusercontent.com"
  client_id_list = ["sts.amazonaws.com"]
  # AWS uses its trusted CA list for GitHub; do not pin a rotating TLS thumbprint.
}

resource "aws_scheduler_schedule_group" "runners" {
  name = local.group_name
}

resource "aws_iam_role" "controller" {
  name                 = "${var.name_prefix}-controller"
  description          = "GitHub controller for tagged ephemeral EC2 workers and termination schedules"
  max_session_duration = 3600
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = concat([
      {
        Sid       = "GitHubOIDC"
        Effect    = "Allow"
        Principal = { Federated = local.oidc_arn }
        Action    = "sts:AssumeRoleWithWebIdentity"
        Condition = {
          StringEquals = {
            "token.actions.githubusercontent.com:aud" = "sts.amazonaws.com"
            "token.actions.githubusercontent.com:sub" = sort(tolist(var.github_oidc_subjects))
          }
        }
      }
      ], var.enable_key_fallback ? [
      {
        Sid       = "OptionalKeyFallback"
        Effect    = "Allow"
        Principal = { AWS = "${local.iam_arn}:user/${var.name_prefix}-bootstrap" }
        Action    = "sts:AssumeRole"
      }
    ] : [])
  })
  depends_on = [aws_iam_openid_connect_provider.github, aws_iam_user.bootstrap]
}

resource "aws_iam_role_policy" "ec2" {
  name = "ephemeral-ec2"
  role = aws_iam_role.controller.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid    = "RegionalDiscovery"
        Effect = "Allow"
        Action = [
          "ec2:DescribeImages", "ec2:DescribeInstances", "ec2:DescribeInstanceStatus",
          "ec2:DescribeInstanceTypes", "ec2:DescribeKeyPairs", "ec2:DescribeNetworkInterfaces",
          "ec2:DescribeRouteTables", "ec2:DescribeSecurityGroups", "ec2:DescribeSubnets",
          "ec2:DescribeVolumes", "ec2:DescribeVpcs"
        ]
        Resource  = "*"
        Condition = { StringEquals = { "aws:RequestedRegion" = var.aws_region } }
      },
      {
        Sid      = "ImagesAndApprovedNetwork"
        Effect   = "Allow"
        Action   = "ec2:RunInstances"
        Resource = concat(["arn:aws:ec2:${var.aws_region}::image/*"], local.subnet_arns, local.group_arns)
      },
      {
        Sid      = "TaggedInstances"
        Effect   = "Allow"
        Action   = "ec2:RunInstances"
        Resource = "${local.ec2_arn}:instance/*"
        Condition = merge(local.create_tag_conditions, {
          StringEquals = merge(local.create_tag_conditions.StringEquals, {
            "ec2:MetadataHttpTokens" = "required"
            "ec2:InstanceMarketType" = "on-demand"
            "ec2:Tenancy"            = "default"
          })
        })
      },
      {
        Sid      = "TaggedNewNetworkInterfaces"
        Effect   = "Allow"
        Action   = "ec2:RunInstances"
        Resource = "${local.ec2_arn}:network-interface/*"
        Condition = merge(local.create_tag_conditions, {
          ArnEquals = { "ec2:Subnet" = local.subnet_arns }
        })
      },
      {
        Sid      = "BoundedTaggedVolumes"
        Effect   = "Allow"
        Action   = "ec2:RunInstances"
        Resource = "${local.ec2_arn}:volume/*"
        Condition = merge(local.create_tag_conditions, {
          StringEquals = merge(local.create_tag_conditions.StringEquals, { "ec2:VolumeType" = "gp3" })
          Bool         = { "ec2:Encrypted" = "true" }
          NumericLessThanEquals = {
            "ec2:VolumeSize"       = var.max_volume_gib
            "ec2:VolumeIops"       = 3000
            "ec2:VolumeThroughput" = 125
          }
        })
      },
      {
        Sid       = "ImportPerRunPublicKey"
        Effect    = "Allow"
        Action    = "ec2:ImportKeyPair"
        Resource  = "${local.ec2_arn}:key-pair/${var.name_prefix}-*"
        Condition = local.create_tag_conditions
      },
      {
        Sid       = "UseAndDeletePoolKeys"
        Effect    = "Allow"
        Action    = ["ec2:RunInstances", "ec2:DeleteKeyPair"]
        Resource  = "${local.ec2_arn}:key-pair/${var.name_prefix}-*"
        Condition = { StringEquals = local.pool_condition }
      },
      {
        Sid    = "TagOnlyDuringCreation"
        Effect = "Allow"
        Action = "ec2:CreateTags"
        Resource = [
          "${local.ec2_arn}:instance/*", "${local.ec2_arn}:volume/*",
          "${local.ec2_arn}:network-interface/*", "${local.ec2_arn}:key-pair/${var.name_prefix}-*"
        ]
        Condition = {
          StringEquals = { "ec2:CreateAction" = ["RunInstances", "ImportKeyPair"] }
          # Tag-key values are case sensitive; the sweeper requires this exact casing.
          "ForAllValues:StringEquals" = {
            "aws:TagKeys" = ["RunnerPool", "RunId", "ExpiresAt", "Name", "adhocKeepaliveModeAction", "adhocKeepalive"]
          }
        }
      },
      {
        Sid       = "InspectAndTerminatePoolInstances"
        Effect    = "Allow"
        Action    = ["ec2:GetConsoleOutput", "ec2:TerminateInstances"]
        Resource  = "${local.ec2_arn}:instance/*"
        Condition = { StringEquals = local.pool_condition }
      }
    ]
  })
}

resource "aws_iam_role" "termination" {
  name        = "${var.name_prefix}-termination"
  description = "Scheduler can terminate only this pool's tagged EC2 workers"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "scheduler.amazonaws.com" }
      Action    = "sts:AssumeRole"
      Condition = {
        StringEquals = {
          "aws:SourceAccount" = var.aws_account_id
          # Scheduler supplies the group ARN, not an individual schedule ARN.
          "aws:SourceArn" = local.group_arn
        }
      }
    }]
  })
}

resource "aws_iam_role_policy" "termination" {
  name = "terminate-pool-instances"
  role = aws_iam_role.termination.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Action    = "ec2:TerminateInstances"
      Resource  = "${local.ec2_arn}:instance/*"
      Condition = { StringEquals = local.pool_condition }
    }]
  })
}

resource "aws_iam_role_policy" "schedules" {
  name = "worker-termination-schedules"
  role = aws_iam_role.controller.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "PerRunTerminationSchedule"
        Effect   = "Allow"
        Action   = ["scheduler:CreateSchedule", "scheduler:GetSchedule", "scheduler:DeleteSchedule"]
        Resource = "${local.scheduler_arn}:schedule/${local.group_name}/${var.name_prefix}-*"
      },
      {
        Sid       = "PassOnlyTerminationRoleToScheduler"
        Effect    = "Allow"
        Action    = "iam:PassRole"
        Resource  = local.termination_arn
        Condition = { StringEquals = { "iam:PassedToService" = "scheduler.amazonaws.com" } }
      }
    ]
  })
}
