terraform {
  # >= 1.9 is REQUIRED: the "installation_id required when manage_webhooks" rule
  # uses a cross-variable validation (references another variable), which only
  # works on Terraform 1.9+.
  required_version = ">= 1.9.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 6.0, < 7.0"
    }
    # Only exercised when var.manage_webhooks = true. Callers that wire webhooks
    # themselves never configure it.
    github = {
      source  = "integrations/github"
      version = ">= 6.2"
    }
    archive = {
      source  = "hashicorp/archive"
      version = ">= 2.4"
    }
    random = {
      source  = "hashicorp/random"
      version = ">= 3.6"
    }
    # Native MicroVM image resource (awscc_lambda_microvm_image) - Cloud Control
    # builds it and waits for the async build, replacing the CLI shell-out.
    awscc = {
      source  = "hashicorp/awscc"
      version = ">= 1.0"
    }
  }
}
