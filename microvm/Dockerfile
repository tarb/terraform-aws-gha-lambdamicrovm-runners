# GitHub Actions self-hosted runner as an AWS Lambda MicroVM image.
#
# Unlike a classic Lambda function, this is built by Lambda from a zip (this
# Dockerfile + the static entrypoint binary) in S3, on top of the managed
# MicroVM base. The entrypoint runs a lifecycle-hook HTTP server (HOOK_PORT)
# that supervises the runner. The runner is REGISTERED AT /run TIME, never at build time — every
# MicroVM boots from the same snapshot, so baking in a registration would make
# all VMs share one identity (see "Snapshot uniqueness" in docs/ARCHITECTURE.md).
FROM public.ecr.aws/lambda/microvms:al2023-minimal

# Lambda MicroVMs are Graviton/arm64-only, so the runner is the linux-arm64 build.
ARG RUNNER_VERSION=2.335.1
ARG RUNNER_SHA256_ARM64=6d1e85bfd1a506a8b17c1f1b9b57dba458ffed90898799aaa9f599520b0d9207

# Runner .NET deps (libicu is mandatory) + tools + python3/pip for JOBS: the
# Actions ecosystem expects python3 on runners (setup steps, scripts, actions),
# so it stays baked in — the supervisor is a static binary and no longer needs it.
# The base is minimal AL2023 (curl-minimal/coreutils-single/openssl-snapsafe
# already present), so don't request the full curl/coreutils/openssl-libs.
RUN dnf install -y --setopt=install_weak_deps=0 \
        libicu lttng-ust krb5-libs zlib ca-certificates \
        git tar gzip xz unzip zip findutils which jq \
        procps-ng hostname python3 python3.11-pip \
    && dnf clean all \
    && rm -rf /var/cache/dnf /var/cache/libdnf5

# Download + verify + extract the runner BINARIES only. Do NOT run config.sh
# here — registration is per-MicroVM and happens at /run.
RUN set -eux; \
    mkdir -p /opt/actions-runner; cd /opt/actions-runner; \
    curl -fSL -o runner.tgz \
      "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-arm64-${RUNNER_VERSION}.tar.gz"; \
    echo "${RUNNER_SHA256_ARM64}  runner.tgz" | sha256sum -c -; \
    tar xzf runner.tgz; \
    rm -f runner.tgz

# Docker engine for jobs that use `docker`, `container:`, or `services:`. Nested
# containers only work when the MicroVM image is created with
# additionalOsCapabilities:["ALL"]. There is no systemd in the container, so
# dockerd is started directly by the entrypoint supervisor.
# iproute (`ip`) is also required by the supervisor's DISABLE_IPV6 handling:
# it installs an unreachable default IPv6 route at boot (see ipv6.rs).
RUN dnf install -y --setopt=install_weak_deps=0 docker iptables iproute \
    && dnf clean all \
    && rm -rf /var/cache/dnf /var/cache/libdnf5

# Docker Compose v2 plugin (not packaged on AL2023) so `docker compose ...` works,
# like GitHub-hosted runners. Many test scripts (e.g. `docker compose run`) need it.
ARG COMPOSE_VERSION=v5.2.0
RUN mkdir -p /usr/libexec/docker/cli-plugins \
    && curl -fSL -o /usr/libexec/docker/cli-plugins/docker-compose \
         "https://github.com/docker/compose/releases/download/${COMPOSE_VERSION}/docker-compose-linux-aarch64" \
    && chmod 0755 /usr/libexec/docker/cli-plugins/docker-compose

# AWS CLI v2 (arm64) so deploy/infra workflows (`aws cloudformation`, `aws lambda`,
# `sam`) work as-is, like GitHub-hosted runners which ship it preinstalled.
RUN curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-aarch64.zip" -o /tmp/awscliv2.zip \
    && cd /tmp && unzip -q awscliv2.zip && ./aws/install \
    && rm -rf /tmp/aws /tmp/awscliv2.zip


# ── Baked toolchains ──────────────────────────────────────
# Every job starts with these warm — the snapshot is shared, so NEVER bake
# secrets or identity. All arm64. Version bumps land via the scheduled image
# rebuild. sccache's S3 config (SCCACHE_BUCKET/REGION) is injected through the
# image environment_variables, not here — it differs per account.

# Native build deps: cargo -sys crates (aws-lc-rs wants cmake+clang, openssl-sys
# wants openssl-devel+pkgconf); gcc is rustc's default linker.
RUN dnf install -y --setopt=install_weak_deps=0 \
        gcc gcc-c++ make cmake clang perl-core openssl-devel pkgconf-pkg-config \
    && dnf clean all \
    && rm -rf /var/cache/dnf /var/cache/libdnf5

# Go toolchain (jobs that vendor modules need no downloads).
ARG GO_VERSION=1.26.4
RUN curl -fSL "https://go.dev/dl/go${GO_VERSION}.linux-arm64.tar.gz" | tar -xz -C /usr/local

# Rust via rustup (system-wide), clippy+rustfmt; sccache wraps rustc.
ARG RUST_VERSION=1.96.1
ARG SCCACHE_VERSION=0.16.0
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path \
      --default-toolchain "${RUST_VERSION}" --profile minimal --component clippy --component rustfmt \
    && curl -fSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-aarch64-unknown-linux-musl.tar.gz" \
       | tar -xz --strip-components=1 -C /usr/local/bin --wildcards '*/sccache' \
    && chmod 0755 /usr/local/bin/sccache

