//! GitHub webhook -> EventBridge proxy.
//!
//! Deliberately does almost NOTHING: verify the HMAC signature and PutEvents
//! the payload onto the bus. All real work (GitHub API, RunMicrovm, pool
//! management) lives in the dispatcher behind EventBridge rules — which retry
//! with backoff and dead-letter on exhaustion. GitHub delivers webhooks
//! exactly once with no retries, so the less this endpoint does, the less can
//! be lost; a PutEvents call has a tiny failure surface compared to inline
//! dispatch.
//!
//! Events: `Source=github.webhook`, `DetailType=<x-github-event>`,
//! `Detail=<payload>`. Oversized payloads (EventBridge caps entries at 256KB)
//! are slimmed to the fields the dispatcher consumes rather than dropped.
//!
//! Compatibility surface (monitoring dashboards grep these — keep stable):
//! env vars `AWS_REGION`/`EVENT_BUS_NAME`/`PARAM_NAME`, the response bodies,
//! the stdout `{"status", "msg"}` line on every response path, and the
//! `{"put_events_failed", "attempt", "code", "msg"}` retry line.

use async_trait::async_trait;
use base64::Engine as _;
use lambda_runtime::LambdaEvent;
use secrecy::SecretString;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use types::fnurl::FnUrlResponse;

/// Keep comfortably under the 256KB PutEvents entry cap — headroom for the
/// envelope EventBridge adds around the detail.
const MAX_DETAIL_BYTES: usize = 240_000;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Every failure in this crate is a "raise to Lambda": the invocation errors,
/// the Function URL answers 502, and the failure is visible in GitHub's
/// webhook delivery UI. Nothing here is worth degrading gracefully over —
/// GitHub redelivers on operator request and the sweep reconciles missed jobs.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct ProxyError(Box<dyn std::error::Error + Send + Sync + 'static>);

impl ProxyError {
    fn wrap(e: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self(Box::new(e))
    }

    fn msg(m: impl Into<String>) -> Self {
        Self(m.into().into())
    }
}

impl From<serde_json::Error> for ProxyError {
    fn from(e: serde_json::Error) -> Self {
        Self::wrap(e)
    }
}

impl From<base64::DecodeError> for ProxyError {
    fn from(e: base64::DecodeError) -> Self {
        Self::wrap(e)
    }
}

// ---------------------------------------------------------------------------
// Function-URL request
// ---------------------------------------------------------------------------

/// The slice of a Lambda Function-URL event this proxy reads. Unknown fields
/// (requestContext, rawPath, ...) are ignored.
#[derive(Debug, Default, Deserialize)]
struct FnUrlEvent {
    /// Header names lowercased on ingest; lookups use lowercase keys.
    #[serde(default, deserialize_with = "lower_keys")]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default, rename = "isBase64Encoded")]
    is_base64_encoded: bool,
}

/// Lowercase header names. Tolerates `"headers": null` and drops non-string
/// values — Function URLs only ever send single-valued string headers.
fn lower_keys<'de, D: Deserializer<'de>>(de: D) -> Result<HashMap<String, String>, D::Error> {
    let raw = Option::<HashMap<String, Value>>::deserialize(de)?;
    Ok(raw
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| match v {
            Value::String(s) => Some((k.to_ascii_lowercase(), s)),
            _ => None,
        })
        .collect())
}

