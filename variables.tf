###############################################################################
# Naming. Region and account are NOT variables - they are derived from the AWS
# provider (data.aws_region + data.aws_caller_identity) in locals.tf.
###############################################################################

variable "name_prefix" {
  description = "Prefix for every created resource (roles, lambda, secret, bucket, image)."
  type        = string
  default     = "gha-microvm"

  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9-]{1,30}[a-z0-9]$", var.name_prefix))
    error_message = "name_prefix must be 3-32 chars, lowercase alphanumeric and hyphens, not starting/ending with a hyphen."
  }
}

variable "tags" {
  description = "Tags applied to all taggable resources."
  type        = map(string)
  default     = {}
}

variable "artifacts_bucket_name" {
  description = "Override the S3 artifacts bucket name. Default (null) creates one named '<name_prefix>-artifacts-<account_id>'."
  type        = string
  default     = null
}

variable "artifact_version" {
  description = "GitHub release of this module whose prebuilt artifacts (dispatcher.zip, webhook-proxy.zip, entrypoint) are deployed; releases are built by .github/workflows/release.yml."
  type        = string
  default     = "v0.0.3"
}

###############################################################################
# GitHub App credentials - the dispatcher's machine identity (App only, no PAT).
###############################################################################

variable "github_app" {
  description = <<-EOT
    GitHub App credentials - the dispatcher's machine identity.
      app_id:          numeric App ID as a string, e.g. "123456" (NOT the Client ID).
      installation_id: App installation ID. Optional for the dispatcher (it derives
                       it per-repo), but REQUIRED when manage_webhooks = true because
                       the github provider's app_auth block needs it.
      private_key:     App private key PEM *contents* (not a file path).
  EOT
  type = object({
    app_id          = string
    installation_id = optional(string)
    private_key     = string
  })
  sensitive = true
  default   = null

  validation {
    condition     = !var.manage_webhooks || try(var.github_app.installation_id, null) != null
    error_message = "github_app.installation_id is required when manage_webhooks = true (the github provider's app_auth needs it)."
  }
}

variable "github_app_secret_arn" {
  description = "Secrets Manager ARN holding the GitHub App credential ({app_id, private_key}). When set, the dispatcher reads it at RUNTIME and the private key never enters Terraform state. Mutually exclusive with github_app. Requires manage_webhooks = false (the github provider still needs a raw key)."
  type        = string
  default     = null

  validation {
    condition     = (var.github_app == null) != (var.github_app_secret_arn == null)
    error_message = "Provide exactly one of github_app or github_app_secret_arn."
  }

  validation {
    condition     = var.github_app_secret_arn == null || !var.manage_webhooks
    error_message = "github_app_secret_arn requires manage_webhooks = false (managed webhooks need a raw key for the github provider)."
  }
}

###############################################################################
# Scope. Provide github_repositories and/or github_organization.
#   repos non-empty            -> a workflow_job webhook per repo
#   repos empty + org set      -> a single org-level webhook
###############################################################################

variable "github_organization" {
  description = "GitHub org (or user) that owns the runners/webhooks. Used as the github provider `owner` in your root module, and (with empty github_repositories) selects an org-level webhook."
  type        = string
  default     = null
}

variable "github_repositories" {
  description = "Repositories to attach runner webhooks to, as 'owner/name'. Empty => an org-level webhook (requires github_organization). All entries must belong to the github provider's owner."
  type        = list(string)
  default     = []

  validation {
    condition     = alltrue([for r in var.github_repositories : can(regex("^[^/]+/[^/]+$", r))])
    error_message = "Each entry in github_repositories must be 'owner/name'."
  }

  validation {
    condition     = length(var.github_repositories) > 0 || var.github_organization != null
    error_message = "Provide github_repositories and/or github_organization to define the runner scope."
  }

  # The github provider has a single `owner`, so every repo webhook is created
  # under it. Catch a cross-owner entry at plan time instead of silently hooking
  # the wrong repo (or a 404 at apply).
  validation {
    condition     = var.github_organization == null || alltrue([for r in var.github_repositories : split("/", r)[0] == var.github_organization])
    error_message = "Every github_repositories entry must be owned by github_organization (i.e. '<github_organization>/name'). The github provider's owner is shared across all webhooks."
  }
}

