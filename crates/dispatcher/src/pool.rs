//! Warm pool: pull-handoff resume and the suspend intake for completed jobs.

use serde_json::{Value, json};
use types::RunPayload;

use crate::app::{App, recently_resumed};
use crate::platform::{Platform, ignore_service};
use crate::pyfmt::{PyErr, dumps, logln, py_repr_str, trunc, truthy};

/// Round to one decimal, matching Python `round(x, 1)` closely enough for the
/// `handoff_seconds` log value.
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

impl<P: Platform> App<'_, P> {
    /// `_try_pool_resume`: resume ONE suspended, current-image VM and hand it
    /// the job by PULL: park the payload in SSM BEFORE resuming, then poll for
    /// the parameter's disappearance (delete = ack). Per-candidate failures
    /// degrade to the next candidate or cold launch; an unclaimed handoff
    /// terminates the VM and cold-launches.
    pub async fn try_pool_resume(&self, run_payload: &RunPayload) -> Result<bool, PyErr> {
        let current = self.image_version().await?;
        for vm in self.list_vms().await? {
            if vm.state != "SUSPENDED" || vm.id.is_empty() {
                continue;
            }
            if !vm.image_version.is_empty() && vm.image_version != current {
                logln(&json!({"pool": "terminate-stale", "microvmId": vm.id}));
                ignore_service(self.p.mv_terminate(&vm.id).await)?;
                continue;
            }
            // A VM near its max lifetime would die MID-JOB if resumed.
            if let Some(born) = vm.started_at
                && born != 0.0
                && (self.p.now() - born) > self.cfg.eol_threshold()
            {
                logln(&json!({"pool": "terminate-near-eol", "microvmId": vm.id}));
                ignore_service(self.p.mv_terminate(&vm.id).await)?;
                continue;
            }
            // Money-risky steps happen BEFORE resume. Failures here move to
            // the NEXT candidate — one bad VM must not wedge the whole pool.
            let detail_state = match self.p.mv_get_state(&vm.id).await {
                Ok(s) => s.to_uppercase(),
                Err(e) if e.service => {
                    logln(&json!({"pool": "describe-failed", "microvmId": vm.id,
                                  "err": trunc(&e.message, 200)}));
                    continue; // transient — try the next candidate
                }
                Err(e) => return Err(e.into()),
            };
            if detail_state != "SUSPENDED" {
                continue; // raced: another invocation claimed it — not ours to touch
            }
            let param = format!("{}/{}", self.cfg.handoff_prefix, vm.id);
            let mut payload = run_payload.clone();
            payload.microvm_id = Some(vm.id.clone());
            let value = dumps(&serde_json::to_value(&payload).map_err(PyErr::json_error)?);
            // SecureString: the payload carries a GitHub installation token.
            if let Err(e) = self.p.ssm_put_secure(&param, &value).await {
                if !e.service {
                    return Err(e.into());
                }
                logln(&json!({"pool": "handoff-put-failed", "microvmId": vm.id,
                              "err": trunc(&e.message, 200)}));
                continue;
            }
            if let Err(e) = self.p.mv_resume(&vm.id).await {
                if !e.service {
                    return Err(e.into());
                }
                // Lost a race or mid-transition.
                logln(&json!({"pool": "resume-failed", "microvmId": vm.id,
                              "err": trunc(&e.message, 200)}));
                ignore_service(self.p.ssm_delete(&param).await)?;
                continue;
            }
            // The sweep's zombie reaper must not eat a VM this container just woke.
            recently_resumed()
                .lock()
                .unwrap()
                .insert(vm.id.clone(), self.p.now());
            // The woken VM polls its parameter every <=5s and DELETES it to
            // claim; disappearance is the ack.
            let woke = self.p.now();
            while self.p.now() - woke < self.cfg.handoff_window as f64 {
                match self.p.ssm_get_parameter(&param, false).await {
                    Err(e) if e.is_code("ParameterNotFound") => {
                        logln(&json!({"pool": "resumed", "microvmId": vm.id,
                                      "handoff_seconds": round1(self.p.now() - woke)}));
                        return Ok(true);
                    }
                    Err(e) if !e.service => return Err(e.into()),
                    _ => {} // present, or transient read error — keep polling
                }
                self.p.sleep(3.0).await;
            }
            // Unclaimed: clean up and terminate rather than bill jobless.
            // Return (not continue): don't chain-burn more pool VMs on what
            // may be a systemic handoff problem.
            logln(
                &json!({"pool": "handoff-unclaimed-terminating", "microvmId": vm.id,
                          "window": self.cfg.handoff_window}),
            );
            ignore_service(self.p.ssm_delete(&param).await)?;
            ignore_service(self.p.mv_terminate(&vm.id).await)?;
            return Ok(false);
        }
        Ok(false)
    }

    /// `_handle_completed`: warm pool intake — NEVER raises.
    pub async fn handle_completed(&self, payload: &Value) -> String {
        match self.handle_completed_inner(payload).await {
            Ok(s) => s,
            Err(e) => {
                logln(&json!({"pool": "completed-intake-error", "err": trunc(&e.msg, 200)}));
                "error (benign)".to_string()
            }
        }
    }

    async fn handle_completed_inner(&self, payload: &Value) -> Result<String, PyErr> {
        if !self.cfg.pool_enabled {
            return Ok("pool disabled".to_string());
        }
        let job = payload.get("workflow_job").cloned().unwrap_or(Value::Null);
        let runner = job
            .get("runner_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let repo = payload
            .get("repository")
            .and_then(|r| r.get("full_name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(prefix) = runner.strip_prefix("gha-mvm-") else {
            return Ok(format!("not ours: {}", py_repr_str(&runner)));
        };
        let vms = self.list_vms().await?;
        let Some(vm) = vms
            .iter()
            .find(|v| !v.id.is_empty() && v.id.contains(prefix))
            .cloned()
        else {
            return Ok(format!("vm for {runner} already gone"));
        };
        if vm.is_pooled_state() {
            return Ok("already suspended".to_string());
        }
        if vm.state == "TERMINATING" || vm.state == "TERMINATED" {
            return Ok(format!("vm for {runner} already gone"));
        }
        if (vms.iter().filter(|v| v.is_pooled_state()).count() as i64) >= self.cfg.pool_max_size {
            logln(&json!({"pool": "full-terminating", "microvmId": vm.id}));
            ignore_service(self.p.mv_terminate(&vm.id).await)?;
            return Ok("pool full - terminated".to_string());
        }
        // Let the entrypoint finish post-job cleanup.
        self.p.sleep(self.cfg.suspend_delay as f64).await;
        // Re-check the cap AFTER the delay: N jobs completing together all
        // pass the pre-sleep check and would overshoot the pool by N.
        let after: i64 = self
            .list_vms()
            .await?
            .iter()
            .filter(|v| v.is_pooled_state())
            .count() as i64;
        if after >= self.cfg.pool_max_size {
            logln(&json!({"pool": "full-terminating", "microvmId": vm.id}));
            ignore_service(self.p.mv_terminate(&vm.id).await)?;
            return Ok("pool full - terminated".to_string());
        }
        // A duplicate/late event must not freeze a VM that took a NEW job:
        // the reused VM re-registers the SAME runner name, so busy == in use.
        match self.runner_busy(repo.as_deref(), payload, &runner).await {
            Ok(true) => return Ok("runner busy with a new job - not suspending".to_string()),
            Ok(false) => {}
            Err(e) => {
                // best-effort check
                logln(&json!({"pool": "busy-check-failed", "err": trunc(&e.msg, 150)}));
            }
        }
        self.p.mv_suspend(&vm.id).await.map_err(PyErr::from)?;
        logln(&json!({"pool": "suspended", "microvmId": vm.id}));
        Ok("suspended".to_string())
    }

    async fn runner_busy(
        &self,
        repo: Option<&str>,
        payload: &Value,
        runner: &str,
    ) -> Result<bool, PyErr> {
        let Some(repo) = repo.filter(|r| !r.is_empty()) else {
            return Ok(false);
        };
        let secret = self.secrets().await?;
        let installation_id = payload
            .get("installation")
            .and_then(|i| i.get("id"))
            .and_then(Value::as_i64);
        let token = self.token_for_repo(&secret, repo, installation_id).await?;
        let url = format!(
            "{}/repos/{}/actions/runners?per_page=100",
            self.cfg.gh_api, repo
        );
        let (_, runners) = self.p.gh_call("GET", &url, &token, None).await?;
        let empty = vec![];
        let list = runners
            .get("runners")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        let rec = list
            .iter()
            .find(|r| r.get("name").and_then(Value::as_str) == Some(runner));
        Ok(rec.is_some_and(|r| truthy(r.get("busy").unwrap_or(&Value::Null))))
    }
}
