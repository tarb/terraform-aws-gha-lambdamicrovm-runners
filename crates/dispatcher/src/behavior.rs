//! Behavior suite over the full service graph (fakes at every seam). The
//! invariant under test everywhere: every failure degrades to cold-launch or
//! terminate — never a stuck job.

use serde_json::{Value, json};
use types::{IdleReason, IdleReport, MicrovmId, RunPayload, RunnerName};

use crate::aws::params::ParamMeta;
use crate::clock::Epoch;
use crate::dispatch::{DispatchError, JobRef};
use crate::github::GithubError;
use crate::github::types::{InstallationId, JobInfo, RepoRef, RunnerInfo, WorkflowRun};
use crate::handler;
use crate::intake::webhook::{LabelSet, WebhookPayload};
use crate::oplog;
use crate::pool::{IntakeOutcome, ResumeOutcome};
use crate::sweep::Deadline;
use crate::testsupport::*;

fn job_ref(repo: &str, job_id: i64) -> JobRef {
    job_ref_labeled(repo, job_id, &[])
}

fn job_ref_labeled(repo: &str, job_id: i64, labels: &[&str]) -> JobRef {
    JobRef {
        repo: repo.to_string(),
        job_id: Some(job_id),
        installation: Some(InstallationId(1)),
        labels: labels.iter().map(|s| s.to_string()).collect::<LabelSet>(),
    }
}

fn webhook(v: Value) -> WebhookPayload {
    serde_json::from_value(v).unwrap()
}

fn completed_payload(runner: &str) -> WebhookPayload {
    webhook(json!({
        "workflow_job": {"id": 1, "runner_name": runner},
        "repository": {"full_name": "org/repo"},
        "installation": {"id": 1},
    }))
}

fn run_payload() -> RunPayload {
    RunPayload::job("https://github.com/o/r", "t", &test_cfg().runner_labels)
}

fn sign(body: &str, secret: &str) -> String {
    use hmac::{KeyInit, Mac};
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn body_msg(resp: &Value) -> String {
    let body: Value = serde_json::from_str(resp["body"].as_str().unwrap()).unwrap();
    body["msg"].as_str().unwrap().to_string()
}

// ── fleet view ───────────────────────────────────────────────────────────

#[tokio::test]
async fn fleet_list_parses_items_and_paginates() {
    let (_svc, f) = harness();
    f.mv.set_pages(vec![
        vec![vm_record("microvm-one", "RUNNING", "9", None)],
        vec![vm_record("microvm-two", "SUSPENDED", "9", None)],
    ]);
    let vms = f.fleet.list().await.unwrap();
    assert_eq!(
        vms.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
        vec!["microvm-one", "microvm-two"]
    );
    assert_eq!(f.mv.list_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn running_count_counts_only_pending_and_running() {
    let (_svc, f) = harness();
    let states = [
        "PENDING",
        "RUNNING",
        "SUSPENDING",
        "SUSPENDED",
        "TERMINATING",
        "TERMINATED",
    ];
    f.mv.set_vms(
        states
            .iter()
            .map(|s| vm_record(&format!("microvm-{}", s.to_lowercase()), s, "9", None))
            .collect(),
    );
    let vms = f.fleet.list().await.unwrap();
    assert_eq!(crate::fleet::Fleet::count_running(&vms), 2);
}

// ── concurrency cap ──────────────────────────────────────────────────────

#[tokio::test]
async fn dispatch_errors_over_cap_so_the_message_retries() {
    let (svc, f) = harness_with(|c| c.max_concurrency = 1);
    f.mv.set_vms(vec![vm("RUNNING")]);
    let err = svc
        .dispatcher
        .dispatch(&job_ref("org/repo", 123))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        DispatchError::CapReached {
            cap: 1,
            job: Some(123)
        }
    ));
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Run)),
        0,
        "RunMicrovm must not be called over the cap"
    );
}

// ── completed intake (suspend side) ──────────────────────────────────────

#[tokio::test]
async fn completed_pool_disabled_is_noop() {
    let (svc, f) = harness();
    let out = svc
        .pool
        .intake_completed(&completed_payload(OUR_RUNNER))
        .await;
    assert_eq!(out, IntakeOutcome::PoolDisabled);
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        0
    );
}

#[tokio::test]
async fn completed_foreign_runner_ignored() {
    let (svc, _f) = harness_with(|c| c.pool_enabled = true);
    let out = svc
        .pool
        .intake_completed(&completed_payload("ubuntu-hosted-3"))
        .await;
    assert!(
        matches!(&out, IntakeOutcome::NotOurs { runner } if runner == "ubuntu-hosted-3"),
        "{out:?}"
    );
}

#[tokio::test]
async fn completed_vm_already_gone() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![]);
    let out = svc
        .pool
        .intake_completed(&completed_payload(OUR_RUNNER))
        .await;
    assert!(matches!(out, IntakeOutcome::AlreadyGone { .. }), "{out:?}");
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        0
    );
}

#[tokio::test]
async fn completed_suspends_finished_vm() {
    let (svc, f) = harness_with(|c| {
        c.pool_enabled = true;
        c.suspend_delay = 0;
    });
    f.mv.set_vms(vec![vm("RUNNING")]);
    let out = svc
        .pool
        .intake_completed(&completed_payload(OUR_RUNNER))
        .await;
    assert_eq!(out, IntakeOutcome::Suspended);
    assert_eq!(
        f.journal
            .events()
            .iter()
            .filter_map(|e| match e {
                JournalEvent::Suspend(id) => Some(id.as_str().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![DEFAULT_VM_ID.to_string()]
    );
}

#[tokio::test]
async fn completed_busy_runner_not_suspended() {
    let (svc, f) = harness_with(|c| {
        c.pool_enabled = true;
        c.suspend_delay = 0;
    });
    f.mv.set_vms(vec![vm("RUNNING")]);
    f.github.set_runners(
        "org/repo",
        vec![RunnerInfo {
            name: OUR_RUNNER.to_string(),
            busy: true,
        }],
    );
    let out = svc
        .pool
        .intake_completed(&completed_payload(OUR_RUNNER))
        .await;
    assert_eq!(out, IntakeOutcome::RunnerBusy);
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        0
    );
}

#[tokio::test]
async fn completed_never_raises() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    *f.mv.list_error.lock().unwrap() = Some(client_error("InternalServerError"));
    let out = svc
        .pool
        .intake_completed(&completed_payload(OUR_RUNNER))
        .await;
    assert_eq!(out, IntakeOutcome::BenignError);
}

#[tokio::test]
async fn completed_recheck_after_delay_catches_racing_completions() {
    // Two completions race: both pass the pre-sleep cap check; the re-check
    // after SUSPEND_DELAY must stop the overshoot.
    let (svc, f) = harness_with(|c| {
        c.pool_enabled = true;
        c.pool_max_size = 1;
    });
    f.mv.set_successive_listings(vec![
        // Pre-sleep view: our VM finished, pool empty.
        vec![vm("RUNNING")],
        // Post-sleep view: the racing completion suspended another VM first.
        vec![
            vm("RUNNING"),
            vm_record("microvm-other", "SUSPENDED", "9", None),
        ],
    ]);
    let out = svc
        .pool
        .intake_completed(&completed_payload(OUR_RUNNER))
        .await;
    assert_eq!(out, IntakeOutcome::PoolFull);
    assert_eq!(
        f.journal.terminated(),
        vec![MicrovmId::new(DEFAULT_VM_ID)],
        "over-cap VM is terminated, not suspended"
    );
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        0
    );
}

