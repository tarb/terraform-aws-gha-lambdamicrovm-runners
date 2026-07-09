//! Hand-rolled fakes for every seam, plus a shared side-effect [`Journal`]
//! for cross-service ordering assertions (park-before-resume and friends).

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use types::MicrovmId;

use crate::aws::AwsApiError;
use crate::aws::microvm::{ImageInfo, LaunchSpec, MicrovmApi, VmPage};
use crate::aws::params::{ParamMeta, ParamStore};
use crate::aws::secretsman::SecretStore;
use crate::clock::{Clock, Epoch};
use crate::config::Config;
use crate::fleet::{Fleet, MicrovmState, VmRecord};
use crate::github::types::{
    InstallationId, InstallationToken, JobInfo, RepoRef, RunStatus, RunnerInfo, WorkflowRun,
};
use crate::github::{GithubApi, GithubError};
use crate::mailbox::Mailbox;
use crate::pool::ResumeLedger;
use crate::secrets::SecretsCache;
use crate::services::Services;

pub const NOW: f64 = 1_700_000_000.0;
pub const DEFAULT_VM_ID: &str = "microvm-aaaa1111-2222-3333-4444-555566667777";
pub const OUR_RUNNER: &str = "gha-mvm-aaaa1111-2222-33";

/// A botocore-shaped service error.
pub fn client_error(code: &str) -> AwsApiError {
    AwsApiError::Service {
        code: Some(code.to_string()),
        message: format!("Op failed ({code}): {code}"),
    }
}

pub fn vm_record(id: &str, state: &str, version: &str, started_at: Option<f64>) -> VmRecord {
    VmRecord {
        id: MicrovmId::new(id),
        state: MicrovmState::parse(state),
        image_version: Some(version.to_string()),
        started_at: Some(Epoch(started_at.unwrap_or(NOW))),
    }
}

pub fn vm(state: &str) -> VmRecord {
    vm_record(DEFAULT_VM_ID, state, "9", None)
}

// ── journal ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum JournalEvent {
    Put(String),
    Delete(String),
    Resume(MicrovmId),
    Suspend(MicrovmId),
    Terminate(MicrovmId),
    Run,
}

#[derive(Default)]
pub struct Journal(Mutex<Vec<JournalEvent>>);

impl Journal {
    fn record(&self, e: JournalEvent) {
        self.0.lock().unwrap().push(e);
    }

    pub fn events(&self) -> Vec<JournalEvent> {
        self.0.lock().unwrap().clone()
    }

    pub fn count(&self, pred: impl Fn(&JournalEvent) -> bool) -> usize {
        self.events().iter().filter(|e| pred(e)).count()
    }

    pub fn terminated(&self) -> Vec<MicrovmId> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                JournalEvent::Terminate(id) => Some(id),
                _ => None,
            })
            .collect()
    }
}

// ── clock ────────────────────────────────────────────────────────────────

