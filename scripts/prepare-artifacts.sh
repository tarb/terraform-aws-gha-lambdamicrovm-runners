#!/usr/bin/env bash
# data.external entrypoint: fetch the release artifacts and stage the image
# build context AT PLAN TIME, so every workspace holds the files before
# anything reads them (archive_file, aws_s3_object.source, the Lambda
# filenames). This is what lets plans stay clean on fresh/ephemeral CI
# workspaces - provisioners only fire on create/replace, so the old
# terraform_data shape had to force a replace (timestamp()) whenever the
# files were absent, churning every plan.
#
# Both sub-scripts are idempotent: a verified artifact set is not
# re-downloaded, and staging is a cheap rebuild whose zip (data.archive_file
# normalizes timestamps) hashes identically for identical content.
#
# Usage (argv via the data.external program list - exec, no shell, so the
# Dockerfile text can't break quoting):
#   prepare-artifacts.sh <version> <artifact-dir> <builtin-dir> <staged-dir> \
#                        <build-context-dir> <dockerfile-override>
#
# Contract: stdout is RESERVED for the JSON result (data.external reads it);
# all sub-script progress goes to stderr.
set -euo pipefail

VERSION="${1:?usage: prepare-artifacts.sh <version> <artifact-dir> <builtin-dir> <staged-dir> <build-context-dir> <dockerfile-override>}"
ARTIFACT_DIR="${2:?}"
BUILTIN_DIR="${3:?}"
STAGED_DIR="${4:?}"
BUILD_CONTEXT_DIR="${5:-}"
DOCKERFILE="${6:-}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

{
  bash "${HERE}/fetch-artifacts.sh" "${VERSION}" "${ARTIFACT_DIR}"
  DOCKERFILE_OVERRIDE="${DOCKERFILE}" bash "${HERE}/stage-context.sh" \
    "${BUILTIN_DIR}" "${STAGED_DIR}" "${ARTIFACT_DIR}/entrypoint" "${BUILD_CONTEXT_DIR}"
} 1>&2

# Minimal JSON string escaping (backslash + double quote); paths come from
# path.module and user config, so anything fancier has already gone wrong.
json_escape() { printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'; }

printf '{"context_dir":"%s","artifact_dir":"%s"}\n' \
  "$(json_escape "${STAGED_DIR}")" "$(json_escape "${ARTIFACT_DIR}")"
