//! Dispatch: concurrency gate → pool resume → cold launch.
//!
//! `Err` from [`Dispatcher::dispatch`] means "retryable" — on the SQS path
//! the message then returns to the queue, and that queue IS the job queue.

use serde_json::json;
use std::sync::Arc;
use types::RunPayload;

use crate::aws::AwsApiError;
use crate::config::Config;
use crate::fleet::Fleet;
use crate::github::types::InstallationId;
use crate::github::{GithubApi, GithubError};
use crate::intake::webhook::LabelSet;
use crate::oplog;
use crate::pool::{Pool, ResumeOutcome};
use crate::secrets::SecretsError;

/// The runner label that opts a job into docker (dockerd + the
/// wait-for-docker job-started hook) when `DOCKER_DEFAULT` is false.
pub const DOCKER_LABEL: &str = "docker";

#[derive(Debug, Clone)]
pub struct JobRef {
    pub repo: String,
    pub job_id: Option<i64>,
    pub installation: Option<InstallationId>,
    /// The queued job's requested labels (webhook and sweep both carry
    /// them): they drive the docker opt-in and are unioned into the
    /// registration labels so the assignment can match.
    pub labels: LabelSet,
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("concurrency cap {cap} reached; job {job:?} waits for retry")]
    CapReached { cap: i64, job: Option<i64> },
    #[error("image {arn} has no ACTIVE version (state {state:?})")]
    NoActiveImage { arn: String, state: Option<String> },
    #[error(transparent)]
    Aws(#[from] AwsApiError),
    #[error(transparent)]
    Github(#[from] GithubError),
    #[error(transparent)]
    Secrets(#[from] SecretsError),
}

pub struct Dispatcher {
    fleet: Arc<Fleet>,
    pool: Arc<Pool>,
    github: Arc<dyn GithubApi>,
    cfg: Arc<Config>,
}

impl Dispatcher {
    pub fn new(
        fleet: Arc<Fleet>,
        pool: Arc<Pool>,
        github: Arc<dyn GithubApi>,
        cfg: Arc<Config>,
    ) -> Self {
        Self {
            fleet,
            pool,
            github,
            cfg,
        }
    }

    pub async fn dispatch(&self, job: &JobRef) -> Result<(), DispatchError> {
        // ONE fleet listing per dispatch, shared by the cap gate and the
        // pool candidate scan: a webhook burst is N parallel invokes, and
        // doubled ListMicrovms calls are what throttled the control plane.
        // Freshness rides on try_resume's per-candidate GetMicrovm re-check,
        // not on re-listing. Throttle-retried — see Fleet::retry_throttle.
        let vms = if self.cfg.max_concurrency != 0 || self.cfg.pool_enabled {
            self.fleet.retry_throttle(|| self.fleet.list()).await?
        } else {
            Vec::new()
        };
        if self.cfg.max_concurrency != 0 && Fleet::count_running(&vms) >= self.cfg.max_concurrency {
            let err = DispatchError::CapReached {
                cap: self.cfg.max_concurrency,
                job: job.job_id,
            };
            oplog::emit(json!({"outcome": "defer", "job_id": job.job_id,
                               "repo": job.repo, "msg": err.to_string()}));
            return Err(err);
        }

        let token = self
            .github
            .token_for_repo(&job.repo, job.installation)
            .await?;
        // Register with the UNION of the configured set and the job's
        // requested labels: a job asking for the extra "docker" label can
        // only be assigned to a runner registered with it.
        let mut payload = RunPayload::job(
            format!("https://github.com/{}", job.repo),
            token.reveal(),
            &union_labels(&self.cfg.runner_labels, &job.labels),
        );
        // Docker is a per-job capability: the "docker" label always opts
        // in; DOCKER_DEFAULT decides for unlabeled jobs. Set before the
        // pool branch so the parked mailbox copy carries it too.
        payload.enable_docker = Some(wants_docker(&job.labels) || self.cfg.docker_default);
        if self.cfg.pool_enabled {
            payload = payload.with_pool(
                u64::try_from(self.cfg.pool_suspend_grace).unwrap_or(0),
                &self.cfg.handoff_prefix,
            );
            // Pooled VMs report their idleness back by direct invoke (both
            // the cold-launch and mailbox-handoff copies carry this).
            payload.dispatcher_fn = self.cfg.dispatcher_fn.clone();
            if let ResumeOutcome::Resumed = self.pool.try_resume(&payload, &vms).await? {
                return Ok(());
            }
        }

        let microvm_id = self.fleet.launch(&payload).await?;
        oplog::emit(json!({"dispatched": true, "repo": job.repo,
                           "job_id": job.job_id, "microvmId": microvm_id}));
        Ok(())
    }
}

/// Does the job's `runs-on` opt into docker? Case-insensitive with
/// whitespace trimmed, because GitHub's own job→runner label matching is
/// case-insensitive — a `Docker`-labeled job WILL be assigned to this
/// runner, so it had better get dockerd.
fn wants_docker(requested: &LabelSet) -> bool {
    requested
        .iter()
        .any(|l| l.trim().eq_ignore_ascii_case(DOCKER_LABEL))
}

/// The registration label list: the configured `RUNNER_LABELS` (order kept,
/// duplicates dropped) followed by any job-requested labels not already in
/// it (in the set's sorted order). Requested labels that cannot survive the
/// payload's comma-joined `labels` wire format — empty/whitespace-only, or
/// containing a comma — are dropped: a comma label would fragment into
/// bogus registration labels on the entrypoint's split (fragments that
/// could wrongly attract OTHER queued jobs), and it can never match as
/// itself anyway.
fn union_labels(configured: &[String], requested: &LabelSet) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(configured.len() + requested.len());
    let representable = requested
        .iter()
        .filter(|l| !l.trim().is_empty() && !l.contains(','));
    for label in configured.iter().chain(representable) {
        if !out.contains(label) {
            out.push(label.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(labels: &[&str]) -> LabelSet {
        labels.iter().map(|s| s.to_string()).collect()
    }

    fn strs(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn union_keeps_configured_order_and_appends_new_requested() {
        let configured = strs(&["self-hosted", "linux", "arm64", "microvm"]);
        let requested = set(&["self-hosted", "microvm", "docker", "big-disk"]);
        assert_eq!(
            union_labels(&configured, &requested),
            strs(&[
                "self-hosted",
                "linux",
                "arm64",
                "microvm",
                "big-disk",
                "docker"
            ])
        );
    }

    #[test]
    fn union_dedups_and_handles_empty_sides() {
        let configured = strs(&["a", "b", "a"]);
        assert_eq!(union_labels(&configured, &set(&[])), strs(&["a", "b"]));
        assert_eq!(union_labels(&[], &set(&["b", "a"])), strs(&["a", "b"]));
        assert_eq!(union_labels(&[], &set(&[])), Vec::<String>::new());
    }

    #[test]
    fn union_drops_labels_the_comma_joined_wire_cannot_carry() {
        // "docker,gpu" would fragment into "docker" + "gpu" on the
        // entrypoint's split — bogus capabilities that could attract other
        // queued jobs. Empty/whitespace-only labels are noise. In-label
        // spaces are fine (they round-trip through the comma join).
        let requested = set(&["docker,gpu", "", "  ", "big disk"]);
        assert_eq!(
            union_labels(&strs(&["a"]), &requested),
            strs(&["a", "big disk"])
        );
    }

    #[test]
    fn docker_opt_in_matches_like_github_does() {
        // GitHub's job→runner label matching is case-insensitive: a
        // "Docker" job lands on this runner, so it must get dockerd.
        assert!(wants_docker(&set(&["self-hosted", "docker"])));
        assert!(wants_docker(&set(&["Docker"])));
        assert!(wants_docker(&set(&["DOCKER"])));
        assert!(wants_docker(&set(&[" docker "])));
        assert!(!wants_docker(&set(&["self-hosted", "microvm"])));
        assert!(!wants_docker(&set(&["dockerd", "no-docker"])));
        assert!(!wants_docker(&set(&[])));
    }
}
