//! GitHub webhook -> EventBridge proxy.
//!
//! Deliberately does almost NOTHING: verify the HMAC signature and PutEvents
//! the payload onto the bus. All real work (GitHub API, RunMicrovm, pool
//! management) lives in the dispatcher behind EventBridge rules — which retry
//! with backoff and dead-letter on exhaustion. GitHub delivers webhooks
//! exactly once with no retries, so the less this endpoint does, the less can
//! be lost; a PutEvents call has a tiny failure surface compared to the old
//! inline dispatch.
//!
//! Events: Source=github.webhook, DetailType=<x-github-event>, Detail=payload.
//! Oversized payloads (EventBridge caps entries at 256KB) are slimmed to the
//! fields the dispatcher consumes rather than dropped.
//!
//! This is a faithful port of `webhook-proxy/handler.py` — env var names,
//! response bodies and operational log lines must stay compatible with the
//! Python original so a mixed fleet interoperates mid-migration.

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::HashMap;

/// Keep comfortably under the 256KB PutEvents entry cap (headroom for the
/// envelope EventBridge adds). Mirrors Python's `MAX_DETAIL_BYTES = 240_000`.
const MAX_DETAIL_BYTES: usize = 240_000;

// ---------------------------------------------------------------------------
// Pure logic (unit-tested)
// ---------------------------------------------------------------------------

/// Python truthiness for JSON values — the Python handler uses `or` /
/// `if event.get(...)` patterns, so `0`, `""`, `[]`, `{}`, `null`, `false`
/// are all falsy.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Verify `X-Hub-Signature-256: sha256=<hex>` against the shared secret.
/// Constant-time via `Mac::verify_slice`.
fn verify(secret: &str, body: &[u8], sig: Option<&str>) -> bool {
    let Some(sig) = sig else { return false };
    let Some(hex_part) = sig.strip_prefix("sha256=") else {
        return false;
    };
    // Python compares against the lowercase hexdigest with compare_digest, so
    // an uppercase-hex signature must NOT verify.
    if hex_part.bytes().any(|b| b.is_ascii_uppercase()) {
        return false;
    }
    let Ok(sig_bytes) = hex::decode(hex_part) else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(&sig_bytes).is_ok()
}

/// The subset the dispatcher consumes — used only when the full payload would
/// exceed the PutEvents entry cap. Field selection mirrors Python `_slim()`
/// exactly (missing fields serialize as `null`, like Python's `None`).
fn slim(payload: &Value) -> Value {
    let get = |v: &Value, k: &str| v.get(k).cloned().unwrap_or(Value::Null);
    // Python: `payload.get("workflow_job") or {}` — falsy values become {}.
    let empty = json!({});
    let sub = |k: &str| match payload.get(k) {
        Some(v) if is_truthy(v) => v.clone(),
        _ => empty.clone(),
    };
    let job = sub("workflow_job");
    json!({
        "action": get(payload, "action"),
        "workflow_job": {
            "id": get(&job, "id"),
            "run_id": get(&job, "run_id"),
            "labels": get(&job, "labels"),
            "runner_name": get(&job, "runner_name"),
            "status": get(&job, "status"),
        },
        "repository": {"full_name": get(&sub("repository"), "full_name")},
        "installation": {"id": get(&sub("installation"), "id")},
        "_slimmed": true,
    })
}

/// Byte-identical to Python's `json.dumps({"msg": msg})` (default separators
/// put a space after the colon).
fn body_json(msg: &str) -> String {
    format!("{{\"msg\": {}}}", Value::String(msg.to_string()))
}

/// Log the operational line and build the Function-URL response — mirrors
/// Python `_resp()`.
fn resp(code: u16, msg: &str) -> Value {
    println!("{}", json!({"status": code, "msg": msg}));
    json!({
        "statusCode": code,
        "headers": {"content-type": "application/json"},
        "body": body_json(msg),
    })
}

/// Lowercase the header names, mirroring
/// `{k.lower(): v for k, v in (event.get("headers") or {}).items()}`.
fn lower_headers(event: &Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(obj) = event.get("headers").and_then(Value::as_object) {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                out.insert(k.to_ascii_lowercase(), s.to_string());
            }
        }
    }
    out
}

/// Decode the Function-URL body, honouring `isBase64Encoded`.
/// A bad base64 body is a hard error (Python raises → Lambda error).
fn decode_body(event: &Value) -> Result<Vec<u8>, Error> {
    let raw = event.get("body").and_then(Value::as_str).unwrap_or("");
    if event.get("isBase64Encoded").is_some_and(is_truthy) {
        Ok(base64::engine::general_purpose::STANDARD.decode(raw)?)
    } else {
        Ok(raw.as_bytes().to_vec())
    }
}

