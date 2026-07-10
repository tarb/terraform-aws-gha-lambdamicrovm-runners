###############################################################################
# MicroVM image, managed natively via the AWS Cloud Control provider
# (awscc_lambda_microvm_image). Cloud Control builds the image from the S3 code
# artifact, waits for the async build to finish, and exposes the active version -
# so there is no shell-out, no polling, and no aws-cli/jq on the apply host.
###############################################################################

# Runner/dockerd logs. Pre-created so retention is managed.
resource "aws_cloudwatch_log_group" "runner" {
  name              = "/aws/lambda-microvms/${var.name_prefix}-runner"
  retention_in_days = var.log_retention_days
  tags              = local.tags
}

resource "awscc_lambda_microvm_image" "runner" {
  name           = "${var.name_prefix}-runner"
  description    = "GitHub Actions self-hosted runner (arm64) on Lambda MicroVMs - ${var.name_prefix}-runner"
  base_image_arn = "arn:aws:lambda:${local.region}:aws:microvm-image:${var.base_image_name}"
  # Cloud Control requires the base image version explicitly (the CLI defaulted it).
  base_image_version = var.base_image_version
  build_role_arn     = aws_iam_role.build.arn

  code_artifact = {
    uri = "s3://${aws_s3_object.microvm_code.bucket}/${aws_s3_object.microvm_code.key}"
  }

  # arm64/Graviton only; memory tier baked into the image.
  cpu_configurations = [{ architecture = "ARM_64" }]
  resources          = [{ minimum_memory_in_mi_b = var.runner_memory_mib }]

  additional_os_capabilities = var.additional_os_capabilities
  egress_network_connectors  = [local.egress_connector_arn]
  # Cloud Control marks EnvironmentVariables as a required key and the provider
  # omits an empty set, so ship one harmless marker. The runner's real config
  # comes from the Dockerfile ENV, not from here. DISABLE_IPV6 makes the
  # supervisor blackhole global guest IPv6 at boot (unreachable default route
  # + accept_ra=0 — NOT the disable_ipv6 sysctls, which kill link-local and
  # fail the READY probe NotStabilized; see variables.tf) — for IPv4-only
  # egress connectors, where dual-stack clients otherwise waste a doomed
  # happy-eyeballs IPv6 attempt per connection.
  environment_variables = concat(
    [{ key = "GHA_RUNNER_MICROVM", value = "1" }],
    var.disable_guest_ipv6 ? [{ key = "DISABLE_IPV6", value = "1" }] : [],
    [for k, v in var.runner_environment_variables : { key = k, value = v }],
  )

  # Lifecycle hooks on :9000 — SERVICE-PINNED, do not change: the schema
  # advertises 1-65535 but the service only dials 9000; a version registered
  # on any other port fails its READY probe and the image build dies
  # NotStabilized. Workloads that need host :9000 must bind elsewhere instead.
  hooks = {
    port = 9000
    microvm_image_hooks = {
      ready                       = "ENABLED"
      ready_timeout_in_seconds    = 120
      validate                    = "ENABLED"
      validate_timeout_in_seconds = 30
    }
    microvm_hooks = {
      run                          = "ENABLED"
      run_timeout_in_seconds       = 10
      resume                       = "ENABLED"
      resume_timeout_in_seconds    = 5
      suspend                      = "ENABLED"
      suspend_timeout_in_seconds   = 10
      terminate                    = "ENABLED"
      terminate_timeout_in_seconds = 15
    }
  }

  logging = {
    cloudwatch = {
      log_group = aws_cloudwatch_log_group.runner.name
    }
  }

  tags = [for k, v in local.tags : { key = k, value = v }]

  depends_on = [
    aws_iam_role_policy.build,
    aws_s3_object.microvm_code,
    aws_cloudwatch_log_group.runner,
  ]
}
