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
//! - XTABLES LOCK: nothing in this crate invokes iptables directly (the
//!   ipv6 fast-fail in ipv6.rs shells out to ip6tables WITH `-w`, and the
//!   image installs the package for dockerd), so no `-w <seconds>` flag is
//!   needed here; dockerd's own iptables calls handle the lock internally.
//! - DISK POISON: /var/lib/docker persists across pool suspend/resume, on a
//!   FIXED 32GB root disk (the microvm API has no storage knob — Resources
//!   is minimum_memory_in_mib only). Two rules follow. (1) NEVER fall back
//!   to the vfs storage driver: vfs full-copies every layer (no CoW), so
//!   one monorepo bake fills the disk (observed: 12GB of /var/lib/docker/vfs
//!   and an ENOSPC'd bake), and mixing drivers in one data-root corrupts
//!   metadata for every later job on that pooled VM. When overlay2 won't
//!   start, the fix is wiping the data-root and retrying overlay2 — handled
//!   between start attempts in [`DockerSupervisor::ensure`]. (2) Reused
//!   pool VMs accumulate build cache with no GC; [`reclaim_disk_if_low`]
//!   wipes the data-root before a job's dockerd starts when free space is
//!   under `DOCKER_MIN_FREE_GB` (default 16) — registry cache-from covers
//!   the lost warm layers, ENOSPC mid-bake does not.

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

/// dockerd's data-root (we never pass `--data-root`, so the default). Lives
/// on the same fixed 32GB root filesystem as everything else and persists
/// across pool suspend/resume — see the DISK POISON hazard above.
const DOCKER_DATA_ROOT: &str = "/var/lib/docker";

