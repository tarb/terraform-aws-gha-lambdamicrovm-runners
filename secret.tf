###############################################################################
# Dispatcher credential store. SSM Parameter Store (SecureString) rather than
# Secrets Manager: standard parameters carry no per-secret monthly fee, so this
# is effectively free vs ~$0.40/secret/month. Holds the webhook HMAC secret +
# the GitHub App credential, in the JSON shape the dispatcher expects.
###############################################################################

resource "random_password" "webhook" {
  length  = 40
  special = false # alphanumeric: safe in HTTP headers and the GitHub UI
}

resource "aws_ssm_parameter" "dispatcher" {
  name        = "/${var.name_prefix}/dispatcher"
  description = "GHA MicroVM dispatcher: webhook secret + GitHub App credential"
  type        = "SecureString" # encrypted at rest with the AWS-managed aws/ssm KMS key
  # JSON shape the dispatcher expects: webhook HMAC secret + GitHub App credential.
  # With github_app_secret_arn, the App credential is fetched at runtime from
  # Secrets Manager (never in tfstate); the param then holds only the webhook
  # HMAC secret. The legacy github_app path still embeds the credential here.
  value = jsonencode(var.github_app_secret_arn != null ? {
    webhook_secret = random_password.webhook.result
    } : {
    webhook_secret  = random_password.webhook.result
    app_id          = var.github_app.app_id
    app_private_key = var.github_app.private_key
  })
  tags = local.tags
}
