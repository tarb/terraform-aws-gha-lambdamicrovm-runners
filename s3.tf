###############################################################################
# Artifacts bucket + the MicroVM code artifact (Dockerfile + the static
# entrypoint binary + wait-for-docker.sh).
###############################################################################

resource "aws_s3_bucket" "artifacts" {
  bucket        = coalesce(var.artifacts_bucket_name, "${var.name_prefix}-artifacts-${local.account_id}")
  force_destroy = true # let `terraform destroy` remove the bucket + its build artifacts
  tags          = local.tags
}

resource "aws_s3_bucket_public_access_block" "artifacts" {
  bucket                  = aws_s3_bucket.artifacts.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "artifacts" {
  bucket = aws_s3_bucket.artifacts.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

# Zip the STAGED image build context: Dockerfile at the archive root (required
# by create-microvm-image), wait-for-docker.sh, and the fetched
# .artifacts/entrypoint binary. The context is assembled into local.context_dir
# by data.external.artifacts (dispatcher.tf) AT PLAN TIME from microvm/ — or
# var.build_context_dir — plus the var.dockerfile override, so the default and
# custom images zip through the same path. source_dir (not inline source
# blocks) because the entrypoint is a binary, which file() cannot carry.
# source_dir references the data source's RESULT (not local.context_dir
# directly) so the zip always reads after staging within the plan; both are
# plan-time reads, so output_md5 is known at plan and only moves when the
# staged content moves — image rebuilds trigger exactly on real change.
#
# With no overrides the staged dir holds exactly what microvm/ held at zip time
# (Dockerfile, wait-for-docker.sh, .artifacts/entrypoint), so the zip content —
# and hence output_md5 — only moves when that file set/content changes, not
# because of the staging indirection itself.
data "archive_file" "microvm_code" {
  type        = "zip"
  source_dir  = data.external.artifacts.result.context_dir
  output_path = "${path.module}/.terraform-build/microvm-code.zip"
  excludes    = [".DS_Store", "**/.DS_Store"]
}

resource "aws_s3_object" "microvm_code" {
  bucket = aws_s3_bucket.artifacts.id
  key    = "microvm-images/${var.name_prefix}-runner/code-artifact-${data.archive_file.microvm_code.output_md5}.zip"
  # SPLIT PLAN/APPLY CONTRACT: archive_file writes this zip during PLAN, and
  # `source` (like filebase64 — both re-evaluate at apply) needs it present in
  # the APPLY workspace too; a saved plan does NOT re-run the plan-time fetch.
  # Pipelines that plan and apply in separate jobs must ship .terraform-build/
  # (the fetched artifacts + the staged image build context + this zip)
  # alongside the plan artifact (e.g. via an upload-artifact step between the
  # plan and apply jobs). GitHub gotcha: actions/upload-artifact drops
  # dot-paths by default (v4.4+) and this dir lives under .terraform/ — set
  # include-hidden-files: true or the artifact ships without it.
  source = data.archive_file.microvm_code.output_path
  etag   = data.archive_file.microvm_code.output_md5
  tags   = local.tags
}
