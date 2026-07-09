//! Thin routing: Lambda event → [`Intake`] → domain services → response.
//!
//! Error discipline: the SQS path never fails the invocation — each record
//! succeeds or lands in `batchItemFailures` (partial batch responses; the
//! event source mapping sets `ReportBatchItemFailures`), so a retryable
//! dispatch failure re-drives exactly that message (the queue is the job
//! queue) and a malformed record dead-letters alone without re-driving
//! already-dispatched batch siblings. The legacy Function-URL path never
//! errors on dispatch failure (single attempt, failure reported in `msg`).

use serde_json::{Value, json};
use std::fmt;
use types::fnurl::FnUrlResponse;

use crate::dispatch::DispatchError;
use crate::github::GithubError;
use crate::intake::fnurl_req::FnUrlRequest;
use crate::intake::webhook::WebhookPayload;
use crate::intake::{Intake, IntakeError};
use crate::oplog;
use crate::pool::IntakeOutcome;
use crate::secrets::SecretsError;
use crate::services::Services;
use crate::sweep::Deadline;

#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    #[error("malformed intake: {0}")]
    Intake(#[from] IntakeError),
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
    #[error(transparent)]
    Secrets(#[from] SecretsError),
    #[error(transparent)]
    Github(#[from] GithubError),
}

pub async fn handle(
    svc: &Services,
    event: Value,
    deadline: Deadline,
) -> Result<Value, HandlerError> {
    match Intake::classify(event)? {
        Intake::SqsBatch(records) => {
            let mut outcomes: Vec<String> = Vec::with_capacity(records.len());
            let mut failures: Vec<Value> = Vec::new();
            for record in records {
                let outcome = match &record.envelope {
                    Ok(envelope) => on_workflow_job(svc, &envelope.payload)
                        .await
                        .map(|o| o.to_string()),
                    Err(e) => Err(HandlerError::Intake(e.clone())),
                };
                outcomes.push(match outcome {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        let msg = e.to_string();
                        oplog::emit(json!({
                            "sqs_item_failed": oplog::trunc(&msg, 300),
                            "messageId": record.message_id,
                        }));
                        failures.push(json!({"itemIdentifier": record.message_id}));
                        format!("failed: {msg}")
                    }
                });
            }
            Ok(json!({ "ok": outcomes, "batchItemFailures": failures }))
        }
        Intake::EventBridge(envelope) => Ok(json!({
            "ok": on_workflow_job(svc, &envelope.payload).await?.to_string()
        })),
        Intake::Sweep => {
            let dispatched = svc.sweeper.sweep(&deadline).await?;
            Ok(json!({"ok": "swept", "dispatched": dispatched}))
        }
        // A VM's direct idle report: never errors (the report is advisory —
        // the VM has its own terminate fallback).
        Intake::Idle(report) => Ok(json!({
            "ok": svc.pool.intake_idle(&report).await.to_string()
        })),
        Intake::FunctionUrl(req) => on_function_url(svc, req).await,
    }
}

enum EventOutcome {
    Dispatched,
    LabelsNotOurs,
    NoRepository,
    Ignored(Option<String>),
    Completed(IntakeOutcome),
}

impl fmt::Display for EventOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dispatched => f.write_str("dispatched"),
            Self::LabelsNotOurs => f.write_str("labels not ours"),
            Self::NoRepository => f.write_str("no repository in payload"),
            Self::Ignored(action) => {
                write!(f, "ignored action {}", action.as_deref().unwrap_or("None"))
            }
            Self::Completed(outcome) => outcome.fmt(f),
        }
    }
}

/// One EventBridge `workflow_job` envelope. Benign outcomes for skips; `Err`
/// only for retryable dispatch failures.
async fn on_workflow_job(
    svc: &Services,
    payload: &WebhookPayload,
) -> Result<EventOutcome, HandlerError> {
    let labels = payload.labels();
    oplog::emit(json!({
        "event": format!("eb:{}", payload.action.as_deref().unwrap_or("None")),
        "repo": payload.repo(),
        "job_id": payload.job_id(),
        "labels": labels.iter().collect::<Vec<_>>(),
    }));
    match payload.action.as_deref() {
        Some("queued") => {
            if !svc.cfg.required_labels.is_subset(&labels) {
                return Ok(EventOutcome::LabelsNotOurs);
            }
            let Some(job) = payload.job_ref() else {
                return Ok(EventOutcome::NoRepository);
            };
            svc.dispatcher.dispatch(&job).await?; // Err propagates == retry
            Ok(EventOutcome::Dispatched)
        }
        Some("completed") => Ok(EventOutcome::Completed(
            svc.pool.intake_completed(payload).await,
        )),
        action => Ok(EventOutcome::Ignored(action.map(str::to_string))),
    }
}

/// The direct Function-URL path — single-attempt semantics; every branch
/// answers GitHub, and dispatch failure is a 200 with the reason in `msg`.
async fn on_function_url(svc: &Services, req: FnUrlRequest) -> Result<Value, HandlerError> {
    // Secret fetch failures DO error (nothing can be verified without it).
    let bundle = svc.secrets.bundle().await?;
    if !types::sig::verify(req.body(), req.signature(), &bundle.webhook_secret) {
        return Ok(respond(401, "invalid signature"));
    }

    let gh_event = req.github_event();
    if gh_event == "ping" {
        return Ok(respond(200, "pong"));
    }
    if gh_event != "workflow_job" {
        return Ok(respond(200, &format!("ignored event {gh_event}")));
    }

    let payload = req.json_payload()?;
    if payload.action.as_deref() == Some("completed") {
        let outcome = svc.pool.intake_completed(&payload).await;
        return Ok(respond(200, &outcome.to_string()));
    }
    if payload.action.as_deref() != Some("queued") {
        return Ok(respond(
            200,
            &format!(
                "ignored action {}",
                payload.action.as_deref().unwrap_or("None")
            ),
        ));
    }

    let labels = payload.labels();
    oplog::emit(json!({
        "event": "queued",
        "repo": payload.repo(),
        "job_id": payload.job_id(),
        "labels": labels.iter().collect::<Vec<_>>(),
        "installation_id": payload.installation_id().map(|i| i.0),
    }));
    if !svc.cfg.required_labels.is_subset(&labels) {
        let have: Vec<&String> = labels.iter().collect();
        let need: Vec<&String> = svc.cfg.required_labels.iter().collect();
        return Ok(respond(
            200,
            &format!("labels {have:?} missing required {need:?}"),
        ));
    }
    let Some(job) = payload.job_ref() else {
        return Ok(respond(400, "no repository in payload"));
    };

    match svc.dispatcher.dispatch(&job).await {
        Ok(()) => Ok(respond(
            202,
            &format!(
                "dispatched for {} job {}",
                job.repo,
                job.job_id.map_or("None".to_string(), |i| i.to_string())
            ),
        )),
        // Single-attempt path: report, never raise.
        Err(e) => Ok(respond(
            200,
            &format!("dispatch failed for {}: {e}", job.repo),
        )),
    }
}

/// Log `{"status", "msg"}` and shape the Function-URL response.
fn respond(status: u16, msg: &str) -> Value {
    oplog::emit(json!({"status": status, "msg": msg}));
    serde_json::to_value(FnUrlResponse::msg(status, msg)).expect("FnUrlResponse always serializes")
}
