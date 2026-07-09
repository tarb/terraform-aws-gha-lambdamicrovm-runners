//! GitHub Actions runner autoscaler — webhook dispatcher (Rust port).
//!
//! Faithful port of `dispatcher/handler.py` (the normative spec). Four intake
//! shapes, routed exactly as Python's `handler()` does:
//!   1. SQS-delivered EventBridge envelopes (retryable failures return Err so
//!      the message goes back to the queue — that queue IS the job queue),
//!   2. direct EventBridge invocation,
//!   3. the `{"sweep": true}` scheduled-reconciliation marker,
//!   4. the legacy Function-URL webhook path (HMAC verify, base64 body;
//!      single-attempt semantics, never errors).

mod app;
mod config;
mod dispatch;
mod fleet;
mod gh;
mod platform;
mod pool;
mod pyfmt;
mod sweep;
mod timeparse;

#[cfg(test)]
mod testutil;

use base64::Engine;
use lambda_runtime::{LambdaEvent, service_fn};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::app::{App, Caches};
use crate::config::Config;
use crate::gh::verify;
use crate::platform::{Platform, RealPlatform};
use crate::pyfmt::{PyErr, dumps, logln, py_list_repr, py_str, truthy, v_index};

/// Which of the four intake shapes an event is (pure, unit-testable).
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    SqsBatch,
    EventBridge,
    Sweep,
    FunctionUrl,
}

pub fn route(event: &Value) -> Route {
    if let Some(records) = event.get("Records").and_then(Value::as_array)
        && !records.is_empty()
        && records[0].get("eventSource").and_then(Value::as_str) == Some("aws:sqs")
    {
        return Route::SqsBatch;
    }
    if event.get("detail-type").and_then(Value::as_str) == Some("workflow_job") {
        return Route::EventBridge;
    }
    if truthy(event.get("sweep").unwrap_or(&Value::Null)) {
        return Route::Sweep;
    }
    Route::FunctionUrl
}

/// `_resp`: log + shape the Function-URL response.
fn resp(code: i64, msg: &str) -> Value {
    logln(&json!({"status": code, "msg": msg}));
    json!({
        "statusCode": code,
        "headers": {"content-type": "application/json"},
        "body": dumps(&json!({"msg": msg})),
    })
}

/// `handler(event, context)`. `remaining_ms` feeds the sweep's deadline check
/// (None mirrors Python's `context=None`).
pub async fn handle<P: Platform>(
    app: &App<'_, P>,
    event: &Value,
    remaining_ms: Option<Box<dyn Fn() -> i64 + Sync>>,
) -> Result<Value, PyErr> {
    match route(event) {
        Route::SqsBatch => {
            let records = event["Records"].as_array().expect("checked by route()");
            let mut results: Vec<Value> = Vec::new();
            for record in records {
                let body = v_index(record, "body")?
                    .as_str()
                    .ok_or_else(|| PyErr::type_error("SQS body is not a string"))?;
                let eb: Value = serde_json::from_str(body).map_err(PyErr::json_error)?;
                results.push(Value::String(handle_workflow_job_event(app, &eb).await?));
            }
            Ok(json!({"ok": results}))
        }
        Route::EventBridge => Ok(json!({"ok": handle_workflow_job_event(app, event).await?})),
        Route::Sweep => {
            let low_time: Box<dyn Fn() -> bool + Sync> = match remaining_ms {
                Some(f) => Box::new(move || f() < 15000),
                None => Box::new(|| false),
            };
            let dispatched = app.sweep(low_time.as_ref()).await?;
            Ok(json!({"ok": "swept", "dispatched": dispatched}))
        }
        Route::FunctionUrl => function_url(app, event).await,
    }
}