// ── idle intake (direct-invoke idle reports) ─────────────────────────────

fn idle(reason: IdleReason) -> IdleReport {
    IdleReport {
        microvm_id: DEFAULT_VM_ID.to_string(),
        reason,
        repo: None,
    }
}

#[tokio::test]
async fn idle_job_complete_suspends_without_delay() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("RUNNING")]);
    let out = svc.pool.intake_idle(&idle(IdleReason::JobComplete)).await;
    assert_eq!(out, IntakeOutcome::Suspended);
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        1
    );
    // The VM reported AFTER its own cleanup: no SUSPEND_DELAY sleep (the
    // config default of 20 s would show up in the fake clock's record).
    assert!(
        f.clock.sleeps.lock().unwrap().is_empty(),
        "idle intake must not sleep"
    );
}

#[tokio::test]
async fn idle_orphan_terminates_even_with_pool_room() {
    // reason=orphan ALWAYS terminates, pool slot free or not: an orphan's
    // guest returns right after reporting (it never re-enters the idle
    // wait), so a suspended orphan could never claim a handoff — a dead
    // pool slot. Adoption needs a guest wait-after-report handshake first.
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("RUNNING")]);
    let out = svc.pool.intake_idle(&idle(IdleReason::Orphan)).await;
    assert_eq!(out, IntakeOutcome::OrphanTerminated);
    assert_eq!(f.journal.terminated(), vec![MicrovmId::new(DEFAULT_VM_ID)]);
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        0,
        "an orphan must never be suspended into the pool"
    );
    // The terminate rides the normal keys: pool + microvmId.
    assert!(
        oplog::capture::lines()
            .iter()
            .any(|l| l["pool"] == "terminate-orphan" && l["microvmId"] == DEFAULT_VM_ID)
    );
}

#[tokio::test]
async fn idle_busy_orphan_is_still_skipped_not_terminated() {
    // The busy guard outranks the orphan terminate: a late orphan report
    // for a VM that took a new job must not kill the job.
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("RUNNING")]);
    *f.github.installations.lock().unwrap() = Ok(vec![InstallationId(1)]);
    *f.github.repos.lock().unwrap() = Ok(vec![RepoRef {
        full_name: "o/r".to_string(),
    }]);
    f.github.set_runners(
        "o/r",
        vec![RunnerInfo {
            name: RunnerName::for_vm(&MicrovmId::new(DEFAULT_VM_ID))
                .as_str()
                .to_string(),
            busy: true,
        }],
    );
    let out = svc.pool.intake_idle(&idle(IdleReason::Orphan)).await;
    assert_eq!(out, IntakeOutcome::RunnerBusy);
    assert!(f.journal.terminated().is_empty());
}

#[tokio::test]
async fn idle_pool_full_terminates() {
    let (svc, f) = harness_with(|c| {
        c.pool_enabled = true;
        c.pool_max_size = 1;
    });
    f.mv.set_vms(vec![
        vm("RUNNING"),
        vm_record("microvm-other", "SUSPENDED", "9", None),
    ]);
    let out = svc.pool.intake_idle(&idle(IdleReason::JobComplete)).await;
    assert_eq!(out, IntakeOutcome::PoolFull);
    assert_eq!(f.journal.terminated(), vec![MicrovmId::new(DEFAULT_VM_ID)]);
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        0
    );
}

#[tokio::test]
async fn idle_with_pool_disabled_terminates() {
    // A disabled pool has room for nothing, but the VM still asked to be
    // dealt with — terminate rather than leak a suspended VM nothing GCs.
    let (svc, f) = harness(); // pool disabled
    f.mv.set_vms(vec![vm("RUNNING")]);
    let out = svc.pool.intake_idle(&idle(IdleReason::JobComplete)).await;
    assert_eq!(out, IntakeOutcome::PoolFull);
    assert_eq!(f.journal.terminated(), vec![MicrovmId::new(DEFAULT_VM_ID)]);
}

#[tokio::test]
async fn idle_stale_image_vm_terminates() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    // Current image version is "9"; this VM runs "8".
    f.mv.set_vms(vec![vm_record(DEFAULT_VM_ID, "RUNNING", "8", None)]);
    let out = svc.pool.intake_idle(&idle(IdleReason::JobComplete)).await;
    assert_eq!(out, IntakeOutcome::StaleImage);
    assert_eq!(f.journal.terminated(), vec![MicrovmId::new(DEFAULT_VM_ID)]);
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        0
    );
}

#[tokio::test]
async fn idle_busy_runner_is_skipped() {
    // A late/duplicate report must not freeze (or kill) a VM that took a
    // new job: its derived runner name shows busy on GitHub.
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("RUNNING")]);
    *f.github.installations.lock().unwrap() = Ok(vec![InstallationId(1)]);
    *f.github.repos.lock().unwrap() = Ok(vec![RepoRef {
        full_name: "o/r".to_string(),
    }]);
    f.github.set_runners(
        "o/r",
        vec![RunnerInfo {
            name: RunnerName::for_vm(&MicrovmId::new(DEFAULT_VM_ID))
                .as_str()
                .to_string(),
            busy: true,
        }],
    );
    let out = svc.pool.intake_idle(&idle(IdleReason::JobComplete)).await;
    assert_eq!(out, IntakeOutcome::RunnerBusy);
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        0
    );
    assert!(f.journal.terminated().is_empty());
}

#[tokio::test]
async fn idle_report_with_repo_hint_uses_the_scoped_busy_check() {
    // The report names its repo: the busy-check asks THAT repo's runner
    // listing (like the completed path) instead of walking installations.
    // Installations are deliberately unscripted here — the fleet-wide
    // fallback would see no runners and report not-busy, so only the scoped
    // check can produce RunnerBusy.
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("RUNNING")]);
    f.github.set_runners(
        "o/r",
        vec![RunnerInfo {
            name: RunnerName::for_vm(&MicrovmId::new(DEFAULT_VM_ID))
                .as_str()
                .to_string(),
            busy: true,
        }],
    );
    let mut report = idle(IdleReason::JobComplete);
    report.repo = Some("o/r".to_string());
    let out = svc.pool.intake_idle(&report).await;
    assert_eq!(out, IntakeOutcome::RunnerBusy);
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        0
    );
    assert!(f.journal.terminated().is_empty());
}

