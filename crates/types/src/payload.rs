//! The run payload — the wire contract between dispatcher and entrypoint.

use crate::id::MicrovmId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Delivered to the VM either via `RunMicrovm.runHookPayload` (cold launch,
/// `microvmId` injected by the platform) or parked in the SSM handoff mailbox
/// (pool resume, `microvmId` set by the dispatcher).
///
/// Field names and aliases ARE the wire contract; the service caps
/// `runHookPayload` at 4096 bytes, so keep it lean.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct RunPayload {
    /// `https://github.com/<owner>/<repo>` — where the runner registers.
    /// Absent only in `encoded_jit_config` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_url: Option<String>,
    /// Short-lived GitHub App installation token. Never log this (`Debug`
    /// redacts it). `pat` accepted as a legacy alias.
    #[serde(default, alias = "pat", skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Pre-minted JIT runner config: run exactly this, skip registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoded_jit_config: Option<String>,
    /// Single-use JIT runner: one job, then terminate or pool.
    #[serde(default)]
    pub ephemeral: bool,
    /// Comma-separated runner labels, baked into the JIT config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<String>,
    /// Warm-pool member: after the job, clean up and await suspend instead of
    /// self-terminating.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pool: bool,
    /// Seconds a pooled VM waits for its suspend before self-terminating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_grace: Option<u64>,
    /// SSM path prefix of the handoff mailbox; the VM polls
    /// `{handoff_prefix}/{microvmId}` while idle-waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_prefix: Option<String>,
    /// The VM's own id. Injected by the platform on cold launch; set
    /// explicitly by the dispatcher on a parked handoff.
    #[serde(
        default,
        rename = "microvmId",
        alias = "microvm_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub microvm_id: Option<String>,
    /// Optional runner group name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_group: Option<String>,
    /// The dispatcher Lambda's own function name, so the VM can report its
    /// idleness back by direct invoke (in-guest `TerminateMicrovm` fails in
    /// PrivateLink-routed VPCs; plain `Invoke` does not). Absent from
    /// payloads built by older dispatchers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatcher_fn: Option<String>,
    /// Forward-compatibility: fields this version doesn't know survive a
    /// round-trip (a newer dispatcher can hand off to an older entrypoint).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn is_false(b: &bool) -> bool {
    !b
}

impl RunPayload {
    /// Ephemeral job payload (the only shape the dispatcher cold-launches).
    pub fn job(github_url: impl Into<String>, token: impl Into<String>, labels: &[String]) -> Self {
        Self {
            github_url: Some(github_url.into()),
            token: Some(token.into()),
            encoded_jit_config: None,
            ephemeral: true,
            labels: Some(labels.join(",")),
            pool: false,
            pool_grace: None,
            handoff_prefix: None,
            microvm_id: None,
            runner_group: None,
            dispatcher_fn: None,
            extra: BTreeMap::new(),
        }
    }

    /// Like [`RunPayload::job`] but with pre-joined labels.
    pub fn new(
        github_url: impl Into<String>,
        token: impl Into<String>,
        labels: impl Into<String>,
    ) -> Self {
        let mut p = Self::job(github_url, token, &[]);
        p.labels = Some(labels.into());
        p
    }

    /// Add warm-pool fields (dispatcher, POOL_ENABLED path).
    pub fn with_pool(mut self, grace: u64, handoff_prefix: &str) -> Self {
        self.pool = true;
        self.pool_grace = Some(grace);
        self.handoff_prefix = Some(handoff_prefix.to_string());
        self
    }

    /// Copy addressed to one VM, for parking in the mailbox.
    pub fn addressed_to(&self, id: &MicrovmId) -> Self {
        let mut copy = self.clone();
        copy.microvm_id = Some(id.as_str().to_string());
        copy
    }
}

