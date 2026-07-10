//! The typed `workflow_job` webhook payload — lenient by construction (every
//! field defaulted, unknown fields ignored): GitHub adds fields freely and a
//! missing one must degrade, not error.

use serde::Deserialize;
use std::collections::BTreeSet;

use crate::dispatch::JobRef;
use crate::github::types::InstallationId;

pub type LabelSet = BTreeSet<String>;

#[derive(Debug, Default, Clone, Deserialize)]
pub struct WebhookPayload {
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub workflow_job: Option<WorkflowJob>,
    #[serde(default)]
    pub repository: Option<Repository>,
    #[serde(default)]
    pub installation: Option<Installation>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct WorkflowJob {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub runner_name: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Repository {
    #[serde(default)]
    pub full_name: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Installation {
    #[serde(default)]
    pub id: Option<i64>,
}

impl WebhookPayload {
    pub fn labels(&self) -> LabelSet {
        self.workflow_job
            .as_ref()
            .and_then(|j| j.labels.as_ref())
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn job_id(&self) -> Option<i64> {
        self.workflow_job.as_ref().and_then(|j| j.id)
    }

    pub fn runner_name(&self) -> &str {
        self.workflow_job
            .as_ref()
            .and_then(|j| j.runner_name.as_deref())
            .unwrap_or("")
    }

    /// `repository.full_name`, non-empty.
    pub fn repo(&self) -> Option<&str> {
        self.repository
            .as_ref()
            .and_then(|r| r.full_name.as_deref())
            .filter(|s| !s.is_empty())
    }

    /// Installation id; 0 means "not really there" (derive from the repo).
    pub fn installation_id(&self) -> Option<InstallationId> {
        self.installation
            .as_ref()
            .and_then(|i| i.id)
            .filter(|&id| id != 0)
            .map(InstallationId)
    }

    /// A dispatchable job reference; `None` without a repository.
    pub fn job_ref(&self) -> Option<JobRef> {
        Some(JobRef {
            repo: self.repo()?.to_string(),
            job_id: self.job_id(),
            installation: self.installation_id(),
            labels: self.labels(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_defaults_and_zero_installation_is_none() {
        let p: WebhookPayload = serde_json::from_value(serde_json::json!({
            "action": "queued",
            "workflow_job": {"id": 5, "labels": ["a"], "unknown": 1},
            "installation": {"id": 0},
            "unknown_top": {"x": 1},
        }))
        .unwrap();
        assert_eq!(p.job_id(), Some(5));
        assert!(p.labels().contains("a"));
        assert!(p.installation_id().is_none(), "0 is not an installation");
        assert!(p.repo().is_none());
        assert!(p.job_ref().is_none(), "no repo, nothing to dispatch");

        let empty: WebhookPayload = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(empty.runner_name(), "");
        assert!(empty.labels().is_empty());
    }

    #[test]
    fn job_ref_carries_the_requested_labels() {
        let p: WebhookPayload = serde_json::from_value(serde_json::json!({
            "action": "queued",
            "workflow_job": {"id": 5, "labels": ["self-hosted", "microvm", "docker"]},
            "repository": {"full_name": "o/r"},
        }))
        .unwrap();
        let job = p.job_ref().unwrap();
        assert_eq!(job.labels, p.labels());
        assert!(job.labels.contains("docker"));
    }

    #[test]
    fn empty_repo_name_is_absent() {
        let p: WebhookPayload = serde_json::from_value(serde_json::json!({
            "repository": {"full_name": ""},
        }))
        .unwrap();
        assert!(p.repo().is_none());
    }
}
