//! Behavior suite over the full service graph (fakes at every seam). The
//! invariant under test everywhere: every failure degrades to cold-launch or
//! terminate — never a stuck job.

use serde_json::{Value, json};
use types::{MicrovmId, RunPayload, RunnerName};

use crate::aws::params::ParamMeta;
use crate::clock::Epoch;
use crate::dispatch::{DispatchError, JobRef};
use crate::github::types::{InstallationId, JobInfo, RepoRef, RunnerInfo, WorkflowRun};
use crate::handler;
use crate::intake::webhook::WebhookPayload;
use crate::pool::{IntakeOutcome, ResumeOutcome};
use crate::sweep::Deadline;
use crate::testsupport::*;

fn job_ref(repo: &str, job_id: i64) -> JobRef {
    JobRef {
        repo: repo.to_string(),
        job_id: Some(job_id),
        installation: Some(InstallationId(1)),
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
    assert_eq!(f.fleet.running_count().await.unwrap(), 2);
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

// ── pool resume (pull handoff) ───────────────────────────────────────────

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
    let out = svc.pool.try_resume(&run_payload()).await.unwrap();
    assert!(matches!(out, ResumeOutcome::NoCandidate));
    assert_eq!(f.journal.count(|e| matches!(e, JournalEvent::Resume(_))), 0);
}

#[tokio::test]
async fn resume_parks_before_resuming_and_treats_delete_as_ack() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm("SUSPENDED")]);
    *f.mv.state.lock().unwrap() = Some(Ok(crate::fleet::MicrovmState::Suspended));
    arm_claimed_handoff(&f);
    let out = svc.pool.try_resume(&run_payload()).await.unwrap();
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
    let out = svc.pool.try_resume(&run_payload()).await.unwrap();
    assert!(matches!(out, ResumeOutcome::Abandoned), "{out:?}");
    assert_eq!(f.journal.count(|e| matches!(e, JournalEvent::Delete(_))), 1);
    assert_eq!(f.journal.terminated(), vec![MicrovmId::new(DEFAULT_VM_ID)]);
}

#[tokio::test]
async fn resume_stale_image_candidate_terminated_not_resumed() {
    let (svc, f) = harness_with(|c| c.pool_enabled = true);
    f.mv.set_vms(vec![vm_record(DEFAULT_VM_ID, "SUSPENDED", "8", None)]);
    let out = svc.pool.try_resume(&run_payload()).await.unwrap();
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
    let out = svc.pool.try_resume(&run_payload()).await.unwrap();
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
    svc.pool.try_resume(&run_payload()).await.unwrap();
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
    assert_eq!(keys, vec!["ephemeral", "github_url", "labels", "token"]);
    assert_eq!(payload["github_url"], "https://github.com/org/repo");
    assert_eq!(payload["token"], "tok");
    assert_eq!(payload["ephemeral"], true);
    assert_eq!(payload["labels"], "self-hosted,linux,arm64,microvm");
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
