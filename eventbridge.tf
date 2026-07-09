# EventBridge ingress: GitHub webhooks land on a DUMB proxy (HMAC verify +
# PutEvents — tiny failure surface), rules route workflow_job events onto an
# SQS job queue, and the dispatcher consumes it. GitHub delivers exactly once
# with no retries; SQS is what makes that survivable: a raising handler
# returns the message (visibility timeout), retried up to ~24h, then
# dead-lettered — nothing vanishes silently, and the bus archive can replay
# incident windows. The concurrency cap leans on this: over the cap the
# dispatcher raises and the message waits on the queue.

resource "aws_cloudwatch_event_bus" "runners" {
  name = "${var.name_prefix}-events"
  tags = local.tags
}

resource "aws_cloudwatch_event_archive" "runners" {
  name             = "${var.name_prefix}-events"
  event_source_arn = aws_cloudwatch_event_bus.runners.arn
  retention_days   = 14 # replay window for incident recovery
}

# ── the proxy ────────────────────────────────────────────────────────────────
# The zip is a prebuilt release artifact fetched (and checksum-verified) by
# terraform_data.artifacts in dispatcher.tf; local.proxy_zip is defined there.

resource "aws_cloudwatch_log_group" "proxy" {
  name              = "/aws/lambda/${var.name_prefix}-webhook-proxy"
  retention_in_days = var.log_retention_days
  tags              = local.tags
}

data "aws_iam_policy_document" "proxy_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "proxy" {
  name                 = "${var.name_prefix}-webhook-proxy"
  assume_role_policy   = data.aws_iam_policy_document.proxy_assume.json
  permissions_boundary = var.permissions_boundary
  tags                 = local.tags
}

