//! The warm pool: pull-handoff resume and the suspend intake for completed
//! jobs. Every per-candidate failure degrades to the next candidate or to a
//! cold launch; every intake failure is benign — never a stuck job.

use serde_json::json;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use types::{IdleReason, IdleReport, MicrovmId, RunPayload, RunnerName};

use crate::aws::{AwsApiError, ignore_service};
use crate::clock::{Clock, Epoch};
use crate::dispatch::DispatchError;
use crate::fleet::{Fleet, MicrovmState, VmRecord};
use crate::github::GithubApi;
use crate::github::GithubError;
use crate::github::types::InstallationId;
use crate::intake::webhook::WebhookPayload;
use crate::mailbox::{ClaimStatus, Mailbox};
use crate::oplog::{self, trunc};

/// How long after a resume this container distrusts idle reports for the
/// resumed VM (the pre-suspend guest's report can thaw and arrive late).
const RESUME_GUARD: Duration = Duration::from_secs(60);

/// VM ids this container resumed recently; the sweep's zombie reaper skips
/// them while their re-registration is in flight.
pub struct ResumeLedger {
    clock: Arc<dyn Clock>,
    marks: Mutex<HashMap<MicrovmId, Epoch>>,
}

impl ResumeLedger {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            marks: Mutex::new(HashMap::new()),
        }
    }

    pub fn mark(&self, id: &MicrovmId) {
        self.marks
            .lock()
            .unwrap()
            .insert(id.clone(), self.clock.now());
    }

    pub fn recently(&self, id: &MicrovmId, within: Duration) -> bool {
        self.marks
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|at| self.clock.now().since(*at) < within.as_secs_f64())
    }
}

#[derive(Debug, Clone)]
pub struct PoolPolicy {
    pub enabled: bool,
    pub max_size: i64,
    pub suspend_delay: Duration,
    pub handoff_window: Duration,
    pub eol_threshold_secs: f64,
}

#[derive(Debug)]
pub enum ResumeOutcome {
    /// A pooled VM claimed the job (identity and handoff time are on the
    /// `pool=resumed` log line).
    Resumed,
    NoCandidate,
    /// Parked but unclaimed: the VM was terminated; the caller cold-launches.
    /// NO further candidates are tried — an unclaimed handoff may be a
    /// systemic problem, and chain-burning the pool would make it worse.
    Abandoned,
}

/// What the suspend intake decided. Rendered into the webhook (or idle
/// direct-invoke) response.
#[derive(Debug, PartialEq)]
pub enum IntakeOutcome {
    PoolDisabled,
    NotOurs {
        runner: String,
    },
    AlreadyGone {
        runner: String,
    },
    AlreadySuspended,
    /// Idle report for a VM that is neither RUNNING nor a state we can name
    /// an outcome for — skipped.
    NotRunning {
        state: String,
    },
    /// Idle report for a VM this container resumed moments ago: the report
    /// belongs to the pre-suspend guest and the resume's job owns the VM.
    RecentlyResumed,
    PoolFull,
    RunnerBusy,
    /// Idle report from a stale-image VM: it could never be safely resumed,
    /// so it is terminated instead of pooled.
    StaleImage,
    /// `reason=orphan` idle report: terminated. An orphan's guest returns
    /// from its idle wait right after reporting, so a suspended orphan could
    /// never claim a handoff — pooling it would be a dead slot.
    OrphanTerminated,
    Suspended,
    BenignError,
}

impl fmt::Display for IntakeOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PoolDisabled => f.write_str("pool disabled"),
            Self::NotOurs { runner } => write!(f, "not ours: {runner:?}"),
            Self::AlreadyGone { runner } => write!(f, "vm for {runner} already gone"),
            Self::AlreadySuspended => f.write_str("already suspended"),
            Self::NotRunning { state } => write!(f, "vm not RUNNING ({state}) - skipped"),
            Self::RecentlyResumed => f.write_str("recently resumed - skipped"),
            Self::PoolFull => f.write_str("pool full - terminated"),
            Self::RunnerBusy => f.write_str("runner busy with a new job - not suspending"),
            Self::StaleImage => f.write_str("stale image - terminated"),
            Self::OrphanTerminated => f.write_str("orphan - terminated"),
            Self::Suspended => f.write_str("suspended"),
            Self::BenignError => f.write_str("error (benign)"),
        }
    }
}