// ---------------------------------------------------------------------------
// AWS seam
// ---------------------------------------------------------------------------

/// Result of one PutEvents call, as the handler consumes it.
/// `error_code`/`error_message` come from the first response entry — the
/// request only ever carries one.
#[derive(Debug, Clone, Default)]
struct PutOutcome {
    failed_entry_count: i64,
    error_code: Option<String>,
    error_message: Option<String>,
}

/// Seam between the pure handler logic and AWS so tests can hand-roll fakes.
/// `Err` from either method models a raised boto3 exception: it propagates
/// out of the handler as a Lambda invocation error (Function URL 502), same
/// as the Python original.
trait Deps {
    async fn webhook_secret(&self) -> Result<String, Error>;
    async fn put_events(&self, detail_type: &str, detail: &str) -> Result<PutOutcome, Error>;
    fn bus_name(&self) -> &str;
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

async fn handle(event: &Value, deps: &impl Deps) -> Result<Value, Error> {
    let headers = lower_headers(event);
    let body = decode_body(event)?;

    let secret = deps.webhook_secret().await?;
    if !verify(
        &secret,
        &body,
        headers.get("x-hub-signature-256").map(String::as_str),
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

    let mut detail = String::from_utf8_lossy(&body).into_owned();
    if detail.len() > MAX_DETAIL_BYTES {
        // The cap is BYTES; String::len is a byte count. A payload that is
        // oversized but not valid JSON raises in Python — surface the same
        // hard error here.
        let payload: Value = serde_json::from_str(&detail)?;
        detail = serde_json::to_string(&slim(&payload))?;
    }

    // put_events does NOT raise on per-entry failures — check the response,
    // retry once, then 500 so the failure is visible in GitHub's delivery UI.
    for attempt in 0..2 {
        let out = deps.put_events(gh_event, &detail).await?;
        if out.failed_entry_count == 0 {
            return Ok(resp(
                202,
                &format!("forwarded {gh_event} to {}", deps.bus_name()),
            ));
        }
        println!(
            "{}",
            json!({
                "put_events_failed": true,
                "attempt": attempt,
                "code": out.error_code,
                "msg": out.error_message,
            })
        );
    }
    Ok(resp(500, "PutEvents failed"))
}

// ---------------------------------------------------------------------------
// Real AWS wiring
// ---------------------------------------------------------------------------

struct RealDeps {
    ssm: aws_sdk_ssm::Client,
    events: aws_sdk_eventbridge::Client,
    bus_name: String,
    param_name: String,
    /// Cached across invocations, like the Python module-level `_secret`.
    secret: tokio::sync::OnceCell<String>,
}

impl Deps for RealDeps {
    async fn webhook_secret(&self) -> Result<String, Error> {
        let secret = self
            .secret
            .get_or_try_init(|| async {
                let out = self
                    .ssm
                    .get_parameter()
                    .name(&self.param_name)
                    .with_decryption(true)
                    .send()
                    .await?;
                let raw = out
                    .parameter()
                    .and_then(|p| p.value())
                    .ok_or("SSM parameter has no value")?;
                let parsed: Value = serde_json::from_str(raw)?;
                let value = parsed
                    .get("webhook_secret")
                    .and_then(Value::as_str)
                    .ok_or("webhook_secret missing from SSM param")?
                    .to_string();
                Ok::<String, Error>(value)
            })
            .await?;
        Ok(secret.clone())
    }

    async fn put_events(&self, detail_type: &str, detail: &str) -> Result<PutOutcome, Error> {
        let entry = aws_sdk_eventbridge::types::PutEventsRequestEntry::builder()
            .source("github.webhook")
            .detail_type(detail_type)
            .detail(detail)
            .event_bus_name(&self.bus_name)
            .build();
        let out = self.events.put_events().entries(entry).send().await?;
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
async fn main() -> Result<(), Error> {
    // Diagnostics only, on stderr — operational log lines are the println!
    // JSON above and must keep their Python-compatible shape on stdout.
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    // Same env contract as the Python module top-level: AWS_REGION defaults
    // to us-east-1; the other two are hard-required (Python raises KeyError
    // at import time).
    let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string());
    let bus_name = std::env::var("EVENT_BUS_NAME").expect("EVENT_BUS_NAME env var is required");
    let param_name = std::env::var("PARAM_NAME").expect("PARAM_NAME env var is required");

    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region))
        .load()
        .await;

    let deps = RealDeps {
        ssm: aws_sdk_ssm::Client::new(&config),
        events: aws_sdk_eventbridge::Client::new(&config),
        bus_name,
        param_name,
        secret: tokio::sync::OnceCell::new(),
    };
    let deps = &deps;

    lambda_runtime::run(service_fn(move |event: LambdaEvent<Value>| async move {
        handle(&event.payload, deps).await
    }))
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// GitHub's documented example webhook secret / payload / signature.
    const GOLDEN_SECRET: &str = "It's a Secret to Everybody";
    const GOLDEN_BODY: &[u8] = b"Hello, World!";
    const GOLDEN_SIG: &str =
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    // -- signature verification ---------------------------------------------

    #[test]
    fn verify_golden_vector() {
        assert!(verify(GOLDEN_SECRET, GOLDEN_BODY, Some(GOLDEN_SIG)));
    }

    #[test]
    fn verify_rejects_bad_inputs() {
        // wrong body
        assert!(!verify(GOLDEN_SECRET, b"Hello, World?", Some(GOLDEN_SIG)));
        // wrong secret
        assert!(!verify("nope", GOLDEN_BODY, Some(GOLDEN_SIG)));
        // missing / empty / unprefixed — Python: `if not sig or not sig.startswith("sha256=")`
        assert!(!verify(GOLDEN_SECRET, GOLDEN_BODY, None));
        assert!(!verify(GOLDEN_SECRET, GOLDEN_BODY, Some("")));
        assert!(!verify(
            GOLDEN_SECRET,
            GOLDEN_BODY,
            Some("sha1=757107ea0eb2509fc211221cce984b8a37570b6d")
        ));
        // truncated / non-hex digests
        assert!(!verify(GOLDEN_SECRET, GOLDEN_BODY, Some("sha256=757107")));
        assert!(!verify(GOLDEN_SECRET, GOLDEN_BODY, Some("sha256=zz")));
        // uppercase hex must fail, exactly like Python's string compare_digest
        let upper = format!("sha256={}", GOLDEN_SIG[7..].to_ascii_uppercase());
        assert!(!verify(GOLDEN_SECRET, GOLDEN_BODY, Some(&upper)));
    }

    // -- slim ----------------------------------------------------------------

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
        let s = slim(&payload);
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
        let s = slim(&json!({}));
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
        // Python: `payload.get("workflow_job") or {}` — null is falsy
        let s = slim(&json!({"workflow_job": null, "repository": null, "installation": null}));
        assert_eq!(s["workflow_job"]["id"], Value::Null);
        assert_eq!(s["repository"]["full_name"], Value::Null);
        assert_eq!(s["installation"]["id"], Value::Null);
    }