#[tokio::test]
async fn idle_report_without_repo_hint_falls_back_to_the_fleet_scan() {
    // Same busy runner, but only findable through the installations walk:
    // a hint-less report must still find it there.
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("RUNNING")]);
    *f.github.installations.lock().unwrap() = Ok(vec![InstallationId(1)]);
    *f.github.repos.lock().unwrap() = Ok(vec![RepoRef {
        full_name: "o/r".to_string(),
    }]);
    f.github.set_runners(
        "o/r",
        vec![RunnerInfo {
            name: RunnerName::for_vm(&MicrovmId::new(DEFAULT_VM_ID))
                .as_str()
                .to_string(),
            busy: true,
        }],
    );
    let out = svc.pool.intake_idle(&idle(IdleReason::JobComplete)).await;
    assert_eq!(out, IntakeOutcome::RunnerBusy);
}

#[tokio::test]
async fn idle_report_for_a_recently_resumed_vm_is_skipped() {
    // This container resumed the VM moments ago: the report is the
    // pre-suspend guest's, thawed late — the resume's run owns the VM.
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("RUNNING")]);
    f.ledger.mark(&MicrovmId::new(DEFAULT_VM_ID));
    let out = svc.pool.intake_idle(&idle(IdleReason::JobComplete)).await;
    assert_eq!(out, IntakeOutcome::RecentlyResumed);
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        0
    );
    assert!(f.journal.terminated().is_empty());
    assert_eq!(
        f.mv.list_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the guard sits BEFORE any fleet call"
    );
    assert!(
        oplog::capture::lines()
            .iter()
            .any(|l| l["pool"] == "idle-skip"
                && l["microvmId"] == DEFAULT_VM_ID
                && l["why"] == "recently-resumed")
    );
}

#[tokio::test]
async fn idle_suspend_recheck_catches_racing_intakes() {
    // TOCTOU on the idle cap: the guard passes on the first listing, but a
    // racing intake fills the pool during the busy/stale checks. The
    // re-list immediately before the freeze must catch it and terminate
    // with the existing full-terminating key.
    let (svc, f) = harness_with(|c| {
        c.pool_enabled = true;
        c.pool_max_size = 1;
    });
    f.mv.set_successive_listings(vec![
        // Intake view: our VM finished, pool empty - guard passes.
        vec![vm("RUNNING")],
        // Pre-suspend re-list: a racing report suspended another VM first.
        vec![
            vm("RUNNING"),
            vm_record("microvm-other", "SUSPENDED", "9", None),
        ],
    ]);
    let out = svc.pool.intake_idle(&idle(IdleReason::JobComplete)).await;
    assert_eq!(out, IntakeOutcome::PoolFull);
    assert_eq!(
        f.journal.terminated(),
        vec![MicrovmId::new(DEFAULT_VM_ID)],
        "over-cap VM is terminated, not suspended"
    );
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        0
    );
    assert!(
        oplog::capture::lines()
            .iter()
            .any(|l| l["pool"] == "full-terminating" && l["microvmId"] == DEFAULT_VM_ID)
    );
}

#[tokio::test]
async fn idle_unknown_or_not_running_vm_is_skipped_with_a_log() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    // Unknown VM.
    f.mv.set_vms(vec![]);
    let out = svc.pool.intake_idle(&idle(IdleReason::Orphan)).await;
    assert!(matches!(out, IntakeOutcome::AlreadyGone { .. }), "{out:?}");
    // Already suspended.
    f.mv.set_vms(vec![vm("SUSPENDED")]);
    let out = svc.pool.intake_idle(&idle(IdleReason::Orphan)).await;
    assert_eq!(out, IntakeOutcome::AlreadySuspended);
    // Pending (not RUNNING).
    f.mv.set_vms(vec![vm("PENDING")]);
    let out = svc.pool.intake_idle(&idle(IdleReason::Orphan)).await;
    assert!(matches!(out, IntakeOutcome::NotRunning { .. }), "{out:?}");
    // Nothing was touched, and every skip logged.
    assert!(f.journal.terminated().is_empty());
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        0
    );
    let skips: Vec<Value> = oplog::capture::lines()
        .into_iter()
        .filter(|l| l["pool"] == "idle-skip")
        .collect();
    assert_eq!(skips.len(), 3, "{skips:?}");
}

#[tokio::test]
async fn idle_intake_never_raises() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    *f.mv.list_error.lock().unwrap() = Some(client_error("InternalServerError"));
    let out = svc.pool.intake_idle(&idle(IdleReason::JobComplete)).await;
    assert_eq!(out, IntakeOutcome::BenignError);
}

#[tokio::test]
async fn idle_report_logs_receipt_and_outcome_keys() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("RUNNING")]);
    svc.pool.intake_idle(&idle(IdleReason::JobComplete)).await;
    let lines = oplog::capture::lines();
    let receipt = lines
        .iter()
        .find(|l| l["pool"] == "idle-report")
        .expect("receipt line");
    assert_eq!(receipt["microvmId"], DEFAULT_VM_ID);
    assert_eq!(receipt["reason"], "job-complete");
    assert!(
        lines
            .iter()
            .any(|l| l["pool"] == "suspended" && l["microvmId"] == DEFAULT_VM_ID),
        "existing outcome key expected: {lines:?}"
    );
}

#[tokio::test]
async fn handler_routes_direct_invoke_idle_events() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("RUNNING")]);
    let event = json!({"idle": {"microvmId": DEFAULT_VM_ID, "reason": "job-complete"}});
    let out = handler::handle(&svc, event, Deadline::none())
        .await
        .unwrap();
    assert_eq!(out, json!({"ok": "suspended"}));
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Suspend(_))),
        1
    );
}

// ── pool resume (pull handoff) ───────────────────────────────────────────

/// Direct `try_resume` tests hand it a fresh listing, exactly as dispatch
/// does with its one shared `ListMicrovms` result.
async fn listed(f: &Fakes) -> Vec<crate::fleet::VmRecord> {
    f.fleet.list().await.unwrap()
}

fn handoff_param() -> String {
    format!("/gha-microvm/handoff/{DEFAULT_VM_ID}")
}

fn arm_claimed_handoff(f: &Fakes) {
    // The parked parameter reads as already deleted: claim is instant.
    f.params
        .forced_get
        .lock()
        .unwrap()
        .insert(handoff_param(), Err(client_error("ParameterNotFound")));
}

