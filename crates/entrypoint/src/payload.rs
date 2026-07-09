//! The /run payload, kept as a raw JSON object with Python-dict access
//! semantics.
//!
//! Deliberately NOT deserialized into `types::RunPayload`: that struct
//! requires `github_url`/`token`/`labels`, but the entrypoint's JIT-direct
//! mode legally receives just `{"encoded_jit_config": "..."}` (plus the
//! platform-injected `microvmId`). The Python treats the payload as a plain
//! dict throughout; this mirrors that, so every shape the Python accepted is
//! accepted here (see contract notes).

use crate::util::py_truthy;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default)]
pub struct Payload(pub Map<String, Value>);

impl Payload {
    pub fn from_value(v: Value) -> Self {
        match v {
            Value::Object(m) => Self(m),
            _ => Self::default(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// `bool(payload.get(key, False))`.
    pub fn truthy(&self, key: &str) -> bool {
        self.get(key).map(py_truthy).unwrap_or(false)
    }

    /// `payload.get(key) or None` for string-valued fields: only a
    /// non-empty string is "present" (falsy strings fall through `or`
    /// chains in the Python).
    pub fn str_or_none(&self, key: &str) -> Option<String> {
        match self.get(key) {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        }
    }

    /// `payload.get("microvmId") or payload.get("microvm_id")`.
    pub fn microvm_id(&self) -> Option<String> {
        self.str_or_none("microvmId")
            .or_else(|| self.str_or_none("microvm_id"))
    }

    /// Sorted keys minus `exclude` — for the `/run ... config-keys=` log
    /// line (the token must never appear, even as a key it is filtered).
    pub fn keys_sorted_excluding(&self, exclude: &str) -> Vec<&str> {
        let mut keys: Vec<&str> = self
            .0
            .keys()
            .map(String::as_str)
            .filter(|k| *k != exclude)
            .collect();
        keys.sort_unstable();
        keys
    }
}

/// Port of `unwrap_run_payload`: Lambda delivers
/// `{"microvmId": "...", "runHookPayload": "<opaque string>"}` to /run.
/// Unwrap the JSON config from `runHookPayload`; fall back to the body
/// itself (local testing posts the config directly). `microvmId` from the
/// outer body is injected when the inner payload lacks it.
pub fn unwrap_run_payload(body: &Map<String, Value>) -> Payload {
    let mut payload: Map<String, Value> = match body.get("runHookPayload") {
        Some(Value::String(s)) if !s.trim().is_empty() => serde_json::from_str::<Value>(s)
            .ok()
            .and_then(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .unwrap_or_default(),
        Some(Value::Object(m)) => m.clone(),
        _ => body.clone(),
    };
    if let Some(mvid) = body.get("microvmId")
        && py_truthy(mvid)
        && !payload.contains_key("microvmId")
    {
        payload.insert("microvmId".to_string(), mvid.clone());
    }
    Payload(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn unwraps_string_run_hook_payload_and_injects_microvm_id() {
        let b = body(json!({
            "microvmId": "microvm-abc",
            "runHookPayload": "{\"github_url\": \"https://github.com/o/r\", \"token\": \"t\"}"
        }));
        let p = unwrap_run_payload(&b);
        assert_eq!(
            p.str_or_none("github_url").unwrap(),
            "https://github.com/o/r"
        );
        assert_eq!(p.microvm_id().unwrap(), "microvm-abc");
        // The opaque wrapper key must not leak into the config.
        assert!(p.get("runHookPayload").is_none());
    }

    #[test]
    fn unwraps_dict_run_hook_payload() {
        let b = body(json!({
            "microvmId": "microvm-abc",
            "runHookPayload": {"labels": "a,b", "pool": true}
        }));
        let p = unwrap_run_payload(&b);
        assert_eq!(p.str_or_none("labels").unwrap(), "a,b");
        assert!(p.truthy("pool"));
        assert_eq!(p.microvm_id().unwrap(), "microvm-abc");
    }

    #[test]
    fn falls_back_to_body_when_no_wrapper() {
        let b = body(json!({"encoded_jit_config": "abcd", "microvmId": "m-1"}));
        let p = unwrap_run_payload(&b);
        assert_eq!(p.str_or_none("encoded_jit_config").unwrap(), "abcd");
        assert_eq!(p.microvm_id().unwrap(), "m-1");
    }

    #[test]
    fn empty_or_whitespace_string_wrapper_falls_back_to_body() {
        let b = body(json!({"runHookPayload": "  ", "microvmId": "m-1"}));
        let p = unwrap_run_payload(&b);
        // dict(body) copy keeps the wrapper key, like the Python.
        assert!(p.get("runHookPayload").is_some());
        assert_eq!(p.microvm_id().unwrap(), "m-1");
    }

    #[test]
    fn invalid_json_string_becomes_empty_payload_plus_injected_id() {
        let b = body(json!({"runHookPayload": "{not json", "microvmId": "m-1"}));
        let p = unwrap_run_payload(&b);
        assert_eq!(p.0.len(), 1);
        assert_eq!(p.microvm_id().unwrap(), "m-1");
    }

    #[test]
    fn inner_microvm_id_is_not_overwritten() {
        let b = body(json!({
            "microvmId": "outer",
            "runHookPayload": {"microvmId": "inner"}
        }));
        let p = unwrap_run_payload(&b);
        assert_eq!(p.microvm_id().unwrap(), "inner");
    }

    #[test]
    fn config_keys_exclude_token() {
        let p = Payload::from_value(json!({
            "token": "SECRET", "github_url": "u", "labels": "l"
        }));
        assert_eq!(
            p.keys_sorted_excluding("token"),
            vec!["github_url", "labels"]
        );
    }

    #[test]
    fn falsy_string_fields_fall_through_or_chains() {
        let p = Payload::from_value(json!({"labels": "", "token": null}));
        assert!(p.str_or_none("labels").is_none());
        assert!(p.str_or_none("token").is_none());
        assert!(p.microvm_id().is_none());
    }

    #[test]
    fn handoff_payload_roundtrips_types_run_payload() {
        // A dispatcher-parked mailbox value (types::RunPayload JSON) must be
        // fully readable through this wrapper.
        let rp = types::RunPayload::new("https://github.com/o/r", "tok", "a,b");
        let v = serde_json::to_value(&rp).unwrap();
        let p = Payload::from_value(v);
        assert_eq!(
            p.str_or_none("github_url").unwrap(),
            "https://github.com/o/r"
        );
        assert_eq!(p.str_or_none("token").unwrap(), "tok");
        assert!(p.truthy("ephemeral"));
    }
}
