# Example: GitHub App auth (recommended) + Terraform-managed webhooks.
#
#   terraform init
#   terraform apply
#
# One apply builds the MicroVM image, deploys the dispatcher, and (because
# manage_webhooks = true) creates the workflow_job webhooks on your repos/org.

terraform {
  required_version = ">= 1.9.0"
  required_providers {
    aws    = { source = "hashicorp/aws", version = ">= 6.0, < 7.0" }
    awscc  = { source = "hashicorp/awscc", version = ">= 1.0" }
    github = { source = "integrations/github", version = ">= 6.2" }
  }
}

provider "aws" {
  region = var.region
}

# The module builds the MicroVM image via awscc; point it at the same region.
provider "awscc" {
  region = var.region
}

# Required only because manage_webhooks = true. Same App identity as the module.
provider "github" {
  owner = var.github_organization
  app_auth {
    id              = var.app_id
    installation_id = var.app_installation_id
    pem_file        = var.app_private_key # PEM contents (or file("app.pem"))
  }
}

module "gha_runner" {
  source = "../../"

  name_prefix         = var.name_prefix
  github_organization = var.github_organization
  github_repositories = var.github_repositories # empty => one org-level webhook

  github_app = {
    app_id          = var.app_id
    installation_id = var.app_installation_id
    private_key     = var.app_private_key
  }

  manage_webhooks      = true
  runner_memory_mib    = 4096
  max_duration_seconds = 1800
}

output "webhook_payload_url" {
  value = module.gha_runner.webhook_payload_url
}

output "image_version" {
  value = module.gha_runner.image_version
}
