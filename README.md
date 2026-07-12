# GitHub Actions runners on AWS Lambda MicroVMs

Terraform module (`terraform-aws-gha-lambdamicrovm-runners`) that runs
**ephemeral, auto-scaling GitHub Actions self-hosted runners inside AWS Lambda
MicroVMs** - Firecracker-isolated, snapshot-resumable, Graviton/arm64 compute.

A webhook-driven dispatcher launches one Lambda MicroVM per queued CI job; the VM
resumes from a prebuilt snapshot in seconds, registers a single-use runner (JIT runner), runs
the job (Docker-in-runner supported), and terminates itself the moment the job
ends - so you never pay for idle runners. One `terraform apply` builds the image,
deploys the dispatcher behind a public Lambda Function URL, and (optionally) wires
the GitHub webhook.

See [**docs/ARCHITECTURE.md**](docs/ARCHITECTURE.md) for how it works and the
[cost comparison vs GitHub-hosted runners](docs/ARCHITECTURE.md#compared-to-github-hosted-runners),
and [**docs/USAGE.md**](docs/USAGE.md) for the full setup guide.

## Usage

```hcl
provider "aws" {
  region = "us-east-1" # a region where Lambda MicroVMs are available
}

provider "awscc" {
  region = "us-east-1" # same region as aws; builds the MicroVM image
}

# Only needed when manage_webhooks = true.
provider "github" {
  owner = "my-org"
  app_auth {
    id              = "123456"
    installation_id = "12345678"
    pem_file        = file("app.pem")
  }
}

module "gha_runner" {
  source = "git::https://github.com/tarb/terraform-aws-gha-lambdamicrovm-runners.git?ref=v0.0.1"

  github_app = {
    app_id          = "123456"
    installation_id = "12345678"
    private_key     = file("app.pem") # PEM contents
  }

  github_organization = "my-org"
  manage_webhooks     = true
}
```

Then run a workflow with `runs-on: [self-hosted, linux, arm64, microvm]`. A runnable
copy is in [`examples/github-app`](examples/github-app).

## Docker on demand

Docker is a **per-job capability**, selected by a runner label: a job that adds
the extra `docker` label to its `runs-on` (e.g.
`runs-on: [self-hosted, microvm, docker]`) gets dockerd plus the
wait-for-docker job-started hook; other jobs are governed by the
`docker_default` variable.

- `docker_default = true` (the default): **every** job gets Docker — identical
  to pre-v0.0.4 behavior, no workflow changes needed.
- `docker_default = false`: only `docker`-labeled jobs get it. Unlabeled jobs
  go lightweight — they skip dockerd startup (the page-in-heavy part of a cold
  boot) and never wait on the docker-readiness hook.

Migration path: first add the `docker` label to the `runs-on` of the jobs that
actually use `docker` / `container:` / `services:`, then flip
`docker_default = false`.

## Custom image

The runner image definition can be replaced without forking the module:

- **`dockerfile`** - raw Dockerfile *text* that replaces the built-in
  [`microvm/Dockerfile`](microvm/Dockerfile).
- **`build_context_dir`** - a directory of extra files added to the image build
  context, so a custom Dockerfile can `COPY` them. Combinable with `dockerfile`
  or usable alone.

A custom Dockerfile must build `FROM` the `public.ecr.aws/lambda/microvms` tag
matching `base_image_name` (the built-in pairs the default `al2023-1` base with
`al2023-minimal`) and must keep the supervisor wiring - the module stages the
static supervisor binary at `.artifacts/entrypoint` in the build context, and
the image has to start it (validated at plan time):

```dockerfile
FROM public.ecr.aws/lambda/microvms:al2023-minimal

# ... your tooling ...

COPY .artifacts/entrypoint /entrypoint
RUN chmod 0755 /entrypoint
CMD ["/entrypoint"]
```

See [docs/USAGE.md#custom-image](docs/USAGE.md#custom-image) for details.

## Prerequisites

- **A GitHub App** - the dispatcher's only credential (App ID, installation ID,
  private key). You create it once; see
  [docs/USAGE.md#you-need-a-github-app](docs/USAGE.md#you-need-a-github-app---create-it-first).
- **On the machine that runs `terraform apply`** (the MicroVM image is a native
  Cloud Control resource; nothing is compiled locally):
  Terraform ≥ 1.9, AWS credentials, `bash` + `curl`. The dispatcher and
  webhook-proxy Lambda zips and the in-guest supervisor binary are prebuilt
  release artifacts, pinned by the module's `artifact_version` variable and
  fetched + SHA-256-verified at plan/apply time by `scripts/fetch-artifacts.sh`.

## Notes & caveats

- **arm64 / Graviton only** - Lambda MicroVMs are Graviton-only, so runners are `linux-arm64`.
- **Public ingress** - the default Lambda Function URL is internet-facing; request
  authenticity rests on the `X-Hub-Signature-256` HMAC check.
- **Prebuilt artifacts** - the dispatcher and webhook proxy are Rust Lambdas on the
  `provided.al2023` arm64 runtime, shipped as release zips built by
  `.github/workflows/release.yml` on tag push; the in-guest supervisor is a static
  musl Rust binary baked into the MicroVM image. `scripts/fetch-artifacts.sh`
  downloads the release pinned by `artifact_version` and verifies checksums - no
  local build toolchain is required.
- **First apply takes a few minutes** - it waits for the MicroVM image build to reach ACTIVE.

<!-- BEGIN_TF_DOCS -->
## Requirements

| Name | Version |
| ---- | ------- |
| <a name="requirement_terraform"></a> [terraform](#requirement\_terraform) | >= 1.9.0 |
| <a name="requirement_archive"></a> [archive](#requirement\_archive) | >= 2.4 |
| <a name="requirement_aws"></a> [aws](#requirement\_aws) | >= 6.0, < 7.0 |
| <a name="requirement_awscc"></a> [awscc](#requirement\_awscc) | >= 1.0 |
| <a name="requirement_external"></a> [external](#requirement\_external) | >= 2.3 |
| <a name="requirement_github"></a> [github](#requirement\_github) | >= 6.2 |
| <a name="requirement_random"></a> [random](#requirement\_random) | >= 3.6 |

## Providers

| Name | Version |
| ---- | ------- |
| <a name="provider_archive"></a> [archive](#provider\_archive) | 2.8.0 |
| <a name="provider_aws"></a> [aws](#provider\_aws) | 6.54.0 |
| <a name="provider_awscc"></a> [awscc](#provider\_awscc) | 1.92.0 |
| <a name="provider_external"></a> [external](#provider\_external) | 2.4.0 |
| <a name="provider_github"></a> [github](#provider\_github) | 6.13.0 |
| <a name="provider_random"></a> [random](#provider\_random) | 3.9.0 |

## Modules

No modules.

## Resources

| Name | Type |
| ---- | ---- |
| [aws_cloudwatch_event_archive.runners](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/cloudwatch_event_archive) | resource |
| [aws_cloudwatch_event_bus.runners](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/cloudwatch_event_bus) | resource |
| [aws_cloudwatch_event_rule.sweep](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/cloudwatch_event_rule) | resource |
| [aws_cloudwatch_event_rule.workflow_job](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/cloudwatch_event_rule) | resource |
| [aws_cloudwatch_event_target.sweep](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/cloudwatch_event_target) | resource |
| [aws_cloudwatch_event_target.workflow_job](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/cloudwatch_event_target) | resource |
| [aws_cloudwatch_log_group.dispatcher](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/cloudwatch_log_group) | resource |
| [aws_cloudwatch_log_group.proxy](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/cloudwatch_log_group) | resource |
| [aws_cloudwatch_log_group.runner](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/cloudwatch_log_group) | resource |
| [aws_iam_role.build](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/iam_role) | resource |
| [aws_iam_role.dispatcher](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/iam_role) | resource |
| [aws_iam_role.exec](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/iam_role) | resource |
| [aws_iam_role.proxy](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/iam_role) | resource |
| [aws_iam_role_policy.build](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/iam_role_policy) | resource |
| [aws_iam_role_policy.dispatcher](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/iam_role_policy) | resource |
| [aws_iam_role_policy.dispatcher_app_secret](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/iam_role_policy) | resource |
| [aws_iam_role_policy.exec](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/iam_role_policy) | resource |
| [aws_iam_role_policy.proxy](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/iam_role_policy) | resource |
| [aws_iam_role_policy_attachment.exec_additional](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/iam_role_policy_attachment) | resource |
| [aws_lambda_event_source_mapping.jobs](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/lambda_event_source_mapping) | resource |
| [aws_lambda_function.dispatcher](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/lambda_function) | resource |
| [aws_lambda_function.proxy](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/lambda_function) | resource |
| [aws_lambda_function_url.proxy](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/lambda_function_url) | resource |
| [aws_lambda_function_url.webhook](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/lambda_function_url) | resource |
| [aws_lambda_permission.events_sweep](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/lambda_permission) | resource |
| [aws_s3_bucket.artifacts](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/s3_bucket) | resource |
| [aws_s3_bucket_public_access_block.artifacts](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/s3_bucket_public_access_block) | resource |
| [aws_s3_bucket_server_side_encryption_configuration.artifacts](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/s3_bucket_server_side_encryption_configuration) | resource |
| [aws_s3_object.microvm_code](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/s3_object) | resource |
| [aws_sqs_queue.events_dlq](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/sqs_queue) | resource |
| [aws_sqs_queue.jobs](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/sqs_queue) | resource |
| [aws_sqs_queue_policy.jobs](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/sqs_queue_policy) | resource |
| [aws_ssm_parameter.dispatcher](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/ssm_parameter) | resource |
| [awscc_lambda_microvm_image.runner](https://registry.terraform.io/providers/hashicorp/awscc/latest/docs/resources/lambda_microvm_image) | resource |
| [github_organization_webhook.runner](https://registry.terraform.io/providers/integrations/github/latest/docs/resources/organization_webhook) | resource |
| [github_repository_webhook.runner](https://registry.terraform.io/providers/integrations/github/latest/docs/resources/repository_webhook) | resource |
| [random_password.webhook](https://registry.terraform.io/providers/hashicorp/random/latest/docs/resources/password) | resource |
| [archive_file.microvm_code](https://registry.terraform.io/providers/hashicorp/archive/latest/docs/data-sources/file) | data source |
| [aws_caller_identity.current](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/data-sources/caller_identity) | data source |
| [aws_iam_policy_document.build](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/data-sources/iam_policy_document) | data source |
| [aws_iam_policy_document.dispatcher](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/data-sources/iam_policy_document) | data source |
| [aws_iam_policy_document.dispatcher_app_secret](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/data-sources/iam_policy_document) | data source |
| [aws_iam_policy_document.dispatcher_trust](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/data-sources/iam_policy_document) | data source |
| [aws_iam_policy_document.exec](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/data-sources/iam_policy_document) | data source |
| [aws_iam_policy_document.jobs_queue](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/data-sources/iam_policy_document) | data source |
| [aws_iam_policy_document.lambda_trust](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/data-sources/iam_policy_document) | data source |
| [aws_iam_policy_document.proxy](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/data-sources/iam_policy_document) | data source |
| [aws_iam_policy_document.proxy_assume](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/data-sources/iam_policy_document) | data source |
| [aws_region.current](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/data-sources/region) | data source |
| [external_external.artifacts](https://registry.terraform.io/providers/hashicorp/external/latest/docs/data-sources/external) | data source |

## Inputs

| Name | Description | Type | Default | Required |
| ---- | ----------- | ---- | ------- | :------: |
| <a name="input_additional_execution_policy_arns"></a> [additional\_execution\_policy\_arns](#input\_additional\_execution\_policy\_arns) | Extra IAM policy ARNs attached to the MicroVM execution role (e.g. sccache cache-bucket access, ECR push). | `list(string)` | `[]` | no |
| <a name="input_additional_os_capabilities"></a> [additional\_os\_capabilities](#input\_additional\_os\_capabilities) | additionalOsCapabilities for the MicroVM image. ["ALL"] enables nested Docker / privileged ops (needed for `docker`/`services:` jobs). Set [] to tighten for non-Docker workloads. | `list(string)` | <pre>[<br/>  "ALL"<br/>]</pre> | no |
| <a name="input_artifact_version"></a> [artifact\_version](#input\_artifact\_version) | GitHub release of this module whose prebuilt artifacts (dispatcher.zip, webhook-proxy.zip, entrypoint) are deployed; releases are built by .github/workflows/release.yml. | `string` | `"v0.0.10"` | no |
| <a name="input_artifacts_bucket_name"></a> [artifacts\_bucket\_name](#input\_artifacts\_bucket\_name) | Override the S3 artifacts bucket name. Default (null) creates one named '<name\_prefix>-artifacts-<account\_id>'. | `string` | `null` | no |
| <a name="input_base_image_name"></a> [base\_image\_name](#input\_base\_image\_name) | AWS-managed MicroVM base image short name, resolved to arn:aws:lambda:<region>:aws:microvm-image:<name>. | `string` | `"al2023-1"` | no |
| <a name="input_base_image_version"></a> [base\_image\_version](#input\_base\_image\_version) | Major version of the AWS-managed base image (base\_image\_name), as a single number (e.g. "0"). Cloud Control requires it and validates the format. | `string` | `"0"` | no |
| <a name="input_build_context_dir"></a> [build\_context\_dir](#input\_build\_context\_dir) | Path to a directory whose files are added to the image build context<br/>alongside the Dockerfile (so a custom `dockerfile` can COPY them; usable<br/>with the built-in Dockerfile too, though it COPYs nothing extra). Default<br/>"" adds nothing. When set it replaces microvm/ as the context BASE, and<br/>the module overlays on top: wait-for-docker.sh, .artifacts/entrypoint,<br/>and the Dockerfile (built-in, or var.dockerfile when set) - a Dockerfile<br/>inside this directory is always overwritten, so the only Dockerfile<br/>override path is var.dockerfile. Prefer an absolute path (e.g.<br/>abspath("${path.root}/runner-context")): relative paths resolve against<br/>the directory terraform runs in. | `string` | `""` | no |
| <a name="input_dispatcher_memory_size"></a> [dispatcher\_memory\_size](#input\_dispatcher\_memory\_size) | Dispatcher Lambda memory (MB). | `number` | `256` | no |
| <a name="input_dispatcher_timeout"></a> [dispatcher\_timeout](#input\_dispatcher\_timeout) | Dispatcher Lambda timeout (seconds). | `number` | `300` | no |
| <a name="input_docker_default"></a> [docker\_default](#input\_docker\_default) | Whether jobs get Docker (dockerd + the wait-for-docker job-started hook)<br/>when their runs-on labels do NOT include the extra "docker" label. A job<br/>that requests the "docker" label always gets it. true (default): every<br/>job gets Docker — the pre-v0.0.4 behavior. Migration to label opt-in:<br/>first add "docker" to the runs-on labels of the jobs that need it (e.g.<br/>[self-hosted, microvm, docker]), then set docker\_default = false so<br/>unlabeled jobs go lightweight — they skip dockerd startup (the<br/>page-in-heavy part of a cold boot) and never stall on the hook.<br/>-> DOCKER\_DEFAULT. | `bool` | `true` | no |
| <a name="input_dockerfile"></a> [dockerfile](#input\_dockerfile) | Raw Dockerfile TEXT (not a path) that replaces the module's built-in<br/>microvm/Dockerfile as the image definition. Default "" keeps the built-in.<br/>Contract for a custom Dockerfile:<br/>  - Lambda builds it server-side ON TOP of the managed base image<br/>    (base\_image\_name/base\_image\_version -> base\_image\_arn). The built-in<br/>    Dockerfile pairs the default al2023-1 base with<br/>    `FROM public.ecr.aws/lambda/microvms:al2023-minimal`; FROM the<br/>    public.ecr.aws/lambda/microvms tag matching your base image.<br/>  - It MUST wire the in-guest supervisor, which the module stages into the<br/>    build context at .artifacts/entrypoint (validated below):<br/>        COPY .artifacts/entrypoint /entrypoint<br/>        RUN chmod 0755 /entrypoint<br/>        CMD ["/entrypoint"]<br/>  - arm64 only (Lambda MicroVMs are Graviton); the supervisor serves the<br/>    lifecycle hooks on the service-pinned port 9000 (see image.tf).<br/>Combine with build\_context\_dir for files the Dockerfile needs to COPY. | `string` | `""` | no |
| <a name="input_egress_network_connector_arn"></a> [egress\_network\_connector\_arn](#input\_egress\_network\_connector\_arn) | Customer-managed Lambda network connector ARN for VPC egress (reach private resources: VPC endpoints, internal services, private cluster APIs). null = AWS-managed INTERNET\_EGRESS. | `string` | `null` | no |
| <a name="input_event_pattern_label"></a> [event\_pattern\_label](#input\_event\_pattern\_label) | Single most-selective runner label used in the EventBridge rule pattern (array patterns are contains-ANY, so matching all required\_labels would over-trigger; the dispatcher re-checks the full subset). | `string` | `"microvm"` | no |
| <a name="input_github_api_url"></a> [github\_api\_url](#input\_github\_api\_url) | GitHub REST API base URL. Set for GitHub Enterprise Server. -> GH\_API\_URL. | `string` | `"https://api.github.com"` | no |
| <a name="input_github_app"></a> [github\_app](#input\_github\_app) | GitHub App credentials - the dispatcher's machine identity.<br/>  app\_id:          numeric App ID as a string, e.g. "123456" (NOT the Client ID).<br/>  installation\_id: App installation ID. Optional for the dispatcher (it derives<br/>                   it per-repo), but REQUIRED when manage\_webhooks = true because<br/>                   the github provider's app\_auth block needs it.<br/>  private\_key:     App private key PEM *contents* (not a file path). | <pre>object({<br/>    app_id          = string<br/>    installation_id = optional(string)<br/>    private_key     = string<br/>  })</pre> | `null` | no |
| <a name="input_github_app_secret_arn"></a> [github\_app\_secret\_arn](#input\_github\_app\_secret\_arn) | Secrets Manager ARN holding the GitHub App credential ({app\_id, private\_key}). When set, the dispatcher reads it at RUNTIME and the private key never enters Terraform state. Mutually exclusive with github\_app. Requires manage\_webhooks = false (the github provider still needs a raw key). | `string` | `null` | no |
| <a name="input_github_organization"></a> [github\_organization](#input\_github\_organization) | GitHub org (or user) that owns the runners/webhooks. Used as the github provider `owner` in your root module, and (with empty github\_repositories) selects an org-level webhook. | `string` | `null` | no |
| <a name="input_github_repositories"></a> [github\_repositories](#input\_github\_repositories) | Repositories to attach runner webhooks to, as 'owner/name'. Empty => an org-level webhook (requires github\_organization). All entries must belong to the github provider's owner. | `list(string)` | `[]` | no |
| <a name="input_image_version"></a> [image\_version](#input\_image\_version) | Pin the runner image version the dispatcher launches (e.g. "3.0"). Default (null) makes the dispatcher resolve the latest ACTIVE version at runtime, so image rebuilds are picked up with no redeploy. | `string` | `null` | no |
| <a name="input_log_retention_days"></a> [log\_retention\_days](#input\_log\_retention\_days) | CloudWatch retention (days) for the dispatcher and runner log groups. | `number` | `14` | no |
| <a name="input_manage_webhooks"></a> [manage\_webhooks](#input\_manage\_webhooks) | If true, Terraform creates the workflow\_job webhooks via the github provider (configure it in your root module). If false, wire them up yourself from the webhook\_payload\_url + webhook\_secret outputs. | `bool` | `true` | no |
| <a name="input_max_concurrency"></a> [max\_concurrency](#input\_max\_concurrency) | Maximum concurrently RUNNING MicroVMs (0 = unlimited). Over the cap the dispatcher defers the job; EventBridge's target retry (backoff, up to 24h) is the queue, so capped jobs wait rather than fail. | `number` | `0` | no |
| <a name="input_max_duration_seconds"></a> [max\_duration\_seconds](#input\_max\_duration\_seconds) | maximumDurationInSeconds passed to RunMicrovm - the hard cap on one job's runtime, after which the MicroVM is auto-terminated (cost backstop). | `number` | `1200` | no |
| <a name="input_name_prefix"></a> [name\_prefix](#input\_name\_prefix) | Prefix for every created resource (roles, lambda, secret, bucket, image). | `string` | `"gha-microvm"` | no |
| <a name="input_permissions_boundary"></a> [permissions\_boundary](#input\_permissions\_boundary) | Permissions boundary ARN applied to every IAM role this module creates (dispatcher/exec/build). Needed when the calling credentials may only create bounded roles. | `string` | `null` | no |
| <a name="input_required_labels"></a> [required\_labels](#input\_required\_labels) | Labels a workflow\_job must ALL carry for the dispatcher to launch a MicroVM (subset match). -> REQUIRED\_LABELS. | `list(string)` | <pre>[<br/>  "self-hosted",<br/>  "microvm"<br/>]</pre> | no |
| <a name="input_runner_environment_variables"></a> [runner\_environment\_variables](#input\_runner\_environment\_variables) | Extra environment variables baked into the MicroVM image environment, visible to every job (e.g. SCCACHE\_BUCKET). NEVER secrets — the snapshot and env are shared by all runs. | `map(string)` | `{}` | no |
| <a name="input_runner_labels"></a> [runner\_labels](#input\_runner\_labels) | Labels the ephemeral runner registers with (arm64 only). -> RUNNER\_LABELS. | `list(string)` | <pre>[<br/>  "self-hosted",<br/>  "linux",<br/>  "arm64",<br/>  "microvm"<br/>]</pre> | no |
| <a name="input_runner_memory_mib"></a> [runner\_memory\_mib](#input\_runner\_memory\_mib) | MicroVM memory (minimumMemoryInMiB) baked into the image. Tiers: 512,1024,2048,4096,8192,... Defaults to 8192 because the default image enables Docker (additional\_os\_capabilities = ["ALL"]) and real Docker builds OOM at less; drop to 4096/2048 for lightweight jobs to cut cost. | `number` | `8192` | no |
| <a name="input_tags"></a> [tags](#input\_tags) | Tags applied to all taggable resources. | `map(string)` | `{}` | no |
| <a name="input_warm_pool"></a> [warm\_pool](#input\_warm\_pool) | Suspend-based warm pool: finished VMs are SUSPENDED (near-free) instead of terminated, and resumed for the next job — skipping boot, dockerd start, and snapshot page-in. Requires the suspend/resume semantics validated on the service (see docs); ships disabled. | <pre>object({<br/>    enabled  = optional(bool, false)<br/>    max_size = optional(number, 4)<br/>  })</pre> | `{}` | no |

## Outputs

| Name | Description |
| ---- | ----------- |
| <a name="output_artifacts_bucket"></a> [artifacts\_bucket](#output\_artifacts\_bucket) | Name of the S3 bucket holding the MicroVM code artifact. |
| <a name="output_dispatcher_function_name"></a> [dispatcher\_function\_name](#output\_dispatcher\_function\_name) | Name of the dispatcher Lambda function. |
| <a name="output_events_webhook_url"></a> [events\_webhook\_url](#output\_events\_webhook\_url) | Preferred payload URL for the workflow\_job webhook (EventBridge ingress: retries + DLQ + replay). webhook\_payload\_url remains supported for deployments still pointing at the direct Function URL. |
| <a name="output_exec_role_arn"></a> [exec\_role\_arn](#output\_exec\_role\_arn) | ARN of the MicroVM execution role (executionRoleArn for RunMicrovm). |
| <a name="output_image_arn"></a> [image\_arn](#output\_image\_arn) | ARN of the MicroVM image (imageIdentifier for RunMicrovm). |
| <a name="output_image_version"></a> [image\_version](#output\_image\_version) | Active version of the MicroVM image (e.g. '8.0'); increments on each rebuild. |
| <a name="output_secret_param_arn"></a> [secret\_param\_arn](#output\_secret\_param\_arn) | ARN of the SSM SecureString parameter. |
| <a name="output_secret_param_name"></a> [secret\_param\_name](#output\_secret\_param\_name) | Name of the SSM Parameter Store SecureString holding the webhook secret + GitHub credential. |
| <a name="output_webhook_payload_url"></a> [webhook\_payload\_url](#output\_webhook\_payload\_url) | Payload URL for the GitHub webhook (content type: application/json). Already wired up when manage\_webhooks = true. |
| <a name="output_webhook_secret"></a> [webhook\_secret](#output\_webhook\_secret) | HMAC secret shared with GitHub (X-Hub-Signature-256). Sensitive: read with `terraform output -raw webhook_secret`. Set as the webhook 'Secret' if wiring up manually (manage\_webhooks = false). |
<!-- END_TF_DOCS -->

## Examples

- [`examples/github-app`](examples/github-app) - GitHub App auth + Terraform-managed webhooks.

## Documentation

- [**docs/USAGE.md**](docs/USAGE.md) - full setup: creating the GitHub App, all inputs/options, ingress, scope, runbook.
- [**docs/ARCHITECTURE.md**](docs/ARCHITECTURE.md) - deep dive: components, snapshot/DNS/self-terminate mechanics, cost model.

## License

[MIT](LICENSE).