    // -- response shape -------------------------------------------------------

    #[test]
    fn resp_matches_python_shape() {
        let r = resp(200, "pong");
        assert_eq!(r["statusCode"], 200);
        assert_eq!(r["headers"], json!({"content-type": "application/json"}));
        // byte-identical to Python json.dumps({"msg": "pong"})
        assert_eq!(r["body"], "{\"msg\": \"pong\"}");
    }

    // -- event plumbing --------------------------------------------------------

    #[test]
    fn headers_are_case_insensitive_and_body_base64_decodes() {
        let event = json!({
            "headers": {"X-GitHub-Event": "ping", "X-Hub-Signature-256": "sha256=aa"},
            "body": base64::engine::general_purpose::STANDARD.encode(b"hi"),
            "isBase64Encoded": true
        });
        let headers = lower_headers(&event);
        assert_eq!(
            headers.get("x-github-event").map(String::as_str),
            Some("ping")
        );
        assert_eq!(
            headers.get("x-hub-signature-256").map(String::as_str),
            Some("sha256=aa")
        );
        assert_eq!(decode_body(&event).unwrap(), b"hi");
        // plain body, and Python's `or ""` fallbacks
        assert_eq!(decode_body(&json!({"body": "raw"})).unwrap(), b"raw");
        assert_eq!(decode_body(&json!({"body": null})).unwrap(), b"");
        assert_eq!(decode_body(&json!({})).unwrap(), b"");
        assert!(lower_headers(&json!({"headers": null})).is_empty());
    }

    // -- full handler with a hand-rolled fake AWS -------------------------------

