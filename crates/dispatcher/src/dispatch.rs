//! Shared dispatch: concurrency gate -> pool resume -> cold launch.

use serde_json::{Value, json};
use types::RunPayload;

use crate::app::App;
use crate::platform::{Platform, RunVmRequest};
use crate::pyfmt::{PyErr, dumps, logln, py_str};

impl<P: Platform> App<'_, P> {
    /// `_dispatch_job`: RAISES (returns Err) on retryable failure — on the
    /// SQS path the message then returns to the queue, which IS the job queue.
    pub async fn dispatch_job(
        &self,
        repo: &str,
        job_id: &Value,
        installation_id: Option<i64>,
    ) -> Result<(), PyErr> {
        if self.cfg.max_concurrency != 0 && self.running_count().await? >= self.cfg.max_concurrency
        {
            let msg = format!(
                "concurrency cap {} reached; job {} waits for retry",
                self.cfg.max_concurrency,
                py_str(job_id)
            );
            logln(&json!({"outcome": "defer", "job_id": job_id, "repo": repo, "msg": msg}));
            return Err(PyErr::runtime(msg));
        }

        let secret = self.secrets().await?;
        let token = self.token_for_repo(&secret, repo, installation_id).await?;
        let mut run_payload = RunPayload::new(
            format!("https://github.com/{repo}"),
            token,
            self.cfg.runner_labels.join(","),
        );
        if self.cfg.pool_enabled {
            run_payload.pool = true;
            run_payload.pool_grace = Some(u64::try_from(self.cfg.pool_suspend_grace).unwrap_or(0));
            // Where the VM polls for its next job after a resume (pull handoff).
            run_payload.handoff_prefix = Some(self.cfg.handoff_prefix.clone());
            if self.try_pool_resume(&run_payload).await? {
                return Ok(());
            }
        }

        let hook = dumps(&serde_json::to_value(&run_payload).map_err(PyErr::json_error)?);
        let microvm_id = self.run_microvm_with_retry(&hook).await?;
        logln(&json!({"dispatched": true, "repo": repo, "job_id": job_id,
                      "microvmId": microvm_id}));
        Ok(())
    }

    /// `_run_microvm_with_retry`: IAM PassRole propagation can briefly deny
    /// RunMicrovm right after a policy edit; retry transient
    /// AccessDeniedException a few times.
    async fn run_microvm_with_retry(
        &self,
        run_hook_payload: &str,
    ) -> Result<Option<String>, PyErr> {
        let image_version = self.image_version().await?;
        let mut attempt = 0;
        loop {
            let req = RunVmRequest {
                image_arn: &self.cfg.image_arn,
                image_version: &image_version,
                exec_role_arn: &self.cfg.exec_role_arn,
                egress: &self.cfg.egress,
                max_duration: self.cfg.max_duration,
                run_hook_payload,
                log_group: &self.cfg.log_group,
            };
            match self.p.mv_run(req).await {
                Ok(id) => return Ok(id),
                Err(e) if e.is_code("AccessDeniedException") && attempt < 3 => {
                    self.p.sleep(1.5).await;
                    attempt += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}