/// `_handle_workflow_job_event`: one EventBridge workflow_job envelope.
/// Returns a benign string for skips; Err for retryable dispatch failures.
async fn handle_workflow_job_event<P: Platform>(
    app: &App<'_, P>,
    eb_event: &Value,
) -> Result<String, PyErr> {
    // payload = eb_event.get("detail") or {}; parsed defensively if a string.
    let detail = eb_event.get("detail").cloned().unwrap_or(Value::Null);
    let payload: Value = if !truthy(&detail) {
        json!({})
    } else if let Value::String(s) = &detail {
        serde_json::from_str(s).map_err(PyErr::json_error)?
    } else {
        detail
    };
    let action = payload.get("action").cloned().unwrap_or(Value::Null);
    let job = payload.get("workflow_job").cloned().unwrap_or(json!({}));
    let labels = job_labels(&job);
    let repo = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .cloned()
        .unwrap_or(Value::Null);
    let installation_id = payload
        .get("installation")
        .and_then(|i| i.get("id"))
        .and_then(Value::as_i64);
    let sorted_labels: Vec<&String> = labels.iter().collect();
    logln(
        &json!({"event": format!("eb:{}", py_str(&action)), "repo": repo,
                  "job_id": job.get("id").cloned().unwrap_or(Value::Null),
                  "labels": sorted_labels}),
    );
    if action.as_str() == Some("queued") {
        if !app.cfg.required_labels.is_subset(&labels) {
            return Ok("labels not ours".to_string());
        }
        let repo_s = repo.as_str().unwrap_or("");
        if repo_s.is_empty() {
            return Ok("no repository in payload".to_string());
        }
        let job_id = job.get("id").cloned().unwrap_or(Value::Null);
        app.dispatch_job(repo_s, &job_id, installation_id).await?; // raises to retry
        return Ok("dispatched".to_string());
    }
    if action.as_str() == Some("completed") {
        return Ok(app.handle_completed(&payload).await);
    }
    Ok(format!("ignored action {}", py_str(&action)))
}

/// The direct Function-URL path — single-attempt semantics, never raises on
/// dispatch failure.
async fn function_url<P: Platform>(app: &App<'_, P>, event: &Value) -> Result<Value, PyErr> {
    let headers: std::collections::HashMap<String, String> = event
        .get("headers")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.to_lowercase(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let raw = event.get("body").and_then(Value::as_str).unwrap_or("");
    let body: Vec<u8> = if truthy(event.get("isBase64Encoded").unwrap_or(&Value::Null)) {
        base64::engine::general_purpose::STANDARD
            .decode(raw)
            .map_err(|e| PyErr::new("binascii.Error", e.to_string()))?
    } else {
        raw.as_bytes().to_vec()
    };

    let secret = app.secrets().await?;
    let webhook_secret = v_index(&secret, "webhook_secret")?
        .as_str()
        .ok_or_else(|| PyErr::type_error("webhook_secret is not a string"))?;
    if !verify(
        &body,
        headers.get("x-hub-signature-256").map(String::as_str),
        webhook_secret,
    ) {
        return Ok(resp(401, "invalid signature"));
    }

    let gh_event = headers
        .get("x-github-event")
        .map(String::as_str)
        .unwrap_or("");
    if gh_event == "ping" {
        return Ok(resp(200, "pong"));
    }
    if gh_event != "workflow_job" {
        return Ok(resp(200, &format!("ignored event {gh_event}")));
    }

    let payload: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).map_err(PyErr::json_error)?
    };
    let action = payload.get("action").cloned().unwrap_or(Value::Null);
    let job = payload.get("workflow_job").cloned().unwrap_or(json!({}));
    let labels = job_labels(&job);
    let repo = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .cloned()
        .unwrap_or(Value::Null);
    let installation_id = payload
        .get("installation")
        .and_then(|i| i.get("id"))
        .cloned()
        .unwrap_or(Value::Null);

    if action.as_str() == Some("completed") {
        // Pool intake also works on this path. Never raises.
        let msg = app.handle_completed(&payload).await;
        return Ok(resp(200, &msg));
    }
    if action.as_str() != Some("queued") {
        return Ok(resp(200, &format!("ignored action {}", py_str(&action))));
    }
    let sorted_labels: Vec<&String> = labels.iter().collect();
    logln(&json!({"event": "queued", "repo": repo,
                  "job_id": job.get("id").cloned().unwrap_or(Value::Null),
                  "labels": sorted_labels, "installation_id": installation_id}));
    if !app.cfg.required_labels.is_subset(&labels) {
        let have: Vec<String> = labels.iter().cloned().collect();
        let need: Vec<String> = app.cfg.required_labels.iter().cloned().collect();
        return Ok(resp(
            200,
            &format!(
                "labels {} missing required {}",
                py_list_repr(&have),
                py_list_repr(&need)
            ),
        ));
    }
    let repo_s = repo.as_str().unwrap_or("");
    if repo_s.is_empty() {
        return Ok(resp(400, "no repository in payload"));
    }

    let job_id = job.get("id").cloned().unwrap_or(Value::Null);
    match app
        .dispatch_job(repo_s, &job_id, installation_id.as_i64())
        .await
    {
        // single-attempt path reports, never raises
        Err(e) => Ok(resp(
            200,
            &format!("dispatch failed for {}: {}: {}", repo_s, e.kind, e.msg),
        )),
        Ok(()) => Ok(resp(
            202,
            &format!("dispatched for {} job {}", repo_s, py_str(&job_id)),
        )),
    }
}