variable "manage_webhooks" {
  description = "If true, Terraform creates the workflow_job webhooks via the github provider (configure it in your root module). If false, wire them up yourself from the webhook_payload_url + webhook_secret outputs."
  type        = bool
  default     = true
}

###############################################################################
# MicroVM / runner sizing and behaviour (arm64 / Graviton only).
###############################################################################

variable "base_image_name" {
  description = "AWS-managed MicroVM base image short name, resolved to arn:aws:lambda:<region>:aws:microvm-image:<name>."
  type        = string
  default     = "al2023-1"
}

variable "base_image_version" {
  description = "Major version of the AWS-managed base image (base_image_name), as a single number (e.g. \"0\"). Cloud Control requires it and validates the format."
  type        = string
  default     = "0"
}

variable "image_version" {
  description = "Pin the runner image version the dispatcher launches (e.g. \"3.0\"). Default (null) makes the dispatcher resolve the latest ACTIVE version at runtime, so image rebuilds are picked up with no redeploy."
  type        = string
  default     = null
}

variable "runner_memory_mib" {
  description = "MicroVM memory (minimumMemoryInMiB) baked into the image. Tiers: 512,1024,2048,4096,8192,... Defaults to 8192 because the default image enables Docker (additional_os_capabilities = [\"ALL\"]) and real Docker builds OOM at less; drop to 4096/2048 for lightweight jobs to cut cost."
  type        = number
  default     = 8192

  validation {
    condition     = contains([512, 1024, 2048, 4096, 8192], var.runner_memory_mib)
    error_message = "runner_memory_mib must be one of the discrete MicroVM tiers: 512, 1024, 2048, 4096, 8192."
  }
}

variable "max_duration_seconds" {
  description = "maximumDurationInSeconds passed to RunMicrovm - the hard cap on one job's runtime, after which the MicroVM is auto-terminated (cost backstop)."
  type        = number
  default     = 1200

  validation {
    condition     = var.max_duration_seconds >= 60 && var.max_duration_seconds <= 28800
    error_message = "max_duration_seconds must be between 60 and 28800 (8h MicroVM max lifetime)."
  }
}

variable "additional_os_capabilities" {
  description = "additionalOsCapabilities for the MicroVM image. [\"ALL\"] enables nested Docker / privileged ops (needed for `docker`/`services:` jobs). Set [] to tighten for non-Docker workloads."
  type        = list(string)
  default     = ["ALL"]
}

variable "required_labels" {
  description = "Labels a workflow_job must ALL carry for the dispatcher to launch a MicroVM (subset match). -> REQUIRED_LABELS."
  type        = list(string)
  default     = ["self-hosted", "microvm"]
}

variable "runner_labels" {
  description = "Labels the ephemeral runner registers with (arm64 only). -> RUNNER_LABELS."
  type        = list(string)
  default     = ["self-hosted", "linux", "arm64", "microvm"]
}

###############################################################################
# Dispatcher Lambda + logging.
###############################################################################

variable "dispatcher_memory_size" {
  description = "Dispatcher Lambda memory (MB)."
  type        = number
  default     = 256
}

variable "dispatcher_timeout" {
  description = "Dispatcher Lambda timeout (seconds)."
  type        = number
  default     = 300
}

variable "log_retention_days" {
  description = "CloudWatch retention (days) for the dispatcher and runner log groups."
  type        = number
  default     = 14
}

variable "github_api_url" {
  description = "GitHub REST API base URL. Set for GitHub Enterprise Server. -> GH_API_URL."
  type        = string
  default     = "https://api.github.com"
}

variable "egress_network_connector_arn" {
  description = "Customer-managed Lambda network connector ARN for VPC egress (reach private resources: VPC endpoints, internal services, private cluster APIs). null = AWS-managed INTERNET_EGRESS."
  type        = string
  default     = null
}

variable "runner_environment_variables" {
  description = "Extra environment variables baked into the MicroVM image environment, visible to every job (e.g. SCCACHE_BUCKET). NEVER secrets — the snapshot and env are shared by all runs."
  type        = map(string)
  default     = {}
}

