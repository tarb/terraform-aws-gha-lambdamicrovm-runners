//! Ground-truth reconciliation sweep: stale-queued re-dispatch, zombie
//! reaper, handoff-parameter GC, pool GC. GitHub's queued jobs are the source
//! of truth; every scope is individually guarded — one broken repo must not
//! blind the sweep to the rest.

use serde_json::{Value, json};
use std::collections::HashSet;

use crate::app::{App, recently_resumed};
use crate::platform::{Platform, ignore_service};
use crate::pyfmt::{PyErr, logln, trunc, v_index};
use crate::timeparse::parse_gh_time;

impl<P: Platform> App<'_, P> {
    /// `_sweep(context)`. `low_time` mirrors the Lambda-deadline check
    /// (`context.get_remaining_time_in_millis() < 15000`); pass `|| false`
    /// when there is no deadline (Python `context=None`).
    pub async fn sweep(&self, low_time: &(dyn Fn() -> bool + Sync)) -> Result<i64, PyErr> {
        let secret = self.secrets().await?;
        let app_jwt = self.app_jwt_from_secret(&secret)?;
        let (_, installations) = self
            .p
            .gh_call(
                "GET",
                &format!("{}/app/installations", self.cfg.gh_api),
                &app_jwt,
                None,
            )
            .await?;
        let mut dispatched = 0i64;
        let now = self.p.now() as i64;
        // Registered runner names across every scanned repo, for the zombie
        // reaper below. scan_complete guards the reaper: a partial view could
        // misread a mid-job VM as a zombie.
        let mut registered_runners: HashSet<String> = HashSet::new();
        let mut scan_complete = true;

        let installs: Vec<Value> = match installations {
            Value::Null => vec![],
            Value::Array(a) => a,
            _ => return Err(PyErr::type_error("installations response is not a list")),
        };
        'outer: for inst in installs.iter().take(10) {
            if low_time() {
                logln(&json!({"sweep": "deadline-bail", "at": "installations"}));
                scan_complete = false;
                break;
            }
            let (token, repos) = match self.installation_repos(&secret, inst).await {
                Ok(x) => x,
                Err(e) => {
                    logln(&json!({"sweep": "installation-failed",
                                  "installation": inst.get("id").cloned().unwrap_or(Value::Null),
                                  "err": trunc(&e.msg, 150)}));
                    scan_complete = false;
                    continue;
                }
            };
            let inst_id = inst.get("id").and_then(Value::as_i64);
            let empty = vec![];
            let repo_list = repos
                .get("repositories")
                .and_then(Value::as_array)
                .unwrap_or(&empty);
            for repo in repo_list.iter().take(100) {
                if low_time() {
                    logln(&json!({"sweep": "deadline-bail",
                                  "at": repo.get("full_name").cloned().unwrap_or(Value::Null)}));
                    scan_complete = false;
                    continue 'outer;
                }
                let full = v_index(repo, "full_name")?
                    .as_str()
                    .ok_or_else(|| PyErr::type_error("full_name is not a string"))?;
                // Runner names for the reaper; a failure here poisons only
                // scan_complete, not the stale-job scan below.
                let runners_url = format!(
                    "{}/repos/{}/actions/runners?per_page=100",
                    self.cfg.gh_api, full
                );
                match self.p.gh_call("GET", &runners_url, &token, None).await {
                    Ok((_, rr)) => {
                        for r in rr
                            .get("runners")
                            .and_then(Value::as_array)
                            .unwrap_or(&empty)
                        {
                            if let Some(name) = r.get("name").and_then(Value::as_str) {
                                registered_runners.insert(name.to_string());
                            }
                        }
                    }
                    Err(e) => {
                        logln(&json!({"sweep": "runner-list-failed", "repo": full,
                                      "err": trunc(&e.msg, 150)}));
                        scan_complete = false;
                    }
                }
                if let Err(e) = self
                    .scan_repo_jobs(full, &token, inst_id, now, &mut dispatched)
                    .await
                {
                    // one repo must not kill the sweep
                    logln(
                        &json!({"sweep": "repo-failed", "repo": full, "err": trunc(&e.msg, 150)}),
                    );
                    continue;
                }
            }
        }

        // Zombie reaper: a PENDING/RUNNING VM whose runner name is registered
        // in NO scanned repo has no job and no way to get one. Only acts on a
        // COMPLETE scan, and only past 2x SWEEP_MIN_AGE.
        if scan_complete && let Err(e) = self.reap_zombies(&registered_runners).await {
            logln(&json!({"sweep": "zombie-reap-failed", "err": trunc(&e.msg, 150)}));
        }

        // Orphaned handoff parameters: written but never claimed AND never
        // cleaned. Tiny, but they must not accumulate or shadow a future VM id.
        if self.cfg.pool_enabled
            && let Err(e) = self.gc_handoff_params().await
        {
            logln(&json!({"sweep": "handoff-gc-failed", "err": trunc(&e.msg, 150)}));
        }

        // Pool GC: stale-image suspended VMs can never be safely resumed, and
        // aged ones must not depend on job traffic to be reaped.
        if self.cfg.pool_enabled
            && let Err(e) = self.gc_pool().await
        {
            logln(&json!({"sweep": "pool-gc-failed", "err": trunc(&e.msg, 150)}));
        }
        logln(&json!({"sweep": "done", "dispatched": dispatched}));
        Ok(dispatched)
    }

    /// The per-installation guarded prologue: mint a token, list repos.
    async fn installation_repos(
        &self,
        secret: &Value,
        inst: &Value,
    ) -> Result<(String, Value), PyErr> {
        let iid = v_index(inst, "id")?
            .as_i64()
            .ok_or_else(|| PyErr::type_error("installation id is not an integer"))?;
        let token = self.installation_token(secret, iid).await?;
        let url = format!("{}/installation/repositories?per_page=100", self.cfg.gh_api);
        let (_, repos) = self.p.gh_call("GET", &url, &token, None).await?;
        Ok((token, repos))
    }

    /// Guarded per-repo stale-queued-job scan; queued runs AND in_progress
    /// runs (a multi-job run with one job executing elsewhere still holds
    /// queued jobs).
    async fn scan_repo_jobs(
        &self,
        full: &str,
        token: &str,
        inst_id: Option<i64>,
        now: i64,
        dispatched: &mut i64,
    ) -> Result<(), PyErr> {
        let empty = vec![];
        let mut runs: Vec<Value> = Vec::new();
        for status in ["queued", "in_progress"] {
            let url = format!(
                "{}/repos/{}/actions/runs?status={}&per_page=30",
                self.cfg.gh_api, full, status
            );
            let (_, r) = self.p.gh_call("GET", &url, token, None).await?;
            runs.extend(
                r.get("workflow_runs")
                    .and_then(Value::as_array)
                    .unwrap_or(&empty)
                    .iter()
                    .cloned(),
            );
        }
        for run in &runs {
            let run_id = v_index(run, "id")?;
            let url = format!(
                "{}/repos/{}/actions/runs/{}/jobs?per_page=100",
                self.cfg.gh_api,
                full,
                crate::pyfmt::py_str(run_id)
            );
            let (_, jobs) = self.p.gh_call("GET", &url, token, None).await?;
            for job in jobs.get("jobs").and_then(Value::as_array).unwrap_or(&empty) {
                if job.get("status").and_then(Value::as_str) != Some("queued") {
                    continue;
                }
                let labels: std::collections::BTreeSet<String> = job
                    .get("labels")
                    .and_then(Value::as_array)
                    .unwrap_or(&empty)
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                if !self.cfg.required_labels.is_subset(&labels) {
                    continue;
                }
                let started = job
                    .get("created_at")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| run.get("created_at").and_then(Value::as_str));
                let age = started
                    .and_then(|s| parse_gh_time(s).ok())
                    .map(|ts| now - ts)
                    .unwrap_or(self.cfg.sweep_min_age + 1);
                if age < self.cfg.sweep_min_age {
                    continue;
                }
                let job_id = v_index(job, "id")?;
                logln(
                    &json!({"sweep": "stale-queued-job", "repo": full, "job_id": job_id,
                              "age": age}),
                );
                match self.dispatch_job(full, job_id, inst_id).await {
                    Ok(()) => *dispatched += 1,
                    Err(e) => {
                        // one job must not kill the sweep
                        logln(&json!({"sweep": "dispatch-failed", "job_id": job_id,
                                      "err": trunc(&e.msg, 200)}));
                    }
                }
            }
        }
        Ok(())
    }

    async fn reap_zombies(&self, registered_runners: &HashSet<String>) -> Result<(), PyErr> {
        for vm in self.list_vms().await? {
            if !(vm.state == "PENDING" || vm.state == "RUNNING") || vm.id.is_empty() {
                continue;
            }
            let Some(born) = vm.started_at else { continue };
            if born == 0.0 || (self.p.now() - born) < (2 * self.cfg.sweep_min_age) as f64 {
                continue;
            }
            // Must mirror the entrypoint's runner-name derivation exactly.
            let name = types::runner_name(&vm.id);
            if registered_runners.contains(&name) {
                continue; // has, had, or is waiting on a job — the watchdog's turf
            }
            let recently = recently_resumed()
                .lock()
                .unwrap()
                .get(&vm.id)
                .copied()
                .unwrap_or(0.0);
            if self.p.now() - recently < 600.0 {
                continue; // just woken by this container — registration in flight
            }
            logln(&json!({"sweep": "reap-zombie-vm", "microvmId": vm.id,
                          "age": (self.p.now() - born) as i64}));
            ignore_service(self.p.mv_terminate(&vm.id).await)?;
        }
        Ok(())
    }

    async fn gc_handoff_params(&self) -> Result<(), PyErr> {
        let params = self
            .p
            .ssm_by_path(&self.cfg.handoff_prefix)
            .await
            .map_err(PyErr::from)?;
        for p in params {
            let Some(lm) = p.last_modified else { continue };
            if self.p.now() - lm > 3600.0 {
                logln(&json!({"sweep": "gc-handoff-param",
                              "name": p.name.clone().map(Value::String).unwrap_or(Value::Null)}));
                let name = p.name.ok_or_else(|| PyErr::key_error("Name"))?;
                ignore_service(self.p.ssm_delete(&name).await)?;
            }
        }
        Ok(())
    }

    async fn gc_pool(&self) -> Result<(), PyErr> {
        let current = self.image_version().await?;
        for vm in self.list_vms().await? {
            if vm.state != "SUSPENDED" {
                continue;
            }
            let mut reason: Option<&str> = None;
            if !vm.image_version.is_empty() && vm.image_version != current {
                reason = Some("stale-image");
            } else if let Some(born) = vm.started_at
                && born != 0.0
                && (self.p.now() - born) > self.cfg.eol_threshold()
            {
                reason = Some("near-eol");
            }
            if let Some(reason) = reason {
                logln(&json!({"sweep": "gc-pool-vm", "reason": reason, "microvmId": vm.id}));
                ignore_service(self.p.mv_terminate(&vm.id).await)?;
            }
        }
        Ok(())
    }
}
