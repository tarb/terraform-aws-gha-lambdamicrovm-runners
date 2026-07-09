//! Shared wire contracts between the dispatcher and the VM entrypoint.
//!
//! These types ARE the compatibility surface: the JSON they produce must stay
//! byte-compatible with what the Python components read and write, so a fleet
//! can run mixed Python/Rust components mid-migration. Field names are
//! therefore snake_case exactly as the Python dicts spell them, and unknown
//! fields are preserved via `extra`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The run payload: delivered to the VM either via `RunMicrovm.runHookPayload`
/// (cold launch, injected by the platform with `microvmId` added) or parked in
/// the SSM handoff mailbox (pool resume, `microvm_id` set by the dispatcher).
///
/// Capped at 4096 bytes by the service (`runHookPayload` shape) — keep it lean.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunPayload {
    /// `https://github.com/<owner>/<repo>` — where the runner registers.
    pub github_url: String,
    /// Short-lived GitHub App installation token. Never log this.
    pub token: String,
    /// Single-use JIT runner: one job, then terminate or pool.
    #[serde(default)]
    pub ephemeral: bool,
    /// Comma-separated runner labels, baked into the JIT config.
    pub labels: String,
    /// Warm pool member: after the job, clean up and await suspend instead of
    /// self-terminating.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pool: bool,
    /// Seconds a pooled VM waits for its suspend before self-terminating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_grace: Option<u64>,
    /// SSM path prefix of the handoff mailbox; the VM polls
    /// `{handoff_prefix}/{microvm_id}` while idle-waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_prefix: Option<String>,
    /// The VM's own id. Injected by the platform on cold launch
    /// (`microvmId`); set explicitly by the dispatcher on a parked handoff.
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
    /// Forward-compatibility: fields this version doesn't know survive a
    /// round-trip (a newer dispatcher can hand off to an older entrypoint).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn is_false(b: &bool) -> bool {
    !b
}

impl RunPayload {
    pub fn new(
        github_url: impl Into<String>,
        token: impl Into<String>,
        labels: impl Into<String>,
    ) -> Self {
        Self {
            github_url: github_url.into(),
            token: token.into(),
            ephemeral: true,
            labels: labels.into(),
            pool: false,
            pool_grace: None,
            handoff_prefix: None,
            microvm_id: None,
            runner_group: None,
            extra: BTreeMap::new(),
        }
    }
}

/// Deterministic runner name for a microVM id — MUST match the Python
/// entrypoint (`gha-mvm-` + first 18 chars of the id sans `microvm-` prefix)
/// because the dispatcher's suspend intake and zombie reaper both derive it
/// to map runners back to VMs.
pub fn runner_name(microvm_id: &str) -> String {
    let bare = microvm_id.strip_prefix("microvm-").unwrap_or(microvm_id);
    let suffix: String = bare.chars().take(18).collect();
    let name = format!("gha-mvm-{suffix}");
    name.chars().take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_name_matches_python_derivation() {
        assert_eq!(
            runner_name("microvm-aaaa1111-2222-3333-4444-555566667777"),
            "gha-mvm-aaaa1111-2222-3333"
        );
        assert_eq!(runner_name("deadbeef"), "gha-mvm-deadbeef");
    }

    #[test]
    fn payload_roundtrips_python_shape() {
        let py = serde_json::json!({
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
        let p: RunPayload = serde_json::from_value(py.clone()).unwrap();
        assert_eq!(p.microvm_id.as_deref(), Some("microvm-abc"));
        assert!(p.pool);
        let back = serde_json::to_value(&p).unwrap();
        assert_eq!(back["microvmId"], "microvm-abc");
        assert_eq!(back["future_field"]["x"], 1);
    }
}
