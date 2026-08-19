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
  description = "Database name used by the survey table."
  type        = string
  default     = "community_survey"
}

variable "database_username" {
  description = "Master username for the Aurora PostgreSQL cluster."
  type        = string
  default     = "survey_admin"
}

variable "database_engine_version" {
  description = "Pinned Aurora PostgreSQL engine version."
  type        = string
  default     = "16.14"
}

variable "database_min_capacity" {
  description = "Aurora Serverless v2 minimum ACUs."
  type        = number
  default     = 0.5
}

variable "database_max_capacity" {
  description = "Aurora Serverless v2 maximum ACUs."
  type        = number
  default     = 2.0
}

variable "database_backup_retention_days" {
  description = "Number of days to retain automated Aurora backups."
  type        = number
  default     = 7
}

variable "database_deletion_protection" {
  description = "Protect the Aurora cluster from accidental deletion."
  type        = bool
  default     = false
}

variable "database_skip_final_snapshot" {
  description = "Skip a final Aurora snapshot when destroying the cluster."
  type        = bool
  default     = true
}

variable "table_name" {
  description = "Aurora table name for survey events."
  type        = string
  default     = "survey_events"
}
