//! The idle-report wire event: VM → dispatcher, as a direct Lambda invoke.
//!
//! In-guest `TerminateMicrovm` fails in PrivateLink-routed VPCs (the MicroVMs
//! sub-API rejects PrivateLink with `AccessDeniedException`), but a standard
//! Lambda `Invoke` works — so the VM reports its idleness and the dispatcher
//! suspends or terminates it from the control plane, where everything works.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The direct-invoke Lambda event:
/// `{"idle": {"microvmId": "...", "reason": "job-complete"|"orphan"}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdleEvent {
    pub idle: IdleReport,
}

/// One VM's statement that it is idle and safe to suspend or terminate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdleReport {
    /// The reporting VM's own id.
    #[serde(rename = "microvmId", alias = "microvm_id")]
    pub microvm_id: String,
    pub reason: IdleReason,
    /// `owner/repo` hint for the dispatcher's busy-check, derived from the
    /// run payload's `github_url`. Optional on the wire: absent when the VM
    /// has no usable URL (or an old guest built the report) — the dispatcher
    /// then falls back to its bounded fleet-wide runner scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// Why the VM considers itself idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdleReason {
    /// The runner finished its job and post-job cleanup is done.
    JobComplete,
    /// Nothing ever used (or reused) this VM: the idle watchdog fired, the
    /// pooled grace window expired unsuspended, or the run flow failed.
    Orphan,
}

impl IdleReason {
    /// The wire spelling (also the log value).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JobComplete => "job-complete",
            Self::Orphan => "orphan",
        }
    }
}

impl fmt::Display for IdleReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_roundtrips_the_wire_shape() {
        // Without a repo hint the key is absent entirely (old-dispatcher
        // compatibility: unknown keys would still be ignored, but absent is
        // byte-identical to the v0.0.3 pre-hint shape).
        let event = IdleEvent {
            idle: IdleReport {
                microvm_id: "microvm-abc".to_string(),
                reason: IdleReason::JobComplete,
                repo: None,
            },
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(
            wire,
            json!({"idle": {"microvmId": "microvm-abc", "reason": "job-complete"}})
        );
        let back: IdleEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn event_roundtrips_with_a_repo_hint() {
        let event = IdleEvent {
            idle: IdleReport {
                microvm_id: "microvm-abc".to_string(),
                reason: IdleReason::JobComplete,
                repo: Some("octo/repo".to_string()),
            },
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(
            wire,
            json!({"idle": {"microvmId": "microvm-abc", "reason": "job-complete",
                            "repo": "octo/repo"}})
        );
        let back: IdleEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn reason_spellings_are_kebab_case() {
        assert_eq!(IdleReason::JobComplete.as_str(), "job-complete");
        assert_eq!(IdleReason::Orphan.as_str(), "orphan");
        let r: IdleReason = serde_json::from_value(json!("orphan")).unwrap();
        assert_eq!(r, IdleReason::Orphan);
        assert!(serde_json::from_value::<IdleReason>(json!("JobComplete")).is_err());
    }

    #[test]
    fn report_accepts_the_snake_case_id_alias() {
        let r: IdleReport =
            serde_json::from_value(json!({"microvm_id": "microvm-x", "reason": "orphan"})).unwrap();
        assert_eq!(r.microvm_id, "microvm-x");
    }
}