data "aws_iam_policy_document" "proxy" {
  statement {
    sid       = "PutEvents"
    actions   = ["events:PutEvents"]
    resources = [aws_cloudwatch_event_bus.runners.arn]
  }

  statement {
    sid       = "ReadWebhookSecret"
    actions   = ["ssm:GetParameter"]
    resources = [aws_ssm_parameter.dispatcher.arn]
  }

  # SecureString under the AWS-managed aws/ssm key: GetParameter(decrypt)
  # needs kms:Decrypt in the identity policy (same statement the dispatcher
  # role carries) or the proxy 500s every webhook.
  statement {
    sid       = "DecryptParam"
    actions   = ["kms:Decrypt"]
    resources = ["*"]
    condition {
      test     = "StringEquals"
      variable = "kms:ViaService"
      values   = ["ssm.${local.region}.amazonaws.com"]
    }
  }

  statement {
    sid       = "Logs"
    actions   = ["logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["${aws_cloudwatch_log_group.proxy.arn}:*"]
  }
}

resource "aws_iam_role_policy" "proxy" {
  name   = "webhook-proxy"
  role   = aws_iam_role.proxy.id
  policy = data.aws_iam_policy_document.proxy.json
}

resource "aws_lambda_function" "proxy" {
  function_name = "${var.name_prefix}-webhook-proxy"
  role          = aws_iam_role.proxy.arn

  filename = local.proxy_zip
  # Redeploy trigger, not an integrity check — see the dispatcher's
  # source_code_hash comment (integrity = TLS + sha256sum -c in the fetch).
  source_code_hash = base64sha256(var.artifact_version)

  runtime       = "provided.al2023"
  architectures = ["arm64"]
  handler       = "bootstrap"
  timeout       = 10
  memory_size   = 128

  environment {
    variables = {
      EVENT_BUS_NAME = aws_cloudwatch_event_bus.runners.name
      PARAM_NAME     = aws_ssm_parameter.dispatcher.name
    }
  }

  depends_on = [
    terraform_data.artifacts,
    aws_iam_role_policy.proxy,
    aws_cloudwatch_log_group.proxy,
  ]

  tags = local.tags
}

resource "aws_lambda_function_url" "proxy" {
  function_name      = aws_lambda_function.proxy.function_name
  authorization_type = "NONE" # HMAC signature IS the auth (verified in-handler)
}

# ── rules → SQS → dispatcher ─────────────────────────────────────────────────
# THE QUEUE IS REAL SQS, not EventBridge target retries: EventBridge invokes
# Lambda targets ASYNCHRONOUSLY, where a function error gets Lambda's own
# async policy (2 retries) and is then dropped — target retry_policy only
# covers EventBridge→Lambda delivery. Routing rules through SQS gives true
# queue semantics: a raising handler returns the message after the visibility
# timeout, retried until maxReceiveCount, then dead-lettered. The
# concurrency-cap defer leans on exactly this.

resource "aws_sqs_queue" "events_dlq" {
  name                      = "${var.name_prefix}-events-dlq"
  message_retention_seconds = 1209600 # 14 days to notice + redrive
  tags                      = local.tags
}

resource "aws_sqs_queue" "jobs" {
  name                       = "${var.name_prefix}-jobs"
  message_retention_seconds  = 1209600
  visibility_timeout_seconds = 360 # > dispatcher timeout; also the defer-retry cadence

  redrive_policy = jsonencode({
    deadLetterTargetArn = aws_sqs_queue.events_dlq.arn
    # ~24h of deferral at the visibility cadence before dead-lettering.
    maxReceiveCount = 240
  })

  tags = local.tags
}

data "aws_iam_policy_document" "jobs_queue" {
  statement {
    actions   = ["sqs:SendMessage"]
    resources = [aws_sqs_queue.jobs.arn]
    principals {
      type        = "Service"
      identifiers = ["events.amazonaws.com"]
    }
    condition {
      test     = "ArnEquals"
      variable = "aws:SourceArn"
      values   = [for r in aws_cloudwatch_event_rule.workflow_job : r.arn]
    }
  }
}

resource "aws_sqs_queue_policy" "jobs" {
  queue_url = aws_sqs_queue.jobs.id
  policy    = data.aws_iam_policy_document.jobs_queue.json
}

locals {
  # queued: dispatch (resume-or-launch). completed: warm-pool suspend intake.
  workflow_job_rules = {
    queued    = { description = "queued microvm jobs -> dispatch" }
    completed = { description = "completed microvm jobs -> pool suspend" }
  }
}

resource "aws_cloudwatch_event_rule" "workflow_job" {
  for_each = local.workflow_job_rules

  name           = "${var.name_prefix}-job-${each.key}"
  description    = each.value.description
  event_bus_name = aws_cloudwatch_event_bus.runners.name

  # NOTE: EventBridge array patterns are OR (contains-ANY) — matching on
  # var.required_labels would fire for any "self-hosted" job in the org. The
  # pattern uses the single most-selective label; the dispatcher re-checks
  # the full subset, so correctness never depends on the pattern.
  event_pattern = jsonencode({
    source        = ["github.webhook"]
    "detail-type" = ["workflow_job"]
    detail = {
      action       = [each.key]
      workflow_job = { labels = [var.event_pattern_label] }
    }
  })

  tags = local.tags
}

resource "aws_cloudwatch_event_target" "workflow_job" {
  for_each = local.workflow_job_rules

  rule           = aws_cloudwatch_event_rule.workflow_job[each.key].name
  event_bus_name = aws_cloudwatch_event_bus.runners.name
  arn            = aws_sqs_queue.jobs.arn
}

resource "aws_lambda_event_source_mapping" "jobs" {
  event_source_arn = aws_sqs_queue.jobs.arn
  function_name    = aws_lambda_function.dispatcher.arn
  batch_size       = 1 # one job per invocation: a failure retries exactly that job

  # Partial batch responses: the dispatcher returns batchItemFailures so only
  # the failed/malformed record retries (and dead-letters alone) — an
  # already-dispatched batch sibling is never re-driven into a duplicate VM.
  # Keeps any batch_size safe, not just 1.
  function_response_types = ["ReportBatchItemFailures"]
}

# ── reconciliation sweep: ground truth beats webhooks ────────────────────────
# Catches every loss class the bus can't: never-delivered webhooks, retries
# exhausted, VMs dead pre-registration. Over-dispatch is cheap — a duplicate
# VM gets no job and the entrypoint idle watchdog reaps it in ~2 minutes.

resource "aws_cloudwatch_event_rule" "sweep" {
  name                = "${var.name_prefix}-sweep"
  description         = "reconcile GitHub queued jobs against the fleet"
  schedule_expression = "rate(5 minutes)"
  tags                = local.tags
}

resource "aws_cloudwatch_event_target" "sweep" {
  rule  = aws_cloudwatch_event_rule.sweep.name
  arn   = aws_lambda_function.dispatcher.arn
  input = jsonencode({ sweep = true })
}

resource "aws_lambda_permission" "events_sweep" {
  statement_id  = "eventbridge-sweep"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.dispatcher.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.sweep.arn
}
