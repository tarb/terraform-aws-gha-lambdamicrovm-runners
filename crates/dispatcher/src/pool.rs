//! The warm pool: pull-handoff resume and the suspend intake for completed
//! jobs. Every per-candidate failure degrades to the next candidate or to a
//! cold launch; every intake failure is benign — never a stuck job.

use serde_json::json;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use types::{MicrovmId, RunPayload, RunnerName};

use crate::aws::{AwsApiError, ignore_service};
use crate::clock::{Clock, Epoch};
use crate::dispatch::DispatchError;
use crate::fleet::{Fleet, MicrovmState};
use crate::github::GithubApi;
use crate::github::GithubError;
use crate::intake::webhook::WebhookPayload;
use crate::mailbox::{ClaimStatus, Mailbox};
use crate::oplog::{self, trunc};

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

/// What the suspend intake decided. Rendered into the webhook response.
#[derive(Debug, PartialEq)]
pub enum IntakeOutcome {
    PoolDisabled,
    NotOurs { runner: String },
    AlreadyGone { runner: String },
    AlreadySuspended,
    PoolFull,
    RunnerBusy,
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
            Self::PoolFull => f.write_str("pool full - terminated"),
            Self::RunnerBusy => f.write_str("runner busy with a new job - not suspending"),
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
    /// disappearance (delete == ack).
    pub async fn try_resume(&self, payload: &RunPayload) -> Result<ResumeOutcome, DispatchError> {
        let current = self.fleet.current_image_version().await?;
        let now = self.clock.now();
        for vm in self.fleet.list().await? {
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
            match self.fleet.state_of(&vm.id).await {
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
            if let Err(e) = self.fleet.resume(&vm.id).await {
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
        match self.intake_completed_inner(payload).await {
            Ok(outcome) => outcome,
            Err(e) => {
                oplog::emit(json!({"pool": "completed-intake-error",
                                   "err": trunc(&e.to_string(), 200)}));
                IntakeOutcome::BenignError
            }
        }
    }

    async fn intake_completed_inner(
        &self,
        payload: &WebhookPayload,
    ) -> Result<IntakeOutcome, PoolIntakeError> {
        if !self.policy.enabled {
            return Ok(IntakeOutcome::PoolDisabled);
        }
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
        let pooled = vms.iter().filter(|v| v.state.pooled()).count() as i64;
        if pooled >= self.policy.max_size {
            oplog::emit(json!({"pool": "full-terminating", "microvmId": vm.id}));
            ignore_service(self.fleet.terminate(&vm.id).await)?;
            return Ok(IntakeOutcome::PoolFull);
        }
        // Let the entrypoint finish post-job cleanup before freezing it.
        self.clock.sleep(self.policy.suspend_delay).await;
        // Re-check the cap AFTER the delay: N jobs completing together all
        // pass the pre-sleep check and would overshoot the pool by N.
        let pooled_after = self
            .fleet
            .list()
            .await?
            .iter()
            .filter(|v| v.state.pooled())
            .count() as i64;
        if pooled_after >= self.policy.max_size {
            oplog::emit(json!({"pool": "full-terminating", "microvmId": vm.id}));
            ignore_service(self.fleet.terminate(&vm.id).await)?;
            return Ok(IntakeOutcome::PoolFull);
        }
        // A duplicate/late event must not freeze a VM that took a NEW job:
        // the reused VM re-registers the SAME runner name, so busy == in use.
        match self.runner_busy(payload, &runner).await {
            Ok(true) => return Ok(IntakeOutcome::RunnerBusy),
            Ok(false) => {}
            Err(e) => {
                // Best-effort check: log and proceed with the suspend.
                oplog::emit(json!({"pool": "busy-check-failed",
                                   "err": trunc(&e.to_string(), 150)}));
            }
        }
        self.fleet.suspend(&vm.id).await?;
        oplog::emit(json!({"pool": "suspended", "microvmId": vm.id}));
        Ok(IntakeOutcome::Suspended)
    }

    async fn runner_busy(
        &self,
        payload: &WebhookPayload,
        runner: &str,
    ) -> Result<bool, GithubError> {
        let Some(repo) = payload.repo() else {
            return Ok(false);
        };
        let token = self
            .github
            .token_for_repo(repo, payload.installation_id())
            .await?;
        let runners = self.github.repo_runners(repo, &token).await?;
        Ok(runners.iter().any(|r| r.name == runner && r.busy))
    }
}

/// Round to one decimal for the `handoff_seconds` log value.
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}
