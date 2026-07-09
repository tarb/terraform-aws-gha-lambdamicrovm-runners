//! Typed GitHub REST responses (lenient: unexpected extra fields ignored).

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::fmt;

use crate::intake::webhook::LabelSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(transparent)]
pub struct InstallationId(pub i64);

impl fmt::Display for InstallationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A short-lived GitHub App installation token (~1h). Never logged.
#[derive(Clone)]
pub struct InstallationToken(SecretString);

impl InstallationToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(SecretString::from(token.into()))
    }

    /// The raw token, for Bearer auth and the run payload (it IS the wire).
    pub fn reveal(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for InstallationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InstallationToken(<redacted>)")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunnerInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub busy: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoRef {
    pub full_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowRun {
    pub id: i64,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobInfo {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub labels: LabelSet,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Queued,
    InProgress,
}

impl RunStatus {
    pub fn as_query(self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::InProgress => "in_progress",
        }
    }
}