#[tokio::test]
async fn resume_no_candidates_cold_launches() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("RUNNING")]);
    let out = svc
        .pool
        .try_resume(&run_payload(), &listed(&f).await)
        .await
        .unwrap();
    assert!(matches!(out, ResumeOutcome::NoCandidate));
    assert_eq!(f.journal.count(|e| matches!(e, JournalEvent::Resume(_))), 0);
}

#[tokio::test]
async fn resume_parks_before_resuming_and_treats_delete_as_ack() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("SUSPENDED")]);
    *f.mv.state.lock().unwrap() = Some(Ok(crate::fleet::MicrovmState::Suspended));
    arm_claimed_handoff(&f);
    let out = svc
        .pool
        .try_resume(&run_payload(), &listed(&f).await)
        .await
        .unwrap();
    assert!(matches!(out, ResumeOutcome::Resumed), "{out:?}");
    let order: Vec<JournalEvent> = f
        .journal
        .events()
        .into_iter()
        .filter(|e| matches!(e, JournalEvent::Put(_) | JournalEvent::Resume(_)))
        .collect();
    assert_eq!(
        order,
        vec![
            JournalEvent::Put(handoff_param()),
            JournalEvent::Resume(MicrovmId::new(DEFAULT_VM_ID)),
        ],
        "payload must be parked BEFORE resume"
    );
    let puts = f.params.puts.lock().unwrap();
    assert_eq!(puts.len(), 1);
    let (name, value) = &puts[0];
    assert_eq!(name, &handoff_param());
    let parked: Value = serde_json::from_str(value).unwrap();
    assert_eq!(parked["microvmId"], DEFAULT_VM_ID);
}

#[tokio::test]
async fn resume_unclaimed_terminates_and_cleans_up() {
    let (svc, f) = harness_with(|c| {
        c.pool_enabled = true;
        c.handoff_window = 0; // never claimed
    });
    f.mv.set_vms(vec![vm("SUSPENDED")]);
    *f.mv.state.lock().unwrap() = Some(Ok(crate::fleet::MicrovmState::Suspended));
    let out = svc
        .pool
        .try_resume(&run_payload(), &listed(&f).await)
        .await
        .unwrap();
    assert!(matches!(out, ResumeOutcome::Abandoned), "{out:?}");
    assert_eq!(f.journal.count(|e| matches!(e, JournalEvent::Delete(_))), 1);
    assert_eq!(f.journal.terminated(), vec![MicrovmId::new(DEFAULT_VM_ID)]);
}

#[tokio::test]
async fn resume_stale_image_candidate_terminated_not_resumed() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm_record(DEFAULT_VM_ID, "SUSPENDED", "8", None)]);
    let out = svc
        .pool
        .try_resume(&run_payload(), &listed(&f).await)
        .await
        .unwrap();
    assert!(matches!(out, ResumeOutcome::NoCandidate));
    assert_eq!(f.journal.terminated(), vec![MicrovmId::new(DEFAULT_VM_ID)]);
    assert_eq!(f.journal.count(|e| matches!(e, JournalEvent::Resume(_))), 0);
}

#[tokio::test]
async fn resume_race_lost_moves_on_without_terminating() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("SUSPENDED")]);
    // Another dispatcher owns it now.
    *f.mv.state.lock().unwrap() = Some(Ok(crate::fleet::MicrovmState::Running));
    let out = svc
        .pool
        .try_resume(&run_payload(), &listed(&f).await)
        .await
        .unwrap();
    assert!(matches!(out, ResumeOutcome::NoCandidate));
    assert!(
        f.journal.terminated().is_empty(),
        "a raced VM is not ours to kill"
    );
}

#[tokio::test]
async fn resumed_vm_is_tracked_for_the_zombie_reaper() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("SUSPENDED")]);
    *f.mv.state.lock().unwrap() = Some(Ok(crate::fleet::MicrovmState::Suspended));
    arm_claimed_handoff(&f);
    svc.pool
        .try_resume(&run_payload(), &listed(&f).await)
        .await
        .unwrap();
    assert!(f.ledger.recently(
        &MicrovmId::new(DEFAULT_VM_ID),
        std::time::Duration::from_secs(600)
    ));
}

#[tokio::test]
async fn parked_payload_polls_on_3s_ticks_until_the_window_closes() {
    let (_svc, f) = harness_with(|c| c.pool_enabled = true);
    let id = MicrovmId::new(DEFAULT_VM_ID);
    let parked = f.mailbox.park(&id, &run_payload()).await.unwrap();
    // The parameter is never deleted: polls ride the 3 s tick to Unclaimed.
    let status = parked
        .await_claim(std::time::Duration::from_secs(10))
        .await
        .unwrap();
    assert!(matches!(status, crate::mailbox::ClaimStatus::Unclaimed));
    assert_eq!(*f.clock.sleeps.lock().unwrap(), vec![3.0, 3.0, 3.0, 3.0]);
}

#[tokio::test]
async fn parked_claim_polling_rides_through_transient_service_errors() {
    let (_svc, f) = harness_with(|c| c.pool_enabled = true);
    let id = MicrovmId::new(DEFAULT_VM_ID);
    let parked = f.mailbox.park(&id, &run_payload()).await.unwrap();
    f.params.forced_get.lock().unwrap().insert(
        f.mailbox.address(&id),
        Err(client_error("InternalServerError")),
    );
    let status = parked
        .await_claim(std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        matches!(status, crate::mailbox::ClaimStatus::Unclaimed),
        "a throttled read is not a claim and not a failure"
    );
}

// ── cold launch ──────────────────────────────────────────────────────────

#[tokio::test]
async fn cold_launch_payload_has_exactly_the_job_fields() {
    let (svc, f) = harness(); // pool disabled
    svc.dispatcher
        .dispatch(&job_ref("org/repo", 7))
        .await
        .unwrap();
    let specs = f.mv.run_specs.lock().unwrap();
    assert_eq!(specs.len(), 1);
    assert_eq!(
        specs[0].image_version, "9",
        "resolved latest ACTIVE version"
    );
    let payload: Value = serde_json::from_str(&specs[0].run_hook_payload).unwrap();
    let mut keys: Vec<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "enable_docker",
            "ephemeral",
            "github_url",
            "labels",
            "token"
        ]
    );
    assert_eq!(payload["github_url"], "https://github.com/org/repo");
    assert_eq!(payload["token"], "tok");
    assert_eq!(payload["ephemeral"], true);
    assert_eq!(payload["labels"], "self-hosted,linux,arm64,microvm");
    assert_eq!(
        payload["enable_docker"], true,
        "DOCKER_DEFAULT unset means every job gets docker"
    );
}

