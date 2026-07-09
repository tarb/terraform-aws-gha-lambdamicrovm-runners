output "webhook_payload_url" {
  description = "Payload URL for the GitHub webhook (content type: application/json). Already wired up when manage_webhooks = true."
  value       = aws_lambda_function_url.webhook.function_url
}

output "webhook_secret" {
  description = "HMAC secret shared with GitHub (X-Hub-Signature-256). Sensitive: read with `terraform output -raw webhook_secret`. Set as the webhook 'Secret' if wiring up manually (manage_webhooks = false)."
  value       = random_password.webhook.result
  sensitive   = true
}

output "image_arn" {
  description = "ARN of the MicroVM image (imageIdentifier for RunMicrovm)."
  value       = awscc_lambda_microvm_image.runner.image_arn
}

output "image_version" {
  description = "Active version of the MicroVM image (e.g. '8.0'); increments on each rebuild."
  value       = awscc_lambda_microvm_image.runner.latest_active_image_version
}

output "exec_role_arn" {
  description = "ARN of the MicroVM execution role (executionRoleArn for RunMicrovm)."
  value       = aws_iam_role.exec.arn
}

output "dispatcher_function_name" {
  description = "Name of the dispatcher Lambda function."
  value       = aws_lambda_function.dispatcher.function_name
}

output "secret_param_name" {
  description = "Name of the SSM Parameter Store SecureString holding the webhook secret + GitHub credential."
  value       = aws_ssm_parameter.dispatcher.name
}

output "secret_param_arn" {
  description = "ARN of the SSM SecureString parameter."
  value       = aws_ssm_parameter.dispatcher.arn
}

output "artifacts_bucket" {
  description = "Name of the S3 bucket holding the MicroVM code artifact."
  value       = aws_s3_bucket.artifacts.id
}

output "events_webhook_url" {
  value       = aws_lambda_function_url.proxy.function_url
  description = "Preferred payload URL for the workflow_job webhook (EventBridge ingress: retries + DLQ + replay). webhook_payload_url remains supported for deployments still pointing at the direct Function URL."
}
