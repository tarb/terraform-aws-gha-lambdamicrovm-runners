//! Runner process supervision: the /run task, the child process handle,
//! the never-got-a-job watchdog, and /terminate de-registration.

use crate::logfmt::log;
use crate::payload::RunConfig;
use crate::registration::{LaunchError, LaunchPlan, PreparedRunner};
use crate::state::AppState;
use crate::{pool, report};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reqwest::Method;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::{Instant, sleep};
use types::IdleReason;

/// Spawn `run_task` detached. Boxed dyn future to break the
/// run_task -> pool_idle_wait -> run_task type recursion.
pub fn spawn_run_task(app: Arc<AppState>, cfg: RunConfig) {
    let fut: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(run_task(app, cfg));
    tokio::spawn(fut);
}

/// One run, end to end. Runs detached off `/run`; must never crash the
/// hook server.
pub async fn run_task(app: Arc<AppState>, cfg: RunConfig) {
    // An idle pool waiter must stand down the INSTANT a run arrives.
    app.gate.announce_run();
    // Resumed mid-cleanup: let the previous cycle's teardown finish before
    // registering (bounded — cleanup is seconds of work).
    app.gate.await_cleaning_done(Duration::from_secs(30)).await;
    // Docker is a PER-RUN capability (payload decision, ENABLE_DOCKER env
    // fallback): a non-docker run never pays dockerd's startup — the
    // page-in-heavy part of a cold boot. Warm it up in the BACKGROUND so it
    // overlaps registration and the GitHub job-assignment handshake instead
    // of blocking before them.
    if cfg.docker_enabled(&app.cfg) {
        tokio::spawn(app.docker.clone().ensure());
    }
    if let Err(e) = flow(&app, &cfg).await {
        log(format!("start_runner error: {e}"));
        if cfg.pool {
            // A pooled VM whose run flow failed has NO reaper (no job -> no
            // completed event; no runner -> no watchdog): report the orphan
            // (falling back to self-terminate), never idle.
            report::report_idle_or_terminate(&*app.aws, &cfg, &app.region, IdleReason::Orphan)
                .await;
        }
    }
}

async fn flow(app: &Arc<AppState>, cfg: &RunConfig) -> Result<(), LaunchError> {
    let Some(plan) = LaunchPlan::decide(cfg) else {
        // Missing credentials is a logged NO-OP, not a failure — no pool
        // teardown here (the zombie reaper / max-duration reap the VM).
        log("ERROR: /run needs encoded_jit_config OR (github_url + token)");
        return Ok(());
    };
    let prepared = plan.prepare(&app.http, &app.cfg).await?;
    let ephemeral = prepared.is_ephemeral();
    if let PreparedRunner::Registered { dereg, .. } = &prepared {
        app.runner.lock().unwrap().registration = Some(dereg.clone());
    }
    log("launching runner");
    let proc = spawn(
        prepared.command(),
        &app.cfg.runner_dir,
        cfg.docker_enabled(&app.cfg),
    )?;
    app.runner.lock().unwrap().proc = Some(proc.handle());
    if ephemeral {
        // Persistent runners legitimately idle between jobs; only the
        // one-job ephemeral flow gets the orphan watchdog.
        tokio::spawn(idle_watchdog(proc.handle(), app.cfg.idle_grace_seconds));
    }
    let exit = proc.wait().await?;
    log(format!("runner exited rc={}", exit.code));
    match ExitPolicy::decide(cfg.pool, ephemeral, exit.watchdog_fired) {
        ExitPolicy::PoolWait => {
            pool::pool_cleanup(app).await;
            // Report idle FIRST (cleanup is done, so no dispatcher-side
            // delay applies): the suspend then arrives in seconds instead of
            // riding the completed webhook, which stays as the backup. The
            // idle wait below — mailbox polling, stand-down guard, final
            // claim — is unchanged; a failed report just means the webhook
            // path (or grace expiry) handles us as before.
            report::report_idle(&*app.aws, cfg, IdleReason::JobComplete).await;
            pool::pool_idle_wait(app.clone(), cfg).await;
        }
        // Stop billing now; don't idle to max-duration. A watchdog-killed
        // runner never ran a job: it reports as an orphan.
        ExitPolicy::SelfTerminate => {
            let reason = if exit.watchdog_fired {
                IdleReason::Orphan
            } else {
                IdleReason::JobComplete
            };
            report::report_idle_or_terminate(&*app.aws, cfg, &app.region, reason).await;
        }
        ExitPolicy::PersistentIdle => {}
    }
    Ok(())
}