impl FnUrlEvent {
    /// The raw request body, decoded per `isBase64Encoded`. An undecodable
    /// base64 body is a hard error (Lambda invocation failure).
    fn body_bytes(&self) -> Result<Vec<u8>, ProxyError> {
        let raw = self.body.as_deref().unwrap_or("");
        if self.is_base64_encoded {
            Ok(base64::engine::general_purpose::STANDARD.decode(raw)?)
        } else {
            Ok(raw.as_bytes().to_vec())
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// `x-github-event`, or "" when absent (forwarded as an empty detail-type).
    fn github_event(&self) -> &str {
        self.header("x-github-event").unwrap_or("")
    }

    fn signature(&self) -> Option<&str> {
        self.header("x-hub-signature-256")
    }
}

// ---------------------------------------------------------------------------
// Oversize fallback: the dispatcher's subset
// ---------------------------------------------------------------------------

/// The subset of a workflow_job webhook the dispatcher consumes — sent only
/// when the full payload would exceed the PutEvents entry cap. Missing fields
/// serialize as `null`; a falsy `workflow_job`/`repository`/`installation`
/// reads as an empty object, so all of its fields land as `null` too.
#[derive(Debug, Serialize)]
struct SlimPayload {
    action: Value,
    workflow_job: SlimJob,
    repository: SlimRepository,
    installation: SlimInstallation,
    #[serde(rename = "_slimmed")]
    slimmed: bool,
}

#[derive(Debug, Serialize)]
struct SlimJob {
    id: Value,
    run_id: Value,
    labels: Value,
    runner_name: Value,
    status: Value,
}

#[derive(Debug, Serialize)]
struct SlimRepository {
    full_name: Value,
}

#[derive(Debug, Serialize)]
struct SlimInstallation {
    id: Value,
}

fn slim(payload: &Value) -> SlimPayload {
    let job = section(payload, "workflow_job");
    let repo = section(payload, "repository");
    let installation = section(payload, "installation");
    SlimPayload {
        action: field(Some(payload), "action"),
        workflow_job: SlimJob {
            id: field(job, "id"),
            run_id: field(job, "run_id"),
            labels: field(job, "labels"),
            runner_name: field(job, "runner_name"),
            status: field(job, "status"),
        },
        repository: SlimRepository {
            full_name: field(repo, "full_name"),
        },
        installation: SlimInstallation {
            id: field(installation, "id"),
        },
        slimmed: true,
    }
}

/// A sub-object of the payload, or `None` when it is absent or falsy
/// (null / false / 0 / "" / [] / {}) so that lookups inside it yield `null`.
fn section<'a>(payload: &'a Value, key: &str) -> Option<&'a Value> {
    payload.get(key).filter(|v| truthy(v))
}

