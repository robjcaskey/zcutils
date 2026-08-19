provider "aws" {
  region = var.aws_region

  default_tags {
    tags = local.selected_tags
  }
}

locals {
  base_name = "${var.project_name}-${var.environment}"
  selected_tags = {
    Project     = var.project_name
    Environment = var.environment
    ManagedBy   = "terraform"
  }
}

data "aws_vpc" "main" {
  default = true
}

data "aws_subnets" "default" {
  filter {
    name   = "vpc-id"
    values = [data.aws_vpc.main.id]
  }
}

resource "aws_db_subnet_group" "survey" {
  name = "${local.base_name}-subnet-group"
  subnet_ids = slice(
    sort(data.aws_subnets.default.ids),
    0,
    min(2, length(data.aws_subnets.default.ids))
  )

  tags = local.selected_tags
}

resource "aws_security_group" "aurora" {
  name        = "${local.base_name}-aurora"
  description = "Aurora ingress controls for survey cluster"
  vpc_id      = data.aws_vpc.main.id

  ingress {
    description = "No external workload traffic needed for Data API mode."
    from_port   = 5432
    to_port     = 5432
    protocol    = "tcp"
    self        = true
  }

  tags = local.selected_tags
}

resource "aws_rds_cluster" "survey" {
  cluster_identifier     = "${local.base_name}-cluster"
  engine                 = "aurora-postgresql"
  engine_version         = var.database_engine_version
  db_subnet_group_name   = aws_db_subnet_group.survey.name
  vpc_security_group_ids = [aws_security_group.aurora.id]

  database_name               = var.database_name
  master_username             = var.database_username
  manage_master_user_password = true

  enable_http_endpoint = true

  serverlessv2_scaling_configuration {
    min_capacity = var.database_min_capacity
    max_capacity = var.database_max_capacity
  }

  storage_encrypted         = true
  backup_retention_period   = var.database_backup_retention_days
  copy_tags_to_snapshot     = true
  deletion_protection       = var.database_deletion_protection
  skip_final_snapshot       = var.database_skip_final_snapshot
  final_snapshot_identifier = var.database_skip_final_snapshot ? null : "${local.base_name}-final"

  tags = local.selected_tags
}

resource "aws_rds_cluster_instance" "writer" {
  identifier          = "${local.base_name}-writer"
  cluster_identifier  = aws_rds_cluster.survey.id
  engine              = aws_rds_cluster.survey.engine
  instance_class      = "db.serverless"
  publicly_accessible = false

  tags = local.selected_tags
}

data "archive_file" "lambda_zip" {
  type        = "zip"
  output_path = "${path.module}/.terraform/zccusan_survey_lambda.zip"

  source {
    content  = file("${path.module}/../lambda/main.py")
    filename = "main.py"
  }

  source {
    content  = file("${path.module}/../lambda/dashboard.html")
    filename = "dashboard.html"
  }
}

resource "aws_iam_role" "lambda" {
  name = "${local.base_name}-lambda-role"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Action = "sts:AssumeRole"
      Effect = "Allow"
      Principal = {
        Service = "lambda.amazonaws.com"
      }
    }]
  })

  tags = local.selected_tags
}

data "aws_iam_policy_document" "lambda_policy" {
  statement {
    effect = "Allow"
    actions = [
      "secretsmanager:GetSecretValue"
    ]
    resources = [aws_rds_cluster.survey.master_user_secret[0].secret_arn]
  }

  statement {
    effect = "Allow"
    actions = [
      "rds-data:ExecuteStatement",
      "rds-data:BatchExecuteStatement",
      "rds-data:BeginTransaction",
      "rds-data:CommitTransaction",
      "rds-data:RollbackTransaction"
    ]
    resources = [aws_rds_cluster.survey.arn]
  }
}

resource "aws_iam_role_policy" "lambda_policy" {
  name   = "${local.base_name}-lambda-policy"
  role   = aws_iam_role.lambda.id
  policy = data.aws_iam_policy_document.lambda_policy.json
}

