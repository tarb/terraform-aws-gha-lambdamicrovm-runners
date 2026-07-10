//! Ground-truth reconciliation sweep: stale-queued re-dispatch, zombie
//! reaper, handoff-parameter GC, pool GC. GitHub's queued jobs are the
//! source of truth; every scope is individually guarded — one broken repo
//! must not blind the sweep to the rest.

use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use types::RunnerName;

use crate::aws::{AwsApiError, ignore_service};
use crate::clock::{Clock, Epoch};
use crate::config::Config;
use crate::dispatch::{DispatchError, Dispatcher, JobRef};
use crate::fleet::Fleet;
use crate::github::types::{InstallationId, InstallationToken, RunStatus};
use crate::github::{GithubApi, GithubError};
use crate::gtime;
use crate::mailbox::Mailbox;
use crate::oplog::{self, trunc};
use crate::pool::ResumeLedger;

/// Orphaned handoff parameters are GC'd past this age.
const HANDOFF_GC_AGE: Duration = Duration::from_secs(3600);
/// A resumed VM is protected from the zombie reaper for this long.
const RESUME_PROTECTION: Duration = Duration::from_secs(600);

/// Lambda-deadline probe: `low()` is true when less than 15 s remain.
pub struct Deadline(Option<Box<dyn Fn() -> i64 + Send + Sync>>);

impl Deadline {
    /// No deadline (nothing outside tests invokes the sweep without one).
    #[cfg(test)]
    pub fn none() -> Self {
        Self(None)
    }

    pub fn from_fn(remaining_ms: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        Self(Some(Box::new(remaining_ms)))
    }

    pub fn from_lambda(deadline_epoch_ms: i64) -> Self {
        Self::from_fn(move || {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            deadline_epoch_ms - now_ms
        })
    }

    pub fn low(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|remaining| remaining() < 15_000)
    }
}

/// Per-repo job-scan bookkeeping for the all-failed canary.
#[derive(Default)]
struct ScanTally {
    attempted: usize,
    failed: usize,
    first_err: Option<String>,
}

impl ScanTally {
    fn succeeded(&mut self) {
        self.attempted += 1;
    }

    fn failed(&mut self, e: &GithubError) {
        self.attempted += 1;
        self.failed += 1;
        self.first_err.get_or_insert_with(|| e.to_string());
    }

    /// `Some((repos, first error))` when at least one repo was attempted and
    /// every attempt failed.
    fn all_failed(&self) -> Option<(usize, &str)> {
        (self.attempted > 0 && self.failed == self.attempted)
            .then(|| (self.attempted, self.first_err.as_deref().unwrap_or("")))
    }
}

fn emit_scan_failed_everywhere(repos: usize, err: &str) {
    oplog::emit(json!({"sweep": "scan-failed-everywhere", "repos": repos,
                       "err": trunc(err, 200)}));
}

pub struct Sweeper {
    github: Arc<dyn GithubApi>,
    fleet: Arc<Fleet>,
    mailbox: Arc<Mailbox>,
    dispatcher: Arc<Dispatcher>,
    ledger: Arc<ResumeLedger>,
    clock: Arc<dyn Clock>,
    cfg: Arc<Config>,
}

impl Sweeper {
    pub fn new(
        github: Arc<dyn GithubApi>,
        fleet: Arc<Fleet>,
        mailbox: Arc<Mailbox>,
        dispatcher: Arc<Dispatcher>,
        ledger: Arc<ResumeLedger>,
        clock: Arc<dyn Clock>,
        cfg: Arc<Config>,
    ) -> Self {
        Self {
            github,
            fleet,
            mailbox,
            dispatcher,
            ledger,
            clock,
            cfg,
        }
    }

