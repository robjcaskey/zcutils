aws_region  = "us-east-1"
environment = "dev"

dsql_deletion_protection = false
dsql_force_destroy       = true

lambda_memory_mb          = 1769
lambda_timeout_seconds    = 12
lambda_log_retention_days = 7
api_log_retention_days    = 7

api_throttling_burst_limit = 25
api_throttling_rate_limit  = 10
