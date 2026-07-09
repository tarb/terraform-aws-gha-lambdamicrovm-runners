# Contributing

Thanks for your interest in improving this module. This is a Terraform module that
runs GitHub Actions self-hosted runners on AWS Lambda MicroVMs; the design is
documented in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). The runtime components
(dispatcher, webhook proxy, in-guest entrypoint supervisor) are Rust crates in
`crates/`, built into release artifacts by `.github/workflows/release.yml`.

## Prerequisites

- **Terraform ≥ 1.9** (`>= 1.9.0`; CI pins the exact version)
- **Rust** (stable toolchain, with `rustfmt` and `clippy`) — to build and test the
  crates in `crates/`
- **`terraform-docs`** — to regenerate the inputs/outputs table in the README

You do **not** need AWS credentials to format, validate, test, or regenerate docs.

## Local checks

Run these before opening a PR — CI runs the same ones:

```bash
terraform fmt -recursive          # format all .tf files
terraform init -backend=false     # download providers (no state/credentials)
terraform validate                # validate the root module

# validate the example too
( cd examples/github-app && terraform init -backend=false && terraform validate )

# regenerate the README inputs/outputs/resources table (reads .terraform-docs.yml)
terraform-docs .

# the Rust gate — CI fails on any of these
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

If you change any `variable`/`output`/resource, re-run `terraform-docs .` — the
`<!-- BEGIN_TF_DOCS -->` block in the README is generated, and CI fails if it drifts.

## Testing

The behavioral spec for the dispatcher (label gating, pool resume/suspend, sweep,
zombie reaper, concurrency cap, wire formats) lives in the dispatcher crate's unit
tests — run it with `cargo test --workspace`. If you change dispatcher behavior,
update or add tests there in the same PR.

Formatting and lints are a hard gate: CI (and the release build) run
`cargo fmt --all -- --check` and `cargo clippy --workspace -- -D warnings`, so
warnings are errors.

## Changing the runner image

The MicroVM image is built by Lambda from `microvm/Dockerfile` +
`microvm/wait-for-docker.sh` + the static musl `entrypoint` supervisor binary
(zipped to S3 — see
[docs/ARCHITECTURE.md §4](docs/ARCHITECTURE.md#4-how-it-works---component-by-component)).
Editing the Dockerfile or hook script — or pinning a new supervisor binary via
`artifact_version` — changes the artifact hash, so the next `terraform apply`
builds a **new image version** and the dispatcher picks it up automatically. The
first build takes a few minutes (it boots the app and snapshots it).

The supervisor itself is `crates/entrypoint`, built as a static
`aarch64-unknown-linux-musl` binary by the release workflow.

## Changing the dispatcher or webhook proxy

The dispatcher (`crates/dispatcher`) and webhook proxy (`crates/webhook-proxy`) are
Rust Lambdas on the `provided.al2023` arm64 runtime. They are **not built at
`terraform apply` time**: `.github/workflows/release.yml` builds the zips (via
`cargo lambda`) and the static entrypoint binary on tag push and publishes them
(with a `SHA256SUMS` file) as a GitHub release. The module's `artifact_version`
variable pins which release is deployed; `scripts/fetch-artifacts.sh` downloads and
checksum-verifies the artifacts at plan/apply time.

So the ship loop is: change the crate → `cargo test --workspace` → merge → tag a
release → bump `artifact_version` where the module is consumed.

## Pull requests

- Keep changes focused; update the docs (`README.md`, `docs/USAGE.md`,
  `docs/ARCHITECTURE.md`) when behavior or inputs change.
- Add a line under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md).
- Never commit secrets, `*.tfstate`, `*.tfvars` (except `*.tfvars.example`), or the
  GitHub App private key. `.gitignore` already excludes them.
- Write clear commit messages that explain *why*, not just *what*.

## License

By contributing you agree that your contributions are licensed under the
[MIT License](LICENSE).
