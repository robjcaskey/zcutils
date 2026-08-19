output "api_endpoint" {
  description = "Public endpoint for submitting survey payloads."
  value       = aws_apigatewayv2_api.survey.api_endpoint
}

output "survey_post_url" {
  description = "HTTPS endpoint for posting survey events."
  value       = "${aws_apigatewayv2_api.survey.api_endpoint}/survey"
}

output "dashboard_url" {
  description = "Public privacy-filtered community dashboard."
  value       = "${aws_apigatewayv2_api.survey.api_endpoint}/dashboard"
}

output "lambda_name" {
  description = "Survey ingestion Lambda name."
  value       = aws_lambda_function.survey.function_name
}

output "rds_cluster_arn" {
  description = "Aurora cluster ARN."
  value       = aws_rds_cluster.survey.arn
}

output "rds_cluster_endpoint" {
  description = "Aurora cluster write endpoint."
  value       = aws_rds_cluster.survey.endpoint
}

output "db_credentials_secret_arn" {
  description = "Secret ARN that stores Data API credentials."
  value       = aws_rds_cluster.survey.master_user_secret[0].secret_arn
}
