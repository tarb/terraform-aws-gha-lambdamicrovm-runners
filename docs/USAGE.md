# Deploy with Terraform

This repository is a Terraform module. A single `terraform apply`:

1. Zips the MicroVM code artifact (`microvm/Dockerfile` + the static `entrypoint` supervisor binary) to S3 and **builds the MicroVM image** (create-or-update, then polls to ACTIVE).
2. Creates the **IAM roles** (build, exec, dispatcher), the **SSM Parameter Store** SecureString (webhook secret + GitHub credential - free vs a Secrets Manager secret), and CloudWatch log groups.
3. Deploys the **dispatcher Lambda** (Rust, `provided.al2023`, arm64) behind a public **Lambda Function URL**.
4. Optionally **wires the GitHub `workflow_job` webhook(s)** via the github provider.

The Lambda zips and the supervisor binary are **prebuilt release artifacts** (built
by `.github/workflows/release.yml` on tag push), pinned by the module's
`artifact_version` variable and fetched + SHA-256-verified at plan/apply time by
`scripts/fetch-artifacts.sh` - nothing is compiled on your machine.

---

## Prerequisites

**On the machine that runs `terraform apply`** (the MicroVM image is a native Cloud Control resource; the Lambda zips and supervisor binary are prebuilt release artifacts fetched by `scripts/fetch-artifacts.sh`):

| Tool | Why |
|---|---|
| **Terraform ≥ 1.9** | Cross-variable validation (`installation_id` required when `manage_webhooks = true`). |
| **AWS credentials** for the target account | Point both the `aws` and `awscc` providers at a region where Lambda MicroVMs are available. |
| **`bash` + `curl`** | `scripts/fetch-artifacts.sh` downloads the release artifacts pinned by `artifact_version` and verifies their SHA-256 checksums at plan/apply time. |
| A **GitHub App** | Created once - Terraform cannot create a GitHub App (GitHub has no API for it; only a browser manifest flow). |

The artifacts (dispatcher zip, webhook-proxy zip, supervisor binary) are built for
arm64 by `.github/workflows/release.yml` on tag push, so the host's own toolchain
doesn't matter - no Rust or zip tooling is needed locally.

---

## You need a GitHub App - create it first

The dispatcher's only credential is a **GitHub App**: a machine identity (not a
user, not a PAT) you create once and *install* on your org/repos. The module needs
three values from it - `app_id`, `private_key`, `installation_id` - so make the App
before you write any Terraform.

**Create it:**