/// `set(job.get("labels", []))` as a sorted string set (Python `sorted(labels)`
/// is byte-order for the ASCII labels in play).
fn job_labels(job: &Value) -> std::collections::BTreeSet<String> {
    job.get("labels")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::main]
async fn main() -> Result<(), lambda_runtime::Error> {
    // Extra diagnostics only — operational log lines go to stdout via
    // println! so their shape stays identical to the Python prints.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let cfg = Arc::new(Config::from_env().map_err(|e| format!("{}: {}", e.kind, e.msg))?);
    let shared = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let platform = Arc::new(RealPlatform::new(&shared));
    let caches = Arc::new(Caches::default());

    lambda_runtime::run(service_fn(move |ev: LambdaEvent<Value>| {
        let cfg = Arc::clone(&cfg);
        let platform = Arc::clone(&platform);
        let caches = Arc::clone(&caches);
        async move {
            let deadline_ms = ev.context.deadline as i64;
            let remaining: Box<dyn Fn() -> i64 + Sync> = Box::new(move || {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                deadline_ms - now_ms
            });
            let app = App::new(platform.as_ref(), cfg.as_ref(), caches.as_ref());
            handle(&app, &ev.payload, Some(remaining))
                .await
                .map_err(|e| lambda_runtime::Error::from(format!("{}: {}", e.kind, e.msg)))
        }
    }))
    .await
}

#[cfg(test)]
mod behavior_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_sqs_batch() {
        let ev = json!({"Records": [{"eventSource": "aws:sqs", "body": "{}"}]});
        assert_eq!(route(&ev), Route::SqsBatch);
    }

    #[test]
    fn routes_non_sqs_records_to_function_url() {
        assert_eq!(route(&json!({"Records": []})), Route::FunctionUrl);
        assert_eq!(
            route(&json!({"Records": [{"eventSource": "aws:s3"}]})),
            Route::FunctionUrl
        );
    }

    #[test]
    fn routes_eventbridge_detail_type() {
        assert_eq!(
            route(&json!({"detail-type": "workflow_job", "detail": {}})),
            Route::EventBridge
        );
        assert_eq!(route(&json!({"detail-type": "other"})), Route::FunctionUrl);
    }

    #[test]
    fn routes_sweep_marker_with_python_truthiness() {
        assert_eq!(route(&json!({"sweep": true})), Route::Sweep);
        assert_eq!(route(&json!({"sweep": 1})), Route::Sweep);
        assert_eq!(route(&json!({"sweep": "yes"})), Route::Sweep);
        assert_eq!(route(&json!({"sweep": false})), Route::FunctionUrl);
        assert_eq!(route(&json!({"sweep": 0})), Route::FunctionUrl);
        assert_eq!(route(&json!({"sweep": ""})), Route::FunctionUrl);
    }

    #[test]
    fn function_url_is_the_fallback() {
        assert_eq!(
            route(&json!({"headers": {}, "body": ""})),
            Route::FunctionUrl
        );
    }
}
