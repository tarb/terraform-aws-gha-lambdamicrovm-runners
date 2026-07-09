###############################################################################
# Dispatcher Lambda (Rust, provided.al2023). A terraform_data fetch step
# downloads the prebuilt release artifacts (dispatcher.zip, webhook-proxy.zip,
# entrypoint) for var.artifact_version from this module's GitHub releases and
# verifies them against the release's SHA256SUMS before anything reads them.
###############################################################################

locals {
  # The fetch step writes these files and the functions/image read them, so the
  # paths are shared (there is no resource attribute to reference for them).
  artifact_dir   = "${path.module}/.terraform-build/${var.artifact_version}"
  dispatcher_zip = "${local.artifact_dir}/dispatcher.zip"
  proxy_zip      = "${local.artifact_dir}/webhook-proxy.zip"

  # STAGED image build context: microvm/ (or var.build_context_dir) overlaid
  # with wait-for-docker.sh, the fetched entrypoint binary, and the Dockerfile
  # (built-in or var.dockerfile) - assembled by scripts/stage-context.sh so
  # data.archive_file.microvm_code (s3.tf) zips one directory regardless of
  # which custom-image overrides are set.
  context_dir = "${path.module}/.terraform-build/context"

  # Plan-time fingerprint of var.build_context_dir (relative path + content of
  # every file). fileset()/filesha256() are evaluated at plan, which only works
  # because the directory is user-provided config that exists at plan time; it
  # cannot capture files another resource generates during the same apply.
  build_context_fingerprint = var.build_context_dir == "" ? "" : sha256(join("\n", [
    for f in sort(fileset(var.build_context_dir, "**")) :
    "${f}:${filesha256("${var.build_context_dir}/${f}")}"
  ]))
}

resource "terraform_data" "artifacts" {
  # Re-fetch + restage when the pinned release or any custom-image input
  # changes, OR when any artifact/staged file is absent from this workspace:
  # ephemeral CI workspaces never carry them, so without the fileexists trigger
  # the provisioner (which only fires on create/replace) never re-runs and the
  # resources below read files that existed only in the workspace that
  # originally created this resource. timestamp() forces the replace; local
  # workspaces that still hold the files stay stable.
  triggers_replace = {
    version = var.artifact_version
    # Custom-image inputs: restage (and thus re-zip) when the Dockerfile
    # override or any file under build_context_dir changes.
    dockerfile    = sha256(var.dockerfile)
    build_context = local.build_context_fingerprint
    # The zip now reads a staged COPY of the built-in context, so edits to
    # microvm/* must force a restage (before staging, the archive read
    # microvm/ directly and picked them up implicitly).
    builtin_context = sha256(join("\n", [
      filesha256("${path.module}/microvm/Dockerfile"),
      filesha256("${path.module}/microvm/wait-for-docker.sh"),
    ]))
    artifacts = alltrue([
      fileexists(local.dispatcher_zip),
      fileexists(local.proxy_zip),
      fileexists("${local.artifact_dir}/entrypoint"),
      fileexists("${local.context_dir}/Dockerfile"),
      fileexists("${local.context_dir}/wait-for-docker.sh"),
      fileexists("${local.context_dir}/.artifacts/entrypoint"),
    ]) ? "present" : timestamp()
  }

  # Download + verify the release artifacts, then assemble the image build
  # context (base dir + wait-for-docker.sh + entrypoint binary + Dockerfile)
  # into local.context_dir for the microvm code zip. The Dockerfile override
  # travels via the environment, not argv, so arbitrary Dockerfile text cannot
  # break shell quoting.
  provisioner "local-exec" {
    command = "bash '${path.module}/scripts/fetch-artifacts.sh' '${var.artifact_version}' '${local.artifact_dir}' && bash '${path.module}/scripts/stage-context.sh' '${path.module}/microvm' '${local.context_dir}' '${local.artifact_dir}/entrypoint' '${var.build_context_dir}'"
    environment = {
      DOCKERFILE_OVERRIDE = var.dockerfile
    }
  }
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
    terraform_data.artifacts,
    aws_iam_role_policy.dispatcher,
    aws_cloudwatch_log_group.dispatcher,
  ]

  tags = local.tags
}