resource "aws_iam_role_policy_attachment" "lambda_basic" {
  role       = aws_iam_role.lambda.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_cloudwatch_log_group" "lambda" {
  name              = "/aws/lambda/${local.base_name}-ingest"
  retention_in_days = var.lambda_log_retention_days
}

resource "aws_lambda_function" "survey" {
  function_name    = "${local.base_name}-ingest"
  role             = aws_iam_role.lambda.arn
  handler          = "main.handler"
  runtime          = var.lambda_runtime
  filename         = data.archive_file.lambda_zip.output_path
  source_code_hash = data.archive_file.lambda_zip.output_base64sha256
  timeout          = var.lambda_timeout_seconds
  memory_size      = var.lambda_memory_mb

  environment {
    variables = {
      SURVEY_DB_CLUSTER_ARN      = aws_rds_cluster.survey.arn
      SURVEY_DB_SECRET_ARN       = aws_rds_cluster.survey.master_user_secret[0].secret_arn
      SURVEY_DB_NAME             = var.database_name
      SURVEY_DB_TABLE            = var.table_name
      SURVEY_ENVIRONMENTS_TABLE  = "community_environments"
      SURVEY_ACTIVE_WINDOW_HOURS = "2"
    }
  }

  depends_on = [aws_cloudwatch_log_group.lambda]
}

resource "aws_apigatewayv2_api" "survey" {
  name          = "${local.base_name}-api"
  protocol_type = "HTTP"
}

resource "aws_apigatewayv2_integration" "survey" {
  api_id = aws_apigatewayv2_api.survey.id

  integration_type       = "AWS_PROXY"
  integration_uri        = aws_lambda_function.survey.invoke_arn
  integration_method     = "POST"
  payload_format_version = "2.0"
}

resource "aws_apigatewayv2_route" "survey" {
  api_id    = aws_apigatewayv2_api.survey.id
  route_key = "POST /survey"
  target    = "integrations/${aws_apigatewayv2_integration.survey.id}"
}

resource "aws_apigatewayv2_route" "dashboard_root" {
  api_id    = aws_apigatewayv2_api.survey.id
  route_key = "GET /"
  target    = "integrations/${aws_apigatewayv2_integration.survey.id}"
}

resource "aws_apigatewayv2_route" "dashboard" {
  api_id    = aws_apigatewayv2_api.survey.id
  route_key = "GET /dashboard"
  target    = "integrations/${aws_apigatewayv2_integration.survey.id}"
}

resource "aws_apigatewayv2_route" "community_environments" {
  api_id    = aws_apigatewayv2_api.survey.id
  route_key = "GET /community/environments"
  target    = "integrations/${aws_apigatewayv2_integration.survey.id}"
}

resource "aws_apigatewayv2_stage" "default" {
  api_id      = aws_apigatewayv2_api.survey.id
  name        = "$default"
  auto_deploy = true

  default_route_settings {
    throttling_burst_limit = var.api_throttling_burst_limit
    throttling_rate_limit  = var.api_throttling_rate_limit
  }

  access_log_settings {
    destination_arn = aws_cloudwatch_log_group.api_access.arn
    format = jsonencode({
      requestId        = "$context.requestId"
      requestTime      = "$context.requestTime"
      httpMethod       = "$context.httpMethod"
      routeKey         = "$context.routeKey"
      status           = "$context.status"
      responseLength   = "$context.responseLength"
      integrationError = "$context.integrationErrorMessage"
    })
  }
}

resource "aws_cloudwatch_log_group" "api_access" {
  name              = "/aws/apigateway/${local.base_name}-api"
  retention_in_days = var.api_log_retention_days
}

resource "aws_lambda_permission" "apigw_invoke" {
  statement_id  = "AllowAPIGatewayInvoke"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.survey.function_name
  principal     = "apigateway.amazonaws.com"
  source_arn    = "${aws_apigatewayv2_api.survey.execution_arn}/*/*/*"
}
