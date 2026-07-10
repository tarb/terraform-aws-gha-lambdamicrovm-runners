# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions are the
module's release tags (`artifact_version`).

## [v0.0.7]

### Fixed

- **`disable_guest_ipv6` is now an ip6tables fast-fail — v0.0.6's route
  blackhole ALSO failed image builds NotStabilized** — both prior
  mechanisms broke the platform's hook channel, and a live-fleet network
  fingerprint finally explained why. The guest's IPv6 is not SLAAC: the
  platform statically installs a global `/128` on eth0 (`accept_ra` is
  already 0 in the base config — v0.0.6's accept_ra writes were no-ops; no
  link-local address exists), a static `default via fe80::1` with a
  PERMANENT neighbor entry, and a hidden guest agent listening on `*:8443`
  over that address. Lifecycle hooks travel control plane → agent `:8443`
  (over guest global v6, from an **off-link** source) → `127.0.0.1:9000`
  (the supervisor's hook server). v0.0.5's `disable_ipv6` sysctls deleted
  the `/128` (agent unreachable); v0.0.6's `unreachable default metric 1`
  route outranked the platform's static default and dropped the agent's
  **reply** packets — either way the READY probe timed out (`Ready hook
  invocation timed out after PT2M`) on every build, both Graviton
  generations. v0.0.7 keeps the deployed surface exactly
  (`disable_guest_ipv6` variable, `DISABLE_IPV6=1` image env, same lenient
  truthy parse) but the supervisor now inserts **one direction-aware
  ip6tables rule** (`ip6tables -w -I OUTPUT -d 2000::/3 -p tcp --syn -j
  REJECT --reject-with tcp-reset`): guest-initiated TCP connects to
  global-unicast v6 get an instant local RST (happy-eyeballs falls back to
  v4 in microseconds — the speedup the flag is for), while `--syn`
  (SYN-without-ACK) can never match the agent's inbound flow or its
  replies, and no address, route, or sysctl is touched. Failure
  warns-and-continues (boot is never blocked); success logs `ipv6 egress
  fast-fail installed (DISABLE_IPV6 set - guest-initiated v6 TCP gets
  RST)`.

## [v0.0.6]

### Fixed

- **`disable_guest_ipv6` blackholes global v6 instead of disabling the
  stack — v0.0.5 image builds failed NotStabilized** — v0.0.5 implemented
  the flag by writing the kernel's
  `net.ipv6.conf.{all,default}.disable_ipv6=1` sysctls at supervisor boot,
  and every image build with the flag on then died `NotStabilized`: the
  platform's lifecycle READY probe (which boots a VM from the candidate
  image and dials the hook server) depends on IPv6 — most plausibly
  link-local — somewhere in its channel, so disabling the whole v6 stack
  breaks the platform contract. (v0.0.5 was never released to a running
  fleet: the failure hits at image build, before any VM serves a job.)
  v0.0.6 keeps the deployed surface exactly (`disable_guest_ipv6` variable,
  `DISABLE_IPV6=1` image env, same lenient truthy parse) but the supervisor
  now installs an **unreachable default IPv6 route** (`ip -6 route replace
  unreachable default metric 1`) and sets
  `net.ipv6.conf.{all,default}.accept_ra=0` — so a later router
  advertisement can't re-install a real default, and `metric 1` outranks
  one that slips in first. Global v6 destinations fail instantly with
  ENETUNREACH (happy-eyeballs falls back to v4 with zero timeout — the same
  speedup v0.0.5 was after) while link-local/loopback IPv6 stay fully
  functional for the platform's hook channel. All failures warn-and-continue
  (boot is never blocked); success logs `ipv6 global routes blackholed
  (DISABLE_IPV6 set - v4-only egress)`. The image now bakes `iproute` (the
  `ip` tool); a supervisor landing on an older image warns and skips the
  route but still writes the accept_ra sysctls.

## [v0.0.5]

### Added