    /// Returns the number of stale jobs re-dispatched.
    pub async fn sweep(&self, deadline: &Deadline) -> Result<i64, GithubError> {
        let installations = match self.github.installations().await {
            Ok(installations) => installations,
            Err(e) => {
                // Nothing was scanned at all: same loud line as the
                // every-repo-failed case (the error still propagates).
                emit_scan_failed_everywhere(0, &e.to_string());
                return Err(e);
            }
        };
        let mut dispatched = 0i64;
        let now = self.clock.now();
        // Registered runner names across every scanned repo, for the zombie
        // reaper. `scan_complete` guards the reaper: a partial view could
        // misread a mid-job VM as a zombie.
        let mut registered: HashSet<String> = HashSet::new();
        let mut scan_complete = true;
        // A single systemic failure (e.g. a missing GitHub App permission
        // 403ing every job scan) hides behind per-repo "repo-failed" lines
        // while "done, dispatched: 0" looks healthy — tally the scans and
        // shout when every attempted one failed.
        let mut scans = ScanTally::default();

        'outer: for inst in installations.iter().take(10) {
            if deadline.low() {
                oplog::emit(json!({"sweep": "deadline-bail", "at": "installations"}));
                scan_complete = false;
                break;
            }
            let (token, repos) = match self.github.installation_repos(*inst).await {
                Ok(x) => x,
                Err(e) => {
                    oplog::emit(json!({"sweep": "installation-failed",
                                       "installation": inst.0,
                                       "err": trunc(&e.to_string(), 150)}));
                    scan_complete = false;
                    continue;
                }
            };
            for repo in repos.iter().take(100) {
                if deadline.low() {
                    oplog::emit(json!({"sweep": "deadline-bail", "at": repo.full_name}));
                    scan_complete = false;
                    continue 'outer;
                }
                // Runner names for the reaper; a failure here poisons only
                // scan_complete, not the stale-job scan below.
                match self.github.repo_runners(&repo.full_name, &token).await {
                    Ok(runners) => registered.extend(runners.into_iter().map(|r| r.name)),
                    Err(e) => {
                        oplog::emit(json!({"sweep": "runner-list-failed",
                                           "repo": repo.full_name,
                                           "err": trunc(&e.to_string(), 150)}));
                        scan_complete = false;
                    }
                }
                match self
                    .scan_repo_jobs(&repo.full_name, &token, Some(*inst), now, &mut dispatched)
                    .await
                {
                    Ok(()) => scans.succeeded(),
                    Err(e) => {
                        // One repo must not kill the sweep.
                        scans.failed(&e);
                        oplog::emit(json!({"sweep": "repo-failed", "repo": repo.full_name,
                                           "err": trunc(&e.to_string(), 150)}));
                        continue;
                    }
                }
            }
        }
        if let Some((repos, err)) = scans.all_failed() {
            emit_scan_failed_everywhere(repos, err);
        }

