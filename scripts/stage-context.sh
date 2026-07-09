#!/usr/bin/env bash
# Assemble the MicroVM image build context into a staging directory, which
# data.archive_file.microvm_code (s3.tf) zips into the S3 code artifact.
# Staging (instead of zipping microvm/ directly) is what lets consumers swap in
# their own Dockerfile / extra context files without forking the module.
#
# Invoked by Terraform (terraform_data.artifacts local-exec), after
# fetch-artifacts.sh has downloaded + verified the entrypoint binary.
#
# Usage: stage-context.sh <builtin-dir> <staged-dir> <entrypoint-bin> [<context-dir>]
#   builtin-dir     the module's microvm/ directory (Dockerfile + wait-for-docker.sh)
#   staged-dir      output directory; recreated from scratch on every run
#   entrypoint-bin  the fetched supervisor binary (verified release artifact)
#   context-dir     optional consumer-supplied build context (var.build_context_dir);
#                   empty/omitted = use <builtin-dir> as the base
#
# Environment:
#   DOCKERFILE_OVERRIDE  raw Dockerfile text (var.dockerfile); empty = built-in.
#                        Passed via the environment, not argv, so arbitrary
#                        Dockerfile text can't break shell quoting.
#
# Layering (later steps overwrite earlier ones):
#   1. base:  <builtin-dir> contents, or <context-dir> contents when set
#   2. <builtin-dir>/wait-for-docker.sh - ALWAYS staged: the built-in
#      Dockerfile and the runner's job-started hook depend on it
#   3. .artifacts/entrypoint - the supervisor binary every image must run
#   4. Dockerfile - DOCKERFILE_OVERRIDE when non-empty, else the built-in.
#      The built-in deliberately overwrites any Dockerfile inside
#      <context-dir>: the only sanctioned override path is var.dockerfile,
#      which carries the /entrypoint supervisor-wiring validation.
#
# With no overrides the result is exactly what microvm/ used to hold at zip
# time - Dockerfile, wait-for-docker.sh, .artifacts/entrypoint - so the
# default code artifact is unchanged by the staging indirection.
set -euo pipefail

BUILTIN="${1:?usage: stage-context.sh <builtin-dir> <staged-dir> <entrypoint-bin> [<context-dir>]}"
STAGED="${2:?usage: stage-context.sh <builtin-dir> <staged-dir> <entrypoint-bin> [<context-dir>]}"
ENTRYPOINT="${3:?usage: stage-context.sh <builtin-dir> <staged-dir> <entrypoint-bin> [<context-dir>]}"
CONTEXT_DIR="${4:-}"

[ -d "${BUILTIN}" ] || { echo "[stage-context] ERROR: builtin dir '${BUILTIN}' not found" >&2; exit 1; }
[ -f "${ENTRYPOINT}" ] || { echo "[stage-context] ERROR: entrypoint binary '${ENTRYPOINT}' not found (fetch-artifacts.sh must run first)" >&2; exit 1; }
if [ -n "${CONTEXT_DIR}" ] && [ ! -d "${CONTEXT_DIR}" ]; then
  echo "[stage-context] ERROR: build_context_dir '${CONTEXT_DIR}' is not a directory (relative paths resolve against the terraform working directory)" >&2
  exit 1
fi

# Rebuild from scratch so files deleted from the source base disappear from
# the context too. -p preserves mode bits (the zip carries them into the
# server-side docker build, so consumer scripts keep their +x).
rm -rf "${STAGED}"
mkdir -p "${STAGED}"
if [ -n "${CONTEXT_DIR}" ]; then
  cp -Rp "${CONTEXT_DIR}"/. "${STAGED}"/
else
  cp -Rp "${BUILTIN}"/. "${STAGED}"/
fi

cp -p "${BUILTIN}/wait-for-docker.sh" "${STAGED}/wait-for-docker.sh"
chmod 0755 "${STAGED}/wait-for-docker.sh"

mkdir -p "${STAGED}/.artifacts"
cp -p "${ENTRYPOINT}" "${STAGED}/.artifacts/entrypoint"
chmod 0755 "${STAGED}/.artifacts/entrypoint"

if [ -n "${DOCKERFILE_OVERRIDE:-}" ]; then
  printf '%s' "${DOCKERFILE_OVERRIDE}" > "${STAGED}/Dockerfile"
  DOCKERFILE_SOURCE="var.dockerfile"
else
  cp -p "${BUILTIN}/Dockerfile" "${STAGED}/Dockerfile"
  DOCKERFILE_SOURCE="built-in"
fi

echo "[stage-context] done: ${STAGED} (base: ${CONTEXT_DIR:-${BUILTIN}}, dockerfile: ${DOCKERFILE_SOURCE})"
