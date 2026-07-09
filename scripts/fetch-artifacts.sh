#!/usr/bin/env bash
# Fetch the prebuilt release artifacts (dispatcher.zip, webhook-proxy.zip,
# entrypoint) for a given GitHub release of this module and verify them against
# the release's SHA256SUMS. Releases are built by .github/workflows/release.yml.
#
# Invoked by Terraform (terraform_data.artifacts local-exec).
#
# Usage: fetch-artifacts.sh <version> <dest-dir>
#   version   release tag, e.g. v0.0.1
#   dest-dir  directory to download into (created if absent)
#
# Idempotent: when all artifacts already exist in <dest-dir> AND pass the
# checksum verification, nothing is downloaded.
set -euo pipefail

VERSION="${1:?usage: fetch-artifacts.sh <version> <dest-dir>}"
DEST="${2:?usage: fetch-artifacts.sh <version> <dest-dir>}"

BASE_URL="https://github.com/tarb/terraform-aws-gha-lambdamicrovm-runners/releases/download/${VERSION}"
ARTIFACTS="dispatcher.zip webhook-proxy.zip entrypoint"

for tool in curl; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "[fetch-artifacts] ERROR: '$tool' not found on PATH (required on the terraform apply host)" >&2
    exit 1
  }
done

mkdir -p "${DEST}"
cd "${DEST}"

# sha256sum on Linux; shasum ships with macOS where coreutils may be absent.
checksum() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$1"
  else
    shasum -a 256 -c "$1"
  fi
}

# SHA256SUMS covers every release asset; keep only the three we deploy so a
# future extra asset doesn't fail the check for a file we never downloaded.
verify() {
  grep -E ' (dispatcher\.zip|webhook-proxy\.zip|entrypoint)$' SHA256SUMS >SHA256SUMS.filtered
  checksum SHA256SUMS.filtered
}

present() {
  for f in ${ARTIFACTS} SHA256SUMS; do
    [ -f "$f" ] || return 1
  done
}

if present && verify >/dev/null 2>&1; then
  echo "[fetch-artifacts] ${VERSION} artifacts already present and verified in ${DEST} - skipping download"
else
  for f in SHA256SUMS ${ARTIFACTS}; do
    echo "[fetch-artifacts] download ${BASE_URL}/${f}"
    curl -fsSL -o "$f" "${BASE_URL}/${f}"
  done
  echo "[fetch-artifacts] verify checksums"
  verify
fi

rm -f SHA256SUMS.filtered
chmod +x entrypoint

echo "[fetch-artifacts] done: ${DEST}"