#[tokio::test]
async fn docker_label_opts_the_job_in_and_default_polarity_holds() {
    // docker_default=false: only the "docker" label enables docker, and the
    // requested labels are unioned into the registration labels.
    let (svc, f) = harness_with(|c| c.docker_default = false);
    svc.dispatcher
        .dispatch(&job_ref_labeled(
            "org/repo",
            7,
            &["self-hosted", "microvm", "docker"],
        ))
        .await
        .unwrap();
    svc.dispatcher
        .dispatch(&job_ref_labeled("org/repo", 8, &["self-hosted", "microvm"]))
        .await
        .unwrap();
    let specs = f.mv.run_specs.lock().unwrap();
    let labeled: Value = serde_json::from_str(&specs[0].run_hook_payload).unwrap();
    assert_eq!(labeled["enable_docker"], true);
    assert_eq!(
        labeled["labels"], "self-hosted,linux,arm64,microvm,docker",
        "static set first (order kept), new requested labels appended"
    );
    let unlabeled: Value = serde_json::from_str(&specs[1].run_hook_payload).unwrap();
    assert_eq!(unlabeled["enable_docker"], false);
    assert_eq!(unlabeled["labels"], "self-hosted,linux,arm64,microvm");
}

#[tokio::test]
async fn docker_default_true_enables_docker_without_the_label() {
    let (svc, f) = harness(); // docker_default defaults to true
    svc.dispatcher
        .dispatch(&job_ref_labeled("org/repo", 7, &["self-hosted", "microvm"]))
        .await
        .unwrap();
    let specs = f.mv.run_specs.lock().unwrap();
    let payload: Value = serde_json::from_str(&specs[0].run_hook_payload).unwrap();
    assert_eq!(payload["enable_docker"], true);
}

#[tokio::test]
async fn parked_pool_handoff_carries_enable_docker() {
    // The mailbox copy is the same payload the cold launch would get, so a
    // resumed VM learns its docker capability too.
    let (svc, f) = harness_with(|c| {
        c.pool_enabled = true;
        c.docker_default = false;
    });
    f.mv.set_vms(vec![vm("SUSPENDED")]);
    *f.mv.state.lock().unwrap() = Some(Ok(crate::fleet::MicrovmState::Suspended));
    arm_claimed_handoff(&f);
    svc.dispatcher
        .dispatch(&job_ref_labeled("org/repo", 7, &["docker"]))
        .await
        .unwrap();
    let puts = f.params.puts.lock().unwrap();
    assert_eq!(puts.len(), 1, "payload parked in the mailbox");
    let parked: Value = serde_json::from_str(&puts[0].1).unwrap();
    assert_eq!(parked["enable_docker"], true);
}

#[tokio::test]
async fn pooled_cold_launch_payload_adds_the_pool_fields() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    svc.dispatcher
        .dispatch(&job_ref("org/repo", 7))
        .await
        .unwrap();
    let specs = f.mv.run_specs.lock().unwrap();
    let payload: Value = serde_json::from_str(&specs[0].run_hook_payload).unwrap();
    let mut keys: Vec<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "enable_docker",
            "ephemeral",
            "github_url",
            "handoff_prefix",
            "labels",
            "pool",
            "pool_grace",
            "token"
        ]
    );
    assert_eq!(payload["pool"], true);
    assert_eq!(payload["pool_grace"], 300);
    assert_eq!(payload["handoff_prefix"], "/gha-microvm/handoff");
}

#[tokio::test]
async fn pooled_payload_carries_the_dispatcher_fn() {
    // Both delivery paths — the cold-launch runHookPayload and the parked
    // mailbox copy — are built from the same payload, so both carry the
    // dispatcher's own function name for the VM's idle report.
    let (svc, f) = harness_with(|c| {
        c.pool_enabled = true;
        c.dispatcher_fn = Some("gha-microvm-dispatcher".to_string());
    });
    // A suspended candidate: the payload is parked in its mailbox before the
    // resume, and the parked copy must carry the field.
    f.mv.set_vms(vec![vm("SUSPENDED")]);
    *f.mv.state.lock().unwrap() = Some(Ok(crate::fleet::MicrovmState::Suspended));
    arm_claimed_handoff(&f);
    svc.dispatcher
        .dispatch(&job_ref("org/repo", 7))
        .await
        .unwrap();
    {
        // Scoped: clippy's await_holding_lock ignores explicit drops.
        let puts = f.params.puts.lock().unwrap();
        assert_eq!(puts.len(), 1, "payload parked in the mailbox");
        let parked: Value = serde_json::from_str(&puts[0].1).unwrap();
        assert_eq!(parked["dispatcher_fn"], "gha-microvm-dispatcher");
    }

    // Cold launch (no candidates): same field in the runHookPayload.
    f.mv.set_vms(vec![]);
    svc.dispatcher
        .dispatch(&job_ref("org/repo", 8))
        .await
        .unwrap();
    let specs = f.mv.run_specs.lock().unwrap();
    let payload: Value = serde_json::from_str(&specs[0].run_hook_payload).unwrap();
    assert_eq!(payload["dispatcher_fn"], "gha-microvm-dispatcher");
}

#[tokio::test]
async fn non_pooled_payload_omits_the_dispatcher_fn() {
    let (svc, f) = harness_with(|c| {
        c.dispatcher_fn = Some("gha-microvm-dispatcher".to_string()); // pool disabled
    });
    svc.dispatcher
        .dispatch(&job_ref("org/repo", 7))
        .await
        .unwrap();
    let specs = f.mv.run_specs.lock().unwrap();
    let payload: Value = serde_json::from_str(&specs[0].run_hook_payload).unwrap();
    assert!(
        !payload.as_object().unwrap().contains_key("dispatcher_fn"),
        "{payload}"
    );
}

#[tokio::test]
async fn run_microvm_retries_transient_access_denied() {
    let (svc, f) = harness();
    *f.mv.run_result.lock().unwrap() = Some(Err(client_error("AccessDeniedException")));
    let err = svc
        .dispatcher
        .dispatch(&job_ref("org/repo", 7))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        DispatchError::Aws(crate::aws::AwsApiError::Service { .. })
    ));
    // 4 attempts total, 1.5 s sleeps in between.
    assert_eq!(f.mv.run_specs.lock().unwrap().len(), 4);
    assert_eq!(*f.clock.sleeps.lock().unwrap(), vec![1.5, 1.5, 1.5]);
}

// ── dispatch: shared listing + throttle retry ────────────────────────────

