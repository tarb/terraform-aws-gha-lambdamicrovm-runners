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
use crate::oplog;
use crate::pool::{Pool, ResumeOutcome};
use crate::secrets::SecretsError;

#[derive(Debug, Clone)]
pub struct JobRef {
    pub repo: String,
    pub job_id: Option<i64>,
    pub installation: Option<InstallationId>,
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
        if self.cfg.max_concurrency != 0
            && self.fleet.running_count().await? >= self.cfg.max_concurrency
        {
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
        let mut payload = RunPayload::job(
            format!("https://github.com/{}", job.repo),
            token.reveal(),
            &self.cfg.runner_labels,
        );
        if self.cfg.pool_enabled {
            payload = payload.with_pool(
                u64::try_from(self.cfg.pool_suspend_grace).unwrap_or(0),
                &self.cfg.handoff_prefix,
            );
            if let ResumeOutcome::Resumed = self.pool.try_resume(&payload).await? {
                return Ok(());
            }
        }

        let microvm_id = self.fleet.launch(&payload).await?;
        oplog::emit(json!({"dispatched": true, "repo": job.repo,
                           "job_id": job.job_id, "microvmId": microvm_id}));
        Ok(())
    }
}