/// What happens after the runner process exits.
#[derive(Debug, PartialEq, Eq)]
pub enum ExitPolicy {
    /// Warm pool: clean up and wait for the dispatcher to suspend us.
    PoolWait,
    SelfTerminate,
    /// Persistent runners idle between jobs by design.
    PersistentIdle,
}

impl ExitPolicy {
    /// A watchdog-killed runner never ran a job — no completed event is
    /// coming, so pooling would just burn the full grace window.
    pub fn decide(pool: bool, ephemeral: bool, watchdog_fired: bool) -> Self {
        if !ephemeral {
            return ExitPolicy::PersistentIdle;
        }
        if pool && !watchdog_fired {
            ExitPolicy::PoolWait
        } else {
            ExitPolicy::SelfTerminate
        }
    }
}

/// A program plus arguments, run from the runner directory with
/// `RUNNER_ALLOW_RUNASROOT=1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// Enough of the running Runner.Listener to poll liveness and SIGTERM it,
/// without owning the child (which the run flow keeps to `wait()` on).
/// `watchdog_fired` is per-process so one run's watchdog can never bleed
/// into the next run's exit decision.
#[derive(Clone)]
pub struct ProcHandle {
    pid: i32,
    exited: Arc<AtomicBool>,
    watchdog_fired: Arc<AtomicBool>,
}

impl ProcHandle {
    pub fn alive(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }

    /// SIGTERM the process if it's still running.
    pub fn terminate(&self) {
        if self.pid > 0 && self.alive() {
            let _ = kill(Pid::from_raw(self.pid), Signal::SIGTERM);
        }
    }
}

pub struct RunnerProcess {
    child: tokio::process::Child,
    handle: ProcHandle,
}

pub struct ExitReport {
    /// Exit code, or negative signal number.
    pub code: i64,
    pub watchdog_fired: bool,
}

/// Spawn the runner. `docker_enabled` decides the job-started hook: the
/// image no longer bakes `ACTIONS_RUNNER_HOOK_JOB_STARTED`, so the wait-
/// for-docker gate exists exactly for the runs that start dockerd — a
/// non-docker run's first step is never stalled by it (env_remove guards
/// against legacy images that still bake the ENV).
pub fn spawn(
    cmd: RunnerCommand,
    cwd: &str,
    docker_enabled: bool,
) -> Result<RunnerProcess, LaunchError> {
    let mut command = tokio::process::Command::new(&cmd.program);
    command
        .args(&cmd.args)
        .current_dir(cwd)
        .env("RUNNER_ALLOW_RUNASROOT", "1");
    if docker_enabled {
        command.env(
            "ACTIONS_RUNNER_HOOK_JOB_STARTED",
            crate::config::WAIT_FOR_DOCKER_HOOK,
        );
    } else {
        command.env_remove("ACTIONS_RUNNER_HOOK_JOB_STARTED");
    }
    let child = command
        .spawn()
        .map_err(|e| LaunchError::Spawn(e.to_string()))?;
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    let handle = ProcHandle {
        pid,
        exited: Arc::new(AtomicBool::new(false)),
        watchdog_fired: Arc::new(AtomicBool::new(false)),
    };
    Ok(RunnerProcess { child, handle })
}

impl RunnerProcess {
    pub fn handle(&self) -> ProcHandle {
        self.handle.clone()
    }

