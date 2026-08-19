aws_region  = "us-east-1"
environment = "prod"

database_engine_version        = "16.14"
database_min_capacity          = 0.5
database_max_capacity          = 2.0
database_backup_retention_days = 14
database_deletion_protection   = true
database_skip_final_snapshot   = false

lambda_memory_mb          = 256
lambda_timeout_seconds    = 12
lambda_log_retention_days = 30
api_log_retention_days    = 30

api_throttling_burst_limit = 100
api_throttling_rate_limit  = 50