pub struct FakeClock {
    now: Mutex<f64>,
    pub sleeps: Mutex<Vec<f64>>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self {
            now: Mutex::new(NOW),
            sleeps: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Clock for FakeClock {
    fn now(&self) -> Epoch {
        Epoch(*self.now.lock().unwrap())
    }

    /// Records the sleep and advances `now` — polling loops always terminate.
    async fn sleep(&self, d: Duration) {
        self.sleeps.lock().unwrap().push(d.as_secs_f64());
        *self.now.lock().unwrap() += d.as_secs_f64();
    }
}

// ── microvm control plane ────────────────────────────────────────────────

/// One recorded RunMicrovm call.
pub struct RecordedLaunch {
    pub run_hook_payload: String,
    pub image_version: String,
}

pub struct FakeMicrovmApi {
    /// Successive full listings (each a Vec of pages). A list traversal that
    /// starts with no token consumes the next listing; the last repeats.
    pub listings: Mutex<Vec<Vec<VmPage>>>,
    sessions: AtomicUsize,
    active_session: AtomicUsize,
    pub list_calls: AtomicUsize,
    pub list_error: Mutex<Option<AwsApiError>>,
    /// Errors returned by the next list calls, one per call, before
    /// `listings` is consulted (throttle-retry tests).
    pub list_error_queue: Mutex<Vec<AwsApiError>>,
    pub state: Mutex<Option<Result<MicrovmState, AwsApiError>>>,
    pub image_latest: Mutex<Option<String>>,
    pub run_result: Mutex<Option<Result<MicrovmId, AwsApiError>>>,
    pub run_specs: Mutex<Vec<RecordedLaunch>>,
    pub resume_error: Mutex<Option<AwsApiError>>,
    journal: Arc<Journal>,
}

impl FakeMicrovmApi {
    pub fn new(journal: Arc<Journal>) -> Self {
        Self {
            listings: Mutex::new(Vec::new()),
            sessions: AtomicUsize::new(0),
            active_session: AtomicUsize::new(0),
            list_calls: AtomicUsize::new(0),
            list_error: Mutex::new(None),
            list_error_queue: Mutex::new(Vec::new()),
            state: Mutex::new(None),
            image_latest: Mutex::new(Some("9".to_string())),
            run_result: Mutex::new(None),
            run_specs: Mutex::new(Vec::new()),
            resume_error: Mutex::new(None),
            journal,
        }
    }

    /// Every listing returns these records, in one page.
    pub fn set_vms(&self, items: Vec<VmRecord>) {
        *self.listings.lock().unwrap() = vec![vec![VmPage {
            items,
            next: None,
            record_keys: record_keys(),
        }]];
    }

    /// Script one listing per traversal (racing re-list tests).
    pub fn set_successive_listings(&self, listings: Vec<Vec<VmRecord>>) {
        *self.listings.lock().unwrap() = listings
            .into_iter()
            .map(|items| {
                vec![VmPage {
                    items,
                    next: None,
                    record_keys: record_keys(),
                }]
            })
            .collect();
    }

    /// Fail the next `n` list calls with `err`, then serve `listings`
    /// normally (throttle-retry tests).
    pub fn fail_next_lists(&self, n: usize, err: AwsApiError) {
        *self.list_error_queue.lock().unwrap() = vec![err; n];
    }

    /// Script one traversal as multiple pages (pagination tests).
    pub fn set_pages(&self, pages: Vec<Vec<VmRecord>>) {
        *self.listings.lock().unwrap() = vec![
            pages
                .into_iter()
                .map(|items| VmPage {
                    items,
                    next: None,
                    record_keys: record_keys(),
                })
                .collect(),
        ];
    }
}

fn record_keys() -> Vec<String> {
    [
        "imageArn",
        "imageVersion",
        "microvmId",
        "startedAt",
        "state",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[async_trait]
impl MicrovmApi for FakeMicrovmApi {
    async fn list_page(
        &self,
        _image_arn: &str,
        token: Option<&str>,
    ) -> Result<VmPage, AwsApiError> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(e) = self.list_error.lock().unwrap().clone() {
            return Err(e);
        }
        {
            let mut queue = self.list_error_queue.lock().unwrap();
            if !queue.is_empty() {
                return Err(queue.remove(0));
            }
        }
        let listings = self.listings.lock().unwrap();
        if listings.is_empty() {
            return Ok(VmPage::default());
        }
        let page_idx = match token {
            Some(t) => t.parse::<usize>().unwrap_or(0),
            None => {
                let next = self.sessions.fetch_add(1, Ordering::SeqCst);
                self.active_session
                    .store(next.min(listings.len() - 1), Ordering::SeqCst);
                0
            }
        };
        let pages = &listings[self.active_session.load(Ordering::SeqCst)];
        let mut page = pages[page_idx.min(pages.len() - 1)].clone();
        page.next = (page_idx + 1 < pages.len()).then(|| (page_idx + 1).to_string());
        Ok(page)
    }

    async fn state(&self, _id: &MicrovmId) -> Result<MicrovmState, AwsApiError> {
        self.state
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Err(client_error("ResourceNotFoundException")))
    }

    async fn image(&self, _image_arn: &str) -> Result<ImageInfo, AwsApiError> {
        Ok(ImageInfo {
            latest_active: self.image_latest.lock().unwrap().clone(),
            state: Some("ACTIVE".to_string()),
        })
    }

    async fn run(&self, spec: &LaunchSpec<'_>) -> Result<MicrovmId, AwsApiError> {
        self.run_specs.lock().unwrap().push(RecordedLaunch {
            run_hook_payload: spec.run_hook_payload.clone(),
            image_version: spec.image_version.to_string(),
        });
        self.journal.record(JournalEvent::Run);
        self.run_result
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Ok(MicrovmId::new("microvm-new")))
    }

    async fn resume(&self, id: &MicrovmId) -> Result<(), AwsApiError> {
        self.journal.record(JournalEvent::Resume(id.clone()));
        match self.resume_error.lock().unwrap().clone() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn suspend(&self, id: &MicrovmId) -> Result<(), AwsApiError> {
        self.journal.record(JournalEvent::Suspend(id.clone()));
        Ok(())
    }

    async fn terminate(&self, id: &MicrovmId) -> Result<(), AwsApiError> {
        self.journal.record(JournalEvent::Terminate(id.clone()));
        Ok(())
    }
}

// ── ssm / secretsmanager ─────────────────────────────────────────────────

pub struct FakeParamStore {
    pub params: Mutex<HashMap<String, String>>,
    /// Forced per-name get results, overriding the store.
    pub forced_get: Mutex<HashMap<String, Result<String, AwsApiError>>>,
    pub puts: Mutex<Vec<(String, String)>>,
    pub by_path: Mutex<Vec<ParamMeta>>,
    journal: Arc<Journal>,
}

impl FakeParamStore {
    pub fn new(journal: Arc<Journal>) -> Self {
        Self {
            params: Mutex::new(HashMap::new()),
            forced_get: Mutex::new(HashMap::new()),
            puts: Mutex::new(Vec::new()),
            by_path: Mutex::new(Vec::new()),
            journal,
        }
    }
}

#[async_trait]
impl ParamStore for FakeParamStore {
    async fn get(&self, name: &str, _decrypt: bool) -> Result<String, AwsApiError> {
        if let Some(r) = self.forced_get.lock().unwrap().get(name) {
            return r.clone();
        }
        self.params
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| client_error("ParameterNotFound"))
    }

    async fn put_secure(&self, name: &str, value: &str) -> Result<(), AwsApiError> {
        self.journal.record(JournalEvent::Put(name.to_string()));
        self.puts
            .lock()
            .unwrap()
            .push((name.to_string(), value.to_string()));
        self.params
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<(), AwsApiError> {
        self.journal.record(JournalEvent::Delete(name.to_string()));
        self.params.lock().unwrap().remove(name);
        Ok(())
    }

    async fn list_by_path(&self, _path: &str) -> Result<Vec<ParamMeta>, AwsApiError> {
        Ok(self.by_path.lock().unwrap().clone())
    }
}

pub struct FakeSecretStore {
    pub map: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl SecretStore for FakeSecretStore {
    async fn secret_string(&self, arn: &str) -> Result<String, AwsApiError> {
        self.map
            .lock()
            .unwrap()
            .get(arn)
            .cloned()
            .ok_or_else(|| client_error("ResourceNotFoundException"))
    }
}

// ── github ───────────────────────────────────────────────────────────────

/// Scripts typed responses per domain operation (no URL matching).
pub struct FakeGithub {
    pub token_result: Mutex<Result<InstallationToken, GithubError>>,
    pub installations: Mutex<Result<Vec<InstallationId>, GithubError>>,
    pub repos: Mutex<Result<Vec<RepoRef>, GithubError>>,
    /// Per-repo runner listings; unscripted repos have no runners.
    pub runners: Mutex<HashMap<String, Result<Vec<RunnerInfo>, GithubError>>>,
    pub queued_runs: Mutex<Vec<WorkflowRun>>,
    pub in_progress_runs: Mutex<Vec<WorkflowRun>>,
    /// Per-repo `workflow_runs` failures (sweep scan-failure tests).
    pub runs_errors: Mutex<HashMap<String, GithubError>>,
    /// Per-run-id job listings.
    pub jobs: Mutex<HashMap<i64, Vec<JobInfo>>>,
}

impl FakeGithub {
    pub fn new() -> Self {
        Self {
            token_result: Mutex::new(Ok(InstallationToken::new("tok"))),
            installations: Mutex::new(Ok(Vec::new())),
            repos: Mutex::new(Ok(Vec::new())),
            runners: Mutex::new(HashMap::new()),
            queued_runs: Mutex::new(Vec::new()),
            in_progress_runs: Mutex::new(Vec::new()),
            runs_errors: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_runners(&self, repo: &str, runners: Vec<RunnerInfo>) {
        self.runners
            .lock()
            .unwrap()
            .insert(repo.to_string(), Ok(runners));
    }

    /// Make `workflow_runs` fail for `repo` (e.g. a 403 from a missing App
    /// permission).
    pub fn fail_runs(&self, repo: &str, err: GithubError) {
        self.runs_errors
            .lock()
            .unwrap()
            .insert(repo.to_string(), err);
    }
}

#[async_trait]
impl GithubApi for FakeGithub {
    async fn token_for_repo(
        &self,
        _repo: &str,
        _installation: Option<InstallationId>,
    ) -> Result<InstallationToken, GithubError> {
        self.token_result.lock().unwrap().clone()
    }

    async fn installations(&self) -> Result<Vec<InstallationId>, GithubError> {
        self.installations.lock().unwrap().clone()
    }

    async fn installation_repos(
        &self,
        _id: InstallationId,
    ) -> Result<(InstallationToken, Vec<RepoRef>), GithubError> {
        let token = self.token_result.lock().unwrap().clone()?;
        Ok((token, self.repos.lock().unwrap().clone()?))
    }

    async fn repo_runners(
        &self,
        repo: &str,
        _token: &InstallationToken,
    ) -> Result<Vec<RunnerInfo>, GithubError> {
        self.runners
            .lock()
            .unwrap()
            .get(repo)
            .cloned()
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn workflow_runs(
        &self,
        repo: &str,
        _token: &InstallationToken,
        status: RunStatus,
    ) -> Result<Vec<WorkflowRun>, GithubError> {
        if let Some(e) = self.runs_errors.lock().unwrap().get(repo) {
            return Err(e.clone());
        }
        Ok(match status {
            RunStatus::Queued => self.queued_runs.lock().unwrap().clone(),
            RunStatus::InProgress => self.in_progress_runs.lock().unwrap().clone(),
        })
    }

    async fn run_jobs(
        &self,
        _repo: &str,
        run_id: i64,
        _token: &InstallationToken,
    ) -> Result<Vec<JobInfo>, GithubError> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .get(&run_id)
            .cloned()
            .unwrap_or_default())
    }
}

// ── harness ──────────────────────────────────────────────────────────────

pub struct Fakes {
    pub clock: Arc<FakeClock>,
    pub mv: Arc<FakeMicrovmApi>,
    pub params: Arc<FakeParamStore>,
    pub sm: Arc<FakeSecretStore>,
    pub github: Arc<FakeGithub>,
    pub fleet: Arc<Fleet>,
    pub mailbox: Arc<Mailbox>,
    pub ledger: Arc<ResumeLedger>,
    pub journal: Arc<Journal>,
}

/// The standard test config (required-env fixture values only).
pub fn test_cfg() -> Config {
    let env: HashMap<&str, &str> = [
        ("AWS_REGION", "us-east-1"),
        (
            "IMAGE_ARN",
            "arn:aws:lambda:us-east-1:111122223333:microvm-image:test",
        ),
        ("EXEC_ROLE_ARN", "arn:aws:iam::111122223333:role/test-exec"),
        (
            "EGRESS_CONNECTOR",
            "arn:aws:lambda:us-east-1:111122223333:network-connector:test",
        ),
        ("PARAM_NAME", "/test/dispatcher"),
    ]
    .into_iter()
    .collect();
    Config::from_lookup(|k| env.get(k).map(|s| s.to_string())).unwrap()
}

pub fn harness() -> (Services, Fakes) {
    harness_with(|_| {})
}

/// Build a full [`Services`] over fakes; `tweak` mutates the standard config.
/// The record-keys canary is pre-tripped and the secret bundle pre-armed.
pub fn harness_with(tweak: impl FnOnce(&mut Config)) -> (Services, Fakes) {
    let mut cfg = test_cfg();
    tweak(&mut cfg);
    let cfg = Arc::new(cfg);

    let journal = Arc::new(Journal::default());
    let clock = Arc::new(FakeClock::new());
    let mv = Arc::new(FakeMicrovmApi::new(journal.clone()));
    let params = Arc::new(FakeParamStore::new(journal.clone()));
    let sm = Arc::new(FakeSecretStore {
        map: Mutex::new(HashMap::new()),
    });
    let github = Arc::new(FakeGithub::new());

    params.params.lock().unwrap().insert(
        cfg.param_name.clone(),
        r#"{"webhook_secret": "x", "app_id": 1, "app_private_key": "test-pem"}"#.to_string(),
    );

    let secrets = Arc::new(SecretsCache::new(
        params.clone(),
        sm.clone(),
        clock.clone(),
        cfg.param_name.clone(),
        cfg.app_secret_arn.clone(),
    ));
    let fleet = Arc::new(Fleet::new(mv.clone(), cfg.clone(), clock.clone()));
    fleet.silence_record_keys_canary();
    let mailbox = Arc::new(Mailbox::new(
        params.clone(),
        cfg.handoff_prefix.clone(),
        clock.clone(),
    ));
    let ledger = Arc::new(ResumeLedger::new(clock.clone()));

    let services = Services::wire(
        cfg,
        clock.clone(),
        secrets,
        github.clone(),
        fleet.clone(),
        mailbox.clone(),
        ledger.clone(),
    );
    let fakes = Fakes {
        clock,
        mv,
        params,
        sm,
        github,
        fleet,
        mailbox,
        ledger,
        journal,
    };
    (services, fakes)
}