# docker buildx — enables `docker buildx bake` / multi-target builds.
ARG BUILDX_VERSION=0.35.0
RUN curl -fSL -o /usr/libexec/docker/cli-plugins/docker-buildx \
      "https://github.com/docker/buildx/releases/download/v${BUILDX_VERSION}/buildx-v${BUILDX_VERSION}.linux-arm64" \
    && chmod 0755 /usr/libexec/docker/cli-plugins/docker-buildx

# terraform + tflint — for Terraform CI jobs
ARG TERRAFORM_VERSION=1.15.7
ARG TFLINT_VERSION=0.63.1
RUN curl -fSL -o /tmp/tf.zip "https://releases.hashicorp.com/terraform/${TERRAFORM_VERSION}/terraform_${TERRAFORM_VERSION}_linux_arm64.zip" \
    && unzip -q /tmp/tf.zip terraform -d /usr/local/bin && rm /tmp/tf.zip \
    && curl -fSL -o /tmp/tflint.zip "https://github.com/terraform-linters/tflint/releases/download/v${TFLINT_VERSION}/tflint_linux_arm64.zip" \
    && unzip -q /tmp/tflint.zip tflint -d /usr/local/bin && rm /tmp/tflint.zip \
    && chmod 0755 /usr/local/bin/terraform /usr/local/bin/tflint

# gh (PR comments, release automation) + yq (YAML processing)
# — both preinstalled on GitHub-hosted runners, so their absence here only
# shows when a job flips to the fleet.
ARG GH_VERSION=2.96.0
ARG YQ_VERSION=4.53.3
RUN curl -fSL "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_arm64.tar.gz" \
      | tar -xz --strip-components=2 -C /usr/local/bin --wildcards '*/bin/gh' \
    && curl -fSL -o /usr/local/bin/yq "https://github.com/mikefarah/yq/releases/download/v${YQ_VERSION}/yq_linux_arm64" \
    && chmod 0755 /usr/local/bin/gh /usr/local/bin/yq

# tf-summarize — renders terraform plan summaries (handy for PR comments).
ARG TF_SUMMARIZE_VERSION=0.3.20
RUN curl -fSL "https://github.com/dineshba/tf-summarize/releases/download/v${TF_SUMMARIZE_VERSION}/tf-summarize_linux_arm64.tar.gz" \
      | tar -xz -C /usr/local/bin tf-summarize \
    && chmod 0755 /usr/local/bin/tf-summarize

# golangci-lint, kubectl, helm, bun
ARG GOLANGCI_VERSION=2.12.2
ARG KUBECTL_VERSION=1.36.2
ARG HELM_VERSION=3.21.2
ARG BUN_VERSION=1.3.14
RUN curl -fSL "https://github.com/golangci/golangci-lint/releases/download/v${GOLANGCI_VERSION}/golangci-lint-${GOLANGCI_VERSION}-linux-arm64.tar.gz" \
      | tar -xz --strip-components=1 -C /usr/local/bin --wildcards '*/golangci-lint' \
    && curl -fSL -o /usr/local/bin/kubectl "https://dl.k8s.io/release/v${KUBECTL_VERSION}/bin/linux/arm64/kubectl" \
    && chmod 0755 /usr/local/bin/kubectl \
    && curl -fSL "https://get.helm.sh/helm-v${HELM_VERSION}-linux-arm64.tar.gz" \
      | tar -xz --strip-components=1 -C /usr/local/bin linux-arm64/helm \
    && curl -fSL -o /tmp/bun.zip "https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/bun-linux-aarch64.zip" \
    && unzip -q /tmp/bun.zip -d /tmp \
    && install -m 0755 /tmp/bun-linux-aarch64/bun /usr/local/bin/bun \
    && rm -rf /tmp/bun.zip /tmp/bun-linux-aarch64

# Toolchain env for every job. CARGO_INCREMENTAL=0 is required for sccache.
ENV PATH=/usr/local/go/bin:/usr/local/cargo/bin:$PATH \
    RUSTC_WRAPPER=/usr/local/bin/sccache \
    CARGO_INCREMENTAL=0

# The supervisor: a static (musl) arm64 binary, fetched from this module's
# GitHub release by scripts/fetch-artifacts.sh into the build context.
COPY .artifacts/entrypoint /entrypoint
RUN chmod 0755 /entrypoint

# Runner job-started hook: gates the job's first step until dockerd is ready, so a
# docker step never races the daemon the entrypoint warms up in the background.
# NOT wired via an image-wide ACTIONS_RUNNER_HOOK_JOB_STARTED ENV: the supervisor
# injects that env into the runner per-run, only for docker-enabled jobs, so
# non-docker jobs never stall on this gate.
COPY wait-for-docker.sh /opt/actions-runner-hooks/wait-for-docker.sh
RUN chmod 0755 /opt/actions-runner-hooks/wait-for-docker.sh

# ENABLE_DOCKER is only the legacy fallback for payloads built by pre-v0.0.4
# dispatchers (no enable_docker field); new dispatchers decide per job.
ENV RUNNER_ALLOW_RUNASROOT=1 \
    HOME=/root \
    HOOK_PORT=9000 \
    RUNNER_DIR=/opt/actions-runner \
    RUNNER_LABELS=self-hosted,linux,arm64,microvm \
    ENABLE_DOCKER=true \
    DOCKER_STORAGE_DRIVER=overlay2 \
    DOTNET_CLI_TELEMETRY_OPTOUT=1 \
    DOTNET_NOLOGO=1 \
    RUNNER_VERSION=${RUNNER_VERSION}

# The hook server (Lambda calls hooks over the network namespace; must be 0.0.0.0).
EXPOSE 9000
CMD ["/entrypoint"]
