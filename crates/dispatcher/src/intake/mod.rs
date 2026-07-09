//! Intake classification: the four shapes a Lambda event can take, decided
//! once, deserialized once. Everything below this boundary is typed.

pub mod fnurl_req;
pub mod webhook;

use serde_json::Value;

use fnurl_req::FnUrlRequest;
use webhook::WebhookPayload;

#[derive(Debug, Clone, thiserror::Error)]
pub enum IntakeError {
    #[error("SQS record has no string body")]
    BadSqsRecord,
    #[error("SQS body is not valid JSON: {0}")]
    BadSqsBody(String),
    #[error("EventBridge detail is malformed: {0}")]
    BadDetail(String),
    #[error("body is not valid base64: {0}")]
    Base64(String),
    #[error("webhook body is malformed: {0}")]
    BadBody(String),
}

pub enum Intake {
    /// `Records[0].eventSource == "aws:sqs"`; bodies are EventBridge
    /// envelopes as JSON strings.
    SqsBatch(Vec<SqsRecord>),
    /// Direct EventBridge invocation (`detail-type == "workflow_job"`).
    EventBridge(Envelope),
    /// Scheduled reconciliation marker: `sweep` present and truthy
    /// (Python-truthy — installed schedules send `true`/`1`; stay tolerant).
    Sweep,
    /// Everything else: the legacy Function-URL webhook.
    FunctionUrl(FnUrlRequest),
}

/// One SQS record, parsed independently of its batch siblings: a malformed
/// record must not block the others (it alone retries via a partial batch
/// response and eventually dead-letters), and a valid sibling must be
/// dispatched exactly once — never re-driven because its neighbor is bad.
pub struct SqsRecord {
    /// SQS `messageId` — the partial-batch-response failure identifier.
    pub message_id: String,
    pub envelope: Result<Envelope, IntakeError>,
}

/// One EventBridge envelope. `detail` may arrive double-encoded (a JSON
/// string) — normalized here.
pub struct Envelope {
    pub payload: WebhookPayload,
}

impl Envelope {
    fn from_event(event: &Value) -> Result<Self, IntakeError> {
        let detail = event.get("detail").unwrap_or(&Value::Null);
        let payload = if !is_truthy(detail) {
            WebhookPayload::default()
        } else if let Value::String(s) = detail {
            serde_json::from_str(s).map_err(|e| IntakeError::BadDetail(e.to_string()))?
        } else {
            serde_json::from_value(detail.clone())
                .map_err(|e| IntakeError::BadDetail(e.to_string()))?
        };
        Ok(Self { payload })
    }
}

impl Intake {
    /// Classification precedence: SQS > EventBridge > Sweep > FunctionUrl.
    pub fn classify(event: Value) -> Result<Intake, IntakeError> {
        if let Some(records) = event.get("Records").and_then(Value::as_array)
            && !records.is_empty()
            && records[0].get("eventSource").and_then(Value::as_str) == Some("aws:sqs")
        {
            let batch = records
                .iter()
                .map(|record| SqsRecord {
                    message_id: record
                        .get("messageId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    envelope: sqs_envelope(record),
                })
                .collect();
            return Ok(Intake::SqsBatch(batch));
        }
        if event.get("detail-type").and_then(Value::as_str) == Some("workflow_job") {
            return Ok(Intake::EventBridge(Envelope::from_event(&event)?));
        }
        if is_truthy(event.get("sweep").unwrap_or(&Value::Null)) {
            return Ok(Intake::Sweep);
        }
        Ok(Intake::FunctionUrl(FnUrlRequest::parse(&event)?))
    }
}

/// One SQS record body → EventBridge envelope. Errors stay per-record.
fn sqs_envelope(record: &Value) -> Result<Envelope, IntakeError> {
    let body = record
        .get("body")
        .and_then(Value::as_str)
        .ok_or(IntakeError::BadSqsRecord)?;
    let eb: Value =
        serde_json::from_str(body).map_err(|e| IntakeError::BadSqsBody(e.to_string()))?;
    Envelope::from_event(&eb)
}

/// Python truthiness over a JSON value — the sweep marker and the
/// `isBase64Encoded` flag arrive from heterogeneous producers.
pub(crate) fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn classify(v: Value) -> Intake {
        Intake::classify(v).unwrap()
    }