- **Optional guest IPv6 disable** — new `disable_guest_ipv6` Terraform
  variable (default `false`). Why: the fleet's egress connector can be
  IPv4-only (NetworkConnector VpcEgressConfiguration `network_protocol =
  "IPv4"`), but the guest has no way to know — dual-stack clients (observed:
  bun's highly concurrent package fetches) burn a happy-eyeballs IPv6
  attempt per connection against a protocol that can never work, a
  per-connection tax that turned a ~1min install into ~11min. When the
  operator knows the connector is v4-only, setting the variable bakes
  `DISABLE_IPV6=1` into the image environment (so flipping it triggers an
  image rebuild) and the supervisor writes `1` to
  `/proc/sys/net/ipv6/conf/{all,default}/disable_ipv6` at boot, before any
  child spawns — removing the entire class. Absent sysctl paths (kernel
  without IPv6) and write failures are tolerated with a WARN each; success
  logs `ipv6 disabled (DISABLE_IPV6 set - v4-only egress)`. Default stays
  off because DualStack connectors (and the managed INTERNET_EGRESS default)
  want IPv6 intact.

## [v0.0.4]

### Added

- **Docker on demand** — docker (dockerd + the wait-for-docker job-started
  hook) is now a per-job capability instead of an unconditional per-VM one.
  Non-docker jobs no longer pay dockerd's startup — the page-in-heavy part
  of a cold boot — and never stall on the hook. How it works: a job opts in
  by carrying the extra `docker` label in `runs-on` (matched
  case-insensitively, like GitHub's own label matching); the new
  `docker_default` Terraform variable (→ dispatcher env `DOCKER_DEFAULT`,
  default `true`) decides for unlabeled jobs. The dispatcher stamps the
  decision into the run payload as the new optional `enable_docker` field
  and registers the runner with the UNION of the configured `runner_labels`
  and the job's requested labels (a job requesting `docker` can only be
  assigned to a runner registered with it). The entrypoint starts dockerd
  and injects `ACTIONS_RUNNER_HOOK_JOB_STARTED` (the wait-for-docker gate)
  into the runner process ONLY for docker-enabled runs; the image no longer
  bakes that ENV. Warm-pool transitions work in both directions: cleanup
  already tears dockerd down between jobs, and each handoff payload decides
  afresh. Back-compat both ways: payloads from old dispatchers lack the
  field and the entrypoint falls back to the image's `ENABLE_DOCKER` env
  (docker on, exactly as before); old entrypoints ignore the new field.
  Migration to label opt-in: label your docker jobs with `docker` in
  `runs-on` first, then set `docker_default = false` so unlabeled jobs go
  lightweight. Defaults preserve today's behavior.

### Fixed

- **wait-for-docker + dockerd startup hardening** — the wait-for-docker
  hook's budget is now wall clock (bash `SECONDS`) instead of an iteration
  count (blocking `docker info` probes could stretch "150s" to 4m30s of real
  time), each probe is bounded by `timeout` (`DOCKER_CHECK_TIMEOUT`, default
  5 s — a hung `docker info` against a stale socket used to stall the hook
  indefinitely), and on final timeout the hook prints the tail of
  `/tmp/dockerd.log` to stderr so the failure diagnoses itself in the job
  log. The dockerd supervisor got the matching fixes: stale pid/socket files
  (docker.pid, docker.sock, containerd pid/sock — snapshot remnants that
  abort or wedge a fresh dockerd) are removed before EVERY launch attempt,
  not just between passes; the between-pass reaper now clears pid files too;
  and every supervisor readiness probe is bounded with a wall-clock
  per-driver window, so a wedged daemon gets killed and retried instead of
  waited on forever.

- **Dispatch stampede vs. the microvm control plane** — a burst of
  simultaneous `workflow_job` webhooks (N parallel dispatcher invokes) used
  to issue TWO `ListMicrovms` calls per dispatch (concurrency-cap gate +
  warm-pool candidate scan) and throttle the low-TPS control plane; all but
  one dispatch failed with `ThrottlingException`, orphaning jobs onto the
  5-minute sweep cadence. Two changes: (1) the dispatch path now fetches ONE
  fleet listing shared by the cap gate and the candidate scan (per-candidate
  `GetMicrovm` re-verification unchanged — that is the freshness that
  matters); (2) dispatch-path control-plane calls (the listing, the
  candidate `GetMicrovm`, `ResumeMicrovm`) get a bounded full-jitter
  throttle retry — 4 attempts, gap ceilings 0.5/1/2 s (per-gap cap 4 s) —
  layered ON TOP of the SDK's own standard retry (3 attempts, 1 s base),
  which was confirmed enabled and throttle-aware but loses the synchronized
  burst race on its own. A call that needed retries logs one
  `{"pool": "throttled", "calls": N}` line. The completed/idle intake paths
  keep their own listings and pre-suspend re-checks. (Structural fix once
  webhook ingress is fully on SQS: ESM `maximum_concurrency` — noted in
  `eventbridge.tf`.)

## [v0.0.3]

### Added

- **VM idle reports** — the entrypoint now reports its idleness to the
  dispatcher by direct Lambda invoke
  (`{"idle": {"microvmId": "...", "reason": "job-complete"|"orphan"}}`)
  instead of relying on in-guest teardown alone. Driver: in VPCs whose Lambda
  API traffic rides a PrivateLink interface endpoint, in-VM
  `TerminateMicrovm` ALWAYS fails (the MicroVMs sub-API rejects PrivateLink
  with `AccessDeniedException` "PrivateLink is not yet supported") — but a
  standard Lambda `Invoke` works over PrivateLink, so the dispatcher acts
  from the control plane where everything works. The dispatcher validates
  the VM is RUNNING and busy-checks the runner on GitHub — scoped to the
  report's optional `repo` hint (derived from the payload's `github_url`)
  when present, falling back to a bounded fleet-wide scan. Then:
  `reason=job-complete` suspends into the pool while there is room
  (re-checking the cap immediately before the freeze, so racing reports
  can't overshoot it) and terminates otherwise; `reason=orphan` ALWAYS
  terminates — an orphan's guest returns right after reporting and never
  re-enters the idle wait, so a suspended orphan could never claim a
  handoff (a dead pool slot). Orphan *adoption* into the pool is future
  work: it requires a guest wait-after-report handshake first. All with no
  artificial delay (the VM reports after its own cleanup). Guards against
  the report racing a resume: the dispatcher skips reports for VMs it
  resumed within the last minute, and the guest detects a suspend/resume
  landing mid-report (wall-clock jump past monotonic progress) and stands
  down — no retry, no self-terminate — because the resume's new run owns
  the VM. Pooled run payloads carry the new optional `dispatcher_fn` field
  (the dispatcher's own function name, from `AWS_LAMBDA_FUNCTION_NAME`);
  with an old dispatcher the field is absent and the VM behaves exactly as
  before. In-VM self-terminate remains the fallback when reporting fails
  outright, and the completed-webhook suspend path stays as backup. The
  exec role gains `lambda:InvokeFunction` on the dispatcher function.
- **Sweep loud failure** — when every attempted repo scan fails (or the
  installations listing itself fails), the sweep emits
  `{"sweep": "scan-failed-everywhere", "repos": N, "err": "..."}` in
  addition to the existing per-scope lines. Rationale: a missing GitHub App
  permission 403'd every scan for two days while `sweep: done, dispatched:
  0` looked healthy.

## [v0.0.2]

- Prior release (EventBridge/SQS ingress, warm pool, sweep, idiomatic Rust
  workspace).

## [v0.0.1]

- Initial release.
