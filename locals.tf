data "aws_caller_identity" "current" {}
data "aws_region" "current" {}

locals {
  # Account/region are woven into several ARNs (IAM policies, the image base +
  # egress connectors), so they stay as locals. Everything else that used to live
  # here is inlined at its single use site.
  account_id = data.aws_caller_identity.current.account_id
  region     = data.aws_region.current.region

  tags = merge({ "managed-by" = "terraform", "module" = "gha-runner-microvm" }, var.tags)

  # Customer-managed VPC egress connector when provided; the AWS-managed
  # internet-egress connector otherwise.
  egress_connector_arn = coalesce(
    var.egress_network_connector_arn,
    "arn:aws:lambda:${local.region}:aws:network-connector:aws-network-connector:INTERNET_EGRESS",
  )
}