#[tokio::test]
async fn dispatch_lists_the_fleet_exactly_once() {
    // The cap gate and the pool candidate scan share ONE ListMicrovms
    // result — the doubled listing is what throttled the control plane
    // under webhook bursts. Candidate freshness rides on the per-candidate
    // GetMicrovm re-check, which must still happen.
    let (svc, f) = harness_with(|c| {
        c.pool_enabled = true;
        c.max_concurrency = 4;
    });
    f.mv.set_vms(vec![vm("SUSPENDED")]);
    *f.mv.state.lock().unwrap() = Some(Ok(crate::fleet::MicrovmState::Suspended));
    arm_claimed_handoff(&f);
    svc.dispatcher
        .dispatch(&job_ref("org/repo", 21))
        .await
        .unwrap();
    assert_eq!(
        f.mv.list_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one ListMicrovms per dispatch, shared by cap gate and pool scan"
    );
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Resume(_))),
        1,
        "the candidate scan still ran (and resumed) off the shared listing"
    );
}

#[tokio::test]
async fn dispatch_retries_throttled_listing_and_succeeds() {
    // The incident shape: a dispatch stampede throttles ListMicrovms. Two
    // throttles then a success must end in a dispatched job, not a job
    // orphaned onto the 5-minute sweep.
    let (svc, f) = harness_with(|c| c.max_concurrency = 4);
    f.mv.set_vms(vec![vm("RUNNING")]);
    f.mv.fail_next_lists(2, client_error("ThrottlingException"));
    svc.dispatcher
        .dispatch(&job_ref("org/repo", 22))
        .await
        .unwrap();
    assert_eq!(
        f.mv.list_calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "two throttled attempts, then the success"
    );
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Run)),
        1,
        "the job launched"
    );
    // Two full-jitter gaps under the schedule's ceilings (0.5 s, then 1 s).
    let sleeps = f.clock.sleeps.lock().unwrap().clone();
    assert_eq!(sleeps.len(), 2, "{sleeps:?}");
    assert!(sleeps[0] <= 0.5 && sleeps[1] <= 1.0, "{sleeps:?}");
    // One burst line, with the throttled-attempt count.
    assert!(
        oplog::capture::lines()
            .iter()
            .any(|l| l["pool"] == "throttled" && l["calls"] == 2),
        "{:?}",
        oplog::capture::lines()
    );
}

#[tokio::test]
async fn dispatch_throttle_retry_is_bounded_and_still_errors_for_the_queue() {
    // Exhaustion: the envelope is 4 attempts, then the error surfaces so
    // the SQS message retries — bounded, never a stuck invoke.
    let (svc, f) = harness_with(|c| c.max_concurrency = 4);
    f.mv.set_vms(vec![vm("RUNNING")]);
    f.mv.fail_next_lists(10, client_error("ThrottlingException"));
    let err = svc
        .dispatcher
        .dispatch(&job_ref("org/repo", 23))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        DispatchError::Aws(crate::aws::AwsApiError::Service { .. })
    ));
    assert_eq!(
        f.mv.list_calls.load(std::sync::atomic::Ordering::SeqCst),
        4,
        "bounded at 4 attempts"
    );
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Run)),
        0,
        "no launch behind an unknown cap"
    );
    assert!(
        oplog::capture::lines()
            .iter()
            .any(|l| l["pool"] == "throttled" && l["calls"] == 4)
    );
}

