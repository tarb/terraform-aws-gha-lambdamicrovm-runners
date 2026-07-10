//! /run payload handling: unwrap the platform envelope, then coerce the
//! loose JSON into a typed [`RunConfig`].
//!
//! The payload is deliberately NOT deserialized into `types::RunPayload`:
//! JIT-direct mode legally receives just `{"encoded_jit_config": "..."}`
//! (plus the platform-injected `microvmId`), and the handoff mailbox may
//! carry payloads written by heterogeneous producers. The [`lenient`]
//! helpers are the one sanctioned home for loose-JSON coercion.

use crate::config::{DEFAULT_LABELS, env_nonempty, env_or};
use crate::logfmt::log;
use secrecy::SecretString;
use serde_json::{Map, Value};
use types::{MicrovmId, RunnerName};

/// Loose-JSON coercions for fields the mailbox contract leaves untyped.
pub mod lenient {
    use serde_json::Value;

    /// Loose truthiness: null / false / 0 / "" / [] / {} are false,
    /// everything else (including "false") is true.
    pub fn truthy(v: &Value) -> bool {
        match v {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
        }
    }

    /// Loose integer coercion: bools are 0/1, floats truncate, strings are
    /// trimmed and parsed; anything else is an error.
    pub fn lenient_i64(v: &Value) -> Result<i64, String> {
        match v {
            Value::Bool(b) => Ok(i64::from(*b)),
            Value::Number(n) => n
                .as_i64()
                .or_else(|| n.as_f64().map(|f| f as i64))
                .ok_or_else(|| format!("cannot convert {n} to an integer")),
            Value::String(s) => s
                .trim()
                .parse::<i64>()
                .map_err(|_| format!("invalid integer literal {s:?}")),
            other => Err(format!("expected a number or string, got {other}")),
        }
    }

    /// A string-valued field is "present" only when non-empty; every other
    /// shape (null, numbers, empty string) counts as absent so fallback
    /// chains keep working.
    pub fn nonempty_str(v: &Value) -> Option<&str> {
        match v {
            Value::String(s) if !s.is_empty() => Some(s),
            _ => None,
        }
    }
}

/// Unwrap the `/run` body. The platform delivers
/// `{"microvmId": "...", "runHookPayload": "<opaque JSON string>"}`; the
/// inner config is parsed out of `runHookPayload` (string or object), with
/// the body itself as the fallback (local testing posts the config
/// directly — in that case the wrapper key, if present but blank, is
/// retained in the copy). The outer `microvmId` is injected only when the
/// inner payload lacks one. An unparseable wrapper string yields an empty
/// config (plus the injected id).
pub fn unwrap_run_payload(body: &Map<String, Value>) -> Map<String, Value> {
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
        && lenient::truthy(mvid)
        && !payload.contains_key("microvmId")
    {
        payload.insert("microvmId".to_string(), mvid.clone());
    }
    payload
}

/// The typed view of one run request, with env fallbacks resolved
/// (payload wins; empty strings count as absent).
#[derive(Debug)]
pub struct RunConfig {
    /// Sorted payload keys with `token` excluded — for the `/run` log line
    /// (the token must never appear, even as a key name).
    pub raw_keys: Vec<String>,
    /// `microvmId` (or `microvm_id`); non-empty strings only.
    pub microvm_id: Option<MicrovmId>,
    /// `encoded_jit_config`, else `ENCODED_JIT_CONFIG`.
    pub jit: Option<String>,
    /// `github_url`, else `GH_URL`.
    pub github_url: Option<String>,
    /// `token` (legacy alias `pat`), else `GH_PAT`.
    pub token: Option<SecretString>,
    pub ephemeral: bool,
    /// `labels`, else `RUNNER_LABELS`, else the built-in default.
    pub labels: String,
    /// Warm-pool member: after the job, clean up and await suspend instead
    /// of self-terminating.
    pub pool: bool,
    /// Present only when the payload carried a usable value; see
    /// [`Self::pool_grace_or_env`].
    pub pool_grace: Option<i64>,
    /// Mailbox path prefix, trailing slashes stripped, non-empty.
    pub handoff_prefix: Option<String>,
    /// Passed through to JIT minting / config.sh; shape intentionally loose.
    pub runner_group: Option<Value>,
    /// The dispatcher Lambda's function name, for idle reports. Absent when
    /// an old dispatcher built the payload — reporting is then skipped.
    pub dispatcher_fn: Option<String>,
    /// Per-run docker capability, decided by the dispatcher ("docker" label
    /// or `DOCKER_DEFAULT`). `None` when an old dispatcher built the payload
    /// — resolution then falls back to the `ENABLE_DOCKER` env; see
    /// [`Self::docker_enabled`].
    pub enable_docker: Option<bool>,
}

