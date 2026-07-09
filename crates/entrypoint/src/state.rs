//! Shared supervisor state — the Rust shape of the Python module-level
//! `_state`, `_pool` and `_docker` dicts.

use crate::aws::AwsApi;
use crate::config::Config;
use crate::util::Event;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Handle to the running Runner.Listener: enough to poll liveness
/// (`exited`, the Python `proc.poll()`) and SIGTERM it (`pid`, the Python
/// `proc.terminate()`) without owning the `Child`, which `start_runner`
/// keeps to `wait()` on.
#[derive(Clone)]
pub struct ProcHandle {
    pub pid: i32,
    pub exited: Arc<AtomicBool>,
}

impl ProcHandle {
    pub fn alive(&self) -> bool {
        !self.exited.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// `proc.terminate()`.
    pub fn terminate(&self) {
        if self.pid > 0 && self.alive() {
            unsafe {
                let _ = libc::kill(self.pid, libc::SIGTERM);
            }
        }
    }
}

/// `_state`: persistent-mode registration bookkeeping for `/terminate`.
#[derive(Default)]
pub struct RunnerState {
    pub proc: Option<ProcHandle>,
    pub registered: bool,
    pub runners_api: Option<String>,
    /// GitHub token for de-registration. Never logged.
    pub pat: Option<String>,
}

/// `_docker`: the dockerd child and readiness flag.
#[derive(Default)]
pub struct DockerState {
    pub child: Option<tokio::process::Child>,
    pub ready: bool,
}

/// Everything the hook server, runner supervisor and pool logic share.
pub struct Sup {
    pub cfg: Config,
    pub aws: Arc<dyn AwsApi>,
    pub http: reqwest::Client,
    /// Region string for the self-terminate log line (Python resolves it the
    /// same way for the CLI `--region` flag).
    pub region_label: String,
    /// `_pool["new_run"]`.
    pub new_run: Event,
    /// `_pool["cleaning"]`.
    pub cleaning: Event,
    /// `_pool["watchdog_killed"]`.
    pub watchdog_killed: AtomicBool,
    pub runner: Mutex<RunnerState>,
    pub docker: tokio::sync::Mutex<DockerState>,
}

impl Sup {
    pub fn new(cfg: Config, aws: Arc<dyn AwsApi>, region_label: String) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            aws,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("http client"),
            region_label,
            new_run: Event::new(),
            cleaning: Event::new(),
            watchdog_killed: AtomicBool::new(false),
            runner: Mutex::new(RunnerState::default()),
            docker: tokio::sync::Mutex::new(DockerState::default()),
        })
    }
}

/// `os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION") or
/// "us-east-1"` — empty values fall through, exactly like Python truthiness.
pub fn region_label() -> String {
    for key in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Ok(v) = std::env::var(key)
            && !v.is_empty()
        {
            return v;
        }
    }
    "us-east-1".to_string()
}
