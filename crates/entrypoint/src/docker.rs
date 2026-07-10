//! dockerd lifecycle. dockerd is started fresh per job (never baked into
//! the snapshot, whose bridge/NAT/DNS would be stale on a resumed host),
//! with the link-local Amazon resolver pinned and `--default-ulimit`
//! matching our achieved nofile limits.
//!
//! Snapshot-restore hazards and where each is handled (so the next reader
//! doesn't re-derive this):
//! - STALE FILES: pid/socket remnants from a pre-snapshot or crashed
//!   dockerd/containerd (a leftover docker.pid aborts dockerd at startup; a
//!   stale docker.sock makes client probes hang instead of failing fast).
//!   Handled by [`remove_stale_runtime_files`] on the pre-start path of
//!   EVERY launch attempt in [`DockerSupervisor::start_pass`], and again in
//!   [`kill_stale_runtimes`] between passes.
//! - WEDGED DAEMON: dockerd alive but never answering. Every `docker info`
//!   probe is bounded ([`PROBE_TIMEOUT`], clamped by [`probe_budget`]) and
//!   each storage driver gets a wall-clock [`READY_WINDOW`], after which the
//!   child is SIGTERMed/killed and the next driver/pass retries — no wait in
//!   the startup path is unbounded.
//! - XTABLES LOCK: nothing in this crate or the image invokes iptables
//!   directly (microvm/Dockerfile only installs the package for dockerd), so
//!   no `-w <seconds>` flag is needed here; dockerd's own iptables calls
//!   handle the lock internally.
//! - NO VFS, EVER: guests have a FIXED 32GB root disk (the microvm API has
//!   no storage knob — Resources is minimum_memory_in_mib only) and
//!   /var/lib/docker persists across pool suspend/resume. The old
//!   overlay2→vfs driver fallback silently degraded a pooled VM to vfs,
//!   whose full-copy layers (no CoW) hit 12GB in one monorepo bake
//!   (ENOSPC) and left the shared data-root corrupted with mixed-driver
//!   metadata. dockerd now runs the configured driver only; a dockerd that
//!   won't start fails LOUDLY (dockerd.log tail in the job log) instead of
//!   degrading, and the VM's bounded lifetime caps the blast radius.

use crate::config::{Config, env_or};
use crate::logfmt::log;
use crate::rlimit::container_default_nofile;
use crate::supervisor::exit_code;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

const DOCKERD_LOG: &str = "/tmp/dockerd.log";

/// Bound on a single `docker info` readiness probe (wedged-daemon hazard):
/// against a stale socket or a hung daemon the client has no timeout of its
/// own and blocks forever, which would stall the whole pass.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Pause between readiness probes.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Wall-clock readiness window per storage driver within a pass. After this
/// the child is SIGTERMed/killed and the next driver (or pass) retries.
/// Wall clock, not an iteration count, so slow probes can't stretch it.
const READY_WINDOW: Duration = Duration::from_secs(30);

/// Runtime files a crashed, killed, or pre-snapshot dockerd + containerd
/// leave behind (stale-files hazard): dockerd refuses to start over an
/// existing docker.pid, and a stale unix socket makes `docker info` hang
/// rather than fail fast. /var/run is a symlink to /run on AL2023, but both
/// spellings are listed so the cleanup holds if that ever changes.
const STALE_RUNTIME_FILES: &[&str] = &[
    "/var/run/docker.pid",
    "/run/docker.pid",
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/var/run/docker/containerd/containerd.pid",
    "/var/run/docker/containerd/containerd.sock",
    "/run/containerd/containerd.pid",
    "/run/containerd/containerd.sock",
];

/// Owns the dockerd child and readiness flag; clone freely (shared state).
#[derive(Clone)]
pub struct DockerSupervisor {
    inner: Arc<DockerInner>,
}

struct DockerInner {
    storage_driver: String,
    nofile_soft: u64,
    nofile_hard: u64,
    state: tokio::sync::Mutex<DockerState>,
}

#[derive(Default)]
struct DockerState {
    child: Option<tokio::process::Child>,
    ready: bool,
}

impl DockerSupervisor {
    pub fn new(cfg: &Config) -> Self {
        Self {
            inner: Arc::new(DockerInner {
                storage_driver: cfg.docker_storage_driver.clone(),
                nofile_soft: cfg.nofile_soft,
                nofile_hard: cfg.nofile_hard,
                state: tokio::sync::Mutex::new(DockerState::default()),
            }),
        }
    }