impl RunConfig {
    /// From a `/run` hook body (unwraps the platform envelope first).
    pub fn from_hook_body(body: &Map<String, Value>) -> Self {
        Self::from_map(unwrap_run_payload(body))
    }

    /// From a claimed mailbox payload (already the bare config object).
    pub fn from_value(v: Value) -> Self {
        match v {
            Value::Object(m) => Self::from_map(m),
            _ => Self::from_map(Map::new()),
        }
    }

    fn from_map(map: Map<String, Value>) -> Self {
        let mut raw_keys: Vec<String> = map.keys().filter(|k| *k != "token").cloned().collect();
        raw_keys.sort_unstable();
        let pool_grace = match map.get("pool_grace") {
            Some(v) if lenient::truthy(v) => match lenient::lenient_i64(v) {
                Ok(n) => Some(n),
                Err(e) => {
                    log(format!(
                        "pool_grace unusable ({e}); falling back to POOL_SUSPEND_GRACE_SECONDS"
                    ));
                    None
                }
            },
            _ => None,
        };
        Self {
            raw_keys,
            microvm_id: field_str(&map, "microvmId")
                .or_else(|| field_str(&map, "microvm_id"))
                .map(MicrovmId::new),
            jit: field_str(&map, "encoded_jit_config")
                .or_else(|| env_nonempty("ENCODED_JIT_CONFIG")),
            github_url: field_str(&map, "github_url").or_else(|| env_nonempty("GH_URL")),
            token: field_str(&map, "token")
                .or_else(|| field_str(&map, "pat"))
                .or_else(|| env_nonempty("GH_PAT"))
                .map(SecretString::from),
            ephemeral: map.get("ephemeral").is_some_and(lenient::truthy),
            labels: field_str(&map, "labels")
                .or_else(|| env_nonempty("RUNNER_LABELS"))
                .unwrap_or_else(|| DEFAULT_LABELS.to_string()),
            pool: map.get("pool").is_some_and(lenient::truthy),
            pool_grace,
            handoff_prefix: field_str(&map, "handoff_prefix")
                .map(|s| s.trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty()),
            runner_group: map.get("runner_group").cloned(),
            dispatcher_fn: field_str(&map, "dispatcher_fn"),
            // JSON null counts as absent (falls back to ENABLE_DOCKER);
            // any present non-null value is coerced loosely.
            enable_docker: map
                .get("enable_docker")
                .filter(|v| !v.is_null())
                .map(lenient::truthy),
        }
    }

    /// GitHub runner name for this VM: [`RunnerName::for_vm`] — THE
    /// fleet-wide derivation, which the dispatcher also runs to map runners
    /// back to VMs — when a usable microVM id is present, otherwise
    /// [`RunnerName::random`]. "Usable" means the derived name keeps a
    /// non-empty id fragment after the prefix (a bare `"microvm-"` id would
    /// collide on the prefix alone).
    pub fn runner_name(&self) -> String {
        self.microvm_id
            .as_ref()
            .map(RunnerName::for_vm)
            .filter(|name| name.as_str().len() > RunnerName::PREFIX.len())
            .unwrap_or_else(RunnerName::random)
            .to_string()
    }

    /// Does THIS run get dockerd + the wait-for-docker job-started hook?
    /// The payload's `enable_docker` when present (a new dispatcher decided
    /// per job); otherwise the `ENABLE_DOCKER` env semantics carried by
    /// `env_cfg` — old-dispatcher payloads behave exactly as before.
    pub fn docker_enabled(&self, env_cfg: &crate::config::Config) -> bool {
        self.enable_docker.unwrap_or(env_cfg.enable_docker)
    }

    /// Grace window for the pool idle wait: the payload value when usable,
    /// else `POOL_SUSPEND_GRACE_SECONDS` (default 300).
    pub fn pool_grace_or_env(&self) -> i64 {
        self.pool_grace.unwrap_or_else(|| {
            env_or("POOL_SUSPEND_GRACE_SECONDS", "300")
                .trim()
                .parse()
                .unwrap_or(300)
        })
    }
}

