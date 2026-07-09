//! Runner registration + supervision — port of `start_runner`,
//! `_idle_watchdog`, `_job_running` and `deregister`.
//!
//! Key design rule preserved from the Python: the runner is registered with
//! GitHub at /run time (post-snapshot), NEVER at build time.

use crate::aws::terminate_self;
use crate::github::{gh_api, mint_jitconfig, runners_api_for};
use crate::payload::Payload;
use crate::state::{ProcHandle, Sup};
use crate::util::{exit_code, log, py_bool};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::{Instant, sleep};

/// Spawn `start_runner` as a detached task (the Python daemon thread).
/// Boxed dyn future to break the start_runner -> pool_idle_wait ->
/// start_runner type recursion.
pub fn spawn_start_runner(sup: Arc<Sup>, payload: Payload) {
    let fut: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(start_runner(sup, payload));
    tokio::spawn(fut);
}

/// Register + run the runner. Runs detached off /run.
pub async fn start_runner(sup: Arc<Sup>, payload: Payload) {
    sup.new_run.set(); // a pool-idle waiter must stand down immediately
    sup.watchdog_killed.store(false, Ordering::SeqCst);
    if sup.cleaning.is_set() {
        // Resumed mid-cleanup: let the previous cycle's teardown finish
        // before registering (bounded — cleanup is seconds of work).
        for _ in 0..30 {
            if !sup.cleaning.is_set() {
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }
    }
    // Warm up dockerd in the BACKGROUND so it overlaps runner registration +
    // the GitHub job-assignment handshake instead of blocking before them.
    if sup.cfg.enable_docker {
        tokio::spawn(crate::docker::ensure_dockerd(sup.clone()));
    }
    if let Err(e) = run_flow(&sup, &payload).await {
        // The supervisor must not crash the hook server.
        log(format!("start_runner error: {e}"));
        if payload.truthy("pool") {
            // A pooled VM whose run flow failed has NO reaper (no job -> no
            // completed event; no runner -> no watchdog): terminate, never idle.
            terminate_self(&*sup.aws, &payload, &sup.region_label).await;
        }
    }
}

async fn run_flow(sup: &Arc<Sup>, payload: &Payload) -> Result<(), String> {
    let cfg = &sup.cfg;
    let name = derive_runner_name(payload.microvm_id().as_deref());
    let labels = payload.str_or_none("labels").unwrap_or_else(|| {
        std::env::var("RUNNER_LABELS").unwrap_or_else(|_| "self-hosted,linux,arm64,microvm".into())
    });
    let jit = payload.str_or_none("encoded_jit_config").or_else(|| {
        std::env::var("ENCODED_JIT_CONFIG")
            .ok()
            .filter(|s| !s.is_empty())
    });

    let is_ephemeral: bool;
    let cmd: Vec<String>;
    if let Some(jit) = jit {
        log("ephemeral mode: running one job via JIT config");
        is_ephemeral = true; // JIT runners always run exactly one job
        cmd = vec![
            format!("{}/bin/Runner.Listener", cfg.runner_dir),
            "run".into(),
            "--jitconfig".into(),
            jit,
        ];
    } else {
        let github_url = payload
            .str_or_none("github_url")
            .or_else(|| std::env::var("GH_URL").ok().filter(|s| !s.is_empty()));
        let pat = payload
            .str_or_none("token")
            .or_else(|| payload.str_or_none("pat"))
            .or_else(|| std::env::var("GH_PAT").ok().filter(|s| !s.is_empty()));
        let (Some(github_url), Some(pat)) = (github_url, pat) else {
            log("ERROR: /run needs encoded_jit_config OR (github_url + token)");
            return Ok(()); // Python `return`s here — no pool teardown
        };
        let ephemeral = payload.truthy("ephemeral");
        is_ephemeral = ephemeral;
        let api = runners_api_for(&cfg.gh_api, &github_url)?;

        // For an ephemeral runner, mint a JIT config ON-BOX from the token:
        // skips config.sh registration while keeping the big JIT blob off
        // runHookPayload (capped at 4096 bytes). Falls back to config.sh.
        let jitcfg = if ephemeral {
            mint_jitconfig(
                &sup.http,
                &api,
                &pat,
                &name,
                &labels,
                payload.get("runner_group"),
            )
            .await
        } else {
            None
        };
        if let Some(jc) = jitcfg {
            log(format!(
                "ephemeral JIT: on-box jitconfig, running one job (name={name})"
            ));
            cmd = vec![
                format!("{}/bin/Runner.Listener", cfg.runner_dir),
                "run".into(),
                "--jitconfig".into(),
                jc,
            ];
        } else {
            log(format!(
                "minting registration token: {api}/registration-token"
            ));
            let (_, tok) = gh_api(
                &sup.http,
                "POST",
                &format!("{api}/registration-token"),
                &pat,
                None,
            )
            .await?;
            let reg_token = tok
                .get("token")
                .and_then(Value::as_str)
                .ok_or_else(|| "registration-token response missing 'token'".to_string())?
                .to_string();
            let mut cfg_cmd: Vec<String> = vec![
                format!("{}/config.sh", cfg.runner_dir),
                "--unattended".into(),
                "--disableupdate".into(),
                "--url".into(),
                github_url.clone(),
                "--token".into(),
                reg_token,
                "--name".into(),
                name.clone(),
                "--labels".into(),
                labels.clone(),
                "--work".into(),
                "_work".into(),
                "--replace".into(),
            ];
            if ephemeral {
                cfg_cmd.push("--ephemeral".into());
            }
            if payload.truthy("runner_group") {
                cfg_cmd.push("--runnergroup".into());
                cfg_cmd.push(value_as_arg(payload.get("runner_group").unwrap()));
            }
            log(format!(
                "configuring runner '{name}' labels={labels} ephemeral={}",
                py_bool(ephemeral)
            ));
            let status = Command::new(&cfg_cmd[0])
                .args(&cfg_cmd[1..])
                .current_dir(&cfg.runner_dir)
                .env("RUNNER_ALLOW_RUNASROOT", "1")
                .status()
                .await
                .map_err(|e| format!("config.sh: {e}"))?;
            if !status.success() {
                // subprocess.run(check=True) equivalent.
                return Err(format!("config.sh exited rc={}", exit_code(status)));
            }
            {
                let mut s = sup.runner.lock().unwrap();
                s.registered = true;
                s.runners_api = Some(api.clone());
                s.pat = Some(pat.clone());
            }
            cmd = vec![format!("{}/run.sh", cfg.runner_dir)];
        }
    }

    log("launching runner");
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .current_dir(&cfg.runner_dir)
        .env("RUNNER_ALLOW_RUNASROOT", "1")
        .spawn()
        .map_err(|e| format!("launch runner: {e}"))?;
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    let exited = Arc::new(AtomicBool::new(false));
    {
        sup.runner.lock().unwrap().proc = Some(ProcHandle {
            pid,
            exited: exited.clone(),
        });
    }
    if is_ephemeral {
        // Persistent runners legitimately idle between jobs; only the
        // one-job ephemeral flow gets the orphan watchdog.
        tokio::spawn(idle_watchdog(sup.clone(), pid, exited.clone()));
    }
    let status = child
        .wait()
        .await
        .map_err(|e| format!("wait for runner: {e}"))?;
    exited.store(true, Ordering::SeqCst);
    log(format!("runner exited rc={}", exit_code(status)));

    if is_ephemeral {
        if payload.truthy("pool") && !sup.watchdog_killed.load(Ordering::SeqCst) {
            // Warm pool: clean up and wait for the dispatcher to suspend us;
            // a watchdog-killed runner never ran a job — no completed event
            // is coming, so pooling would just burn the full grace window.
            crate::pool::pool_cleanup(sup).await;
            crate::pool::pool_idle_wait(sup.clone(), payload).await?;
        } else {
            // Stop billing now; don't idle to max-duration.
            terminate_self(&*sup.aws, payload, &sup.region_label).await;
        }
    }
    Ok(())
}

/// Runner name for this VM: `types::runner_name` (the fleet-wide normative
/// derivation) when a microvm id is present, otherwise a random suffix like
/// Python's `secrets.token_hex(4)`.
pub fn derive_runner_name(microvm_id: Option<&str>) -> String {
    match microvm_id {
        Some(id) if !id.replace("microvm-", "").is_empty() => types::runner_name(id),
        _ => format!("gha-mvm-{}", random_hex8()),
    }
}

fn random_hex8() -> String {
    use std::io::Read;
    let mut buf = [0u8; 4];
    let read_ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !read_ok {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        buf = (nanos ^ std::process::id()).to_be_bytes();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A `--runnergroup` argv value from a JSON payload field.
fn value_as_arg(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
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

/// `b"Runner.Worker" in cmdline_bytes`.
pub fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Reap a runner that never gets a job. Exits silently the moment a job
/// starts (or the runner dies) — only the never-assigned case is cut short.
/// Killing the Listener routes through the normal exit path, which
/// self-terminates the VM.
async fn idle_watchdog(sup: Arc<Sup>, pid: i32, exited: Arc<AtomicBool>) {
    let grace = sup.cfg.idle_grace_seconds;
    let deadline = Instant::now() + Duration::from_secs(grace.max(0) as u64);
    while Instant::now() < deadline {
        sleep(Duration::from_secs(5)).await;
        if exited.load(Ordering::SeqCst) || job_running() {
            return;
        }
    }
    if !exited.load(Ordering::SeqCst) && !job_running() {
        log(format!(
            "no job started within {grace}s - terminating idle runner (orphaned dispatch)"
        ));
        // Pooled flow must terminate, not idle for suspend.
        sup.watchdog_killed.store(true, Ordering::SeqCst);
        if pid > 0 {
            unsafe {
                let _ = libc::kill(pid, libc::SIGTERM);
            }
        }
    }
}

/// Best-effort de-register + stop on /terminate (persistent mode only).
pub async fn deregister(sup: Arc<Sup>) {
    if let Err(e) = deregister_inner(&sup).await {
        log(format!("deregister error: {e}"));
    }
}

async fn deregister_inner(sup: &Sup) -> Result<(), String> {
    let (registered, api, pat) = {
        let s = sup.runner.lock().unwrap();
        (s.registered, s.runners_api.clone(), s.pat.clone())
    };
    if let (true, Some(api), Some(pat)) = (registered, api, pat) {
        let (_, tok) = gh_api(
            &sup.http,
            "POST",
            &format!("{api}/remove-token"),
            &pat,
            None,
        )
        .await?;
        let remove_token = tok
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| "remove-token response missing 'token'".to_string())?
            .to_string();
        let run = Command::new(format!("{}/config.sh", sup.cfg.runner_dir))
            .args(["remove", "--token", &remove_token])
            .current_dir(&sup.cfg.runner_dir)
            .env("RUNNER_ALLOW_RUNASROOT", "1")
            .status();
        // Python: timeout=30, exit status NOT checked.
        tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .map_err(|_| "config.sh remove timed out after 30s".to_string())?
            .map_err(|e| format!("config.sh remove: {e}"))?;
        log("runner de-registered");
    }
    let proc = { sup.runner.lock().unwrap().proc.clone() };
    if let Some(p) = proc {
        p.terminate();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_name_uses_types_derivation_when_id_present() {
        let id = "microvm-aaaa1111-2222-3333-4444-555566667777";
        assert_eq!(derive_runner_name(Some(id)), types::runner_name(id));
        assert_eq!(derive_runner_name(Some(id)), "gha-mvm-aaaa1111-2222-3333");
        assert_eq!(derive_runner_name(Some("deadbeef")), "gha-mvm-deadbeef");
    }

    #[test]
    fn runner_name_falls_back_to_random_hex_suffix() {
        for id in [None, Some(""), Some("microvm-")] {
            let name = derive_runner_name(id);
            let suffix = name.strip_prefix("gha-mvm-").expect("prefix");
            assert_eq!(suffix.len(), 8, "token_hex(4) is 8 chars: {name}");
            assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }

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
}