        if scan_complete && let Err(e) = self.reap_zombies(&registered).await {
            oplog::emit(json!({"sweep": "zombie-reap-failed",
                               "err": trunc(&e.to_string(), 150)}));
        }
        if self.cfg.pool_enabled
            && let Err(e) = self.mailbox.gc(HANDOFF_GC_AGE).await
        {
            oplog::emit(json!({"sweep": "handoff-gc-failed",
                               "err": trunc(&e.to_string(), 150)}));
        }
        if self.cfg.pool_enabled
            && let Err(e) = self.gc_pool().await
        {
            oplog::emit(json!({"sweep": "pool-gc-failed",
                               "err": trunc(&e.to_string(), 150)}));
        }
        oplog::emit(json!({"sweep": "done", "dispatched": dispatched}));
        Ok(dispatched)
    }

    /// Per-repo stale-queued-job scan. Queued AND in_progress runs are
    /// checked: a multi-job run with one job executing elsewhere still holds
    /// queued jobs.
    async fn scan_repo_jobs(
        &self,
        repo: &str,
        token: &InstallationToken,
        installation: Option<InstallationId>,
        now: Epoch,
        dispatched: &mut i64,
    ) -> Result<(), GithubError> {
        let mut runs = Vec::new();
        for status in [RunStatus::Queued, RunStatus::InProgress] {
            runs.extend(self.github.workflow_runs(repo, token, status).await?);
        }
        for run in &runs {
            let jobs = self.github.run_jobs(repo, run.id, token).await?;
            for job in jobs {
                if job.status.as_deref() != Some("queued") {
                    continue;
                }
                if !self.cfg.required_labels.is_subset(&job.labels) {
                    continue;
                }
                // Age from the job's created_at, falling back to the run's;
                // unparsable/missing means "old enough" — dispatch.
                let created = job
                    .created_at
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .or(run.created_at.as_deref());
                let age = created
                    .and_then(|s| gtime::parse_gh_time(s).ok())
                    .map(|ts| now.since(ts) as i64)
                    .unwrap_or(self.cfg.sweep_min_age + 1);
                if age < self.cfg.sweep_min_age {
                    continue;
                }
                oplog::emit(json!({"sweep": "stale-queued-job", "repo": repo,
                                   "job_id": job.id, "age": age}));
                let job_ref = JobRef {
                    repo: repo.to_string(),
                    job_id: job.id,
                    installation,
                    labels: job.labels.clone(),
                };
                match self.dispatcher.dispatch(&job_ref).await {
                    Ok(()) => *dispatched += 1,
                    Err(e) => {
                        // One job must not kill the sweep.
                        oplog::emit(json!({"sweep": "dispatch-failed", "job_id": job.id,
                                           "err": trunc(&e.to_string(), 200)}));
                    }
                }
            }
        }
        Ok(())
    }

    /// A PENDING/RUNNING VM whose derived runner name is registered in NO
    /// scanned repo has no job and no way to get one. Only acts on a
    /// COMPLETE scan, only past 2× SWEEP_MIN_AGE, and never on a VM this
    /// container just resumed (its re-registration is in flight).
    async fn reap_zombies(&self, registered: &HashSet<String>) -> Result<(), AwsApiError> {
        let now = self.clock.now();
        for vm in self.fleet.list().await? {
            if !vm.state.holds_capacity() {
                continue;
            }
            let Some(age) = vm.age(now) else { continue };
            if age < (2 * self.cfg.sweep_min_age) as f64 {
                continue;
            }
            if registered.contains(RunnerName::for_vm(&vm.id).as_str()) {
                continue; // has, had, or is waiting on a job — the watchdog's turf
            }
            if self.ledger.recently(&vm.id, RESUME_PROTECTION) {
                continue;
            }
            oplog::emit(json!({"sweep": "reap-zombie-vm", "microvmId": vm.id,
                               "age": age as i64}));
            ignore_service(self.fleet.terminate(&vm.id).await)?;
        }
        Ok(())
    }

    /// Stale-image suspended VMs can never be safely resumed, and aged ones
    /// must not depend on job traffic to be reaped.
    async fn gc_pool(&self) -> Result<(), DispatchError> {
        let current = self.fleet.current_image_version().await?;
        let now = self.clock.now();
        for vm in self.fleet.list().await? {
            if vm.state != crate::fleet::MicrovmState::Suspended {
                continue;
            }
            let reason = if vm.stale_image(&current) {
                "stale-image"
            } else if vm.near_eol(now, self.cfg.eol_threshold()) {
                "near-eol"
            } else {
                continue;
            };
            oplog::emit(json!({"sweep": "gc-pool-vm", "reason": reason,
                               "microvmId": vm.id}));
            ignore_service(self.fleet.terminate(&vm.id).await).map_err(DispatchError::Aws)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_none_is_never_low() {
        assert!(!Deadline::none().low());
    }

    #[test]
    fn deadline_low_under_15s() {
        assert!(Deadline::from_fn(|| 14_999).low());
        assert!(!Deadline::from_fn(|| 15_000).low());
    }

    #[test]
    fn scan_tally_fires_only_when_all_attempts_failed() {
        let err = GithubError::Status {
            status: 403,
            endpoint: "first".to_string(),
        };
        let later = GithubError::Status {
            status: 500,
            endpoint: "second".to_string(),
        };

        // Nothing attempted: no line (the repos:0 case is the installations
        // failure, handled at its call site).
        assert!(ScanTally::default().all_failed().is_none());

        let mut tally = ScanTally::default();
        tally.failed(&err);
        tally.failed(&later);
        let (repos, first) = tally.all_failed().expect("all failed");
        assert_eq!(repos, 2);
        assert!(first.contains("first"), "keeps the FIRST error: {first}");

        let mut tally = ScanTally::default();
        tally.failed(&err);
        tally.succeeded();
        assert!(tally.all_failed().is_none(), "one success silences it");
    }
}