    /// Bring docker up for the upcoming job. Called ONLY for docker-enabled
    /// runs (the run task resolves the per-run payload decision, with the
    /// `ENABLE_DOCKER` env as the legacy fallback). Spawned in the
    /// background off the run task so it overlaps registration; the job's
    /// first step still can't race the daemon thanks to the wait-for-docker
    /// job-started hook. Retries a few times: on a freshly-resumed MicroVM
    /// dockerd can crash if its bridge/iptables setup runs before the
    /// network settles.
    pub async fn ensure(self) {
        let mut state = self.inner.state.lock().await;
        if docker_info_ok(PROBE_TIMEOUT).await {
            state.ready = true;
            return;
        }
        log("starting dockerd for this job");
        ensure_cgroup2().await;
        // Read lazily so operators can tune without a rebuild; unparsable
        // falls back to 4.
        let attempts: u32 = env_or("DOCKERD_START_ATTEMPTS", "4").parse().unwrap_or(4);
        for attempt in 1..=attempts {
            if self.start_pass(&mut state).await {
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

    /// One start attempt with the configured storage driver. Returns true
    /// when dockerd became ready. There is deliberately NO driver fallback:
    /// v0.0.7-era code fell back to vfs when overlay2 failed on a resumed
    /// VM, and vfs's full-copy layers filled the fixed 32GB disk in one
    /// bake while poisoning the shared data-root with mixed-driver
    /// metadata (see the NO VFS hazard in the module docs).
    async fn start_pass(&self, state: &mut DockerState) -> bool {
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
        {
            let driver = &self.inner.storage_driver;
            let (out, err) = match (logf.try_clone(), logf.try_clone()) {
                (Ok(a), Ok(b)) => (a, b),
                _ => {
                    log(format!("could not clone {DOCKERD_LOG} handle"));
                    return false;
                }
            };
            log(format!("starting dockerd (storage-driver={driver})"));
            // Stale-files hazard: clean pid/socket remnants before EVERY
            // launch attempt — including the first attempt of the first
            // pass, which otherwise starts over whatever the snapshot (or a
            // just-killed sibling attempt) left behind. Process reaping for
            // earlier attempts is kill_stale_runtimes() between passes;
            // within a pass the previous child is already dead or killed
            // below before we get back here.
            remove_stale_runtime_files();
            let spawned = Command::new("dockerd")
                .arg("--host=unix:///var/run/docker.sock")
                .arg(format!("--storage-driver={driver}"))
                .args(["--exec-opt", "native.cgroupdriver=cgroupfs"])
                .args([
                    "--default-ulimit",
                    &container_default_nofile(self.inner.nofile_soft, self.inner.nofile_hard),
                ])
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
                    return false;
                }
            };
            // Wedged-daemon hazard: the readiness wait is bounded by WALL
            // CLOCK (READY_WINDOW), with every probe clamped to the window's
            // remainder — so a dockerd that is alive but never answers gets
            // SIGTERM/kill + retry below instead of an unbounded wait.
            let deadline = tokio::time::Instant::now() + READY_WINDOW;
            let mut child_exited = false;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                if docker_info_ok(probe_budget(remaining)).await {
                    state.ready = true;
                    log(format!("dockerd ready (storage-driver={driver})"));
                    state.child = Some(child);
                    return true;
                }
                if let Ok(Some(status)) = child.try_wait() {
                    log(format!(
                        "dockerd exited (storage-driver={driver}, rc={})",
                        exit_code(status)
                    ));
                    log_dockerd_tail(25); // surface WHY it crashed
                    child_exited = true;
                    break;
                }
                tokio::time::sleep(PROBE_INTERVAL.min(remaining)).await;
            }
            if !child_exited {
                // Alive but never became ready: terminate, then kill if
                // stuck.
                if let Some(pid) = child.id() {
                    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
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

    /// Stop the managed dockerd (SIGTERM, 15 s wait) and drop readiness.
    /// Used by the pool cleanup between jobs.
    pub async fn teardown(&self) {
        let mut state = self.inner.state.lock().await;
        if let Some(mut child) = state.child.take() {
            let result: Result<(), String> = async {
                if child.try_wait().map_err(|e| e.to_string())?.is_none() {
                    if let Some(pid) = child.id() {
                        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
                    }
                    tokio::time::timeout(Duration::from_secs(15), child.wait())
                        .await
                        .map_err(|_| "dockerd did not exit within 15s".to_string())?
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                log(format!("pool cleanup: dockerd teardown: {e}"));
            }
        }
        state.ready = false;
    }
}

/// `docker info` succeeding is THE readiness probe. Bounded (wedged-daemon
/// hazard): a hung probe counts as not-ready, and `kill_on_drop` ensures the
/// timed-out `docker info` process does not linger.
async fn docker_info_ok(bound: Duration) -> bool {
    let mut cmd = Command::new("docker");
    cmd.arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(bound, cmd.status()).await {
        Ok(Ok(status)) => status.success(),
        _ => false,
    }
}

/// Budget for the next readiness probe: the standard [`PROBE_TIMEOUT`],
/// clamped to the pass window's remainder so a pass never overruns
/// [`READY_WINDOW`] even when its last probe hangs.
fn probe_budget(remaining: Duration) -> Duration {
    PROBE_TIMEOUT.min(remaining)
}

/// Remove stale pid/socket files (stale-files hazard; see
/// [`STALE_RUNTIME_FILES`]). Best-effort: missing files are fine.
fn remove_stale_runtime_files() {
    for path in STALE_RUNTIME_FILES {
        let _ = std::fs::remove_file(path);
    }
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
/// clear their pid/socket files for a clean pass. (start_pass also removes
/// the files before every launch, so even the first pass — where this reaper
/// has not run yet — starts clean; see STALE_RUNTIME_FILES.)
pub async fn kill_stale_runtimes() -> Result<(), String> {
    for name in ["dockerd", "containerd"] {
        Command::new("pkill")
            .args(["-9", "-x", name])
            .status()
            .await
            .map_err(|e| format!("pkill {name}: {e}"))?;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    remove_stale_runtime_files();
    Ok(())
}

/// On total failure, dump cgroup/mount state so the next iteration
/// diagnoses from evidence rather than guesswork.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The stale-files hazard list must cover dockerd's pid + socket under
    /// BOTH the /run and /var/run spellings (symlinked on AL2023, but the
    /// cleanup must not depend on that), and containerd's pid + socket for
    /// both the dockerd-managed and standalone paths.
    #[test]
    fn stale_file_list_covers_pid_and_socket_variants() {
        for path in [
            "/var/run/docker.pid",
            "/run/docker.pid",
            "/var/run/docker.sock",
            "/run/docker.sock",
            "/var/run/docker/containerd/containerd.pid",
            "/var/run/docker/containerd/containerd.sock",
            "/run/containerd/containerd.pid",
            "/run/containerd/containerd.sock",
        ] {
            assert!(
                STALE_RUNTIME_FILES.contains(&path),
                "stale-file list is missing {path}"
            );
        }
    }

    #[test]
    fn probe_budget_caps_at_probe_timeout() {
        assert_eq!(probe_budget(Duration::from_secs(3600)), PROBE_TIMEOUT);
        assert_eq!(probe_budget(READY_WINDOW), PROBE_TIMEOUT);
    }

    #[test]
    fn probe_budget_never_exceeds_remaining_window() {
        // The clamp is what bounds a pass at READY_WINDOW even when its
        // final probe hangs: the probe gets only the window's remainder.
        assert_eq!(probe_budget(Duration::from_secs(2)), Duration::from_secs(2));
        assert_eq!(probe_budget(Duration::ZERO), Duration::ZERO);
    }

    /// Worst-case pass arithmetic: with probes clamped to the remainder, the
    /// single driver's readiness wait can never exceed its wall-clock window
    /// (plus SIGTERM grace), regardless of how probes behave. There is no
    /// vfs fallback to double it — see the NO VFS hazard in the module docs.
    #[test]
    fn pass_wait_is_bounded_by_ready_window() {
        assert!(probe_budget(READY_WINDOW) <= READY_WINDOW);
        assert!(PROBE_INTERVAL <= READY_WINDOW);
        let term_grace = Duration::from_secs(5); // SIGTERM wait in start_pass
        let worst_case_pass = READY_WINDOW + term_grace;
        assert!(worst_case_pass < Duration::from_secs(45));
    }
}
