###############################################################################
# Dispatcher Lambda (Rust, provided.al2023). A plan-time fetch step
# (data.external.artifacts) downloads the prebuilt release artifacts
# (dispatcher.zip, webhook-proxy.zip, entrypoint) for var.artifact_version from
# this module's GitHub releases and verifies them against the release's
# SHA256SUMS before anything reads them.
###############################################################################

locals {
  # The plan-time fetch (data.external.artifacts below) writes these files and
  # the functions/image read them, so the paths are shared.
  artifact_dir   = "${path.module}/.terraform-build/${var.artifact_version}"
  dispatcher_zip = "${local.artifact_dir}/dispatcher.zip"
  proxy_zip      = "${local.artifact_dir}/webhook-proxy.zip"

  # STAGED image build context: microvm/ (or var.build_context_dir) overlaid
  # with wait-for-docker.sh, the fetched entrypoint binary, and the Dockerfile
  # (built-in or var.dockerfile) - assembled by scripts/stage-context.sh so
  # data.archive_file.microvm_code (s3.tf) zips one directory regardless of
  # which custom-image overrides are set.
  context_dir = "${path.module}/.terraform-build/context"

}

# Fetch + verify the release artifacts and assemble the image build context
# AT PLAN TIME. A data source (re-)runs on every plan, so every workspace —
# including a fresh ephemeral CI one — holds the files before anything reads
# them, and downstream values (zip md5, S3 key, lambda hashes) are
# deterministic: plans are clean unless content actually changed. The previous
# terraform_data shape had to force a replace (timestamp()) whenever files
# were absent, churning every CI plan. Idempotent: verified artifacts aren't
# re-downloaded; staging is a cheap rebuild that always reflects the current
# dockerfile/build_context inputs, so no change-detection triggers are needed.
# Argv rides the exec array (no shell), so the Dockerfile text can't break
# quoting. Requires bash + curl on the PLAN host as well as the apply host.
#
# Split plan/apply pipelines: a saved plan does NOT re-run this at apply, and
# aws_s3_object.source + the lambda filenames re-read the files there — ship
# .terraform-build/ alongside the plan artifact (see s3.tf).
data "external" "artifacts" {
  program = [
    "bash", "${path.module}/scripts/prepare-artifacts.sh",
    var.artifact_version,
    local.artifact_dir,
    "${path.module}/microvm",
    local.context_dir,
    var.build_context_dir,
    var.dockerfile,
  ]
}

resource "aws_cloudwatch_log_group" "dispatcher" {
  name              = "/aws/lambda/${var.name_prefix}-dispatcher"
  retention_in_days = var.log_retention_days
  tags              = local.tags
}

resource "aws_lambda_function" "dispatcher" {
  function_name = "${var.name_prefix}-dispatcher"
  role          = aws_iam_role.dispatcher.arn

  filename = local.dispatcher_zip
  # REDEPLOY TRIGGER, not an integrity check: a synthetic provider-only value
  # that is known at plan (the fetched zip is not) and flips exactly when the
  # pinned release changes; it does not need to equal the real CodeSha256.
  # Artifact integrity is TLS on the download + `sha256sum -c` against the
  # release's SHA256SUMS in scripts/fetch-artifacts.sh.
  source_code_hash = base64sha256(var.artifact_version)

  runtime       = "provided.al2023"
  architectures = ["arm64"]
  handler       = "bootstrap"
  timeout       = var.dispatcher_timeout
  memory_size   = var.dispatcher_memory_size

  # The dispatcher reads AWS_REGION from the runtime, so it is intentionally
  # absent here (it is a reserved key Lambda rejects).
  environment {
    variables = {
      IMAGE_ARN = awscc_lambda_microvm_image.runner.image_arn
      # Empty => the dispatcher resolves the latest ACTIVE version at runtime (so a
      # rebuilt image is picked up with no redeploy and no two-apply lag). A non-null
      # var.image_version pins it. Deliberately NOT wired to the awscc computed
      # `latest_active_image_version` - that lags a plan behind on updates.
      IMAGE_VERSION         = var.image_version != null ? var.image_version : ""
      EXEC_ROLE_ARN         = aws_iam_role.exec.arn
      EGRESS_CONNECTOR      = local.egress_connector_arn
      MAX_DURATION          = tostring(var.max_duration_seconds)
      LOG_GROUP             = aws_cloudwatch_log_group.runner.name
      PARAM_NAME            = aws_ssm_parameter.dispatcher.name
      APP_SECRET_ARN        = var.github_app_secret_arn != null ? var.github_app_secret_arn : ""
      REQUIRED_LABELS       = join(",", var.required_labels)
      RUNNER_LABELS         = join(",", var.runner_labels)
      GH_API_URL            = var.github_api_url
      MAX_CONCURRENCY       = tostring(var.max_concurrency)
      DOCKER_DEFAULT        = var.docker_default ? "true" : "false"
      HANDOFF_PREFIX        = "/${var.name_prefix}/handoff"
      POOL_ENABLED          = var.warm_pool.enabled ? "true" : "false"
      POOL_MAX_SIZE         = tostring(var.warm_pool.max_size)
      SUSPEND_DELAY_SECONDS = "20"
      SWEEP_MIN_AGE_SECONDS = "360"
    }
  }

  # No attribute links the fetch to the function, so depend explicitly: the zip
  # must exist before the create/update within one apply.
  depends_on = [
    data.external.artifacts,
    aws_iam_role_policy.dispatcher,
    aws_cloudwatch_log_group.dispatcher,
  ]

  tags = local.tags
}