/// Anything the intake flow can trip over — all benign by the time it leaves
/// this module.
#[derive(Debug, thiserror::Error)]
enum PoolIntakeError {
    #[error(transparent)]
    Aws(#[from] AwsApiError),
    #[error(transparent)]
    Github(#[from] GithubError),
    /// Image-version resolution (the idle stale-image check).
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
}

/// What triggered a suspend-or-terminate decision for a finished VM.
enum IntakeSource<'a> {
    /// A `workflow_job` completed webhook. It races the VM's own post-job
    /// cleanup, so the suspend is delayed; the VM is found by runner name and
    /// the busy-check has the webhook's repo to ask.
    Completed(&'a WebhookPayload),
    /// A direct idle report from the VM itself, sent AFTER its own cleanup —
    /// no delay; the VM names itself, and the busy-check asks the report's
    /// repo hint when present (falling back to a bounded scan of the App's
    /// repos for the derived runner name).
    Idle(&'a IdleReport),
}

pub struct Pool {
    fleet: Arc<Fleet>,
    mailbox: Arc<Mailbox>,
    github: Arc<dyn GithubApi>,
    ledger: Arc<ResumeLedger>,
    clock: Arc<dyn Clock>,
    policy: PoolPolicy,
}

impl Pool {
    pub fn new(
        fleet: Arc<Fleet>,
        mailbox: Arc<Mailbox>,
        github: Arc<dyn GithubApi>,
        ledger: Arc<ResumeLedger>,
        clock: Arc<dyn Clock>,
        policy: PoolPolicy,
    ) -> Self {
        Self {
            fleet,
            mailbox,
            github,
            ledger,
            clock,
            policy,
        }
    }

    /// The pull-handoff resume: pick ONE suspended current-image VM, park the
    /// payload in its mailbox BEFORE resuming, then poll for the parameter's
    /// disappearance (delete == ack). `vms` is the dispatch's ONE shared
    /// fleet listing (the cap gate used it too) — it may be stale, and that
    /// is fine: every candidate is re-verified with a fresh GetMicrovm below
    /// before anything state-changing happens.
    pub async fn try_resume(
        &self,
        payload: &RunPayload,
        vms: &[VmRecord],
    ) -> Result<ResumeOutcome, DispatchError> {
        let current = self.fleet.current_image_version().await?;
        let now = self.clock.now();
        for vm in vms {
            if vm.state != MicrovmState::Suspended {
                continue;
            }
            if vm.stale_image(&current) {
                oplog::emit(json!({"pool": "terminate-stale", "microvmId": vm.id}));
                ignore_service(self.fleet.terminate(&vm.id).await)?;
                continue;
            }
            // A VM near its max lifetime would die MID-JOB if resumed.
            if vm.near_eol(now, self.policy.eol_threshold_secs) {
                oplog::emit(json!({"pool": "terminate-near-eol", "microvmId": vm.id}));
                ignore_service(self.fleet.terminate(&vm.id).await)?;
                continue;
            }
            // Fresh state read: the listing may be stale. Failures here move
            // to the NEXT candidate — one bad VM must not wedge the pool.
            match self
                .fleet
                .retry_throttle(|| self.fleet.state_of(&vm.id))
                .await
            {
                Ok(MicrovmState::Suspended) => {}
                // Raced: another invocation claimed it — not ours to touch.
                Ok(_) => continue,
                Err(e) if e.is_service() => {
                    oplog::emit(json!({"pool": "describe-failed", "microvmId": vm.id,
                                       "err": trunc(&e.to_string(), 200)}));
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            // Park BEFORE resume: the money-risky step comes last.
            let parked = match self.mailbox.park(&vm.id, payload).await {
                Ok(parked) => parked,
                Err(e) if e.is_service() => {
                    oplog::emit(json!({"pool": "handoff-put-failed", "microvmId": vm.id,
                                       "err": trunc(&e.to_string(), 200)}));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if let Err(e) = self
                .fleet
                .retry_throttle(|| self.fleet.resume(&vm.id))
                .await
            {
                if !e.is_service() {
                    return Err(e.into());
                }
                // Lost a race or mid-transition.
                oplog::emit(json!({"pool": "resume-failed", "microvmId": vm.id,
                                   "err": trunc(&e.to_string(), 200)}));
                parked.cancel().await?;
                continue;
            }
            // The sweep's zombie reaper must not eat a VM we just woke.
            self.ledger.mark(&vm.id);
            match parked.await_claim(self.policy.handoff_window).await? {
                ClaimStatus::Claimed { seconds } => {
                    oplog::emit(json!({"pool": "resumed", "microvmId": vm.id,
                                       "handoff_seconds": round1(seconds)}));
                    return Ok(ResumeOutcome::Resumed);
                }
                ClaimStatus::Unclaimed => {
                    // Clean up and terminate rather than bill jobless.
                    oplog::emit(json!({"pool": "handoff-unclaimed-terminating",
                                       "microvmId": vm.id,
                                       "window": self.policy.handoff_window.as_secs()}));
                    parked.cancel().await?;
                    ignore_service(self.fleet.terminate(&vm.id).await)?;
                    return Ok(ResumeOutcome::Abandoned);
                }
            }
        }
        Ok(ResumeOutcome::NoCandidate)
    }

    /// Suspend intake for completed jobs. NEVER errors — the completed event
    /// is advisory; any failure is logged and reported as benign.
    pub async fn intake_completed(&self, payload: &WebhookPayload) -> IntakeOutcome {
        if !self.policy.enabled {
            return IntakeOutcome::PoolDisabled;
        }
        match self.intake_inner(&IntakeSource::Completed(payload)).await {
            Ok(outcome) => outcome,
            Err(e) => {
                oplog::emit(json!({"pool": "completed-intake-error",
                                   "err": trunc(&e.to_string(), 200)}));
                IntakeOutcome::BenignError
            }
        }
    }

    /// Intake for a VM's direct idle report. NEVER errors, like
    /// [`Self::intake_completed`]. Runs even with the pool disabled: the VM
    /// asked to be dealt with, and "no room" then terminates it. Only
    /// `reason=job-complete` may suspend into the pool; `reason=orphan`
    /// always terminates (see the orphan branch in `intake_inner`).
    pub async fn intake_idle(&self, report: &IdleReport) -> IntakeOutcome {
        oplog::emit(json!({"pool": "idle-report",
                           "microvmId": report.microvm_id,
                           "reason": report.reason.as_str()}));
        // A VM we resumed within the last minute is mid-handoff: its report
        // was sent by the PRE-suspend guest (frozen mid-report, thawed now)
        // and the new run owns the VM — acting on it would kill a live job.
        // The ledger is per-container: a resume by ANOTHER dispatcher
        // container is not seen here and rides on the busy/RUNNING checks
        // below instead — acceptable, this guard just closes the window the
        // busy check cannot (the resumed job may not have re-registered yet).
        if self
            .ledger
            .recently(&MicrovmId::new(report.microvm_id.as_str()), RESUME_GUARD)
        {
            oplog::emit(json!({"pool": "idle-skip",
                               "microvmId": report.microvm_id,
                               "why": "recently-resumed"}));
            return IntakeOutcome::RecentlyResumed;
        }
        match self.intake_inner(&IntakeSource::Idle(report)).await {
            Ok(outcome) => outcome,
            Err(e) => {
                oplog::emit(json!({"pool": "idle-intake-error",
                                   "err": trunc(&e.to_string(), 200)}));
                IntakeOutcome::BenignError
            }
        }
    }

    /// The shared suspend-or-terminate flow behind both intakes.
    async fn intake_inner(
        &self,
        source: &IntakeSource<'_>,
    ) -> Result<IntakeOutcome, PoolIntakeError> {
        // Locate and validate the VM — by runner name (webhook) or by its
        // own id (idle report; skip-with-a-log unless it is RUNNING).
        // NotOurs is decided BEFORE the fleet list: most completed webhooks
        // are for runners that aren't ours and must stay AWS-call-free.
        let (vm, vms) = match source {
            IntakeSource::Completed(payload) => {
                let runner = payload.runner_name().to_string();
                let Some(ours) = RunnerName::parse(&runner) else {
                    return Ok(IntakeOutcome::NotOurs { runner });
                };
                let vms = self.fleet.list().await?;
                let Some(vm) = vms
                    .iter()
                    .find(|v| v.id.as_str().contains(ours.fragment))
                    .cloned()
                else {
                    return Ok(IntakeOutcome::AlreadyGone { runner });
                };
                if vm.state.pooled() {
                    return Ok(IntakeOutcome::AlreadySuspended);
                }
                if vm.state.gone() {
                    return Ok(IntakeOutcome::AlreadyGone { runner });
                }
                (vm, vms)
            }
            IntakeSource::Idle(report) => {
                let vms = self.fleet.list().await?;
                let Some(vm) = vms
                    .iter()
                    .find(|v| v.id.as_str() == report.microvm_id)
                    .cloned()
                else {
                    oplog::emit(json!({"pool": "idle-skip",
                                       "microvmId": report.microvm_id,
                                       "state": "gone"}));
                    return Ok(IntakeOutcome::AlreadyGone {
                        runner: report.microvm_id.clone(),
                    });
                };
                if vm.state != MicrovmState::Running {
                    let state = format!("{:?}", vm.state);
                    oplog::emit(json!({"pool": "idle-skip",
                                       "microvmId": report.microvm_id,
                                       "state": state}));
                    return Ok(if vm.state.pooled() {
                        IntakeOutcome::AlreadySuspended
                    } else if vm.state.gone() {
                        IntakeOutcome::AlreadyGone {
                            runner: report.microvm_id.clone(),
                        }
                    } else {
                        IntakeOutcome::NotRunning { state }
                    });
                }
                (vm, vms)
            }
        };

        let pooled = pooled_count(&vms);
        if let IntakeSource::Completed(_) = source {
            if pooled >= self.policy.max_size {
                return self.terminate_full(&vm).await;
            }
            // Webhook path only: the completed event races the entrypoint's
            // own post-job cleanup — wait it out before freezing. (An idle
            // report is sent AFTER cleanup, so it skips the delay.)
            self.clock.sleep(self.policy.suspend_delay).await;
            // Re-check the cap AFTER the delay: N jobs completing together
            // all pass the pre-sleep check and would overshoot the pool by N.
            if pooled_count(&self.fleet.list().await?) >= self.policy.max_size {
                return self.terminate_full(&vm).await;
            }
        }

        // A duplicate/late event must not freeze a VM that took a NEW job:
        // the reused VM re-registers the SAME runner name, so busy == in use.
        let busy = match source {
            IntakeSource::Completed(payload) => {
                self.runner_busy(payload, payload.runner_name()).await
            }
            IntakeSource::Idle(report) => {
                let runner = RunnerName::for_vm(&vm.id);
                match report.repo.as_deref() {
                    // The report named its repo: one scoped listing, exactly
                    // like the completed path.
                    Some(repo) => self.runner_busy_in(repo, None, runner.as_str()).await,
                    None => self.runner_busy_anywhere(runner.as_str()).await,
                }
            }
        };
        match busy {
            Ok(true) => return Ok(IntakeOutcome::RunnerBusy),
            Ok(false) => {}
            Err(e) => {
                // Best-effort check: log and proceed with the suspend.
                oplog::emit(json!({"pool": "busy-check-failed",
                                   "err": trunc(&e.to_string(), 150)}));
            }
        }

        if let IntakeSource::Idle(report) = source {
            // An orphan ALWAYS terminates (busy VMs were skipped above). Its
            // guest returns the moment the report is answered — it never
            // re-enters the idle wait — so a suspended orphan could never
            // claim a handoff: a dead pool slot that blocks a live one.
            // (A job-complete guest re-enters the idle wait after reporting
            // and CAN claim later handoffs, so only it may pool.) Adoption
            // is future work: it needs a guest wait-after-report handshake.
            if report.reason == IdleReason::Orphan {
                oplog::emit(json!({"pool": "terminate-orphan", "microvmId": vm.id}));
                ignore_service(self.fleet.terminate(&vm.id).await)?;
                return Ok(IntakeOutcome::OrphanTerminated);
            }
            // A stale-image VM could never be safely resumed: terminate.
            let current = self.fleet.current_image_version().await?;
            if vm.stale_image(&current) {
                oplog::emit(json!({"pool": "terminate-stale", "microvmId": vm.id}));
                ignore_service(self.fleet.terminate(&vm.id).await)?;
                return Ok(IntakeOutcome::StaleImage);
            }
            // A disabled pool has room for nothing.
            if !self.policy.enabled || pooled >= self.policy.max_size {
                return self.terminate_full(&vm).await;
            }
            // That count rode the intake's FIRST listing, and the busy/stale
            // checks above are real network time: re-list immediately before
            // the freeze so N racing idle reports can't overshoot the cap by
            // N (mirrors the completed path's post-delay re-check).
            if pooled_count(&self.fleet.list().await?) >= self.policy.max_size {
                return self.terminate_full(&vm).await;
            }
        }

        self.fleet.suspend(&vm.id).await?;
        oplog::emit(json!({"pool": "suspended", "microvmId": vm.id}));
        Ok(IntakeOutcome::Suspended)
    }

    /// Pool-full teardown, shared by both intakes.
    async fn terminate_full(&self, vm: &VmRecord) -> Result<IntakeOutcome, PoolIntakeError> {
        oplog::emit(json!({"pool": "full-terminating", "microvmId": vm.id}));
        ignore_service(self.fleet.terminate(&vm.id).await)?;
        Ok(IntakeOutcome::PoolFull)
    }

    async fn runner_busy(
        &self,
        payload: &WebhookPayload,
        runner: &str,
    ) -> Result<bool, GithubError> {
        let Some(repo) = payload.repo() else {
            return Ok(false);
        };
        self.runner_busy_in(repo, payload.installation_id(), runner)
            .await
    }

    /// Repo-scoped busy probe: one runner listing in `repo`. The
    /// installation id is optional — `token_for_repo` derives it from the
    /// repo when absent (idle reports carry a repo hint, no installation).
    async fn runner_busy_in(
        &self,
        repo: &str,
        installation: Option<InstallationId>,
        runner: &str,
    ) -> Result<bool, GithubError> {
        let token = self.github.token_for_repo(repo, installation).await?;
        let runners = self.github.repo_runners(repo, &token).await?;
        Ok(runners.iter().any(|r| r.name == runner && r.busy))
    }

    /// Busy probe with no repo hint (idle reports carry only the VM id):
    /// walk the App's installations' repos — bounded like the sweep — until
    /// the runner name is found. A name registers in exactly one repo, so
    /// the first hit decides.
    async fn runner_busy_anywhere(&self, runner: &str) -> Result<bool, GithubError> {
        for inst in self.github.installations().await?.iter().take(10) {
            let (token, repos) = self.github.installation_repos(*inst).await?;
            for repo in repos.iter().take(100) {
                let runners = self.github.repo_runners(&repo.full_name, &token).await?;
                if let Some(r) = runners.iter().find(|r| r.name == runner) {
                    return Ok(r.busy);
                }
            }
        }
        Ok(false)
    }
}

/// SUSPENDED + SUSPENDING — what occupies warm-pool slots.
fn pooled_count(vms: &[VmRecord]) -> i64 {
    vms.iter().filter(|v| v.state.pooled()).count() as i64
}

/// Round to one decimal for the `handoff_seconds` log value.
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}