    #[test]
    fn routes_sqs_batch() {
        let ev = json!({"Records": [
            {"eventSource": "aws:sqs", "messageId": "m-1", "body": "{}"}
        ]});
        let Intake::SqsBatch(records) = classify(ev) else {
            panic!("expected SqsBatch");
        };
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message_id, "m-1");
        assert!(records[0].envelope.is_ok());
    }

    #[test]
    fn malformed_sqs_record_does_not_poison_its_batch_siblings() {
        // One bad record must stay a per-record error: the valid job in the
        // same batch is parsed (and will be dispatched) regardless.
        let eb = json!({"detail": {"action": "queued", "workflow_job": {"id": 7}}});
        let ev = json!({"Records": [
            {"eventSource": "aws:sqs", "messageId": "good", "body": eb.to_string()},
            {"eventSource": "aws:sqs", "messageId": "bad-json", "body": "{not json"},
            {"eventSource": "aws:sqs", "messageId": "bad-detail",
             "body": json!({"detail": {"action": 42}}).to_string()},
            {"eventSource": "aws:sqs", "messageId": "no-body"},
        ]});
        let Intake::SqsBatch(records) = classify(ev) else {
            panic!("expected SqsBatch");
        };
        assert_eq!(records.len(), 4);
        let good = records[0].envelope.as_ref().unwrap();
        assert_eq!(good.payload.job_id(), Some(7));
        assert!(matches!(
            records[1].envelope,
            Err(IntakeError::BadSqsBody(_))
        ));
        assert!(matches!(
            records[2].envelope,
            Err(IntakeError::BadDetail(_))
        ));
        assert!(matches!(
            records[3].envelope,
            Err(IntakeError::BadSqsRecord)
        ));
    }

    #[test]
    fn routes_non_sqs_records_to_function_url() {
        assert!(matches!(
            classify(json!({"Records": []})),
            Intake::FunctionUrl(_)
        ));
        assert!(matches!(
            classify(json!({"Records": [{"eventSource": "aws:s3"}]})),
            Intake::FunctionUrl(_)
        ));
    }

    #[test]
    fn routes_eventbridge_detail_type() {
        assert!(matches!(
            classify(json!({"detail-type": "workflow_job", "detail": {}})),
            Intake::EventBridge(_)
        ));
        assert!(matches!(
            classify(json!({"detail-type": "other"})),
            Intake::FunctionUrl(_)
        ));
    }

    #[test]
    fn routes_sweep_marker_with_python_truthiness() {
        assert!(matches!(classify(json!({"sweep": true})), Intake::Sweep));
        assert!(matches!(classify(json!({"sweep": 1})), Intake::Sweep));
        assert!(matches!(classify(json!({"sweep": "yes"})), Intake::Sweep));
        assert!(matches!(
            classify(json!({"sweep": false})),
            Intake::FunctionUrl(_)
        ));
        assert!(matches!(
            classify(json!({"sweep": 0})),
            Intake::FunctionUrl(_)
        ));
        assert!(matches!(
            classify(json!({"sweep": ""})),
            Intake::FunctionUrl(_)
        ));
    }

    #[test]
    fn function_url_is_the_fallback() {
        assert!(matches!(
            classify(json!({"headers": {}, "body": ""})),
            Intake::FunctionUrl(_)
        ));
    }

    #[test]
    fn eventbridge_detail_accepts_double_encoding() {
        let inner = json!({"action": "queued", "workflow_job": {"id": 9}});
        let ev = json!({"detail-type": "workflow_job", "detail": inner.to_string()});
        let Intake::EventBridge(env) = classify(ev) else {
            panic!("expected EventBridge");
        };
        assert_eq!(env.payload.job_id(), Some(9));
    }

    #[test]
    fn truthiness_matches_python() {
        assert!(!is_truthy(&json!(null)));
        assert!(!is_truthy(&json!(0)));
        assert!(!is_truthy(&json!("")));
        assert!(!is_truthy(&json!([])));
        assert!(!is_truthy(&json!({})));
        assert!(is_truthy(&json!(true)));
        assert!(is_truthy(&json!(1)));
        assert!(is_truthy(&json!("x")));
    }
}
