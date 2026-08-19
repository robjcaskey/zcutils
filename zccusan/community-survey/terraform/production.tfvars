aws_region  = "us-east-1"
environment = "prod"

dsql_deletion_protection = true
dsql_force_destroy       = false

lambda_memory_mb          = 1769
lambda_timeout_seconds    = 12
lambda_log_retention_days = 30
api_log_retention_days    = 30

api_throttling_burst_limit = 100
api_throttling_rate_limit  = 50
