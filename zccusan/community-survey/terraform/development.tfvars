aws_region  = "us-east-1"
environment = "dev"

database_engine_version        = "16.14"
database_min_capacity          = 0.5
database_max_capacity          = 1.0
database_backup_retention_days = 1
database_deletion_protection   = false
database_skip_final_snapshot   = true

lambda_memory_mb          = 256
lambda_timeout_seconds    = 12
lambda_log_retention_days = 7
api_log_retention_days    = 7

api_throttling_burst_limit = 25
api_throttling_rate_limit  = 10