1. GitHub → your org (or account) → **Settings → Developer settings → GitHub Apps → New GitHub App**.
2. **Name** it (e.g. `microvm-runners`). Homepage URL can be anything.
3. **Webhook → uncheck "Active".** The App's *own* webhook is not used - the runner
   webhook is wired separately (see [Ingress](#ingress) / `manage_webhooks`).
4. **Permissions:**
   - For **per-repo** runners → Repository → **Administration: Read & write**
     (this registers runners *and* lets Terraform create the repo webhook).
   - For **org-wide** runners → Organization → **Self-hosted runners: Read & write**
     (add Organization → **Webhooks: Read & write** if `manage_webhooks = true`).
   - *Metadata: Read* is selected automatically.
5. **Create the App.** Note the **App ID** on its page, then click
   **Generate a private key** → save the downloaded `.pem`.
6. **Install App** (left sidebar) onto your org / the target repos. The URL after
   install ends in `…/installations/<number>` - that number is the **installation ID**.

**Which of the three values you actually need:**

| Mode | Needs |
|---|---|
| `manage_webhooks = false` (you wire the webhook by hand) | `app_id` + `private_key` (`installation_id` optional - the dispatcher derives it per repo) |
| `manage_webhooks = true` (Terraform wires the webhook) | `app_id` + `private_key` + `installation_id` (the github provider's `app_auth` needs it) |

Keep the `.pem` out of git - pass it via `TF_VAR_app_private_key` or
`private_key = file("app.pem")`.

---

## Quick start (GitHub App + managed webhooks)

> Plug the three values from the App above into `app_id` / `installation_id` /
> `private_key` below.

```hcl
terraform {
  required_version = ">= 1.9.0"
  required_providers {
    aws    = { source = "hashicorp/aws", version = ">= 6.0, < 7.0" }
    awscc  = { source = "hashicorp/awscc", version = ">= 1.0" }
    github = { source = "integrations/github", version = ">= 6.2" }
  }
}

provider "aws" {
  region = "us-east-1"
}

provider "awscc" {
  region = "us-east-1" # same region as aws; the module builds the image via awscc
}

# Required only because manage_webhooks = true (same App identity as the module).
provider "github" {
  owner = "my-org"
  app_auth {
    id              = "123456"
    installation_id = "12345678"
    pem_file        = file("app.private-key.pem")
  }
}

module "gha_runner" {
  source = "git::https://github.com/tarb/terraform-aws-gha-lambdamicrovm-runners.git?ref=v0.0.1" # or a local path

  name_prefix         = "gha-microvm"
  github_organization = "my-org"
  github_repositories = ["my-org/api", "my-org/web"] # or [] for an org-level webhook

  github_app = {
    app_id          = "123456"
    installation_id = "12345678"
    private_key     = file("app.private-key.pem")
  }

  manage_webhooks = true
}

output "webhook_payload_url" { value = module.gha_runner.webhook_payload_url }
```

```bash
terraform init
terraform apply        # builds the image (a few minutes on first apply), deploys, wires webhooks
```

Then run a workflow with `runs-on: [self-hosted, linux, arm64, microvm]`. A runnable copy lives in [`examples/github-app`](../examples/github-app).

---

## Auth: GitHub App

`github_app = { app_id, installation_id, private_key }` is **required** - see
[You need a GitHub App](#you-need-a-github-app---create-it-first) above for how to
obtain the three values. It's the dispatcher's only credential (a machine identity
with fine-grained, ~1h auto-rotating tokens); when `manage_webhooks = true` the
github provider reuses the same App identity to create the webhook.

---

## Ingress

GitHub reaches the dispatcher through a **public Lambda Function URL** (`authorization_type = NONE`). The aws provider auto-adds the public invoke permission, so there's nothing else to configure. The endpoint is internet-facing; request authenticity relies entirely on the **`X-Hub-Signature-256` HMAC** check the dispatcher runs against the webhook secret.

---

## Scope: repositories and/or organization

- `github_repositories = ["owner/name", ...]` → one `workflow_job` webhook **per repo**. All entries must belong to the github provider's `owner` (validated).
- `github_repositories = []` + `github_organization = "my-org"` → a **single org-level** webhook covering every repo.

---

## Wiring webhooks yourself (`manage_webhooks = false`)

If you'd rather not give Terraform GitHub access, set `manage_webhooks = false`. Then **omit the github provider** entirely and wire the webhook by hand:

```bash
terraform output webhook_payload_url
terraform output -raw webhook_secret   # -raw: the value is marked sensitive
```

In GitHub (repo or org **Settings → Webhooks → Add webhook**):

- **Payload URL**: the `webhook_payload_url` output
- **Content type**: `application/json` *(required - the HMAC is computed over the raw JSON bytes)*
- **Secret**: the `webhook_secret` output
- **Events**: *Let me select individual events* → **Workflow jobs** only

---

## Inputs (most-used)

| Variable | Default | Purpose |
|---|---|---|
| `name_prefix` | `"gha-microvm"` | Prefix for all resource names. |
| `github_app` | _(required)_ | `{ app_id, installation_id?, private_key }` (sensitive). |
| `github_organization` | `null` | Owner of repos/webhooks; with empty `github_repositories`, selects an org webhook. |
| `github_repositories` | `[]` | `["owner/name", ...]` for per-repo webhooks. |
| `manage_webhooks` | `true` | Create the webhooks via the github provider. |
| `runner_memory_mib` | `8192` | MicroVM memory. 8192 suits Docker builds; lower to cut cost. |
| `max_duration_seconds` | `1200` | Hard per-job cap (auto-terminate backstop). |
| `additional_os_capabilities` | `["ALL"]` | `["ALL"]` enables nested Docker; `[]` to tighten. |
| `required_labels` | `["self-hosted","microvm"]` | Labels a job must carry to trigger a MicroVM. |
| `runner_labels` | `["self-hosted","linux","arm64","microvm"]` | Labels the runner registers with. |
| `log_retention_days` | `14` | CloudWatch retention. |
| `github_api_url` | `https://api.github.com` | Set for GitHub Enterprise Server. |

See [`variables.tf`](../variables.tf) for the full set (validations, dispatcher sizing, base image name, bucket override).

## Outputs

| Output | Purpose |
|---|---|
| `webhook_payload_url` | The payload URL (already wired when `manage_webhooks = true`). |
| `webhook_secret` | HMAC secret (sensitive - use `terraform output -raw`). |
| `image_arn` / `image_version` | The MicroVM image identifier + active version. |
| `exec_role_arn` | MicroVM execution role. |
| `dispatcher_function_name` | Dispatcher Lambda name. |
| `secret_param_name` / `secret_param_arn` | SSM SecureString parameter (name / ARN). |
| `artifacts_bucket` | S3 artifacts bucket name. |

---

## Custom image

The module's [`microvm/Dockerfile`](../microvm/Dockerfile) bakes in a broad CI
toolchain. To ship your own image definition instead - slimmer, different tools,
different base - use these two variables (independently or together), no fork
needed:

| Variable | Purpose |
|---|---|
| `dockerfile` | Raw Dockerfile **text** (not a path) that replaces the built-in Dockerfile. |
| `build_context_dir` | Directory whose files are added to the image build context, so a custom Dockerfile can `COPY` them. |

```hcl
module "gha_runner" {
  # ...

  dockerfile        = file("${path.root}/runner.Dockerfile")
  build_context_dir = abspath("${path.root}/runner-context") # optional extra COPY files
}
```

A custom Dockerfile has two contracts:

1. **Base image** - Lambda builds it server-side on top of the managed base
   (`base_image_name`/`base_image_version`). `FROM` the
   `public.ecr.aws/lambda/microvms` tag that matches: the default `al2023-1`
   base pairs with `FROM public.ecr.aws/lambda/microvms:al2023-minimal`
   (arm64 only - Lambda MicroVMs are Graviton).
2. **Supervisor wiring** (validated at plan time) - the module stages the static
   supervisor binary at `.artifacts/entrypoint` in the build context; the image
   must install and run it, or the MicroVM never serves its lifecycle hooks and
   the image build fails its READY probe:

   ```dockerfile
   FROM public.ecr.aws/lambda/microvms:al2023-minimal

   # ... your tooling ...

   COPY .artifacts/entrypoint /entrypoint
   RUN chmod 0755 /entrypoint
   CMD ["/entrypoint"]
   ```

Notes:

- `microvm/wait-for-docker.sh` is always staged into the build context, so a
  custom Dockerfile can keep the built-in job-started hook
  (`COPY wait-for-docker.sh /opt/actions-runner-hooks/...` +
  `ACTIONS_RUNNER_HOOK_JOB_STARTED`) that gates jobs until dockerd is up.
- A `Dockerfile` inside `build_context_dir` is ignored (overwritten): the only
  Dockerfile override path is `dockerfile`, which carries the validation.
- Changing `dockerfile` or any file under `build_context_dir` re-stages the
  context and triggers a new image-version build on the next apply.

---

## Notes

- **First apply** takes a few minutes - it waits for the MicroVM image build to reach ACTIVE before the dispatcher is configured with the version.
- **Updating runner tooling** (the `microvm/Dockerfile`, the [custom-image inputs](#custom-image), or the supervisor binary via a new `artifact_version`) changes the artifact hash, which triggers a new image-version build on the next apply; the dispatcher picks up the new version automatically.
- The Function URL endpoint is **public**; its only protection is the `X-Hub-Signature-256` HMAC check against the webhook secret.