    pub async fn wait(mut self) -> Result<ExitReport, LaunchError> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|e| LaunchError::Wait(e.to_string()))?;
        self.handle.exited.store(true, Ordering::SeqCst);
        Ok(ExitReport {
            code: exit_code(status),
            watchdog_fired: self.handle.watchdog_fired.load(Ordering::SeqCst),
        })
    }
}

/// Exit code, or negative signal number when signal-killed.
pub fn exit_code(status: std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .map(i64::from)
        .unwrap_or_else(|| -i64::from(status.signal().unwrap_or(0)))
}

/// Reap a runner that never gets a job. Exits silently the moment a job
/// starts (or the runner dies) — only the never-assigned case is cut short.
/// SIGTERMing the Listener routes through the normal exit path, which then
/// self-terminates the VM ([`ExitPolicy::decide`] sees `watchdog_fired`).
pub async fn idle_watchdog(handle: ProcHandle, grace_secs: i64) {
    let deadline = Instant::now() + Duration::from_secs(grace_secs.max(0) as u64);
    while Instant::now() < deadline {
        sleep(Duration::from_secs(5)).await;
        if !handle.alive() || job_running() {
            return;
        }
    }
    if handle.alive() && !job_running() {
        log(format!(
            "no job started within {grace_secs}s - terminating idle runner (orphaned dispatch)"
        ));
        handle.watchdog_fired.store(true, Ordering::SeqCst);
        handle.terminate();
    }
}

/// A Runner.Worker process exists exactly while a job is executing.
pub fn job_running() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(format!("/proc/{name}/cmdline"))
            && contains_subslice(&bytes, b"Runner.Worker")
        {
            return true;
        }
    }
    false
}

fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Best-effort de-register + stop on /terminate (config.sh-mode only).
/// Errors are logged, never raised.
pub async fn deregister(app: Arc<AppState>) {
    if let Err(e) = deregister_inner(&app).await {
        log(format!("deregister error: {e}"));
    }
}

