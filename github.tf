###############################################################################
# GitHub webhooks (only when manage_webhooks = true). The github provider is
# configured in your ROOT module (owner + app_auth or token); this module just
# consumes the webhook resources, gated so manage_webhooks = false creates none.
#
#   github_repositories non-empty  -> a workflow_job webhook per repo
#   github_repositories empty + org -> a single org-level webhook
#
# ignore_changes on configuration[0].secret suppresses the provider's masked-
# secret perpetual diff. Trade-off: if you ROTATE the webhook secret, Terraform
# won't push the new value to GitHub on its own - replace the webhook resource
# (terraform apply -replace=...) so the new secret reaches GitHub and stays in
# sync with SSM Parameter Store.
###############################################################################

resource "github_repository_webhook" "runner" {
  # repos non-empty => one webhook per repo.
  for_each = var.manage_webhooks && length(var.github_repositories) > 0 ? toset(var.github_repositories) : toset([])

  # The provider's `owner` supplies the org/user; this is the repo name only.
  repository = element(split("/", each.value), 1)
  active     = true
  events     = ["workflow_job"]

  configuration {
    url          = aws_lambda_function_url.webhook.function_url
    content_type = "json" # MUST be json so the signed bytes == the HMAC-checked bytes
    insecure_ssl = false
    secret       = random_password.webhook.result
  }

  # The provider returns the secret masked, which otherwise shows a perpetual diff.
  lifecycle {
    ignore_changes = [configuration[0].secret]
  }
}

resource "github_organization_webhook" "runner" {
  # No repos + an org set => a single org-level webhook.
  count = var.manage_webhooks && length(var.github_repositories) == 0 && var.github_organization != null ? 1 : 0

  active = true
  events = ["workflow_job"]

  configuration {
    url          = aws_lambda_function_url.webhook.function_url
    content_type = "json"
    insecure_ssl = false
    secret       = random_password.webhook.result
  }

  lifecycle {
    ignore_changes = [configuration[0].secret]
  }
}