#[tokio::test]
async fn dispatch_without_cap_or_pool_never_lists() {
    // max_concurrency == 0 and pool disabled: the dispatch path has no use
    // for a listing and must not spend control-plane TPS on one.
    let (svc, f) = harness();
    svc.dispatcher
        .dispatch(&job_ref("org/repo", 24))
        .await
        .unwrap();
    assert_eq!(f.mv.list_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(f.journal.count(|e| matches!(e, JournalEvent::Run)), 1);
}

// ── handler routing ──────────────────────────────────────────────────────

#[tokio::test]
async fn eb_completed_event_routes_to_pool_intake() {
    let (svc, _f) = harness(); // pool disabled -> "pool disabled"
    let eb = json!({"detail-type": "workflow_job",
                    "detail": {"action": "completed",
                               "workflow_job": {"id": 1, "runner_name": OUR_RUNNER, "labels": []}}});
    let out = handler::handle(&svc, eb, Deadline::none()).await.unwrap();
    assert_eq!(out, json!({"ok": "pool disabled"}));
}

#[tokio::test]
async fn sqs_queued_event_with_foreign_labels_is_benign() {
    let (svc, _f) = harness();
    let eb = json!({"detail-type": "workflow_job",
                    "detail": {"action": "queued",
                               "workflow_job": {"id": 5, "labels": ["ubuntu-latest"]},
                               "repository": {"full_name": "o/r"}}});
    let event = json!({"Records": [
        {"eventSource": "aws:sqs", "messageId": "m-1", "body": eb.to_string()}
    ]});
    let out = handler::handle(&svc, event, Deadline::none())
        .await
        .unwrap();
    assert_eq!(
        out,
        json!({"ok": ["labels not ours"], "batchItemFailures": []})
    );
}

#[tokio::test]
async fn sqs_dispatch_failure_reports_only_that_batch_item_for_retry() {
    let (svc, f) = harness_with(|c| c.max_concurrency = 1);
    f.mv.set_vms(vec![vm("RUNNING")]);
    let eb = json!({"detail-type": "workflow_job",
                    "detail": {"action": "queued",
                               "workflow_job": {"id": 5, "labels": ["self-hosted", "microvm"]},
                               "repository": {"full_name": "o/r"}}});
    let event = json!({"Records": [
        {"eventSource": "aws:sqs", "messageId": "m-1", "body": eb.to_string()}
    ]});
    let out = handler::handle(&svc, event, Deadline::none())
        .await
        .unwrap();
    assert_eq!(
        out["batchItemFailures"],
        json!([{"itemIdentifier": "m-1"}]),
        "the failed message retries via a partial batch response"
    );
    let msg = out["ok"][0].as_str().unwrap();
    assert!(msg.contains("concurrency cap"), "{msg}");
}

#[tokio::test]
async fn sqs_malformed_record_fails_alone_while_siblings_dispatch() {
    // The finding-1 scenario: [valid queued job, malformed record]. The
    // valid job must dispatch exactly once; only the malformed record may
    // land in batchItemFailures (it alone retries, then dead-letters).
    let (svc, f) = harness();
    let eb = json!({"detail-type": "workflow_job",
                    "detail": {"action": "queued",
                               "workflow_job": {"id": 5, "labels": ["self-hosted", "microvm"]},
                               "repository": {"full_name": "o/r"},
                               "installation": {"id": 1}}});
    let event = json!({"Records": [
        {"eventSource": "aws:sqs", "messageId": "good", "body": eb.to_string()},
        {"eventSource": "aws:sqs", "messageId": "bad", "body": "{not json"}
    ]});
    let out = handler::handle(&svc, event, Deadline::none())
        .await
        .unwrap();
    assert_eq!(out["ok"][0], "dispatched");
    assert_eq!(out["batchItemFailures"], json!([{"itemIdentifier": "bad"}]));
    assert_eq!(
        f.journal.count(|e| matches!(e, JournalEvent::Run)),
        1,
        "the valid job launches exactly once"
    );
}

#[tokio::test]
async fn function_url_never_errors_on_dispatch_failure() {
    let (svc, f) = harness_with(|c| c.max_concurrency = 1);
    f.mv.set_vms(vec![vm("RUNNING")]);
    let body = json!({
        "action": "queued",
        "workflow_job": {"id": 9, "labels": ["self-hosted", "microvm"]},
        "repository": {"full_name": "o/r"},
        "installation": {"id": 1},
    })
    .to_string();
    let event = json!({
        "headers": {"X-Hub-Signature-256": sign(&body, "x"), "X-GitHub-Event": "workflow_job"},
        "body": body,
    });
    let out = handler::handle(&svc, event, Deadline::none())
        .await
        .unwrap();
    assert_eq!(out["statusCode"], 200);
    let msg = body_msg(&out);
    assert!(msg.starts_with("dispatch failed for o/r:"), "{msg}");
    assert!(msg.contains("concurrency cap"), "{msg}");
}

#[tokio::test]
async fn function_url_rejects_bad_signature_and_answers_ping() {
    let (svc, _f) = harness();
    let bad = json!({"headers": {"x-hub-signature-256": "sha256=deadbeef"}, "body": "{}"});
    let out = handler::handle(&svc, bad, Deadline::none()).await.unwrap();
    assert_eq!(out["statusCode"], 401);

    let ping = json!({
        "headers": {"X-Hub-Signature-256": sign("{}", "x"), "X-GitHub-Event": "ping"},
        "body": "{}",
    });
    let out = handler::handle(&svc, ping, Deadline::none()).await.unwrap();
    assert_eq!(out["statusCode"], 200);
    assert_eq!(body_msg(&out), "pong");
    assert_eq!(out["headers"]["content-type"], "application/json");
}

#[tokio::test]
async fn function_url_decodes_base64_bodies() {
    use base64::Engine;
    let (svc, _f) = harness();
    let body = json!({"action": "ping-ish"}).to_string();
    let event = json!({
        "headers": {"X-Hub-Signature-256": sign(&body, "x"), "X-GitHub-Event": "workflow_job"},
        "body": base64::engine::general_purpose::STANDARD.encode(&body),
        "isBase64Encoded": true,
    });
    let out = handler::handle(&svc, event, Deadline::none())
        .await
        .unwrap();
    assert_eq!(out["statusCode"], 200);
    assert_eq!(body_msg(&out), "ignored action ping-ish");
}

#[tokio::test]
async fn function_url_missing_repository_is_400() {
    let (svc, _f) = harness();
    let body = json!({
        "action": "queued",
        "workflow_job": {"id": 9, "labels": ["self-hosted", "microvm"]},
    })
    .to_string();
    let event = json!({
        "headers": {"X-Hub-Signature-256": sign(&body, "x"), "X-GitHub-Event": "workflow_job"},
        "body": body,
    });
    let out = handler::handle(&svc, event, Deadline::none())
        .await
        .unwrap();
    assert_eq!(out["statusCode"], 400);
    assert_eq!(body_msg(&out), "no repository in payload");
}

// ── sweep ────────────────────────────────────────────────────────────────

const ZOMBIE_VM_ID: &str = "microvm-bbbb1111-2222-3333-4444-555566667777";

fn arm_sweep_github(f: &Fakes) {
    *f.github.installations.lock().unwrap() = Ok(vec![InstallationId(1)]);
    *f.github.repos.lock().unwrap() = Ok(vec![RepoRef {
        full_name: "o/r".to_string(),
    }]);
    f.github.set_runners(
        "o/r",
        vec![RunnerInfo {
            name: "gha-mvm-zzzz".to_string(),
            busy: false,
        }],
    );
    *f.github.queued_runs.lock().unwrap() = vec![WorkflowRun {
        id: 42,
        created_at: Some("2020-01-01T00:00:00Z".to_string()),
    }];
    f.github.jobs.lock().unwrap().insert(
        42,
        vec![JobInfo {
            id: Some(7),
            status: Some("queued".to_string()),
            labels: ["self-hosted", "microvm"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            created_at: Some("2020-01-01T00:00:00Z".to_string()),
        }],
    );
}

#[tokio::test]
async fn sweep_redispatches_stale_queued_jobs_and_reaps_zombies() {
    let (svc, f) = harness();
    // One RUNNING VM, older than 2x SWEEP_MIN_AGE, whose derived runner name
    // is registered nowhere -> zombie.
    f.mv.set_vms(vec![vm_record(
        ZOMBIE_VM_ID,
        "RUNNING",
        "9",
        Some(NOW - 800.0),
    )]);
    arm_sweep_github(&f);
    let dispatched = svc.sweeper.sweep(&Deadline::none()).await.unwrap();
    assert_eq!(dispatched, 1, "the stale queued job is re-dispatched");
    assert_eq!(f.mv.run_specs.lock().unwrap().len(), 1);
    assert_eq!(f.journal.terminated(), vec![MicrovmId::new(ZOMBIE_VM_ID)]);
}

#[tokio::test]
async fn sweep_partial_scan_never_reaps() {
    let (svc, f) = harness();
    f.mv.set_vms(vec![vm_record(
        ZOMBIE_VM_ID,
        "RUNNING",
        "9",
        Some(NOW - 800.0),
    )]);
    arm_sweep_github(&f);
    // Deadline pressure from the start: scan_complete = false.
    let dispatched = svc.sweeper.sweep(&Deadline::from_fn(|| 0)).await.unwrap();
    assert_eq!(dispatched, 0);
    assert!(
        f.journal.terminated().is_empty(),
        "partial view must not reap"
    );
}

#[tokio::test]
async fn sweep_deadline_bail_mid_repo_keeps_dispatches_but_never_reaps() {
    let (svc, f) = harness();
    f.mv.set_vms(vec![vm_record(
        ZOMBIE_VM_ID,
        "RUNNING",
        "9",
        Some(NOW - 800.0),
    )]);
    arm_sweep_github(&f);
    *f.github.repos.lock().unwrap() = Ok(vec![
        RepoRef {
            full_name: "o/r".to_string(),
        },
        RepoRef {
            full_name: "o/r2".to_string(),
        },
    ]);
    // Deadline turns low at the second repo's loop head.
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let deadline = Deadline::from_fn(move || {
        if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
            100_000
        } else {
            0
        }
    });
    let dispatched = svc.sweeper.sweep(&deadline).await.unwrap();
    assert_eq!(dispatched, 1, "the first repo's stale job was dispatched");
    assert!(
        f.journal.terminated().is_empty(),
        "partial view must not reap"
    );
}

#[tokio::test]
async fn sweep_registered_runner_is_not_a_zombie() {
    let (svc, f) = harness();
    f.mv.set_vms(vec![vm_record(
        ZOMBIE_VM_ID,
        "RUNNING",
        "9",
        Some(NOW - 800.0),
    )]);
    *f.github.installations.lock().unwrap() = Ok(vec![InstallationId(1)]);
    *f.github.repos.lock().unwrap() = Ok(vec![RepoRef {
        full_name: "o/r".to_string(),
    }]);
    // The VM's derived runner name IS registered -> the watchdog's turf.
    f.github.set_runners(
        "o/r",
        vec![RunnerInfo {
            name: RunnerName::for_vm(&MicrovmId::new(ZOMBIE_VM_ID))
                .as_str()
                .to_string(),
            busy: false,
        }],
    );
    svc.sweeper.sweep(&Deadline::none()).await.unwrap();
    assert!(f.journal.terminated().is_empty());
}

fn scan_failed_lines() -> Vec<Value> {
    oplog::capture::lines()
        .into_iter()
        .filter(|l| l["sweep"] == "scan-failed-everywhere")
        .collect()
}

#[tokio::test]
async fn sweep_emits_loud_line_when_every_repo_scan_fails() {
    // The two-day-silent failure mode: a missing App permission 403s every
    // job scan while "sweep: done, dispatched: 0" looks healthy.
    let (svc, f) = harness();
    arm_sweep_github(&f);
    *f.github.repos.lock().unwrap() = Ok(vec![
        RepoRef {
            full_name: "o/r".to_string(),
        },
        RepoRef {
            full_name: "o/r2".to_string(),
        },
    ]);
    let forbidden = GithubError::Status {
        status: 403,
        endpoint: "repos/o/r/actions/runs".to_string(),
    };
    f.github.fail_runs("o/r", forbidden.clone());
    f.github.fail_runs("o/r2", forbidden);
    let dispatched = svc.sweeper.sweep(&Deadline::none()).await.unwrap();
    assert_eq!(dispatched, 0);
    let lines = scan_failed_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["repos"], 2);
    let err = lines[0]["err"].as_str().unwrap();
    assert!(err.contains("403"), "{err}");
}

