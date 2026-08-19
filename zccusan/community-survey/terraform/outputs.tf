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

output "dsql_cluster_arn" {
  description = "Request-metered Aurora DSQL cluster ARN."
  value       = aws_dsql_cluster.survey.arn
}

output "dsql_cluster_endpoint" {
  description = "PostgreSQL-compatible Aurora DSQL endpoint."
  value       = "${aws_dsql_cluster.survey.identifier}.dsql.${var.aws_region}.on.aws"
}
