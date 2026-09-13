# Disabled by default. This user's key cannot directly call EC2 or Scheduler.
resource "aws_iam_user" "bootstrap" {
  count = var.enable_key_fallback ? 1 : 0
  name  = "${var.name_prefix}-bootstrap"
}

resource "aws_iam_user_policy" "bootstrap" {
  count = var.enable_key_fallback ? 1 : 0
  name  = "assume-runner-controller-only"
  user  = aws_iam_user.bootstrap[0].name
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = "sts:AssumeRole"
      Resource = aws_iam_role.controller.arn
    }]
  })
}

resource "aws_iam_access_key" "bootstrap" {
  count   = var.enable_key_fallback ? 1 : 0
  user    = aws_iam_user.bootstrap[0].name
  pgp_key = var.key_pgp_public_key_base64
}