variable "additional_execution_policy_arns" {
  description = "Extra IAM policy ARNs attached to the MicroVM execution role (e.g. sccache cache-bucket access, ECR push)."
  type        = list(string)
  default     = []
}

variable "permissions_boundary" {
  description = "Permissions boundary ARN applied to every IAM role this module creates (dispatcher/exec/build). Needed when the calling credentials may only create bounded roles."
  type        = string
  default     = null
}

variable "max_concurrency" {
  description = "Maximum concurrently RUNNING MicroVMs (0 = unlimited). Over the cap the dispatcher defers the job; EventBridge's target retry (backoff, up to 24h) is the queue, so capped jobs wait rather than fail."
  type        = number
  default     = 0
}

variable "warm_pool" {
  description = "Suspend-based warm pool: finished VMs are SUSPENDED (near-free) instead of terminated, and resumed for the next job — skipping boot, dockerd start, and snapshot page-in. Requires the suspend/resume semantics validated on the service (see docs); ships disabled."
  type = object({
    enabled  = optional(bool, false)
    max_size = optional(number, 4)
  })
  default = {}
}

variable "event_pattern_label" {
  description = "Single most-selective runner label used in the EventBridge rule pattern (array patterns are contains-ANY, so matching all required_labels would over-trigger; the dispatcher re-checks the full subset)."
  type        = string
  default     = "microvm"
}

###############################################################################
# Custom image definition - bring your own Dockerfile and/or extra build-
# context files without forking the module. The image build context is staged
# by scripts/stage-context.sh; whatever the combination, it always ends up
# containing wait-for-docker.sh and the .artifacts/entrypoint supervisor.
###############################################################################

variable "dockerfile" {
  description = <<-EOT
    Raw Dockerfile TEXT (not a path) that replaces the module's built-in
    microvm/Dockerfile as the image definition. Default "" keeps the built-in.
    Contract for a custom Dockerfile:
      - Lambda builds it server-side ON TOP of the managed base image
        (base_image_name/base_image_version -> base_image_arn). The built-in
        Dockerfile pairs the default al2023-1 base with
        `FROM public.ecr.aws/lambda/microvms:al2023-minimal`; FROM the
        public.ecr.aws/lambda/microvms tag matching your base image.
      - It MUST wire the in-guest supervisor, which the module stages into the
        build context at .artifacts/entrypoint (validated below):
            COPY .artifacts/entrypoint /entrypoint
            RUN chmod 0755 /entrypoint
            CMD ["/entrypoint"]
      - arm64 only (Lambda MicroVMs are Graviton); the supervisor serves the
        lifecycle hooks on the service-pinned port 9000 (see image.tf).
    Combine with build_context_dir for files the Dockerfile needs to COPY.
  EOT
  type        = string
  default     = ""

  validation {
    condition     = var.dockerfile == "" || strcontains(var.dockerfile, "/entrypoint")
    error_message = <<-EOT
      dockerfile must wire the supervisor binary (staged into the build context
      at .artifacts/entrypoint), or the MicroVM never serves its lifecycle
      hooks and the image build fails its READY probe. Include:
        COPY .artifacts/entrypoint /entrypoint
        RUN chmod 0755 /entrypoint
        CMD ["/entrypoint"]
    EOT
  }
}

variable "build_context_dir" {
  description = <<-EOT
    Path to a directory whose files are added to the image build context
    alongside the Dockerfile (so a custom `dockerfile` can COPY them; usable
    with the built-in Dockerfile too, though it COPYs nothing extra). Default
    "" adds nothing. When set it replaces microvm/ as the context BASE, and
    the module overlays on top: wait-for-docker.sh, .artifacts/entrypoint,
    and the Dockerfile (built-in, or var.dockerfile when set) - a Dockerfile
    inside this directory is always overwritten, so the only Dockerfile
    override path is var.dockerfile. Prefer an absolute path (e.g.
    abspath("$${path.root}/runner-context")): relative paths resolve against
    the directory terraform runs in.
  EOT
  type        = string
  default     = ""
}