fn field_str(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(lenient::nonempty_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
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
        let cfg = RunConfig::from_hook_body(&b);
        assert_eq!(cfg.github_url.as_deref(), Some("https://github.com/o/r"));
        assert_eq!(cfg.microvm_id, Some(MicrovmId::new("microvm-abc")));
        // The opaque wrapper key must not leak into the config.
        assert!(!cfg.raw_keys.contains(&"runHookPayload".to_string()));
    }

    #[test]
    fn unwraps_object_run_hook_payload() {
        let b = body(json!({
            "microvmId": "microvm-abc",
            "runHookPayload": {"labels": "a,b", "pool": true}
        }));
        let cfg = RunConfig::from_hook_body(&b);
        assert_eq!(cfg.labels, "a,b");
        assert!(cfg.pool);
        assert_eq!(cfg.microvm_id, Some(MicrovmId::new("microvm-abc")));
    }

    #[test]
    fn falls_back_to_body_when_no_wrapper() {
        let b = body(json!({"encoded_jit_config": "abcd", "microvmId": "m-1"}));
        let cfg = RunConfig::from_hook_body(&b);
        assert_eq!(cfg.jit.as_deref(), Some("abcd"));
        assert_eq!(cfg.microvm_id, Some(MicrovmId::new("m-1")));
    }

    #[test]
    fn empty_or_whitespace_string_wrapper_falls_back_to_body() {
        let b = body(json!({"runHookPayload": "  ", "microvmId": "m-1"}));
        let unwrapped = unwrap_run_payload(&b);
        // The body copy keeps the (blank) wrapper key.
        assert!(unwrapped.contains_key("runHookPayload"));
        let cfg = RunConfig::from_map(unwrapped);
        assert_eq!(cfg.microvm_id, Some(MicrovmId::new("m-1")));
        assert!(cfg.raw_keys.contains(&"runHookPayload".to_string()));
    }

    #[test]
    fn invalid_json_string_becomes_empty_payload_plus_injected_id() {
        let b = body(json!({"runHookPayload": "{not json", "microvmId": "m-1"}));
        let unwrapped = unwrap_run_payload(&b);
        assert_eq!(unwrapped.len(), 1);
        assert_eq!(
            RunConfig::from_map(unwrapped).microvm_id,
            Some(MicrovmId::new("m-1"))
        );
    }

    #[test]
    fn inner_microvm_id_is_not_overwritten() {
        let b = body(json!({
            "microvmId": "outer",
            "runHookPayload": {"microvmId": "inner"}
        }));
        let cfg = RunConfig::from_hook_body(&b);
        assert_eq!(cfg.microvm_id, Some(MicrovmId::new("inner")));
    }

    #[test]
    fn config_keys_exclude_token() {
        let cfg = RunConfig::from_value(json!({
            "token": "SECRET", "github_url": "u", "labels": "l"
        }));
        assert_eq!(cfg.raw_keys, vec!["github_url", "labels"]);
    }

    #[test]
    fn falsy_string_fields_fall_through_fallback_chains() {
        let cfg = RunConfig::from_value(json!({"labels": "", "token": null}));
        assert_eq!(cfg.labels, DEFAULT_LABELS);
        assert!(cfg.token.is_none());
        assert!(cfg.microvm_id.is_none());
    }

    #[test]
    fn handoff_payload_roundtrips_types_run_payload() {
        // A dispatcher-parked mailbox value (types::RunPayload JSON) must be
        // fully readable through this config.
        let rp = types::RunPayload::new("https://github.com/o/r", "tok", "a,b");
        let cfg = RunConfig::from_value(serde_json::to_value(&rp).unwrap());
        assert_eq!(cfg.github_url.as_deref(), Some("https://github.com/o/r"));
        assert_eq!(cfg.token.unwrap().expose_secret(), "tok");
        assert!(cfg.ephemeral);
    }

    #[test]
    fn legacy_pat_alias_is_accepted_for_the_token() {
        let cfg = RunConfig::from_value(json!({"pat": "legacy"}));
        assert_eq!(cfg.token.unwrap().expose_secret(), "legacy");
    }

    #[test]
    fn dispatcher_fn_is_read_and_absent_or_empty_stays_none() {
        let cfg = RunConfig::from_value(json!({"dispatcher_fn": "gha-microvm-dispatcher"}));
        assert_eq!(cfg.dispatcher_fn.as_deref(), Some("gha-microvm-dispatcher"));
        // Old-dispatcher payloads (no field) and blank values mean "cannot
        // report" — reporting is skipped and behavior matches v0.0.2.
        assert!(RunConfig::from_value(json!({})).dispatcher_fn.is_none());
        assert!(
            RunConfig::from_value(json!({"dispatcher_fn": ""}))
                .dispatcher_fn
                .is_none()
        );
    }

    #[test]
    fn enable_docker_reads_the_payload_and_null_or_absent_stays_none() {
        assert_eq!(
            RunConfig::from_value(json!({"enable_docker": true})).enable_docker,
            Some(true)
        );
        assert_eq!(
            RunConfig::from_value(json!({"enable_docker": false})).enable_docker,
            Some(false)
        );
        // Old-dispatcher payloads: absent (or null) means "no decision".
        assert!(RunConfig::from_value(json!({})).enable_docker.is_none());
        assert!(
            RunConfig::from_value(json!({"enable_docker": null}))
                .enable_docker
                .is_none()
        );
    }

    #[test]
    fn docker_enabled_prefers_the_payload_and_falls_back_to_the_env_knob() {
        use crate::state::testsupport::test_config;
        let mut env = test_config("/opt/actions-runner");
        // The payload decision wins over the env in BOTH polarities.
        env.enable_docker = false;
        assert!(RunConfig::from_value(json!({"enable_docker": true})).docker_enabled(&env));
        env.enable_docker = true;
        assert!(!RunConfig::from_value(json!({"enable_docker": false})).docker_enabled(&env));
        // Absent field: exactly the ENABLE_DOCKER env semantics.
        assert!(RunConfig::from_value(json!({})).docker_enabled(&env));
        env.enable_docker = false;
        assert!(!RunConfig::from_value(json!({})).docker_enabled(&env));
    }

    #[test]
    fn handoff_prefix_strips_trailing_slashes_and_drops_empties() {
        let cfg = RunConfig::from_value(json!({"handoff_prefix": "/p/handoff/"}));
        assert_eq!(cfg.handoff_prefix.as_deref(), Some("/p/handoff"));
        let cfg = RunConfig::from_value(json!({"handoff_prefix": "///"}));
        assert!(cfg.handoff_prefix.is_none());
    }

    #[test]
    fn pool_grace_prefers_a_usable_payload_value() {
        let cfg = RunConfig::from_value(json!({"pool_grace": 42}));
        assert_eq!(cfg.pool_grace_or_env(), 42);
        // String values parse leniently.
        let cfg = RunConfig::from_value(json!({"pool_grace": " 41 "}));
        assert_eq!(cfg.pool_grace_or_env(), 41);
        // Falsy (0) and unusable values fall back to the env default.
        let cfg = RunConfig::from_value(json!({"pool_grace": 0}));
        assert_eq!(cfg.pool_grace_or_env(), 300);
        let cfg = RunConfig::from_value(json!({"pool_grace": "abc"}));
        assert_eq!(cfg.pool_grace_or_env(), 300);
    }

    #[test]
    fn runner_name_uses_the_fleet_wide_derivation_when_id_present() {
        let id = "microvm-aaaa1111-2222-3333-4444-555566667777";
        let cfg = RunConfig::from_value(json!({"microvmId": id}));
        assert_eq!(
            cfg.runner_name(),
            RunnerName::for_vm(&MicrovmId::new(id)).as_str()
        );
        assert_eq!(cfg.runner_name(), "gha-mvm-aaaa1111-2222-3333");
        let cfg = RunConfig::from_value(json!({"microvmId": "deadbeef"}));
        assert_eq!(cfg.runner_name(), "gha-mvm-deadbeef");
        // Degenerate repeated-prefix ids follow for_vm too (strip the leading
        // prefix once) — the one case the old `.replace` gate got wrong.
        let cfg = RunConfig::from_value(json!({"microvmId": "microvm-microvm-x"}));
        assert_eq!(cfg.runner_name(), "gha-mvm-microvm-x");
    }

    #[test]
    fn runner_name_falls_back_to_a_random_hex_suffix() {
        for v in [
            json!({}),
            json!({"microvmId": ""}),
            json!({"microvmId": "microvm-"}),
        ] {
            let name = RunConfig::from_value(v).runner_name();
            let suffix = name.strip_prefix("gha-mvm-").expect("prefix");
            assert_eq!(suffix.len(), 8, "expected 8 hex chars: {name}");
            assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn truthiness_is_loose() {
        assert!(!lenient::truthy(&json!(null)));
        assert!(!lenient::truthy(&json!(false)));
        assert!(!lenient::truthy(&json!(0)));
        assert!(!lenient::truthy(&json!("")));
        assert!(!lenient::truthy(&json!([])));
        assert!(!lenient::truthy(&json!({})));
        assert!(lenient::truthy(&json!("false"))); // non-empty string is truthy
        assert!(lenient::truthy(&json!(1)));
        assert!(lenient::truthy(&json!({"a": 1})));
    }

    #[test]
    fn lenient_i64_coercions() {
        assert_eq!(lenient::lenient_i64(&json!("300")).unwrap(), 300);
        assert_eq!(lenient::lenient_i64(&json!(" 42 ")).unwrap(), 42);
        assert_eq!(lenient::lenient_i64(&json!(300.9)).unwrap(), 300);
        assert_eq!(lenient::lenient_i64(&json!(true)).unwrap(), 1);
        assert!(lenient::lenient_i64(&json!("abc")).is_err());
        assert!(lenient::lenient_i64(&json!([1])).is_err());
    }
}
