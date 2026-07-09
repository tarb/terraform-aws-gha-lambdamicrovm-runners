# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions are the
module's release tags (`artifact_version`).

## [Unreleased]

### Fixed

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
