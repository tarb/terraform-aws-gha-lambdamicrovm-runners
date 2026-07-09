//! Behavioral spec ported 1:1 from tests/test_dispatcher.py: every path must
//! degrade to cold-launch or terminate — never a stuck job.

use serde_json::{Value, json};

use crate::app::{App, recently_resumed};
use crate::config::Config;
use crate::platform::VmPage;
use crate::testutil::{
    DEFAULT_VM_ID, FakePlatform, client_error, quiet_caches, test_cfg, vm, vm_item,
};
use types::RunPayload;

// ── fleet view ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_vms_parses_items_and_paginates() {
    let p = FakePlatform::new();
    *p.list_pages.lock().unwrap() = vec![
        VmPage {
            items: vec![vm_item("microvm-one", "RUNNING", "9", None)],
            next_token: None,
        },
        VmPage {
            items: vec![vm_item("microvm-two", "SUSPENDED", "9", None)],
            next_token: None,
        },
    ];
    let cfg = test_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let vms = app.list_vms().await.unwrap();
    assert_eq!(
        vms.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
        vec!["microvm-one", "microvm-two"]
    );
    assert_eq!(
        vms.iter().map(|v| v.state.as_str()).collect::<Vec<_>>(),
        vec!["RUNNING", "SUSPENDED"]
    );
    assert_eq!(p.list_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn running_count_counts_only_pending_and_running() {
    let states = [
        "PENDING",
        "RUNNING",
        "SUSPENDING",
        "SUSPENDED",
        "TERMINATING",
        "TERMINATED",
    ];
    let items = states
        .iter()
        .map(|s| vm_item(&format!("microvm-{}", s.to_lowercase()), s, "9", None))
        .collect();
    let p = FakePlatform::new().with_vms(items);
    let cfg = test_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    assert_eq!(app.running_count().await.unwrap(), 2);
}

// ── concurrency cap ──────────────────────────────────────────────────────────

#[tokio::test]
async fn dispatch_raises_over_cap() {
    let p = FakePlatform::new().with_vms(vec![vm("RUNNING")]);
    let mut cfg = test_cfg();
    cfg.max_concurrency = 1;
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let err = app
        .dispatch_job("org/repo", &json!(123), Some(1))
        .await
        .unwrap_err();
    assert_eq!(err.kind, "RuntimeError");
    assert!(err.msg.contains("concurrency cap"), "{}", err.msg);
    assert!(
        p.run_payloads.lock().unwrap().is_empty(),
        "run_microvm must not be called"
    );
}

// ── completed intake (suspend side) ──────────────────────────────────────────

fn completed_payload(runner: &str) -> Value {
    json!({
        "workflow_job": {"id": 1, "runner_name": runner},
        "repository": {"full_name": "org/repo"},
        "installation": {"id": 1},
    })
}

const OUR_RUNNER: &str = "gha-mvm-aaaa1111-2222-33";

#[tokio::test]
async fn completed_pool_disabled_is_noop() {
    let p = FakePlatform::new();
    let cfg = test_cfg(); // pool_enabled = false
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    assert_eq!(
        app.handle_completed(&completed_payload(OUR_RUNNER)).await,
        "pool disabled"
    );
    assert!(p.suspend_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn completed_foreign_runner_ignored() {
    let p = FakePlatform::new();
    let mut cfg = test_cfg();
    cfg.pool_enabled = true;
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let out = app
        .handle_completed(&completed_payload("ubuntu-hosted-3"))
        .await;
    assert!(out.starts_with("not ours"), "{out}");
    assert_eq!(out, "not ours: 'ubuntu-hosted-3'"); // Python repr formatting
}

#[tokio::test]
async fn completed_vm_already_gone() {
    let p = FakePlatform::new().with_vms(vec![]);
    let mut cfg = test_cfg();
    cfg.pool_enabled = true;
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let out = app.handle_completed(&completed_payload(OUR_RUNNER)).await;
    assert!(out.contains("already gone"), "{out}");
    assert!(p.suspend_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn completed_suspends_finished_vm() {
    let p = FakePlatform::new().with_vms(vec![vm("RUNNING")]);
    p.arm_github_auth();
    p.add_gh_rule("/actions/runners", Ok((200, json!({"runners": []}))));
    let mut cfg = test_cfg();
    cfg.pool_enabled = true;
    cfg.suspend_delay = 0;
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    assert_eq!(
        app.handle_completed(&completed_payload(OUR_RUNNER)).await,
        "suspended"
    );
    assert_eq!(
        *p.suspend_calls.lock().unwrap(),
        vec![DEFAULT_VM_ID.to_string()]
    );
}

#[tokio::test]
async fn completed_busy_runner_not_suspended() {
    let p = FakePlatform::new().with_vms(vec![vm("RUNNING")]);
    p.arm_github_auth();
    p.add_gh_rule(
        "/actions/runners",
        Ok((
            200,
            json!({"runners": [{"name": OUR_RUNNER, "busy": true}]}),
        )),
    );
    let mut cfg = test_cfg();
    cfg.pool_enabled = true;
    cfg.suspend_delay = 0;
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let out = app.handle_completed(&completed_payload(OUR_RUNNER)).await;
    assert!(out.contains("busy"), "{out}");
    assert!(p.suspend_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn completed_never_raises() {
    let mut p = FakePlatform::new();
    p.list_error = Some(client_error("InternalServerError"));
    let mut cfg = test_cfg();
    cfg.pool_enabled = true;
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    assert_eq!(
        app.handle_completed(&completed_payload(OUR_RUNNER)).await,
        "error (benign)"
    );
}

// ── pool resume (pull handoff) ───────────────────────────────────────────────

fn pool_cfg() -> Config {
    let mut cfg = test_cfg();
    cfg.pool_enabled = true;
    cfg
}

fn run_payload() -> RunPayload {
    RunPayload::new(
        "https://github.com/o/r",
        "t",
        "self-hosted,linux,arm64,microvm",
    )
}

#[tokio::test]
async fn resume_no_candidates_cold_launches() {
    let p = FakePlatform::new().with_vms(vec![vm("RUNNING")]);
    let cfg = pool_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    assert!(!app.try_pool_resume(&run_payload()).await.unwrap());
    assert!(p.resume_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn resume_parks_before_resuming_and_treats_delete_as_ack() {
    let p = FakePlatform::new().with_vms(vec![vm("SUSPENDED")]);
    *p.get_state.lock().unwrap() = Some(Ok("SUSPENDED".to_string()));
    let param = format!("/gha-microvm/handoff/{DEFAULT_VM_ID}");
    p.forced_get
        .lock()
        .unwrap()
        .insert(param.clone(), Err(client_error("ParameterNotFound")));
    let cfg = pool_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    assert!(app.try_pool_resume(&run_payload()).await.unwrap());
    let order: Vec<String> = p
        .event_kinds()
        .into_iter()
        .filter(|k| k == "put" || k == "resume")
        .collect();
    assert_eq!(
        order,
        vec!["put", "resume"],
        "payload must be parked BEFORE resume"
    );
    let puts = p.put_calls.lock().unwrap();
    assert_eq!(puts.len(), 1);
    let (name, value, ptype) = &puts[0];
    assert_eq!(name, &param);
    assert_eq!(ptype, "SecureString");
    let parked: Value = serde_json::from_str(value).unwrap();
    assert_eq!(parked["microvmId"], DEFAULT_VM_ID);
}

#[tokio::test]
async fn resume_unclaimed_terminates_and_cleans_up() {
    let p = FakePlatform::new().with_vms(vec![vm("SUSPENDED")]);
    *p.get_state.lock().unwrap() = Some(Ok("SUSPENDED".to_string()));
    let mut cfg = pool_cfg();
    cfg.handoff_window = 0; // never claimed
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    assert!(!app.try_pool_resume(&run_payload()).await.unwrap());
    assert_eq!(p.delete_calls.lock().unwrap().len(), 1);
    assert_eq!(p.terminate_calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn resume_stale_image_candidate_terminated_not_resumed() {
    let p = FakePlatform::new().with_vms(vec![vm_item(DEFAULT_VM_ID, "SUSPENDED", "8", None)]);
    let cfg = pool_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    assert!(!app.try_pool_resume(&run_payload()).await.unwrap());
    assert_eq!(p.terminate_calls.lock().unwrap().len(), 1);
    assert!(p.resume_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn resume_race_lost_moves_on_without_terminating() {
    let p = FakePlatform::new().with_vms(vec![vm("SUSPENDED")]);
    *p.get_state.lock().unwrap() = Some(Ok("RUNNING".to_string())); // another dispatcher owns it now
    let cfg = pool_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    assert!(!app.try_pool_resume(&run_payload()).await.unwrap());
    assert!(p.terminate_calls.lock().unwrap().is_empty());
}

// ── extra coverage beyond the Python suite ───────────────────────────────────

#[tokio::test]
async fn resumed_vm_is_tracked_for_the_zombie_reaper() {
    let p = FakePlatform::new().with_vms(vec![vm("SUSPENDED")]);
    *p.get_state.lock().unwrap() = Some(Ok("SUSPENDED".to_string()));
    let param = format!("/gha-microvm/handoff/{DEFAULT_VM_ID}");
    p.forced_get
        .lock()
        .unwrap()
        .insert(param, Err(client_error("ParameterNotFound")));
    let cfg = pool_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    assert!(app.try_pool_resume(&run_payload()).await.unwrap());
    assert!(
        recently_resumed()
            .lock()
            .unwrap()
            .contains_key(DEFAULT_VM_ID)
    );
}

#[tokio::test]
async fn cold_launch_payload_matches_python_wire_shape() {
    let p = FakePlatform::new().with_vms(vec![]);
    p.arm_github_auth();
    let cfg = test_cfg(); // pool disabled
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    app.dispatch_job("org/repo", &json!(7), Some(1))
        .await
        .unwrap();
    let payloads = p.run_payloads.lock().unwrap();
    assert_eq!(payloads.len(), 1);
    // Byte-identical to Python json.dumps of the run_payload dict.
    assert_eq!(
        payloads[0],
        "{\"github_url\": \"https://github.com/org/repo\", \"token\": \"tok\", \
         \"ephemeral\": true, \"labels\": \"self-hosted,linux,arm64,microvm\"}"
    );
}

#[tokio::test]
async fn pooled_cold_launch_payload_has_pool_fields_in_python_order() {
    let p = FakePlatform::new().with_vms(vec![]);
    p.arm_github_auth();
    let cfg = pool_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    app.dispatch_job("org/repo", &json!(7), Some(1))
        .await
        .unwrap();
    let payloads = p.run_payloads.lock().unwrap();
    assert_eq!(
        payloads[0],
        "{\"github_url\": \"https://github.com/org/repo\", \"token\": \"tok\", \
         \"ephemeral\": true, \"labels\": \"self-hosted,linux,arm64,microvm\", \
         \"pool\": true, \"pool_grace\": 300, \"handoff_prefix\": \"/gha-microvm/handoff\"}"
    );
}

#[tokio::test]
async fn run_microvm_retries_transient_access_denied() {
    let p = FakePlatform::new().with_vms(vec![]);
    p.arm_github_auth();
    *p.run_result.lock().unwrap() = Some(Err(client_error("AccessDeniedException")));
    let cfg = test_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let err = app
        .dispatch_job("org/repo", &json!(7), Some(1))
        .await
        .unwrap_err();
    assert_eq!(err.kind, "ClientError");
    // 4 attempts total, 1.5s sleeps in between
    assert_eq!(p.run_payloads.lock().unwrap().len(), 4);
    assert_eq!(*p.sleeps.lock().unwrap(), vec![1.5, 1.5, 1.5]);
}

#[tokio::test]
async fn eb_completed_event_routes_to_pool_intake() {
    let p = FakePlatform::new();
    let cfg = test_cfg(); // pool disabled -> "pool disabled"
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let eb = json!({"detail-type": "workflow_job",
                    "detail": {"action": "completed",
                               "workflow_job": {"id": 1, "runner_name": OUR_RUNNER, "labels": []}}});
    let out = crate::handle(&app, &eb, None).await.unwrap();
    assert_eq!(out, json!({"ok": "pool disabled"}));
}

#[tokio::test]
async fn sqs_queued_event_with_foreign_labels_is_benign() {
    let p = FakePlatform::new();
    let cfg = test_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let eb = json!({"detail-type": "workflow_job",
                    "detail": {"action": "queued",
                               "workflow_job": {"id": 5, "labels": ["ubuntu-latest"]},
                               "repository": {"full_name": "o/r"}}});
    let event = json!({"Records": [
        {"eventSource": "aws:sqs", "body": serde_json::to_string(&eb).unwrap()}
    ]});
    let out = crate::handle(&app, &event, None).await.unwrap();
    assert_eq!(out, json!({"ok": ["labels not ours"]}));
}

#[tokio::test]
async fn sqs_dispatch_failure_propagates_so_the_message_retries() {
    let p = FakePlatform::new().with_vms(vec![vm("RUNNING")]);
    let mut cfg = test_cfg();
    cfg.max_concurrency = 1; // gate trips -> RuntimeError
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let eb = json!({"detail-type": "workflow_job",
                    "detail": {"action": "queued",
                               "workflow_job": {"id": 5, "labels": ["self-hosted", "microvm"]},
                               "repository": {"full_name": "o/r"}}});
    let event = json!({"Records": [
        {"eventSource": "aws:sqs", "body": serde_json::to_string(&eb).unwrap()}
    ]});
    let err = crate::handle(&app, &event, None).await.unwrap_err();
    assert_eq!(err.kind, "RuntimeError");
}

#[tokio::test]
async fn function_url_never_errors_on_dispatch_failure() {
    let p = FakePlatform::new().with_vms(vec![vm("RUNNING")]);
    p.arm_github_auth();
    let mut cfg = test_cfg();
    cfg.max_concurrency = 1; // dispatch will fail at the cap gate
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let body = serde_json::to_string(&json!({
        "action": "queued",
        "workflow_job": {"id": 9, "labels": ["self-hosted", "microvm"]},
        "repository": {"full_name": "o/r"},
        "installation": {"id": 1},
    }))
    .unwrap();
    let sig = sign(&body, "x");
    let event = json!({
        "headers": {"X-Hub-Signature-256": sig, "X-GitHub-Event": "workflow_job"},
        "body": body,
    });
    let out = crate::handle(&app, &event, None).await.unwrap();
    assert_eq!(out["statusCode"], 200);
    let msg: Value = serde_json::from_str(out["body"].as_str().unwrap()).unwrap();
    let text = msg["msg"].as_str().unwrap();
    assert!(
        text.starts_with("dispatch failed for o/r: RuntimeError: concurrency cap"),
        "{text}"
    );
}

#[tokio::test]
async fn function_url_rejects_bad_signature_and_answers_ping() {
    let p = FakePlatform::new();
    p.arm_github_auth();
    let cfg = test_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let bad = json!({"headers": {"x-hub-signature-256": "sha256=deadbeef"}, "body": "{}"});
    let out = crate::handle(&app, &bad, None).await.unwrap();
    assert_eq!(out["statusCode"], 401);

    let body = "{}";
    let ping = json!({
        "headers": {"X-Hub-Signature-256": sign(body, "x"), "X-GitHub-Event": "ping"},
        "body": body,
    });
    let out = crate::handle(&app, &ping, None).await.unwrap();
    assert_eq!(out["statusCode"], 200);
    assert_eq!(out["body"], "{\"msg\": \"pong\"}");
}

#[tokio::test]
async fn function_url_decodes_base64_bodies() {
    use base64::Engine;
    let p = FakePlatform::new();
    p.arm_github_auth();
    let cfg = test_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let body = serde_json::to_string(&json!({"action": "ping-ish"})).unwrap();
    let sig = sign(&body, "x");
    let event = json!({
        "headers": {"X-Hub-Signature-256": sig, "X-GitHub-Event": "workflow_job"},
        "body": base64::engine::general_purpose::STANDARD.encode(&body),
        "isBase64Encoded": true,
    });
    let out = crate::handle(&app, &event, None).await.unwrap();
    assert_eq!(out["statusCode"], 200);
    assert_eq!(out["body"], "{\"msg\": \"ignored action ping-ish\"}");
}

// ── sweep ────────────────────────────────────────────────────────────────────

const ZOMBIE_VM_ID: &str = "microvm-bbbb1111-2222-3333-4444-555566667777";

fn arm_sweep_github(p: &FakePlatform) {
    p.arm_github_auth();
    p.add_gh_rule("/app/installations", Ok((200, json!([{"id": 1}]))));
    p.add_gh_rule(
        "/installation/repositories",
        Ok((200, json!({"repositories": [{"full_name": "o/r"}]}))),
    );
    p.add_gh_rule(
        "/actions/runners",
        Ok((200, json!({"runners": [{"name": "gha-mvm-zzzz"}]}))),
    );
    p.add_gh_rule(
        "status=queued",
        Ok((
            200,
            json!({"workflow_runs": [{"id": 42, "created_at": "2020-01-01T00:00:00Z"}]}),
        )),
    );
    p.add_gh_rule(
        "status=in_progress",
        Ok((200, json!({"workflow_runs": []}))),
    );
    p.add_gh_rule(
        "/jobs?",
        Ok((
            200,
            json!({"jobs": [{"id": 7, "status": "queued",
                                   "labels": ["self-hosted", "microvm"],
                                   "created_at": "2020-01-01T00:00:00Z"}]}),
        )),
    );
}

#[tokio::test]
async fn sweep_redispatches_stale_queued_jobs_and_reaps_zombies() {
    use crate::testutil::NOW;
    // One RUNNING VM, older than 2x SWEEP_MIN_AGE, whose derived runner name
    // is registered nowhere -> zombie.
    let p = FakePlatform::new().with_vms(vec![vm_item(
        ZOMBIE_VM_ID,
        "RUNNING",
        "9",
        Some(NOW - 800.0),
    )]);
    arm_sweep_github(&p);
    let cfg = test_cfg(); // pool disabled: handoff/pool GC skipped like Python
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    let dispatched = app.sweep(&|| false).await.unwrap();
    assert_eq!(dispatched, 1, "the stale queued job is re-dispatched");
    assert_eq!(p.run_payloads.lock().unwrap().len(), 1);
    assert_eq!(
        *p.terminate_calls.lock().unwrap(),
        vec![ZOMBIE_VM_ID.to_string()]
    );
}

#[tokio::test]
async fn sweep_partial_scan_never_reaps() {
    use crate::testutil::NOW;
    let p = FakePlatform::new().with_vms(vec![vm_item(
        ZOMBIE_VM_ID,
        "RUNNING",
        "9",
        Some(NOW - 800.0),
    )]);
    arm_sweep_github(&p);
    let cfg = test_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    // Deadline pressure from the start: scan_complete = false.
    let dispatched = app.sweep(&|| true).await.unwrap();
    assert_eq!(dispatched, 0);
    assert!(
        p.terminate_calls.lock().unwrap().is_empty(),
        "partial view must not reap"
    );
}

#[tokio::test]
async fn sweep_registered_runner_is_not_a_zombie() {
    use crate::testutil::NOW;
    let p = FakePlatform::new().with_vms(vec![vm_item(
        ZOMBIE_VM_ID,
        "RUNNING",
        "9",
        Some(NOW - 800.0),
    )]);
    p.arm_github_auth();
    p.add_gh_rule("/app/installations", Ok((200, json!([{"id": 1}]))));
    p.add_gh_rule(
        "/installation/repositories",
        Ok((200, json!({"repositories": [{"full_name": "o/r"}]}))),
    );
    // The VM's derived runner name IS registered -> the watchdog's turf.
    p.add_gh_rule(
        "/actions/runners",
        Ok((
            200,
            json!({"runners": [{"name": types::runner_name(ZOMBIE_VM_ID)}]}),
        )),
    );
    p.add_gh_rule("status=", Ok((200, json!({"workflow_runs": []}))));
    let cfg = test_cfg();
    let caches = quiet_caches();
    let app = App::new(&p, &cfg, &caches);
    app.sweep(&|| false).await.unwrap();
    assert!(p.terminate_calls.lock().unwrap().is_empty());
}

fn sign(body: &str, secret: &str) -> String {
    use hmac::{KeyInit, Mac};
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}
