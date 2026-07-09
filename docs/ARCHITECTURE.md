# GitHub Actions self-hosted runners on AWS Lambda MicroVMs

Run **ephemeral, auto-scaling GitHub Actions self-hosted runners inside AWS Lambda
MicroVMs** - Firecracker-isolated, snapshot-resumable, Graviton/arm64 compute.
A webhook-driven dispatcher launches exactly one MicroVM per queued CI job; the
MicroVM boots from a prebuilt snapshot, registers a single-use runner with GitHub,
runs the job (Docker-in-runner supported), and **terminates itself the instant the
job finishes** so you never pay for idle time.

| | |
|---|---|
| **Account / region** | your AWS account · a MicroVM-enabled region (e.g. `eu-west-1`) |
| **Architecture** | arm64 (Graviton) - MicroVMs are Graviton-only |
| **Runner** | [`actions/runner` v2.335.1](https://github.com/actions/runner) (linux-arm64) |
| **Image** | a MicroVM image the module builds from `microvm/Dockerfile` + the static `entrypoint` supervisor binary (arm64) |
| **Base image** | `public.ecr.aws/lambda/microvms:al2023-minimal` |
| **Auth** | GitHub App |
| **Ingress** | webhook-proxy Function URL → EventBridge bus → SQS → dispatcher Lambda (legacy: direct dispatcher Function URL) |
| **Deploy** | **[Terraform module](USAGE.md)** - one `terraform apply` |

> 🚀 **Just want to deploy it?** See **[USAGE.md](USAGE.md)** - one `terraform apply` builds the image, deploys the webhook-proxy → EventBridge → SQS → dispatcher ingress, and wires the GitHub webhooks. The sections below explain how the system works.

---

## Table of contents

1. [What this is](#1-what-this-is)
2. [Why Lambda MicroVMs](#2-why-lambda-microvms)
3. [Architecture](#3-architecture)
4. [How it works - component by component](#4-how-it-works---component-by-component)
5. [Key mechanisms (the tricky bits)](#5-key-mechanisms-the-tricky-bits)
6. [Pros](#6-pros)
7. [Cons & limitations](#7-cons--limitations)
8. [Cost model](#8-cost-model)
9. [Repository layout](#9-repository-layout)
10. [Setup / deploy](#10-setup--deploy)
11. [Operations / runbook](#11-operations--runbook)
12. [Troubleshooting](#12-troubleshooting)
13. [Security considerations](#13-security-considerations)
14. [References](#14-references)

---

## 1. What this is

A self-hosted GitHub Actions **runner fleet** with no always-on infrastructure.
Instead of EC2/ECS runners sitting idle waiting for jobs, this design uses
[**AWS Lambda MicroVMs**](https://docs.aws.amazon.com/lambda/latest/dg/lambda-microvms-guide.html):
a per-job, strongly-isolated micro-VM that exists only for the lifetime of one CI
job.

The end-to-end loop:

1. A CI job with matching `runs-on` labels enters the **`queued`** state.
2. GitHub fires a `workflow_job` **webhook** at the webhook-proxy's public Lambda
   Function URL; the proxy verifies it and puts it on the EventBridge bus, from
   which it reaches the dispatcher through the SQS jobs queue.
3. The dispatcher mints a short-lived GitHub credential and calls **`RunMicrovm`**.
4. The MicroVM resumes from a snapshot in ~seconds, registers a **single-use
   (ephemeral)** runner, and GitHub hands it the job.
5. The job runs (it can build images, run `docker compose`, start service
   containers, call AWS, etc.).
6. When the job finishes the runner exits and the MicroVM **terminates itself** -
   billing stops immediately.

---

## 2. Why Lambda MicroVMs

|  | GitHub-hosted | Classic Lambda function | EC2 / ECS runner | **Lambda MicroVM (this)** |
|---|---|---|---|---|
| Per-job isolation | ✅ | ✅ | ⚠️ (shared host) | ✅ (Firecracker) |
| Idle cost | n/a | none | **high** (always-on) | **none** (per-job) |
| Max job length | 6 h | **15 min** | unbounded | **8 h** |
| Docker / nested containers | ✅ | ❌ | ✅ | ✅ (with `["ALL"]` caps) |
| Writable rootfs | ✅ | `/tmp` only | ✅ | ✅ |
| Cold start | n/a | ms | minutes (boot+join) | **~seconds** (snapshot resume) |
| Suspend/resume w/ state | ❌ | ❌ | stop/start (slow) | ✅ |
| Private network egress | ❌ | VPC | VPC | VPC or internet connector |
| Scale to zero | ✅ | ✅ | ❌ (or slow) | ✅ |

**The sweet spot:** you want GitHub-hosted-like "scale to zero, pay per job,
fresh environment every time" **plus** Docker, longer jobs, your own AWS account,
and VPC/network control - without managing a runner autoscaler on EC2/ECS.

Classic Lambda *functions* can't do it: 15-minute cap, no Docker daemon, read-only
rootfs. MicroVMs lift all three limits while keeping the serverless economics.

---

## 3. Architecture

```
                          GitHub (cloud)
   repo/org ── workflow_job webhook ─────────┐         ▲ runner long-polls for its job
                                             │         │ (outbound HTTPS)
                                             ▼         │
   ┌───────────────────────── AWS account 111122223333 / <region> ─────────────────────────┐
   │                                                                                          │
   │   webhook-proxy Function URL (public, authType NONE)                                     │
   │            │  POST /  (webhook)                                                          │
   │            ▼                                                                             │
   │   webhook-proxy (Lambda) ── verify HMAC sig → PutEvents                                  │
   │            │                                                                             │
   │            ▼                                                                             │
   │   EventBridge bus (14-day archive)                                                       │
   │            │  rules: workflow_job queued/completed, label-filtered                       │
   │            ▼                                                                             │
   │   SQS jobs queue (visibility 360 s, DLQ after ~24 h)                                     │
   │            │  Lambda event source mapping (batch 1)                                      │
   │            ▼                                                                             │
   │   ┌──────────────────────┐   legacy: direct Function URL (HMAC verified here)            │
   │   │  Dispatcher (Lambda)  │── GitHub App JWT → installation token (cached) ──▶ GitHub    │
   │   │  gha-microvm-         │                                                              │
   │   │  dispatcher (rs, arm) │── RunMicrovm(image,    exec role, egress connector,          │
   │   └──────────────────────┘                 maxDuration, runHookPayload={url,token,…}) ─┐ │
   │            │ reads                                                                     │ │
   │            ▼                                                                           ▼ │
   │   SSM Param Store                                              ┌──────────────────────────┐
   │   /gha-microvm/dispatcher                                      │   MicroVM (Firecracker)   │
   │   {webhook_secret, app_id, app_private_key}                    │  resumed from snapshot    │
   │                                                                │                           │
   │   MicrovmImage  gha-microvm-runner       ───── snapshot ────▶ │  entrypoint :9000         │
   │   (built from zip(Dockerfile+entrypoint) + al2023-minimal)     │   ├─ /run  → register     │
   │                                                                │   │   ephemeral runner    │
   │   IAM: gha-microvm-exec-role (logs + TerminateMicrovm)         │   ├─ dockerd (per job)    │
   │        gha-microvm-build-role (S3 + logs)                      │   ├─ runner runs 1 job    │
   │        gha-microvm-dispatcher-role (RunMicrovm/Pass*/secret)   │   └─ TerminateMicrovm self│
   │                                                                └───────────┬───────────────┘
   │   INTERNET_EGRESS network connector ◀──────────── outbound ────────────────┘
   │   CloudWatch  /aws/lambda-microvms/gha-microvm-runner  (runner + dockerd logs)            │
   └──────────────────────────────────────────────────────────────────────────────────────────┘
```

### Two-resource model

Lambda MicroVMs split into two resources (mirrored in the AWS API):

- **`MicrovmImage`** - a *versioned* artifact built once from
  `zip(Dockerfile + the entrypoint binary) + a managed base image`. Building it boots your
  app, calls `/ready`, and **snapshots disk + memory**. Each version has a
  per-architecture build (here: ARM_64). This is the slow part (minutes), done at
  release time.
- **`Microvm`** - a *running instance* created from an image version with
  `RunMicrovm`. It **resumes from the snapshot** (seconds, init already done),
  attaches an execution role + network connectors, and receives a per-run
  `runHookPayload`.

### Request flow (one CI job)

```
queued webhook → webhook-proxy verifies HMAC → PutEvents onto the bus
   → rule matches → SQS jobs queue → dispatcher (event source mapping)
   → mints installation token
   → RunMicrovm(ephemeral payload) → MicroVM resumes → POST /run
   → entrypoint registers an --ephemeral runner with GitHub
   → GitHub assigns the job → runner runs it → runner process exits
   → entrypoint calls TerminateMicrovm on its own id → billing stops
   (maximumDurationInSeconds is a hard backstop if anything above fails)
```

---

## 4. How it works - component by component

### 4.1 The MicroVM image (`microvm/Dockerfile`)

Built **by Lambda** from a zip in S3 (not a normal `docker build` you push to ECR).
`FROM public.ecr.aws/lambda/microvms:al2023-minimal` (arm64), it installs:

- **Runner deps** - `libicu` (mandatory for the .NET runner or it aborts with
  "Couldn't find a valid ICU package"), plus `git tar gzip xz unzip jq` etc. The
  base is *minimal* AL2023, so `curl-minimal`/`coreutils-single`/`openssl-snapsafe`
  are already present - requesting the full packages causes conflicts.
- **The runner binaries** - `actions/runner` v2.335.1 linux-arm64, downloaded and
  **SHA-256 verified**. `config.sh` is **not** run at build time (see
  [snapshot uniqueness](#51-snapshot-uniqueness)).
- **Docker engine + iptables** - for `docker`, `container:`, and `services:` jobs.
  Nested containers only work because the image is created with
  `additionalOsCapabilities: ["ALL"]`.
- **Docker Compose v2 plugin** (`v5.2.0`, the `docker-compose-linux-aarch64`
  binary) - not packaged on AL2023, so it's baked into `/usr/libexec/docker/cli-plugins/`.
- **AWS CLI v2 (arm64)** - so deploy/infra jobs (`aws cloudformation`, `aws lambda`,
  `sam`) work like GitHub-hosted runners. (The supervisor itself calls the AWS API
  directly - it does not shell out to the CLI.)
- **python3** - stays in the image for workflow jobs that need it; the lifecycle-hook
  supervisor itself is a static binary with no interpreter dependency.

`CMD ["/entrypoint"]`. Key env: `RUNNER_LABELS=self-hosted,linux,arm64,microvm`, `ENABLE_DOCKER=true`, `DOCKER_STORAGE_DRIVER=overlay2`.

### 4.2 The entrypoint supervisor (`crates/entrypoint`)

A **static musl Rust binary** (dependency-free at runtime, baked into the image)
serving HTTP on **`:9000`**; it is both the
**lifecycle-hook handler** and the **runner supervisor**. Hooks are
`POST /aws/lambda-microvms/runtime/v1/<hook>`:

| Hook | Phase | What it does |
|---|---|---|
| `/ready` | build | 200 once runner binaries exist. **Never registers** - the snapshot is shared. `dockerd` is intentionally *not* started here. |
| `/validate` | build | 200 (lets Lambda sample & prefetch snapshot pages for faster resume). |
| `/run` | runtime | Unwraps `runHookPayload`, starts the runner in a background thread, returns 200 fast (the hook has a 10 s budget). |
| `/resume` `/suspend` | runtime | 200 (no-op; runner reconnects after resume). |
| `/terminate` | runtime | Best-effort de-register (persistent mode) + stop. |

The `/run` handler supports **two modes**, chosen by the payload:

- **JIT / ephemeral** (`{"encoded_jit_config": "<base64>"}`) - runs the runner with
  `--jitconfig`; always exactly one job.
- **Token** (`{"github_url", "token", "ephemeral": true, "labels"}`) - mints a
  registration token from `token` (the App installation token), runs
  `config.sh --ephemeral`, then `run.sh`. **This is what the dispatcher uses**: the
  JIT blob can't ride in `runHookPayload` (the service caps it at 4096 bytes,
  despite the doc string saying 16 KB), so we pass a small token and register on-box.

When the (ephemeral) runner process exits, the supervisor reports its
idleness to the dispatcher (when the `/run` payload named it via
`dispatcher_fn`), falling back to `terminate_self()` — `TerminateMicrovm` on
the VM's **own** `microvmId` (delivered in the `/run` body) - see
[cost / self-terminate](#53-cost-model--self-termination).

`dockerd` is started **fresh per job** (not baked into the snapshot) - see
[Docker + DNS](#52-docker-in-runner--the-dns-fix). `start_runner()` warms it up in a
**background thread** so it overlaps runner registration + the GitHub job-assignment
handshake instead of blocking before them. A docker step still can't race the
daemon: the runner's **job-started hook** (`ACTIONS_RUNNER_HOOK_JOB_STARTED` ->
`wait-for-docker.sh`) runs after the job is assigned but before its first step, and
blocks until `docker info` succeeds (best-effort, capped by `DOCKER_WAIT_TIMEOUT`).

### 4.3 The dispatcher (`crates/dispatcher`)

A small Lambda **function** (Rust on `provided.al2023`, arm64, 256 MB). It is the
autoscaler. On the primary path it is invoked by the **SQS jobs queue** via an
event source mapping (batch 1), consuming `workflow_job` events the webhook-proxy
has already HMAC-verified and `PutEvents`-ed onto the EventBridge bus; a failing
dispatch raises, returning the message to the queue for retry (DLQ after ~24 h).
The dispatcher's own public Lambda Function URL (authType NONE) still exists and
works for deployments that haven't moved their webhook to the proxy URL
(the `events_webhook_url` output vs the legacy `webhook_payload_url`) - on that
direct path delivery is single-attempt (no queue) and the dispatcher **verifies
the webhook** itself: `X-Hub-Signature-256` HMAC against `webhook_secret`,
rejecting anything else (401). For each event it:

1. **Filters** - ignores non-`workflow_job` events and any `action` other than
   `queued`.
2. **Label-gates** - only dispatches if the job's labels are a superset of
   `REQUIRED_LABELS` (default `self-hosted,microvm`).
3. **Mints a GitHub App token** (`token_for_repo`): sign an RS256 JWT with the app
   private key (`iss` = numeric App ID as a *string*; `exp` < 10 min),
   exchange it for a repo-scoped **installation access token** (~1 h, cached 60 s
   per container), which can register runners.
4. **Launches the MicroVM** - `RunMicrovm(imageIdentifier, imageVersion,
   executionRoleArn, egressNetworkConnectors=[INTERNET_EGRESS],
   maximumDurationInSeconds, runHookPayload={github_url, token, ephemeral:true,
   labels})`. Retries a transient `AccessDeniedException` (IAM PassRole propagation).

It never logs the private key or any token. Secret reads are cached 60 s so config
changes propagate to warm containers without a redeploy.

### 4.4 IAM roles

Role names below are the defaults for `name_prefix = "gha-microvm"`; each role is
`<name_prefix>-build-role` / `-exec-role` / `-dispatcher-role`.

| Role | Trust | Permissions |
|---|---|---|
| **`gha-microvm-build-role`** | `lambda.amazonaws.com` (+ `aws:SourceAccount` condition) | S3 read artifact / write build output on the artifacts bucket; CloudWatch logs. Used **during image build**. |
| **`gha-microvm-exec-role`** | same | CloudWatch logs **+ `lambda:InvokeFunction` on the dispatcher** (idle reports) **+ `lambda:TerminateMicrovm`** (self-terminate fallback). Assumed **at runtime** by the MicroVM. |
| **`gha-microvm-dispatcher-role`** | `lambda.amazonaws.com` | `lambda:RunMicrovm/TerminateMicrovm/GetMicrovm/ListMicrovms`; `lambda:PassNetworkConnector` (the egress connector); `iam:PassRole` (the exec role only); `ssm:GetParameter` + `kms:Decrypt` (the dispatcher parameter). |

Least-privilege notes: the exec role's `TerminateMicrovm` is `Resource:"*"` (IAM
can't scope "terminate only myself"; the backstop is the max-duration cap). The
dispatcher's `iam:PassRole` is scoped to the exec role ARN only.

### 4.5 Networking

- **Egress** - every MicroVM is launched with the managed **`INTERNET_EGRESS`**
  network connector so the runner can long-poll GitHub and pull images. Swap for a
  `VPC_EGRESS` connector if you need private networking / a fixed egress IP (then
  the control-plane self-terminate call needs a NAT or an `lambda-microvms`
  interface VPC endpoint).
- **Ingress** - the runner needs **none** (it dials out to GitHub). GitHub POSTs
  webhooks to the *webhook-proxy*'s public **Lambda Function URL** (authType NONE),
  which gives GitHub a plain public `https://…/` to POST to; events then reach the
  dispatcher via EventBridge → SQS (the dispatcher's legacy direct Function URL
  also remains public). The aws provider auto-adds the public
  `lambda:InvokeFunctionUrl` permission; authenticity rests on the HMAC check.
- **DNS** - see [§5.2](#52-docker-in-runner--the-dns-fix); the short version is
  outbound UDP to public resolvers is blocked, so containers must use the Amazon
  link-local resolver `169.254.169.253`.

### 4.6 Authentication (GitHub App)

The dispatcher authenticates as a **GitHub App** - a machine identity not tied to
any person, scoped to the repos you install it on, minting short-lived tokens.

- App needs **Repository permission "Administration: Read & write"** (to register
  repo runners) *or* **Organization permission "Self-hosted runners: Read & write"**
  (for org runners).
- The webhook subscribes to **"Workflow jobs"** and points at the webhook-proxy's
  Function URL (the `events_webhook_url` output; the legacy `webhook_payload_url`
  direct-to-dispatcher URL still works), with the same `webhook_secret`.
- The secret holds `{webhook_secret, app_id, app_private_key}`. The dispatcher
  derives the installation from the webhook (or looks it up by repo).

---

## 5. Key mechanisms (the tricky bits)

### 5.1 Snapshot uniqueness

Every MicroVM resumes from **the same** disk+memory snapshot. If you ran
`config.sh` (runner registration) or generated secrets/SSH keys at **build** time,
every VM would share one runner identity / one RNG state. So:

- **Register at `/run`, never at build.** Per-VM config arrives in `runHookPayload`.
- Runner **names** are derived from the per-VM `microvmId` (`gha-mvm-<id-prefix>`),
  not a baked-in constant.
- The managed base image re-seeds OpenSSL entropy on resume; if you add your own
  CSPRNG-dependent code, reseed it on `/resume`.

### 5.2 Docker-in-runner & the DNS fix

Three non-obvious problems, all solved in the entrypoint supervisor (`crates/entrypoint`):

1. **Stale daemon networking.** If `dockerd` were started at boot it would be in the
   snapshot, and on resume its bridge/NAT/DNS would be stale → nested containers
   can't reach anything. **Fix:** start `dockerd` **fresh per job** in
   `start_runner()` (not at `/ready`, not in the snapshot), trying `overlay2` then
   falling back to `vfs`.
2. **DNS resolution fails inside containers.** MicroVMs **block outbound UDP to
   public resolvers** and ship a local DNS *stub* that container/build network
   namespaces can't reach. Docker's default fallback to `8.8.8.8` therefore fails
   ("Temporary failure resolving deb.debian.org") - and a nested `docker build` /
   `apt-get` hangs for minutes then errors. **Fix:** run
   `dockerd --dns 169.254.169.253` (the **Amazon link-local resolver**), which the
   daemon also propagates into BuildKit build sandboxes. *(Credit:
   [willpeixoto.dev](https://willpeixoto.dev/aws-lambda-microvms-untrusted-code-isolation).)*
3. **fd starvation inside containers.** The guest gives the entrypoint supervisor a **hard
   `nofile` of 1024** (no systemd unit to set `LimitNOFILE`); `dockerd` inherits
   it, and containers/BuildKit `RUN` steps inherit the daemon's - so fd-heavy
   builds hit `EMFILE` (e.g. JS bundlers doing parallel asset compression), while
   GitHub-hosted runners give containers 65536+. **Fix:** the supervisor raises
   its own `RLIMIT_NOFILE` to `65536:1048576` before spawning anything (root may
   raise the hard limit up to `fs.nr_open`), and passes
   `--default-ulimit nofile=...` (clamped to the achieved hard limit) so plain
   containers get an explicit sane default too.

> **Gotcha for workloads:** `docker/build-push-action` spins up its own
> *buildx-container* builder (a separate network namespace), which re-introduces
> the DNS problem. Prefer plain `docker build` + `docker push` (which use the
> daemon's BuildKit, so they inherit the `--dns` setting). `docker compose --build`
> needs a newer buildx than the image ships - classic `docker build` avoids that too.

### 5.3 Cost model & self-termination

`vCPU-Second-ARM` is billed **continuously while a MicroVM is `RUNNING`, even when
idle**. The lifecycle states:

| State | Compute charge | Other charge |
|---|---|---|
| `RUNNING` | **yes** (vCPU + memory GB-s) | - |
| `SUSPENDED` | no | snapshot **storage** (~$0.02/GB write, read on resume) |
| `TERMINATED` | **none** | none |

For a one-shot ephemeral runner you want **`TERMINATED`**, fast. The naïve setup
(no teardown) leaves each VM `RUNNING` until the `maximumDurationInSeconds` cap -
e.g. a 3-minute job billed for a 20-minute idle tail. At e.g. ~50 jobs/day that is
**~86 vCPU-hours/day** of pure waste.

**Why not an `idlePolicy`?** Idle is measured by **ingress** traffic, and a runner
has none (it only polls GitHub *outbound*) - an `idlePolicy` would suspend it
**mid-job**. Wrong tool.

**The fix — report, then fall back:** when the ephemeral runner process exits
(and post-job cleanup is done), the entrypoint supervisor first **reports its
idleness to the dispatcher** with a direct Lambda invoke
(`{"idle": {"microvmId": "...", "reason": "job-complete"|"orphan"}}`,
RequestResponse, 2 attempts); the dispatcher then suspends the VM into the
warm pool (room permitting) or terminates it **from the control plane**.
Only when reporting is impossible (no `dispatcher_fn` in the run payload —
an old dispatcher) or fails outright does the VM fall back to the in-VM
`terminate_self()` (`TerminateMicrovm` on its own `microvmId`), which itself
falls back to the sweep reaper / max-duration backstop.

**Why the indirection?** In a VPC whose Lambda API traffic rides a
**PrivateLink interface endpoint**, in-VM `TerminateMicrovm` **always
fails** — the MicroVMs sub-API rejects PrivateLink with
`AccessDeniedException` "PrivateLink is not yet supported". A standard
Lambda `Invoke` works fine over PrivateLink, so the VM asks the dispatcher
to act where everything works. (Self-*terminate* is otherwise allowed - only
self-*suspend* isn't, because suspend must snapshot the still-running
caller.) Result: VM lifetime drops from the max-duration cap to **≈ the real
job length** — for short jobs an **~85% cut** in billed runtime.
`maximumDurationInSeconds` stays as a hard backstop in case every layer of
the chain fails.

### 5.4 Ephemeral vs persistent runners

- **Ephemeral** (one job, auto-deregistered) - what the dispatcher uses. Clean,
  secure (no reused state), pairs with self-terminate. Slight per-job resume cost.
- **Persistent** mode - one VM serves many jobs for up
  to 8 h. Lower per-job overhead, but **must run with no `idlePolicy`** or it gets
  suspended for lack of ingress, and it doesn't self-terminate (you stop it, or the
  max-duration cap does).

---

## 6. Pros

- **Scale to zero, pay per job.** No idle runner fleet; ~seconds to a warm runner
  via snapshot resume.
- **Strong per-job isolation** (Firecracker micro-VM) - good for untrusted /
  multi-tenant CI.
- **Real Docker** - `docker build`, `docker compose`, `container:` jobs, and
  `services:` all work (`["ALL"]` caps + per-job `dockerd`).
- **Up to 8-hour jobs** and a **writable rootfs** (unlike Lambda functions).
- **Your account, your network** - VPC egress, your AWS creds via the exec role,
  CloudWatch logs.
- **Machine-identity auth** - GitHub App, short-lived repo-scoped tokens, nothing
  tied to a person; secrets never logged.
- **Fast, cheap teardown** - self-terminate stops billing the moment a job ends.
- **Reproducible locally** - the image is just a container; you can run/inspect it
  with Docker.

## 7. Cons & limitations

- **arm64 only.** MicroVMs are Graviton; your jobs and any images they build/run
  must be arm64-compatible. (Migrating an x86 workload means flipping base images
  *and* any downstream Lambda `Architectures` to arm64.)
- **Region-limited.** Lambda MicroVMs are available only in select regions (e.g.
  `eu-west-1`), not everywhere - pick a supported region. The S3 artifact bucket +
  network connectors must be in-region.
- **Newish / sharp edges.** Requires AWS CLI ≥ 2.35 (service model dated 2025-09-09);
  some behaviour (e.g. self-terminate) isn't deeply documented - validate it.
- **DNS / buildx footguns** (see §5.2) - nested builds need the right resolver and
  prefer classic `docker build`.
- **Snapshot is single-size.** One image = one memory tier; you can't pick instance
  sizes per run. Plan one image per size.
- **Image versions cost storage** even with nothing running - clean up old versions.
- **Public webhook ingress.** The Lambda Function URL is internet-facing; security
  rests on the HMAC signature (rotate `webhook_secret`, keep it secret).
- **Snapshot uniqueness discipline** required - never bake identity/secrets in.
- **The installation token still enters the VM** (token mode). A hardened version
  would deliver a single-use JIT config via S3 instead.
- **No self-suspend** from inside; suspend must be driven externally.

## 8. Cost model

You pay for (per the [Lambda pricing](https://aws.amazon.com/lambda/pricing/) MicroVM line items):

- **`vCPU-Second-ARM` + memory GB-second** - only while `RUNNING`. This is the bulk;
  self-terminate is what keeps it proportional to actual job time.
- **Snapshot read/write** (~$0.02/GB) - on suspend/resume. Ephemeral + terminate
  avoids the suspend side.
- **Image storage** (~$0.08/GB-month, ~1-week min retention) - per image *version*.
  Delete old versions.
- **Lambda Function URL / SSM Parameter Store / CloudWatch** - negligible at CI volumes (Function URLs add no charge; SSM standard params are free).

### Compared to GitHub-hosted runners

[GitHub-hosted runners](https://docs.github.com/en/billing/concepts/product-billing/github-actions)
are billed **per minute** (each job rounds up to the next whole minute) out of a
monthly included-minutes bucket, then per-minute overage. Lambda MicroVMs are
billed **per second** of actual `RUNNING` time (vCPU-seconds + GB-seconds) straight
on your AWS bill - no per-account cap, no idle cost.

| | GitHub-hosted (Linux arm64, 2-core) | This module (Lambda MicroVM, ARM) |
|---|---|---|
| Billing unit | per **minute**, rounded up per job | per **vCPU-second** + GB-second |
| Rate | **$0.005 / min** (2 vCPU) | **~$0.0042 / min** at 2 vCPU / 4 GB · **~$0.0084 / min** at 4 vCPU / 8 GB |
| Included / free | 2,000–50,000 min/mo by plan; public repos free | none - AWS rates from second one |
| Scale to zero | n/a (managed) | **$0** idle (self-terminates; suspended/terminated cost nothing) |
| Short-job waste | up to ~1 min rounded up **per job** | none - per-second |
| Extra you also pay | none | a few seconds/job of snapshot-resume + register + self-terminate, plus snapshot storage (~$0.08/GB-mo per image version) |

*AWS rates: US East, ARM/Graviton - $0.0000276944/vCPU-s + $0.0000036667/GB-s, 2 GB : 1 vCPU; other regions vary. GitHub rates: 2026 published.*

**Rules of thumb:**
- **Under your included minutes?** GitHub-hosted is *free* - nothing beats it.
- **Lots of short jobs?** Per-second billing avoids GitHub's per-job minute rounding - a 40-second job bills ~40 s, not a full minute.
- **High volume / spiky, or you need Docker, arm64, VPC egress, or data residency?** MicroVMs run at low ARM rates with scale-to-zero and no per-account ceiling - at the cost of operational ownership and a few seconds of per-job overhead.

For the same 2 vCPU, per minute of *actual work* the two are within ~15%; the real wins are **per-second granularity**, **no included-minute ceiling**, and **zero idle cost**, traded against **running it yourself** and small per-job snapshot/boot overhead.

Levers: the **memory tier** (we use the 8 GB default for speed - drop it for cheaper,
slower jobs), the **self-terminate** fix (§5.3), and `maximumDurationInSeconds`
(tight enough to bound failures, longer than your longest job).

---

## 9. Repository layout

```
crates/                          ← the Rust workspace (all runtime code)
  dispatcher/                    webhook autoscaler Lambda: verify event → GitHub App token →
                                 RunMicrovm; warm pool, sweep, zombie reaper. Behavioral spec
                                 lives in this crate's unit tests.
  webhook-proxy/                 ingress Lambda: HMAC verify + PutEvents onto the event bus
  entrypoint/                    in-guest supervisor (static musl binary): lifecycle-hook
                                 server (:9000) + runner supervisor + self-terminate
  types/                         shared wire types (RunPayload, …)

microvm/                         ← the MicroVM runner image
  Dockerfile                     FROM al2023-minimal + runner(arm64) + docker + compose + aws-cli;
                                 CMD runs the entrypoint supervisor binary
  wait-for-docker.sh             runner job-started hook: gate the first step until dockerd is ready

scripts/
  fetch-artifacts.sh             fetch the release artifacts (dispatcher.zip, webhook-proxy.zip,
                                 entrypoint) pinned by `artifact_version` and verify their SHA-256
                                 checksums; invoked by Terraform at plan/apply time

.github/workflows/
  ci.yml                         terraform fmt/validate + cargo fmt/clippy/test
  release.yml                    on tag push: build the Lambda zips (provided.al2023, arm64) and
                                 the static musl entrypoint, checksum, publish a GitHub release

versions.tf                      ┐
variables.tf                     │
locals.tf                        │
iam.tf                           │
s3.tf                            │  the Terraform module (root) - build/exec/dispatcher
image.tf                         │  IAM, artifacts bucket, MicroVM image, SSM secret,
secret.tf                        │  dispatcher Lambda, public Function URL, EventBridge
dispatcher.tf                    │  ingress, GitHub webhook wiring, and outputs
eventbridge.tf                   │
ingress.tf                       │
github.tf                        │
outputs.tf                       ┘

docs/
  USAGE.md                       deploy guide - the one apply, all inputs/outputs
  ARCHITECTURE.md                this document (design deep dive)

examples/
  github-app/                    Terraform usage example - GitHub App auth
  hello-microvm.yml              minimal workflow (runs-on the microvm labels)
  docker-microvm.yml             docker / container: / services: examples

README.md                        overview + quick usage + generated inputs/outputs
CHANGELOG.md                     release notes
CONTRIBUTING.md                  dev loop + PR guidance
LICENSE                          MIT
```

---

## 10. Setup / deploy

Deployment is **Terraform-only** - the module at the repo root builds the MicroVM
image, deploys the webhook-proxy → EventBridge → SQS → dispatcher ingress,
provisions the IAM roles + SSM secret + artifacts bucket, and wires the GitHub
webhook, all from one `terraform apply`.

See **[USAGE.md](USAGE.md)** for the full guide: prerequisites, all inputs
and outputs, and the GitHub App usage example (`examples/github-app/`).

---

## 11. Operations / runbook

**Ship a new image version** (e.g. after editing `microvm/Dockerfile`, or bumping
`artifact_version` to pick up a new entrypoint supervisor binary): run
`terraform apply`. The image rebuilds to a new version and the dispatcher picks it
up automatically.

**Redeploy the dispatcher** (code/config): run `terraform apply` - the dispatcher
redeploys automatically when the pinned `artifact_version` (or its configuration)
changes.

(There is no manual-runner path: `RunMicrovm` is always driven by the dispatcher in
response to a `workflow_job` webhook.)

**Tail logs:**

```bash
aws logs tail /aws/lambda/gha-microvm-dispatcher --region <your-region> --follow          # dispatch
aws logs tail /aws/lambda-microvms/gha-microvm-runner --region <your-region> --follow     # runner + self-terminate
```

**Inspect MicroVMs:**

```bash
aws lambda-microvms list-microvms --region <your-region> \
  --query 'items[?state==`RUNNING`].[microvmId,imageVersion,startedAt]' --output table
aws lambda-microvms terminate-microvm --region <your-region> --microvm-identifier microvm-xxxx
```

**Clean up old image versions** (storage cost):
`aws lambda-microvms delete-microvm-image-version --image-identifier <arn> --image-version <n>`.

---

## 12. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| Job stays **queued**, no runner | Labels mismatch (`runs-on` ≠ runner labels / `REQUIRED_LABELS`); App lacks *Administration: R/W* or isn't installed; webhook not delivering (check dispatcher logs + GitHub webhook "Recent Deliveries"). |
| Dispatcher **401** | `X-Hub-Signature-256` ≠ HMAC of body with `webhook_secret` - secret mismatch between GitHub and the stored credential. |
| `Issuer (iss) must be a string` / `Integer` | App JWT: use the **numeric App ID as a string**, not the Client ID or a placeholder. |
| `AccessDeniedException … iam:PassRole` on RunMicrovm | IAM propagation right after a policy edit - the dispatcher retries; if persistent, check `PassExecRole`/`PassNetworkConnector`. |
| Nested **`docker build` DNS** failures ("Temporary failure resolving …") | The buildx-container path bypasses the daemon DNS - use classic `docker build` (the daemon runs with `--dns 169.254.169.253`). |
| `exec format error` building/running an image | arch mismatch - an x86 base/image on the arm64 runner. Everything must be arm64. |
| High `vCPU-Second-ARM` bill | VMs idling to `maximumDurationInSeconds`. Confirm the runner self-terminates + the exec role has `lambda:TerminateMicrovm`. |
| Runner aborts: "valid ICU package" | `libicu` missing from the image. |
| `RunMicrovm` rejects the memory value | Not a supported memory tier - use a power-of-two tier (512, 1024, 2048, 4096, 8192, … MiB); `runner_memory_mib` is validated to 512-32768. |

---

## 13. Security considerations

- **Webhook**: HMAC-verified; keep `webhook_secret` secret and rotate it. The
  ingress is public - the signature is the only gate.
- **Auth**: the **GitHub App** is a machine identity issuing short-lived,
  repo-scoped installation tokens. The dispatcher never logs the key or tokens.
- **Isolation**: each job gets a fresh Firecracker micro-VM (good for untrusted
  code). Don't switch to persistent runners for untrusted workloads.
- **Least privilege**: exec role = logs + self-terminate only; build role = the
  artifacts bucket only; dispatcher `iam:PassRole` scoped to the exec role.
- **Confused-deputy**: build/exec trust policies pin `aws:SourceAccount`.
- **Snapshot hygiene**: never bake credentials/identity into the image; inject
  per-VM via `runHookPayload`. Reseed any CSPRNG on `/resume`.
- **Idle-report control edge**: every runner VM can invoke the dispatcher
  (`lambda:InvokeFunction` on the dispatcher function, for idle reports), so a
  compromised job could report *any* VM id idle and nudge the dispatcher into
  suspending or terminating it. This is the **same trust domain** as the
  `lambda:TerminateMicrovm` `Resource: "*"` grant every VM already holds — a
  hostile VM can already kill fleet members directly, so the report path adds
  no new privilege. The guards are the dispatcher's own checks: only RUNNING
  VMs are acted on, and a busy runner (per GitHub) is never frozen or killed.
  Possible future hardening: a payload-embedded per-VM authenticator (a
  secret delivered in `runHookPayload` and echoed back in the report) so the
  dispatcher can verify a report really comes from the VM it names.
- **Further hardening**: deliver a single-use **JIT config via S3** so no
  reusable installation token ever lands inside the VM; tighten egress to a VPC
  connector with an `lambda-microvms` VPC endpoint.

---

## 14. References

- [AWS Lambda MicroVMs - Developer Guide](https://docs.aws.amazon.com/lambda/latest/dg/lambda-microvms-guide.html)
- [Launching & cost model](https://docs.aws.amazon.com/lambda/latest/dg/microvms-launching.html)
- [Lambda pricing](https://aws.amazon.com/lambda/pricing/)
- [`actions/runner`](https://github.com/actions/runner) · [JIT / ephemeral runners](https://docs.github.com/actions/hosting-your-own-runners/managing-self-hosted-runners/autoscaling-with-self-hosted-runners)
- [GitHub Apps - authenticating as an installation](https://docs.github.com/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app-installation)
- [Untrusted-code isolation on Lambda MicroVMs (DNS fix)](https://willpeixoto.dev/aws-lambda-microvms-untrusted-code-isolation)

## EventBridge ingress, warm pool, concurrency

Webhook path: GitHub → proxy Lambda (HMAC + PutEvents only) → bus →
rules (`workflow_job` + single-label match — array patterns are contains-ANY,
the dispatcher re-checks the full subset) → SQS jobs queue → dispatcher
(event source mapping, batch 1). SQS is the queue on purpose: EventBridge
target retries do not cover Lambda FUNCTION errors (async invoke, 2 retries,
dropped); a raising handler here returns the message to the queue instead.
`queued` dispatches (concurrency gate → pool resume → cold launch, raising on
retryable failure so the rule's retry policy is the job queue); `completed`
feeds the warm pool (suspend the job's VM, mapped via `runner_name` =
`gha-mvm-<microvm-id-prefix>`); a 5-minute sweep reconciles GitHub's queued
jobs against the fleet and GCs suspended VMs on stale image versions. DLQ +
14-day archive under everything; replay is safe — resumed/launched VMs whose
job was already taken get no work and the idle watchdog reaps them.

Pool lifecycle (entrypoint): job exits → dockerd teardown + `_work` wipe →
idle report (`reason=job-complete`) to the dispatcher, whose suspend then
arrives in seconds (the completed webhook stays as backup) → wait for the
suspend; a wall-clock jump against monotonic time marks resume (grace
resets, new `/run` re-registers a fresh ephemeral runner with the new job's
token); never suspended → idle report (`reason=orphan` — the dispatcher
terminates it from the control plane; an orphan's guest never re-enters the
idle wait, so it is never pooled) → self-terminate (backstop).

API shapes (`ListMicrovms` → `items`, `microvmIdentifier=` params, endpoint +
`X-aws-proxy-auth` token from `CreateMicrovmAuthToken` for the resume `/run`)
are typed by the `aws-sdk-lambdamicrovms` client (generated from the service
model); the raw list-record keys are still logged on first call as a
shape-drift canary. Two service behaviours are
undocumented and handled conservatively: suspended time is assumed to count
toward max lifetime (near-EOL pool VMs are terminated rather than resumed), and
post-resume readiness is observable via the `pool: resumed` /
`pool: handoff-unclaimed-terminating` log lines.
