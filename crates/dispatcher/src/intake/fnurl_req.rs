//! The legacy Function-URL webhook request: lowercased headers, optionally
//! base64-encoded body. Signature verification is the shared [`types::sig`]
//! contract.

use base64::Engine;
use serde_json::Value;
use std::collections::HashMap;

use crate::intake::webhook::WebhookPayload;
use crate::intake::{IntakeError, is_truthy};

pub struct FnUrlRequest {
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl FnUrlRequest {
    pub fn parse(event: &Value) -> Result<Self, IntakeError> {
        let headers = event
            .get("headers")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.to_lowercase(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let raw = event.get("body").and_then(Value::as_str).unwrap_or("");
        let body = if is_truthy(event.get("isBase64Encoded").unwrap_or(&Value::Null)) {
            base64::engine::general_purpose::STANDARD
                .decode(raw)
                .map_err(|e| IntakeError::Base64(e.to_string()))?
        } else {
            raw.as_bytes().to_vec()
        };
        Ok(Self { headers, body })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn github_event(&self) -> &str {
        self.header("x-github-event").unwrap_or("")
    }

    pub fn signature(&self) -> Option<&str> {
        self.header("x-hub-signature-256")
    }

    /// Parse the webhook body; an empty body is an empty payload.
    pub fn json_payload(&self) -> Result<WebhookPayload, IntakeError> {
        if self.body.is_empty() {
            return Ok(WebhookPayload::default());
        }
        serde_json::from_slice(&self.body).map_err(|e| IntakeError::BadBody(e.to_string()))
    }
}
