//! dockerd lifecycle — port of `start_dockerd` / `ensure_dockerd` and their
//! helpers. dockerd is started fresh per job (never baked into the snapshot,
//! whose bridge/NAT/DNS would be stale on a resumed host), with the
//! link-local Amazon resolver pinned and `--default-ulimit` matching our
//! achieved nofile limits.

use crate::config::{Config, env_or};
use crate::rlimit::container_default_nofile;
use crate::state::{DockerState, Sup};
use crate::util::{exit_code, log};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

const DOCKERD_LOG: &str = "/tmp/dockerd.log";

/// `docker info` succeeding is THE readiness probe.
pub async fn docker_info_ok() -> bool {
    Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Dump the tail of dockerd's log to CloudWatch, so a startup crash is
/// visible even after the self-terminating MicroVM is gone.
fn log_dockerd_tail(n: usize) {
    match std::fs::read_to_string(DOCKERD_LOG) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(n);
            for ln in &lines[start..] {
                log(format!("  dockerd| {}", ln.trim_end()));
            }
        }
        Err(e) => log(format!("  (could not read /tmp/dockerd.log: {e})")),
    }
}

/// No systemd in the guest, so nothing mounts the unified cgroup hierarchy
/// containerd/runc need. Mount cgroup2 at /sys/fs/cgroup only if neither
/// cgroup2 nor a v1 hierarchy is already present.
async fn ensure_cgroup2() {
    if std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        return; // cgroup2 unified already mounted
    }
    if std::path::Path::new("/sys/fs/cgroup/memory").is_dir() {
        return; // cgroup v1 hybrid already mounted; leave it
    }
    if let Err(e) = std::fs::create_dir_all("/sys/fs/cgroup") {
        log(format!("cgroup2 mount attempt failed (continuing): {e}"));
        return;
    }
    match Command::new("mount")
        .args(["-t", "cgroup2", "none", "/sys/fs/cgroup"])
        .status()
        .await
    {
        Ok(status) => log(format!(
            "mounted cgroup2 at /sys/fs/cgroup (rc={})",
            exit_code(status)
        )),
        Err(e) => log(format!("cgroup2 mount attempt failed (continuing): {e}")),
    }
}

/// dockerd teardown leaves its managed containerd orphaned; reap both and
/// clear their sockets for a clean pass.
pub async fn kill_stale_runtimes() -> Result<(), String> {
    for name in ["dockerd", "containerd"] {
        Command::new("pkill")
            .args(["-9", "-x", name])
            .status()
            .await
            .map_err(|e| format!("pkill {name}: {e}"))?;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    for sock in [
        "/var/run/docker.sock",
        "/var/run/docker/containerd/containerd.sock",
        "/run/containerd/containerd.sock",
    ] {
        let _ = std::fs::remove_file(sock);
    }
    Ok(())
}

/// On total failure, dump cgroup/mount state so the next iteration diagnoses
/// from evidence rather than guesswork.
async fn log_runtime_diag() {
    let probes = [
        ("filesystems", "grep cgroup /proc/filesystems || true"),
        ("cgroup-mounts", "mount | grep cgroup || true"),
        ("cgroup-ls", "ls -la /sys/fs/cgroup 2>&1 | head -20"),
    ];
    for (label, script) in probes {
        let run = Command::new("sh").args(["-c", script]).output();
        match tokio::time::timeout(Duration::from_secs(5), run).await {
            Ok(Ok(out)) => {
                for ln in String::from_utf8_lossy(&out.stdout).lines() {
                    log(format!("  diag[{label}]| {ln}"));
                }
            }
            Ok(Err(e)) => log(format!("  diag[{label}]| ({e})")),
            Err(_) => log(format!("  diag[{label}]| (timed out after 5s)")),
        }
    }
}

/// One pass over the storage drivers. Returns true if dockerd became ready.
async fn start_dockerd_pass(cfg: &Config, state: &mut DockerState) -> bool {
    if let Err(e) = std::fs::create_dir_all("/var/run") {
        log(format!("could not create /var/run: {e}"));
    }
    let logf = match std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(DOCKERD_LOG)
    {
        Ok(f) => f,
        Err(e) => {
            log(format!("could not open {DOCKERD_LOG}: {e}"));
            return false;
        }
    };
    let mut drivers = vec![cfg.docker_storage_driver.clone()];
    if cfg.docker_storage_driver != "vfs" {
        drivers.push("vfs".to_string());
    }
    for driver in &drivers {
        let (out, err) = match (logf.try_clone(), logf.try_clone()) {
            (Ok(a), Ok(b)) => (a, b),
            _ => {
                log(format!("could not clone {DOCKERD_LOG} handle"));
                return false;
            }
        };
        log(format!("starting dockerd (storage-driver={driver})"));
        let spawned = Command::new("dockerd")
            .arg("--host=unix:///var/run/docker.sock")
            .arg(format!("--storage-driver={driver}"))
            .args(["--exec-opt", "native.cgroupdriver=cgroupfs"])
            .args(["--default-ulimit", &container_default_nofile(cfg)])
            // Amazon link-local resolver (UDP-to-public is blocked).
            .args(["--dns", "169.254.169.253"])
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                log(format!(
                    "dockerd spawn failed (storage-driver={driver}): {e}"
                ));
                continue;
            }
        };
        let mut child_exited = false;
        for _ in 0..30 {
            if docker_info_ok().await {
                state.ready = true;
                log(format!("dockerd ready (storage-driver={driver})"));
                state.child = Some(child);
                return true;
            }
            if let Ok(Some(status)) = child.try_wait() {
                log(format!(
                    "dockerd exited (storage-driver={driver}, rc={}); trying next",
                    exit_code(status)
                ));
                log_dockerd_tail(25); // surface WHY it crashed
                child_exited = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if !child_exited {
            // Alive but never became ready: terminate, then kill if stuck.
            if let Some(pid) = child.id() {
                unsafe {
                    let _ = libc::kill(pid as i32, libc::SIGTERM);
                }
            }
            if tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }
    false
}

/// Start dockerd fresh in the LIVE MicroVM, retrying a few times: on a
/// freshly-resumed MicroVM dockerd can crash if its bridge/iptables setup
/// runs before the network has settled.
pub async fn start_dockerd(cfg: &Config, state: &mut DockerState) {
    if !cfg.enable_docker || docker_info_ok().await {
        state.ready = cfg.enable_docker;
        return;
    }
    ensure_cgroup2().await;
    let attempts: u32 = env_or("DOCKERD_START_ATTEMPTS", "4").parse().unwrap_or(4);
    for attempt in 1..=attempts {
        if start_dockerd_pass(cfg, state).await {
            return;
        }
        if attempt < attempts {
            log(format!(
                "dockerd not ready (attempt {attempt}/{attempts}); reaping stale runtimes, retrying in 3s"
            ));
            if let Err(e) = kill_stale_runtimes().await {
                log(format!("reaping stale runtimes failed: {e}"));
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
    log("WARN: dockerd did not become ready after retries; see 'dockerd|' lines above");
    log_runtime_diag().await;
}

/// Bring docker up for the upcoming job (spawned in the background off
/// `start_runner` so it overlaps registration; the job's first step still
/// can't race the daemon thanks to the wait-for-docker job-started hook).
pub async fn ensure_dockerd(sup: Arc<Sup>) {
    if !sup.cfg.enable_docker {
        return;
    }
    let mut state = sup.docker.lock().await;
    if docker_info_ok().await {
        state.ready = true;
        return;
    }
    log("starting dockerd for this job");
    start_dockerd(&sup.cfg, &mut state).await;
}
