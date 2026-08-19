variable "aws_region" {
  description = "AWS region for deployment."
  type        = string
  default     = "us-east-1"
}

variable "project_name" {
  description = "Prefix used for resource names."
  type        = string
  default     = "zccusan-community-survey"
}

variable "environment" {
  description = "Environment/stage label."
  type        = string
  default     = "dev"
}

variable "lambda_runtime" {
  description = "Lambda Python runtime."
  type        = string
  default     = "python3.12"
}

variable "lambda_memory_mb" {
  description = "Lambda memory size in MB."
  type        = number
  default     = 256
}

variable "lambda_timeout_seconds" {
  description = "Lambda timeout in seconds."
  type        = number
  default     = 12
}

variable "lambda_log_retention_days" {
  description = "CloudWatch retention for Lambda logs."
  type        = number
  default     = 30
}

variable "api_log_retention_days" {
  description = "CloudWatch retention for HTTP API access logs."
  type        = number
  default     = 30
}

variable "api_throttling_burst_limit" {
  description = "Maximum HTTP API burst before API Gateway throttles requests."
  type        = number
  default     = 100
}

variable "api_throttling_rate_limit" {
  description = "Sustained public HTTP API request rate limit per second."
  type        = number
  default     = 50
}

variable "database_name" {
  description = "Aurora DSQL database name used by the survey tables."
  type        = string
  default     = "postgres"
}

variable "dsql_deletion_protection" {
  description = "Protect the Aurora DSQL cluster from accidental deletion."
  type        = bool
  default     = false
}

variable "dsql_force_destroy" {
  description = "Permit Terraform to delete a non-empty Aurora DSQL cluster."
  type        = bool
  default     = true
}

variable "table_name" {
  description = "Aurora DSQL table name for raw survey events."
  type        = string
  default     = "survey_events"
}
