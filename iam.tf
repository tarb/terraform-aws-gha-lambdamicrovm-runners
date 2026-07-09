###############################################################################
# Trust policies. The MicroVM build/exec roles are assumed by the lambda service
# principal; the confused-deputy guard pins the source account.
###############################################################################

data "aws_iam_policy_document" "lambda_trust" {
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
    condition {
      test     = "StringEquals"
      variable = "aws:SourceAccount"
      values   = [local.account_id]
    }
  }
}

# The dispatcher is an ordinary Lambda function; no SourceAccount condition.
data "aws_iam_policy_document" "dispatcher_trust" {
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

###############################################################################
# Build role - assumed by Lambda DURING the MicroVM image build. Reads the code
# artifact from S3 and writes build logs.
###############################################################################

resource "aws_iam_role" "build" {
  permissions_boundary = var.permissions_boundary
  name                 = "${var.name_prefix}-build-role"
  assume_role_policy   = data.aws_iam_policy_document.lambda_trust.json
  tags                 = local.tags
}

data "aws_iam_policy_document" "build" {
  statement {
    sid       = "ReadArtifactWriteBuildOutput"
    effect    = "Allow"
    actions   = ["s3:GetObject", "s3:PutObject", "s3:ListBucket"]
    resources = [aws_s3_bucket.artifacts.arn, "${aws_s3_bucket.artifacts.arn}/*"]
  }
  statement {
    sid       = "BuildLogs"
    effect    = "Allow"
    actions   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["arn:aws:logs:${local.region}:${local.account_id}:*"]
  }
}

resource "aws_iam_role_policy" "build" {
  name   = "build"
  role   = aws_iam_role.build.id
  policy = data.aws_iam_policy_document.build.json
}

###############################################################################
# Exec role - assumed AT RUNTIME by each MicroVM. Writes runner/dockerd logs and
# may self-terminate when an ephemeral job ends (stops billing immediately).
###############################################################################

resource "aws_iam_role" "exec" {
  permissions_boundary = var.permissions_boundary
  name                 = "${var.name_prefix}-exec-role"
  assume_role_policy   = data.aws_iam_policy_document.lambda_trust.json
  tags                 = local.tags
}

data "aws_iam_policy_document" "exec" {
  statement {
    sid       = "RuntimeLogs"
    effect    = "Allow"
    actions   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["arn:aws:logs:${local.region}:${local.account_id}:*"]
  }
  statement {
    sid       = "SelfTerminate"
    effect    = "Allow"
    actions   = ["lambda:TerminateMicrovm"]
    resources = ["*"]
  }
  # Pull-handoff mailbox: a resumed VM fetches + deletes its own parked run
  # payload. Path-wide (the VM id is dynamic); tokens inside are short-lived,
  # repo-scoped, and every runner shares this trust domain anyway.
  statement {
    sid       = "ClaimHandoff"
    effect    = "Allow"
    actions   = ["ssm:GetParameter", "ssm:DeleteParameter"]
    resources = ["arn:aws:ssm:${local.region}:${local.account_id}:parameter/${var.name_prefix}/handoff/*"]
  }
  statement {
    sid       = "DecryptHandoff"
    effect    = "Allow"
    actions   = ["kms:Decrypt"]
    resources = ["*"]
    condition {
      test     = "StringEquals"
      variable = "kms:ViaService"
      values   = ["ssm.${local.region}.amazonaws.com"]
    }
  }
}

resource "aws_iam_role_policy" "exec" {
  name   = "exec"
  role   = aws_iam_role.exec.id
  policy = data.aws_iam_policy_document.exec.json
}

###############################################################################
# Dispatcher role - assumed by the dispatcher Lambda. Launches/terminates
# MicroVMs, passes the egress connector + exec role, and reads the secret.
###############################################################################

resource "aws_iam_role" "dispatcher" {
  permissions_boundary = var.permissions_boundary
  name                 = "${var.name_prefix}-dispatcher-role"
  assume_role_policy   = data.aws_iam_policy_document.dispatcher_trust.json
  tags                 = local.tags
}

data "aws_iam_policy_document" "dispatcher" {
  statement {
    sid       = "Logs"
    effect    = "Allow"
    actions   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["arn:aws:logs:${local.region}:${local.account_id}:*"]
  }
  statement {
    sid       = "MicroVMOps"
    effect    = "Allow"
    actions   = ["lambda:RunMicrovm", "lambda:TerminateMicrovm", "lambda:GetMicrovm", "lambda:GetMicrovmImage", "lambda:ListMicrovms", "lambda:SuspendMicrovm", "lambda:ResumeMicrovm", "lambda:CreateMicrovmAuthToken"]
    resources = ["*"]
  }

  # Consume the EventBridge-fed job queue (event source mapping).
  statement {
    sid       = "ConsumeJobQueue"
    actions   = ["sqs:ReceiveMessage", "sqs:DeleteMessage", "sqs:GetQueueAttributes"]
    resources = [aws_sqs_queue.jobs.arn]
  }
  statement {
    sid     = "PassNetworkConnector"
    effect  = "Allow"
    actions = ["lambda:PassNetworkConnector"]
    resources = compact([
      "arn:aws:lambda:${local.region}:aws:network-connector:aws-network-connector:*",
      var.egress_network_connector_arn,
    ])
  }
  statement {
    sid       = "PassExecRole"
    effect    = "Allow"
    actions   = ["iam:PassRole"]
    resources = [aws_iam_role.exec.arn]
  }
  statement {
    sid       = "ReadDispatcherParam"
    effect    = "Allow"
    actions   = ["ssm:GetParameter"]
    resources = [aws_ssm_parameter.dispatcher.arn]
  }
  # Pull-handoff mailbox: the dispatcher parks a resumed VM's run payload at
  # /{name_prefix}/handoff/{microvmId} and polls for its deletion (the claim
  # ack); the sweep GCs orphans by path.
  statement {
    sid     = "HandoffParams"
    effect  = "Allow"
    actions = ["ssm:PutParameter", "ssm:GetParameter", "ssm:DeleteParameter", "ssm:GetParametersByPath"]
    resources = [
      "arn:aws:ssm:${local.region}:${local.account_id}:parameter/${var.name_prefix}/handoff",
      "arn:aws:ssm:${local.region}:${local.account_id}:parameter/${var.name_prefix}/handoff/*",
    ]
  }
  statement {
    sid    = "DecryptDispatcherParam"
    effect = "Allow"
    # GenerateDataKey covers SecureString PutParameter under the aws/ssm key.
    actions   = ["kms:Decrypt", "kms:GenerateDataKey"]
    resources = ["*"]
    # Scope the AWS-managed aws/ssm key use to SSM only.
    condition {
      test     = "StringEquals"
      variable = "kms:ViaService"
      values   = ["ssm.${local.region}.amazonaws.com"]
    }
  }
}

resource "aws_iam_role_policy" "dispatcher" {
  name   = "dispatcher"
  role   = aws_iam_role.dispatcher.id
  policy = data.aws_iam_policy_document.dispatcher.json
}

# Extra permissions for the runner execution role (e.g. sccache bucket, ECR).
resource "aws_iam_role_policy_attachment" "exec_additional" {
  # count (not for_each): the ARNs are computed by the caller (known after
  # apply), and for_each requires apply-time-known keys.
  count      = length(var.additional_execution_policy_arns)
  role       = aws_iam_role.exec.name
  policy_arn = var.additional_execution_policy_arns[count.index]
}

# Dispatcher reads the GitHub App credential from Secrets Manager at runtime
# (github_app_secret_arn path) so the private key never lands in tfstate.
data "aws_iam_policy_document" "dispatcher_app_secret" {
  count = var.github_app_secret_arn != null ? 1 : 0

  statement {
    sid       = "ReadGitHubAppSecret"
    effect    = "Allow"
    actions   = ["secretsmanager:GetSecretValue"]
    resources = [var.github_app_secret_arn]
  }

  statement {
    sid       = "DecryptGitHubAppSecret"
    effect    = "Allow"
    actions   = ["kms:Decrypt"]
    resources = ["*"]
    condition {
      test     = "StringEquals"
      variable = "kms:ViaService"
      values   = ["secretsmanager.${local.region}.amazonaws.com"]
    }
  }
}

resource "aws_iam_role_policy" "dispatcher_app_secret" {
  count  = var.github_app_secret_arn != null ? 1 : 0
  name   = "github-app-secret"
  role   = aws_iam_role.dispatcher.id
  policy = data.aws_iam_policy_document.dispatcher_app_secret[0].json
}
