//! Shared supervisor state: everything the hook server, run tasks and pool
//! logic hand around.

use crate::aws::CloudControl;
use crate::config::Config;
use crate::docker::DockerSupervisor;
use crate::gate::PoolGate;
use crate::registration::Registration;
use crate::supervisor::ProcHandle;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct AppState {
    pub cfg: Config,
    pub aws: Arc<dyn CloudControl>,
    pub http: reqwest::Client,
    /// Region string for the self-terminate log line.
    pub region: String,
    pub gate: PoolGate,
    /// std Mutex: the lock is never held across an await.
    pub runner: Mutex<RunnerSlot>,
    pub docker: DockerSupervisor,
    /// Is a job executing right now? Injected: the default scans /proc for
    /// Runner.Worker, which unit tests must never do — a test suite running
    /// ON a GitHub Actions runner finds the CI's own worker process and
    /// changes the outcome under test (observed live in v0.0.2 CI).
    pub job_probe: fn() -> bool,
}

/// The current runner, if any.
#[derive(Default)]
pub struct RunnerSlot {
    /// Liveness + SIGTERM for `/terminate`.
    pub proc: Option<ProcHandle>,
    /// config.sh-mode registration to undo on `/terminate`.
    pub registration: Option<Registration>,
}

impl AppState {
    pub fn new(cfg: Config, aws: Arc<dyn CloudControl>, region: String) -> Arc<Self> {
        let docker = DockerSupervisor::new(&cfg);
        Arc::new(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                // GitHub hard-403s requests without a User-Agent.
                .user_agent("gha-microvm-runner")
                .build()
                .expect("http client"),
            docker,
            cfg,
            aws,
            region,
            gate: PoolGate::new(),
            runner: Mutex::new(RunnerSlot::default()),
            job_probe: crate::supervisor::job_running,
        })
    }
}

#[cfg(test)]
pub mod testsupport {
    use super::*;
    use crate::aws::testsupport::FakeCloud;

    /// A config that never talks to real infrastructure: docker disabled,
    /// runner_dir pointed wherever the test wants.
    pub fn test_config(runner_dir: &str) -> Config {
        Config {
            hook_port: 9000,
            runner_dir: runner_dir.to_string(),
            gh_api: "https://api.github.com".to_string(),
            enable_docker: false,
            docker_storage_driver: "overlay2".to_string(),
            nofile_soft: 65536,
            nofile_hard: 1048576,
            idle_grace_seconds: 120,
        }
    }

    pub fn test_app(runner_dir: &str) -> (Arc<AppState>, Arc<FakeCloud>) {
        let fake = Arc::new(FakeCloud::default());
        let mut app = AppState::new(
            test_config(runner_dir),
            fake.clone(),
            "us-east-1".to_string(),
        );
        // The real probe scans the host /proc — on a GitHub-hosted CI box
        // that finds the CI runner's OWN Runner.Worker and flips outcomes.
        Arc::get_mut(&mut app).expect("fresh state").job_probe = || false;
        (app, fake)
    }

    /// A per-test scratch directory (no tempfile dependency).
    pub fn temp_dir(tag: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("entrypoint-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }
}