    struct FakeDeps {
        secret: String,
        bus: String,
        /// One outcome per expected put_events call, in order.
        outcomes: Mutex<Vec<PutOutcome>>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl FakeDeps {
        fn new(outcomes: Vec<PutOutcome>) -> Self {
            Self {
                secret: GOLDEN_SECRET.to_string(),
                bus: "test-bus".to_string(),
                outcomes: Mutex::new(outcomes),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Deps for FakeDeps {
        async fn webhook_secret(&self) -> Result<String, Error> {
            Ok(self.secret.clone())
        }

        async fn put_events(&self, detail_type: &str, detail: &str) -> Result<PutOutcome, Error> {
            self.calls
                .lock()
                .unwrap()
                .push((detail_type.to_string(), detail.to_string()));
            Ok(self.outcomes.lock().unwrap().remove(0))
        }

        fn bus_name(&self) -> &str {
            &self.bus
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
        let deps = FakeDeps::new(vec![]);
        let mut event = signed_event("workflow_job", "{}");
        event["headers"]["X-Hub-Signature-256"] = json!("sha256=00");
        let r = handle(&event, &deps).await.unwrap();
        assert_eq!(r["statusCode"], 401);
        assert_eq!(r["body"], "{\"msg\": \"invalid signature\"}");
        assert!(deps.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ping_short_circuits_before_put_events() {
        let deps = FakeDeps::new(vec![]);
        let r = handle(&signed_event("ping", "{\"zen\": \"anything\"}"), &deps)
            .await
            .unwrap();
        assert_eq!(r["statusCode"], 200);
        assert_eq!(r["body"], "{\"msg\": \"pong\"}");
        assert!(deps.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn forwards_small_payload_verbatim() {
        let deps = FakeDeps::new(vec![PutOutcome::default()]);
        let body = "{\"action\": \"queued\"}";
        let r = handle(&signed_event("workflow_job", body), &deps)
            .await
            .unwrap();
        assert_eq!(r["statusCode"], 202);
        assert_eq!(
            r["body"],
            "{\"msg\": \"forwarded workflow_job to test-bus\"}"
        );
        let calls = deps.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "workflow_job");
        assert_eq!(calls[0].1, body); // NOT slimmed, byte-for-byte passthrough
    }

    #[tokio::test]
    async fn oversize_payload_falls_back_to_slim() {
        let deps = FakeDeps::new(vec![PutOutcome::default()]);
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
        let r = handle(&signed_event("workflow_job", &body), &deps)
            .await
            .unwrap();
        assert_eq!(r["statusCode"], 202);
        let calls = deps.calls.lock().unwrap();
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
    async fn base64_encoded_body_verifies_and_forwards() {
        let deps = FakeDeps::new(vec![PutOutcome::default()]);
        let body = b"{\"action\": \"queued\"}";
        let event = json!({
            "headers": {
                "x-github-event": "workflow_job",
                "x-hub-signature-256": sign(GOLDEN_SECRET, body)
            },
            "body": base64::engine::general_purpose::STANDARD.encode(body),
            "isBase64Encoded": true
        });
        let r = handle(&event, &deps).await.unwrap();
        assert_eq!(r["statusCode"], 202);
        assert_eq!(
            deps.calls.lock().unwrap()[0].1,
            String::from_utf8_lossy(body)
        );
    }

    #[tokio::test]
    async fn put_events_failure_retries_once_then_500() {
        let failed = PutOutcome {
            failed_entry_count: 1,
            error_code: Some("ThrottlingException".to_string()),
            error_message: Some("slow down".to_string()),
        };
        let deps = FakeDeps::new(vec![failed.clone(), failed]);
        let r = handle(&signed_event("workflow_job", "{}"), &deps)
            .await
            .unwrap();
        assert_eq!(r["statusCode"], 500);
        assert_eq!(r["body"], "{\"msg\": \"PutEvents failed\"}");
        assert_eq!(deps.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn put_events_failure_then_success_is_202() {
        let deps = FakeDeps::new(vec![
            PutOutcome {
                failed_entry_count: 1,
                error_code: None,
                error_message: None,
            },
            PutOutcome::default(),
        ]);
        let r = handle(&signed_event("workflow_job", "{}"), &deps)
            .await
            .unwrap();
        assert_eq!(r["statusCode"], 202);
        assert_eq!(deps.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn missing_github_event_header_forwards_empty_detail_type() {
        let deps = FakeDeps::new(vec![PutOutcome::default()]);
        let body = "{}";
        let event = json!({
            "headers": {"x-hub-signature-256": sign(GOLDEN_SECRET, body.as_bytes())},
            "body": body
        });
        let r = handle(&event, &deps).await.unwrap();
        assert_eq!(r["statusCode"], 202);
        assert_eq!(r["body"], "{\"msg\": \"forwarded  to test-bus\"}");
        assert_eq!(deps.calls.lock().unwrap()[0].0, "");
    }
}
