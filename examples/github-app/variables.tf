variable "region" {
  type    = string
  default = "us-east-1"
}

variable "name_prefix" {
  type    = string
  default = "gha-microvm"
}

variable "github_organization" {
  type        = string
  description = "Org/user that owns the repos and the GitHub App installation."
}

variable "github_repositories" {
  type        = list(string)
  default     = []
  description = "Repos as 'owner/name'. Leave empty for a single org-level webhook."
}

variable "app_id" {
  type        = string
  description = "Numeric GitHub App ID as a string (not the Client ID)."
}

variable "app_installation_id" {
  type        = string
  description = "GitHub App installation ID (required for managed webhooks)."
}

variable "app_private_key" {
  type        = string
  sensitive   = true
  description = "GitHub App private key PEM contents."
}