async fn deregister_inner(app: &AppState) -> Result<(), String> {
    let registration = { app.runner.lock().unwrap().registration.clone() };
    if let Some(reg) = registration {
        let (_, tok) = crate::github::gh_api(
            &app.http,
            Method::POST,
            &reg.api.url("remove-token"),
            &reg.token,
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
        let remove_token = tok
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| "remove-token response missing 'token'".to_string())?
            .to_string();
        let run = tokio::process::Command::new(format!("{}/config.sh", app.cfg.runner_dir))
            .args(["remove", "--token", &remove_token])
            .current_dir(&app.cfg.runner_dir)
            .env("RUNNER_ALLOW_RUNASROOT", "1")
            .status();
        // Bounded, and the exit status is deliberately NOT checked —
        // teardown is imminent either way.
        tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .map_err(|_| "config.sh remove timed out after 30s".to_string())?
            .map_err(|e| format!("config.sh remove: {e}"))?;
        log("runner de-registered");
    }
    let proc = { app.runner.lock().unwrap().proc.clone() };
    if let Some(p) = proc {
        p.terminate();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testsupport::test_app;
    use serde_json::json;

    #[test]
    fn cmdline_scan_matches_runner_worker_bytes() {
        // /proc cmdline uses NUL separators.
        let cmdline = b"/opt/actions-runner/bin/Runner.Worker\x00spawnclient\x00106\x00109";
        assert!(contains_subslice(cmdline, b"Runner.Worker"));
        assert!(!contains_subslice(
            b"/usr/bin/dockerd\x00",
            b"Runner.Worker"
        ));
        assert!(!contains_subslice(b"", b"Runner.Worker"));
    }

    #[test]
    fn exit_policy_table() {
        use ExitPolicy::*;
        assert_eq!(ExitPolicy::decide(true, true, false), PoolWait);
        // A watchdog-killed pooled runner terminates: no completed event is
        // coming, so pooling would burn the whole grace window.
        assert_eq!(ExitPolicy::decide(true, true, true), SelfTerminate);
        assert_eq!(ExitPolicy::decide(false, true, false), SelfTerminate);
        assert_eq!(ExitPolicy::decide(false, true, true), SelfTerminate);
        assert_eq!(ExitPolicy::decide(true, false, false), PersistentIdle);
        assert_eq!(ExitPolicy::decide(false, false, true), PersistentIdle);
    }

    #[tokio::test]
    async fn spawn_injects_the_hook_env_only_for_docker_runs() {
        use crate::config::WAIT_FOR_DOCKER_HOOK;
        use crate::state::testsupport::temp_dir;
        let dir = temp_dir("spawn-hook-env");
        // Exits 0 iff the child sees the hook env with exactly the baked path.
        let probe = RunnerCommand {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "test \"${{ACTIONS_RUNNER_HOOK_JOB_STARTED:-}}\" = \"{WAIT_FOR_DOCKER_HOOK}\""
                ),
            ],
        };
        let on = spawn(probe.clone(), &dir, true).unwrap();
        assert_eq!(
            on.wait().await.unwrap().code,
            0,
            "docker-enabled run must see the wait-for-docker hook"
        );
        let off = spawn(probe.clone(), &dir, false).unwrap();
        assert_ne!(
            off.wait().await.unwrap().code,
            0,
            "non-docker run must not see the hook env"
        );
        // Legacy images bake the ENV image-wide; a non-docker run on one
        // must still strip it (env_remove), or its first step stalls on the
        // wait-for-docker timeout.
        unsafe { std::env::set_var("ACTIONS_RUNNER_HOOK_JOB_STARTED", WAIT_FOR_DOCKER_HOOK) };
        let legacy = spawn(probe, &dir, false).unwrap();
        let code = legacy.wait().await.unwrap().code;
        unsafe { std::env::remove_var("ACTIONS_RUNNER_HOOK_JOB_STARTED") };
        assert_ne!(code, 0, "a baked image ENV must be stripped");
    }

    #[test]
    fn pooled_transitions_decide_docker_per_run_not_per_vm() {
        // docker job -> suspend -> non-docker job -> suspend -> docker job:
        // each handoff payload resolves independently (pool_cleanup tears
        // dockerd down between jobs; the next run's payload alone decides
        // whether it comes back). An old-dispatcher handoff mid-sequence
        // falls back to the ENABLE_DOCKER env.
        use crate::state::testsupport::test_config;
        let mut env = test_config("/nonexistent");
        env.enable_docker = true; // the image's legacy default
        let seq = [
            (json!({"pool": true, "enable_docker": true}), true),
            (json!({"pool": true, "enable_docker": false}), false),
            (json!({"pool": true, "enable_docker": true}), true),
            (json!({"pool": true}), true), // old dispatcher: env fallback
        ];
        for (payload, want) in seq {
            assert_eq!(
                RunConfig::from_value(payload.clone()).docker_enabled(&env),
                want,
                "{payload}"
            );
        }
        env.enable_docker = false;
        assert!(
            !RunConfig::from_value(json!({"pool": true})).docker_enabled(&env),
            "env fallback follows ENABLE_DOCKER=false too"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pooled_run_flow_failure_self_terminates() {
        let (app, fake) = test_app("/nonexistent");
        // A bare-host github_url fails registration before any network IO.
        let cfg = RunConfig::from_value(json!({
            "github_url": "https://github.com",
            "token": "t",
            "ephemeral": true,
            "pool": true,
            "microvmId": "microvm-x"
        }));
        run_task(app, cfg).await;
        assert_eq!(fake.terminated.lock().unwrap().as_slice(), ["microvm-x"]);
    }

    #[tokio::test(start_paused = true)]
    async fn missing_credentials_is_a_noop_not_a_pool_teardown() {
        let (app, fake) = test_app("/nonexistent");
        let cfg = RunConfig::from_value(json!({"pool": true, "microvmId": "microvm-x"}));
        run_task(app, cfg).await;
        assert!(fake.terminated.lock().unwrap().is_empty());
    }
}