#[tokio::test]
async fn sweep_stays_quiet_when_any_repo_scan_succeeds() {
    let (svc, f) = harness();
    arm_sweep_github(&f);
    *f.github.repos.lock().unwrap() = Ok(vec![
        RepoRef {
            full_name: "o/r".to_string(),
        },
        RepoRef {
            full_name: "o/broken".to_string(),
        },
    ]);
    f.github.fail_runs(
        "o/broken",
        GithubError::Status {
            status: 403,
            endpoint: "repos/o/broken/actions/runs".to_string(),
        },
    );
    let dispatched = svc.sweeper.sweep(&Deadline::none()).await.unwrap();
    assert_eq!(dispatched, 1, "the healthy repo still dispatched");
    assert!(
        scan_failed_lines().is_empty(),
        "one success must silence the all-failed line"
    );
}

#[tokio::test]
async fn sweep_installations_failure_emits_loud_line_and_still_raises() {
    let (svc, f) = harness();
    *f.github.installations.lock().unwrap() = Err(GithubError::Status {
        status: 403,
        endpoint: "app/installations".to_string(),
    });
    let err = svc.sweeper.sweep(&Deadline::none()).await.unwrap_err();
    assert!(matches!(err, GithubError::Status { status: 403, .. }));
    let lines = scan_failed_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["repos"], 0);
    assert!(lines[0]["err"].as_str().unwrap().contains("403"));
}

#[tokio::test]
async fn sweep_gc_reclaims_aged_handoff_params_and_bad_pool_vms() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    // Suspended fleet: one stale-image, one near-EOL, one healthy.
    f.mv.set_vms(vec![
        vm_record("microvm-stale", "SUSPENDED", "8", None),
        vm_record("microvm-old", "SUSPENDED", "9", Some(NOW - 700.0)), // eol threshold 600
        vm_record("microvm-good", "SUSPENDED", "9", None),
    ]);
    *f.params.by_path.lock().unwrap() = vec![
        ParamMeta {
            name: "/gha-microvm/handoff/microvm-ancient".to_string(),
            last_modified: Some(Epoch(NOW - 3700.0)),
        },
        ParamMeta {
            name: "/gha-microvm/handoff/microvm-fresh".to_string(),
            last_modified: Some(Epoch(NOW - 100.0)),
        },
        ParamMeta {
            name: "/gha-microvm/handoff/microvm-unknown-age".to_string(),
            last_modified: None,
        },
    ];
    svc.sweeper.sweep(&Deadline::none()).await.unwrap();
    let deleted: Vec<String> = f
        .journal
        .events()
        .into_iter()
        .filter_map(|e| match e {
            JournalEvent::Delete(name) => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(
        deleted,
        vec!["/gha-microvm/handoff/microvm-ancient".to_string()]
    );
    assert_eq!(
        f.journal.terminated(),
        vec![
            MicrovmId::new("microvm-stale"),
            MicrovmId::new("microvm-old")
        ]
    );
}

// ── secrets ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn secrets_inline_app_credentials_accept_numeric_app_id() {
    let (svc, _f) = harness();
    let bundle = svc.secrets.bundle().await.unwrap();
    let app = bundle.app().unwrap();
    assert_eq!(app.app_id, "1");
    assert_eq!(
        secrecy::ExposeSecret::expose_secret(&bundle.webhook_secret),
        "x"
    );
}

#[tokio::test]
async fn secrets_manager_overlay_wins_and_private_key_falls_back() {
    let (svc, f) = harness_with(|c| c.app_secret_arn = Some("arn:app".to_string()));
    f.sm.map.lock().unwrap().insert(
        "arn:app".to_string(),
        r#"{"app_id": "77", "private_key": "pem-sm"}"#.to_string(),
    );
    let bundle = svc.secrets.bundle().await.unwrap();
    let app = bundle.app().unwrap();
    assert_eq!(app.app_id, "77", "overlay replaces the inline credential");
    assert_eq!(
        secrecy::ExposeSecret::expose_secret(&app.private_key),
        "pem-sm",
        "private_key accepted when app_private_key is absent"
    );
}

#[tokio::test]
async fn secrets_missing_app_credentials_is_a_typed_error() {
    let (svc, f) = harness();
    f.params.params.lock().unwrap().insert(
        "/test/dispatcher".to_string(),
        r#"{"webhook_secret": "x"}"#.to_string(),
    );
    let bundle = svc.secrets.bundle().await.unwrap();
    assert!(matches!(
        bundle.app().unwrap_err(),
        crate::secrets::SecretsError::MissingAppCredentials
    ));
}