/// Default free-space floor (GiB) under which the data-root is wiped before
/// a job's dockerd starts. Override with the `DOCKER_MIN_FREE_GB` env (via
/// `runner_environment_variables`). 16 leaves a full-width monorepo bake
/// (~6 parallel cargo-chef targets) headroom on the 32GB disk.
const DEFAULT_MIN_FREE_GB: u64 = 16;

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
        // No daemon is running here (the early probe above returned false),
        // so wiping is safe. Pool-reuse accumulation is caught before it
        // becomes a mid-bake ENOSPC.
        reclaim_disk_if_low(DOCKER_DATA_ROOT);
        // Read lazily so operators can tune without a rebuild; unparsable
        // falls back to 4.
        let attempts: u32 = env_or("DOCKERD_START_ATTEMPTS", "4").parse().unwrap_or(4);
        let mut wiped_for_recovery = false;
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
                // Two failures usually mean corrupt data-root state (e.g. a
                // snapshot taken mid-write), not the network-settle crash the
                // plain retries cover. Wipe once and let overlay2 try fresh —
                // NEVER degrade to vfs (DISK POISON hazard above).
                if attempt >= 2 && !wiped_for_recovery {
                    log(
                        "dockerd failed repeatedly; wiping docker data-root (corrupt-state recovery)",
                    );
                    wipe_dir_children(std::path::Path::new(DOCKER_DATA_ROOT));
                    wiped_for_recovery = true;
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
    /// metadata (see the DISK POISON hazard in the module docs). Recovery
    /// from a corrupt data-root is the wipe-and-retry in [`Self::ensure`].
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

/// Wipe the docker data-root before this job's dockerd starts when the
/// filesystem is under the free-space floor (`DOCKER_MIN_FREE_GB`, default
/// [`DEFAULT_MIN_FREE_GB`]). Pool VMs reuse the data-root across jobs with
/// no GC, so accumulation otherwise surfaces as ENOSPC in the middle of
/// whichever bake draws the dirty VM. A wipe costs one cold layer pull
/// (registry cache-from covers rebuild speed); an ENOSPC costs the job.
/// Must only be called while no dockerd is running.
fn reclaim_disk_if_low(root: &str) {
    let min_free = min_free_bytes(&env_or("DOCKER_MIN_FREE_GB", ""));
    let Some(avail) = available_bytes(root) else {
        return; // statvfs failed (already logged); don't guess
    };
    if avail >= min_free {
        return;
    }
    log(format!(
        "docker data-root low on space ({} free < {} floor) - wiping {root} for a clean slate",
        fmt_gib(avail),
        fmt_gib(min_free)
    ));
    wipe_dir_children(std::path::Path::new(root));
    if let Some(after) = available_bytes(root) {
        log(format!(
            "docker data-root wiped ({} free now)",
            fmt_gib(after)
        ));
    }
}

/// The free-space floor in bytes: `raw` (the `DOCKER_MIN_FREE_GB` env) as
/// whole GiB, falling back to [`DEFAULT_MIN_FREE_GB`] when unset/garbage.
fn min_free_bytes(raw: &str) -> u64 {
    let gb: u64 = raw.parse().unwrap_or(DEFAULT_MIN_FREE_GB);
    gb.saturating_mul(1024 * 1024 * 1024)
}

/// Available bytes (unprivileged view, f_bavail) on the filesystem holding
/// `path`. None (with a log line) when statvfs fails — e.g. the path does
/// not exist yet on a pristine image.
fn available_bytes(path: &str) -> Option<u64> {
    match nix::sys::statvfs::statvfs(path) {
        Ok(s) => {
            let avail = u128::from(s.blocks_available()) * u128::from(s.fragment_size());
            Some(u64::try_from(avail).unwrap_or(u64::MAX))
        }
        Err(e) => {
            log(format!("statvfs {path} failed: {e}"));
            None
        }
    }
}

/// `bytes` as a one-decimal GiB label for log lines.
fn fmt_gib(bytes: u64) -> String {
    format!("{:.1}GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// Remove everything INSIDE `root`, keeping `root` itself (it can be a
/// mount point, and dockerd recreates the layout it needs). Best-effort:
/// each failure is logged and the rest proceeds — a partial wipe still
/// frees space.
fn wipe_dir_children(root: &std::path::Path) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            log(format!("WARN: could not list {}: {e}", root.display()));
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // DirEntry::file_type does not follow symlinks, so a symlinked dir
        // is removed as a file (the link), never traversed.
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let removed = if is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = removed {
            log(format!("WARN: could not remove {}: {e}", path.display()));
        }
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
    /// vfs fallback to double it — see the DISK POISON hazard.
    #[test]
    fn pass_wait_is_bounded_by_ready_window() {
        assert!(probe_budget(READY_WINDOW) <= READY_WINDOW);
        assert!(PROBE_INTERVAL <= READY_WINDOW);
        let term_grace = Duration::from_secs(5); // SIGTERM wait in start_pass
        let worst_case_pass = READY_WINDOW + term_grace;
        assert!(worst_case_pass < Duration::from_secs(45));
    }

    #[test]
    fn min_free_bytes_parses_whole_gib_and_defaults_on_garbage() {
        assert_eq!(min_free_bytes("2"), 2 * 1024 * 1024 * 1024);
        assert_eq!(min_free_bytes("0"), 0); // explicit opt-out stays 0
        let default = DEFAULT_MIN_FREE_GB * 1024 * 1024 * 1024;
        for garbage in ["", "16GB", "-1", "lots"] {
            assert_eq!(min_free_bytes(garbage), default, "input {garbage:?}");
        }
    }

    #[test]
    fn available_bytes_reports_a_real_filesystem() {
        // Any existing path works; the value just has to be a sane Some.
        let avail = available_bytes(crate::state::testsupport::temp_dir("statvfs").as_str());
        assert!(avail.is_some_and(|b| b > 0));
    }

    #[test]
    fn wipe_dir_children_empties_but_keeps_the_root() {
        let root = std::path::PathBuf::from(crate::state::testsupport::temp_dir("wipe"));
        std::fs::create_dir_all(root.join("vfs/dir/deeper")).unwrap();
        std::fs::write(root.join("vfs/dir/deeper/layer"), b"x").unwrap();
        std::fs::write(root.join("engine-id"), b"x").unwrap();
        std::os::unix::fs::symlink("/nonexistent-target", root.join("dangling")).unwrap();

        wipe_dir_children(&root);

        // The mount point itself must survive; its contents must not.
        assert!(root.is_dir());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn wipe_dir_children_tolerates_a_missing_root() {
        // Pristine image: /var/lib/docker may not exist yet. Must not log
        // spurious warnings or panic.
        wipe_dir_children(std::path::Path::new("/definitely/not/a/real/path"));
    }
}