/// A field of an (optional) sub-object; `null` when either is missing.
fn field(container: Option<&Value>, key: &str) -> Value {
    container
        .and_then(|v| v.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

/// JSON falsiness: null, false, zero, and empty string/array/object.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

/// Result of one PutEvents call, as the handler consumes it. The request only
/// ever carries one entry, so `error_code`/`error_message` come from the
/// first response entry.
#[derive(Debug, Clone, Default)]
struct PutOutcome {
    failed_entry_count: i64,
    error_code: Option<String>,
    error_message: Option<String>,
}

/// The event bus the verified payload is forwarded to. `Err` models a failed
/// API call (as opposed to a per-entry failure reported in `PutOutcome`) and
/// propagates out of the handler as a Lambda invocation error.
#[async_trait]
trait EventBus: Send + Sync {
    async fn put(&self, detail_type: &str, detail: &str) -> Result<PutOutcome, ProxyError>;
    fn bus_name(&self) -> &str;
}

/// Where the webhook shared secret comes from.
#[async_trait]
trait SecretSource: Send + Sync {
    async fn webhook_secret(&self) -> Result<SecretString, ProxyError>;
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// One operational log line: single JSON object on stdout.
fn oplog(line: Value) {
    println!("{line}");
}

/// Build the Function-URL reply and emit its `{"status", "msg"}` log line.
fn reply(status: u16, msg: &str) -> Result<Value, ProxyError> {
    oplog(json!({"status": status, "msg": msg}));
    Ok(serde_json::to_value(FnUrlResponse::msg(status, msg))?)
}

async fn handle(
    event: &Value,
    secrets: &dyn SecretSource,
    bus: &dyn EventBus,
) -> Result<Value, ProxyError> {
    let event = FnUrlEvent::deserialize(event)?;
    let body = event.body_bytes()?;

    // HMAC before anything else — nothing unverified gets past this point.
    // The check itself is the shared `types::sig` contract (constant-time,
    // lowercase hex only), same as the dispatcher's Function-URL path.
    let secret = secrets.webhook_secret().await?;
    if !types::sig::verify(&body, event.signature(), &secret) {
        return reply(401, "invalid signature");
    }

    let gh_event = event.github_event();
    if gh_event == "ping" {
        return reply(200, "pong");
    }

    let mut detail = String::from_utf8_lossy(&body).into_owned();
    if detail.len() > MAX_DETAIL_BYTES {
        // The cap is BYTES (String::len is a byte count). An oversized
        // payload that is not valid JSON is a hard error.
        let payload: Value = serde_json::from_str(&detail)?;
        detail = serde_json::to_string(&slim(&payload))?;
    }

    // PutEvents does NOT error on per-entry failures — check the response,
    // retry once, then 500 so the failure shows in GitHub's delivery UI.
    for attempt in 0..2 {
        let out = bus.put(gh_event, &detail).await?;
        if out.failed_entry_count == 0 {
            return reply(202, &format!("forwarded {gh_event} to {}", bus.bus_name()));
        }
        oplog(json!({
            "put_events_failed": true,
            "attempt": attempt,
            "code": out.error_code,
            "msg": out.error_message,
        }));
    }
    reply(500, "PutEvents failed")
}

// ---------------------------------------------------------------------------
// Real AWS wiring
// ---------------------------------------------------------------------------

/// The secret-bundle SSM parameter; only `webhook_secret` matters here.
#[derive(Deserialize)]
struct SecretBundle {
    webhook_secret: SecretString,
}

struct SsmSecrets {
    ssm: aws_sdk_ssm::Client,
    param_name: String,
    /// Fetched once per warm container.
    cached: tokio::sync::OnceCell<SecretString>,
}

#[async_trait]
impl SecretSource for SsmSecrets {
    async fn webhook_secret(&self) -> Result<SecretString, ProxyError> {
        let secret = self
            .cached
            .get_or_try_init(|| async {
                let out = self
                    .ssm
                    .get_parameter()
                    .name(&self.param_name)
                    .with_decryption(true)
                    .send()
                    .await
                    .map_err(ProxyError::wrap)?;
                let raw = out
                    .parameter()
                    .and_then(|p| p.value())
                    .ok_or_else(|| ProxyError::msg("SSM parameter has no value"))?;
                let bundle: SecretBundle = serde_json::from_str(raw)?;
                Ok::<SecretString, ProxyError>(bundle.webhook_secret)
            })
            .await?;
        Ok(secret.clone())
    }
}

struct EventBridgeBus {
    events: aws_sdk_eventbridge::Client,
    bus_name: String,
}

#[async_trait]
impl EventBus for EventBridgeBus {
    async fn put(&self, detail_type: &str, detail: &str) -> Result<PutOutcome, ProxyError> {
        let entry = aws_sdk_eventbridge::types::PutEventsRequestEntry::builder()
            .source("github.webhook")
            .detail_type(detail_type)
            .detail(detail)
            .event_bus_name(&self.bus_name)
            .build();
        let out = self
            .events
            .put_events()
            .entries(entry)
            .send()
            .await
            .map_err(ProxyError::wrap)?;
        let first = out.entries().first();
        Ok(PutOutcome {
            failed_entry_count: i64::from(out.failed_entry_count()),
            error_code: first.and_then(|e| e.error_code().map(str::to_string)),
            error_message: first.and_then(|e| e.error_message().map(str::to_string)),
        })
    }

    fn bus_name(&self) -> &str {
        &self.bus_name
    }
}

#[tokio::main]
async fn main() -> Result<(), lambda_runtime::Error> {
    // Diagnostics only, on stderr — the operational lines are the stdout
    // JSON emitted by `oplog` and must keep their key shape.
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string());
    let bus_name = std::env::var("EVENT_BUS_NAME").expect("EVENT_BUS_NAME env var is required");
    let param_name = std::env::var("PARAM_NAME").expect("PARAM_NAME env var is required");

    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region))
        .load()
        .await;

    let secrets = SsmSecrets {
        ssm: aws_sdk_ssm::Client::new(&config),
        param_name,
        cached: tokio::sync::OnceCell::new(),
    };
    let bus = EventBridgeBus {
        events: aws_sdk_eventbridge::Client::new(&config),
        bus_name,
    };
    let (secrets, bus) = (&secrets, &bus);

    lambda_runtime::run(lambda_runtime::service_fn(
        move |event: LambdaEvent<Value>| async move {
            handle(&event.payload, secrets, bus)
                .await
                .map_err(lambda_runtime::Error::from)
        },
    ))
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// GitHub's documented example webhook secret. Signature verification
    /// itself (golden vector, rejects) is tested where it lives: `types::sig`.
    const GOLDEN_SECRET: &str = "It's a Secret to Everybody";

    fn secret(s: &str) -> SecretString {
        SecretString::from(s.to_string())
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        use hmac::{Hmac, KeyInit, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    /// Parse the JSON string in the response's `body` field.
    fn body_json(resp: &Value) -> Value {
        serde_json::from_str(resp["body"].as_str().expect("body is a string")).unwrap()
    }

    // -- slim -----------------------------------------------------------------

    #[test]
    fn slim_selects_dispatcher_subset() {
        let payload = json!({
            "action": "queued",
            "workflow_job": {
                "id": 42,
                "run_id": 7,
                "labels": ["self-hosted", "microvm"],
                "runner_name": "gha-mvm-abc",
                "status": "queued",
                "steps": ["huge", "stuff", "dropped"],
                "html_url": "https://github.com/o/r/runs/42"
            },
            "repository": {"full_name": "o/r", "description": "dropped"},
            "installation": {"id": 123, "node_id": "dropped"},
            "sender": {"login": "dropped"}
        });
        let s = serde_json::to_value(slim(&payload)).unwrap();
        assert_eq!(
            s,
            json!({
                "action": "queued",
                "workflow_job": {
                    "id": 42,
                    "run_id": 7,
                    "labels": ["self-hosted", "microvm"],
                    "runner_name": "gha-mvm-abc",
                    "status": "queued"
                },
                "repository": {"full_name": "o/r"},
                "installation": {"id": 123},
                "_slimmed": true
            })
        );
    }

    #[test]
    fn slim_missing_fields_become_null() {
        let s = serde_json::to_value(slim(&json!({}))).unwrap();
        assert_eq!(
            s,
            json!({
                "action": null,
                "workflow_job": {
                    "id": null,
                    "run_id": null,
                    "labels": null,
                    "runner_name": null,
                    "status": null
                },
                "repository": {"full_name": null},
                "installation": {"id": null},
                "_slimmed": true
            })
        );
        // falsy sub-objects read as empty, so their fields are null
        let s = serde_json::to_value(slim(
            &json!({"workflow_job": null, "repository": null, "installation": null}),
        ))
        .unwrap();
        assert_eq!(s["workflow_job"]["id"], Value::Null);
        assert_eq!(s["repository"]["full_name"], Value::Null);
        assert_eq!(s["installation"]["id"], Value::Null);
    }

    // -- response shape ---------------------------------------------------------

    #[test]
    fn resp_matches_python_shape() {
        let r = reply(200, "pong").unwrap();
        assert_eq!(r["statusCode"], 200);
        assert_eq!(r["headers"], json!({"content-type": "application/json"}));
        assert_eq!(body_json(&r), json!({"msg": "pong"}));
    }

    // -- event plumbing ---------------------------------------------------------

    #[test]
    fn headers_are_case_insensitive_and_body_base64_decodes() {
        let event = json!({
            "headers": {"X-GitHub-Event": "ping", "X-Hub-Signature-256": "sha256=aa"},
            "body": base64::engine::general_purpose::STANDARD.encode(b"hi"),
            "isBase64Encoded": true
        });
        let parsed = FnUrlEvent::deserialize(&event).unwrap();
        assert_eq!(parsed.github_event(), "ping");
        assert_eq!(parsed.signature(), Some("sha256=aa"));
        assert_eq!(parsed.body_bytes().unwrap(), b"hi");
        // plain body, and absent/null fallbacks
        let plain = FnUrlEvent::deserialize(&json!({"body": "raw"})).unwrap();
        assert_eq!(plain.body_bytes().unwrap(), b"raw");
        let null_body = FnUrlEvent::deserialize(&json!({"body": null})).unwrap();
        assert_eq!(null_body.body_bytes().unwrap(), b"");
        let empty = FnUrlEvent::deserialize(&json!({})).unwrap();
        assert_eq!(empty.body_bytes().unwrap(), b"");
        let null_headers = FnUrlEvent::deserialize(&json!({"headers": null})).unwrap();
        assert!(null_headers.headers.is_empty());
    }

    // -- full handler with hand-rolled fakes --------------------------------------

    struct FakeSecrets;

    #[async_trait]
    impl SecretSource for FakeSecrets {
        async fn webhook_secret(&self) -> Result<SecretString, ProxyError> {
            Ok(secret(GOLDEN_SECRET))
        }
    }

    struct FakeBus {
        /// One outcome per expected put call, in order.
        outcomes: Mutex<Vec<PutOutcome>>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl FakeBus {
        fn new(outcomes: Vec<PutOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl EventBus for FakeBus {
        async fn put(&self, detail_type: &str, detail: &str) -> Result<PutOutcome, ProxyError> {
            self.calls
                .lock()
                .unwrap()
                .push((detail_type.to_string(), detail.to_string()));
            Ok(self.outcomes.lock().unwrap().remove(0))
        }

        fn bus_name(&self) -> &str {
            "test-bus"
        }
    }

    fn signed_event(gh_event: &str, body: &str) -> Value {
        json!({
            "headers": {
                "X-GitHub-Event": gh_event,
                "X-Hub-Signature-256": sign(GOLDEN_SECRET, body.as_bytes())
            },
            "body": body,
            "isBase64Encoded": false
        })
    }

    #[tokio::test]
    async fn invalid_signature_is_401() {
        let bus = FakeBus::new(vec![]);
        let mut event = signed_event("workflow_job", "{}");
        event["headers"]["X-Hub-Signature-256"] = json!("sha256=00");
        let r = handle(&event, &FakeSecrets, &bus).await.unwrap();
        assert_eq!(r["statusCode"], 401);
        assert_eq!(body_json(&r), json!({"msg": "invalid signature"}));
        assert!(bus.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ping_short_circuits_before_put_events() {
        let bus = FakeBus::new(vec![]);
        let r = handle(
            &signed_event("ping", "{\"zen\": \"anything\"}"),
            &FakeSecrets,
            &bus,
        )
        .await
        .unwrap();
        assert_eq!(r["statusCode"], 200);
        assert_eq!(body_json(&r), json!({"msg": "pong"}));
        assert!(bus.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn forwards_small_payload_verbatim() {
        let bus = FakeBus::new(vec![PutOutcome::default()]);
        let body = "{\"action\": \"queued\"}";
        let r = handle(&signed_event("workflow_job", body), &FakeSecrets, &bus)
            .await
            .unwrap();
        assert_eq!(r["statusCode"], 202);
        assert_eq!(
            body_json(&r),
            json!({"msg": "forwarded workflow_job to test-bus"})
        );
        let calls = bus.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "workflow_job");
        assert_eq!(calls[0].1, body); // NOT slimmed, byte-for-byte passthrough
    }

    #[tokio::test]
    async fn oversize_payload_falls_back_to_slim() {
        let bus = FakeBus::new(vec![PutOutcome::default()]);
        let padding = "x".repeat(MAX_DETAIL_BYTES);
        let payload = json!({
            "action": "queued",
            "workflow_job": {"id": 1, "run_id": 2, "labels": ["a"], "runner_name": null,
                              "status": "queued", "steps": padding},
            "repository": {"full_name": "o/r"},
            "installation": {"id": 9}
        });
        let body = serde_json::to_string(&payload).unwrap();
        assert!(body.len() > MAX_DETAIL_BYTES);
        let r = handle(&signed_event("workflow_job", &body), &FakeSecrets, &bus)
            .await
            .unwrap();
        assert_eq!(r["statusCode"], 202);
        let calls = bus.calls.lock().unwrap();
        let detail: Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(detail["_slimmed"], true);
        assert_eq!(detail["workflow_job"]["id"], 1);
        assert_eq!(detail["workflow_job"]["run_id"], 2);
        assert_eq!(detail["repository"]["full_name"], "o/r");
        assert_eq!(detail["installation"]["id"], 9);
        assert!(detail["workflow_job"].get("steps").is_none());
        assert!(calls[0].1.len() <= MAX_DETAIL_BYTES);
    }

    #[tokio::test]
    async fn oversized_non_json_payload_is_a_hard_error() {
        let bus = FakeBus::new(vec![]);
        let body = "x".repeat(MAX_DETAIL_BYTES + 1);
        let r = handle(&signed_event("workflow_job", &body), &FakeSecrets, &bus).await;
        assert!(r.is_err());
        assert!(bus.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn base64_encoded_body_verifies_and_forwards() {
        let bus = FakeBus::new(vec![PutOutcome::default()]);
        let body = b"{\"action\": \"queued\"}";
        let event = json!({
            "headers": {
                "x-github-event": "workflow_job",
                "x-hub-signature-256": sign(GOLDEN_SECRET, body)
            },
            "body": base64::engine::general_purpose::STANDARD.encode(body),
            "isBase64Encoded": true
        });
        let r = handle(&event, &FakeSecrets, &bus).await.unwrap();
        assert_eq!(r["statusCode"], 202);
        assert_eq!(
            bus.calls.lock().unwrap()[0].1,
            String::from_utf8_lossy(body)
        );
    }

    #[tokio::test]
    async fn undecodable_base64_body_is_a_hard_error() {
        let bus = FakeBus::new(vec![]);
        let event = json!({
            "headers": {},
            "body": "%%% not base64 %%%",
            "isBase64Encoded": true
        });
        assert!(handle(&event, &FakeSecrets, &bus).await.is_err());
        assert!(bus.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn put_events_failure_retries_once_then_500() {
        let failed = PutOutcome {
            failed_entry_count: 1,
            error_code: Some("ThrottlingException".to_string()),
            error_message: Some("slow down".to_string()),
        };
        let bus = FakeBus::new(vec![failed.clone(), failed]);
        let r = handle(&signed_event("workflow_job", "{}"), &FakeSecrets, &bus)
            .await
            .unwrap();
        assert_eq!(r["statusCode"], 500);
        assert_eq!(body_json(&r), json!({"msg": "PutEvents failed"}));
        assert_eq!(bus.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn put_events_failure_then_success_is_202() {
        let bus = FakeBus::new(vec![
            PutOutcome {
                failed_entry_count: 1,
                error_code: None,
                error_message: None,
            },
            PutOutcome::default(),
        ]);
        let r = handle(&signed_event("workflow_job", "{}"), &FakeSecrets, &bus)
            .await
            .unwrap();
        assert_eq!(r["statusCode"], 202);
        assert_eq!(bus.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn missing_github_event_header_forwards_empty_detail_type() {
        let bus = FakeBus::new(vec![PutOutcome::default()]);
        let body = "{}";
        let event = json!({
            "headers": {"x-hub-signature-256": sign(GOLDEN_SECRET, body.as_bytes())},
            "body": body
        });
        let r = handle(&event, &FakeSecrets, &bus).await.unwrap();
        assert_eq!(r["statusCode"], 202);
        assert_eq!(body_json(&r), json!({"msg": "forwarded  to test-bus"}));
        assert_eq!(bus.calls.lock().unwrap()[0].0, "");
    }
}