impl fmt::Debug for RunPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunPayload")
            .field("github_url", &self.github_url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("encoded_jit_config", &self.encoded_jit_config)
            .field("ephemeral", &self.ephemeral)
            .field("labels", &self.labels)
            .field("pool", &self.pool)
            .field("pool_grace", &self.pool_grace)
            .field("handoff_prefix", &self.handoff_prefix)
            .field("microvm_id", &self.microvm_id)
            .field("runner_group", &self.runner_group)
            .field("dispatcher_fn", &self.dispatcher_fn)
            .field("extra", &self.extra)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_roundtrips_wire_shape() {
        let wire = serde_json::json!({
            "github_url": "https://github.com/o/r",
            "token": "t",
            "ephemeral": true,
            "labels": "self-hosted,linux,arm64,microvm",
            "pool": true,
            "pool_grace": 300,
            "handoff_prefix": "/p/handoff",
            "microvmId": "microvm-abc",
            "future_field": {"x": 1}
        });
        let p: RunPayload = serde_json::from_value(wire).unwrap();
        assert_eq!(p.microvm_id.as_deref(), Some("microvm-abc"));
        assert!(p.pool);
        let back = serde_json::to_value(&p).unwrap();
        assert_eq!(back["microvmId"], "microvm-abc");
        assert_eq!(back["future_field"]["x"], 1);
    }

    #[test]
    fn read_aliases_pat_and_microvm_id() {
        let p: RunPayload =
            serde_json::from_value(serde_json::json!({"pat": "t", "microvm_id": "microvm-abc"}))
                .unwrap();
        assert_eq!(p.token.as_deref(), Some("t"));
        assert_eq!(p.microvm_id.as_deref(), Some("microvm-abc"));
    }

    #[test]
    fn optional_fields_omitted_pool_skipped_when_false_ephemeral_always_present() {
        let p = RunPayload::job("https://github.com/o/r", "t", &["a".into(), "b".into()]);
        let v = serde_json::to_value(&p).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["ephemeral", "github_url", "labels", "token"]);
        assert_eq!(v["labels"], "a,b");
        assert_eq!(v["ephemeral"], true);
    }

    #[test]
    fn with_pool_and_addressed_to_set_pool_fields() {
        let p = RunPayload::job("https://github.com/o/r", "t", &["a".into()])
            .with_pool(300, "/p/handoff")
            .addressed_to(&MicrovmId::new("microvm-abc"));
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["pool"], true);
        assert_eq!(v["pool_grace"], 300);
        assert_eq!(v["handoff_prefix"], "/p/handoff");
        assert_eq!(v["microvmId"], "microvm-abc");
    }

    #[test]
    fn dispatcher_fn_roundtrips() {
        let wire = serde_json::json!({
            "github_url": "https://github.com/o/r",
            "token": "t",
            "dispatcher_fn": "gha-microvm-dispatcher",
        });
        let p: RunPayload = serde_json::from_value(wire).unwrap();
        assert_eq!(p.dispatcher_fn.as_deref(), Some("gha-microvm-dispatcher"));
        let back = serde_json::to_value(&p).unwrap();
        assert_eq!(back["dispatcher_fn"], "gha-microvm-dispatcher");
    }

    #[test]
    fn absent_dispatcher_fn_stays_absent_both_directions() {
        // Backward compatibility: an old dispatcher's payload (no field)
        // parses to None, and None never serializes the key.
        let p: RunPayload =
            serde_json::from_value(serde_json::json!({"github_url": "u", "token": "t"})).unwrap();
        assert!(p.dispatcher_fn.is_none());
        let back = serde_json::to_value(&p).unwrap();
        assert!(!back.as_object().unwrap().contains_key("dispatcher_fn"));
    }

    #[test]
    fn debug_redacts_the_token() {
        let p = RunPayload::job("https://github.com/o/r", "s3cret-token", &[]);
        let dbg = format!("{p:?}");
        assert!(!dbg.contains("s3cret-token"), "{dbg}");
        assert!(dbg.contains("<redacted>"));
    }
}
