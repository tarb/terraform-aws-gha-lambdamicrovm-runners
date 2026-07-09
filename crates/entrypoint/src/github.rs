//! Thin GitHub API client for runner registration: same headers on every
//! call, 30 s timeout, and a non-2xx status is an error (callers rely on
//! that to fall back or bubble into the run task's catch-all). Tokens are
//! never logged and never appear in error strings.

use crate::logfmt::log;
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde_json::Value;
use std::fmt;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum GhError {
    /// GitHub answered outside 2xx. The URL is safe to print; the token
    /// lives in a header and is never part of the message.
    #[error("HTTP Error {status}: {url}")]
    Status { status: u16, url: String },
    #[error("{0}")]
    Transport(String),
    #[error("{0}")]
    Shape(String),
    #[error("no owner/repo path in github url {0}")]
    BadUrl(String),
}

/// Base URL of the runners API for a repo or org:
/// `https://github.com/OWNER/REPO` maps to `.../repos/OWNER/REPO/actions/runners`,
/// a single path segment to `.../orgs/ORG/actions/runners`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnersApi(String);

impl RunnersApi {
    /// The scheme is optional; a bare host (no path) is an error.
    pub fn from_url(gh_api_base: &str, github_url: &str) -> Result<Self, GhError> {
        let rest = github_url.split_once("://").map_or(github_url, |(_, r)| r);
        let path = rest
            .split_once('/')
            .ok_or_else(|| GhError::BadUrl(github_url.to_string()))?
            .1
            .trim_matches('/');
        Ok(Self(if path.contains('/') {
            format!("{gh_api_base}/repos/{path}/actions/runners")
        } else {
            format!("{gh_api_base}/orgs/{path}/actions/runners")
        }))
    }

    /// Full URL of an endpoint under this runners API.
    pub fn url(&self, endpoint: &str) -> String {
        format!("{}/{endpoint}", self.0)
    }
}

impl fmt::Display for RunnersApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Request body for `generate-jitconfig`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct JitRequest {
    pub name: String,
    pub runner_group_id: i64,
    pub labels: Vec<String>,
    pub work_folder: &'static str,
}

impl JitRequest {
    /// Group id: a digit-string or positive integer selects that group;
    /// everything else means the default group (1). Empty labels are
    /// dropped.
    pub fn new(name: &str, labels: &str, runner_group: Option<&Value>) -> Self {
        let runner_group_id: i64 = match runner_group {
            Some(Value::String(s)) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
                s.parse().unwrap_or(1)
            }
            Some(Value::Number(n)) => n.as_i64().filter(|v| *v > 0).unwrap_or(1),
            _ => 1,
        };
        Self {
            name: name.to_string(),
            runner_group_id,
            labels: labels
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            work_folder: "_work",
        }
    }
}

/// One GitHub API call. Empty response bodies decode as JSON null.
pub async fn gh_api(
    client: &reqwest::Client,
    method: Method,
    url: &str,
    token: &SecretString,
    body: Option<&Value>,
) -> Result<(u16, Value), GhError> {
    let mut req = client
        .request(method, url)
        .header("Authorization", format!("Bearer {}", token.expose_secret()))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Content-Type", "application/json")
        .header("User-Agent", "lambda-microvm-runner")
        .timeout(Duration::from_secs(30));
    if let Some(b) = body {
        req = req.body(serde_json::to_vec(b).map_err(|e| GhError::Transport(e.to_string()))?);
    }
    // reqwest errors never include request headers, so the token can't leak.
    let resp = req
        .send()
        .await
        .map_err(|e| GhError::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(GhError::Status {
            status,
            url: url.to_string(),
        });
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GhError::Transport(e.to_string()))?;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).map_err(|e| GhError::Transport(e.to_string()))?
    };
    Ok((status, value))
}

/// Mint a JIT runner config via `generate-jitconfig`. Returns the
/// `encoded_jit_config` blob, or `None` on any failure so the caller falls
/// back to config.sh registration.
pub async fn mint_jitconfig(
    client: &reqwest::Client,
    api: &RunnersApi,
    token: &SecretString,
    req: &JitRequest,
) -> Option<String> {
    let body = serde_json::to_value(req).ok()?;
    match gh_api(
        client,
        Method::POST,
        &api.url("generate-jitconfig"),
        token,
        Some(&body),
    )
    .await
    {
        Ok((_, cfg)) => cfg
            .get("encoded_jit_config")
            .and_then(Value::as_str)
            .map(str::to_string),
        Err(e) => {
            log(format!(
                "generate-jitconfig failed ({e}); falling back to config.sh registration"
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const API: &str = "https://api.github.com";

    #[test]
    fn repo_url_maps_to_repo_runners_api() {
        assert_eq!(
            RunnersApi::from_url(API, "https://github.com/octo/repo").unwrap(),
            RunnersApi("https://api.github.com/repos/octo/repo/actions/runners".into())
        );
    }

    #[test]
    fn org_url_maps_to_org_runners_api() {
        assert_eq!(
            RunnersApi::from_url(API, "https://github.com/octo-org").unwrap(),
            RunnersApi("https://api.github.com/orgs/octo-org/actions/runners".into())
        );
        // Trailing slashes are stripped before classification.
        assert_eq!(
            RunnersApi::from_url(API, "https://github.com/octo-org/").unwrap(),
            RunnersApi("https://api.github.com/orgs/octo-org/actions/runners".into())
        );
    }

    #[test]
    fn schemeless_url_still_parses() {
        assert_eq!(
            RunnersApi::from_url(API, "github.com/o/r").unwrap(),
            RunnersApi("https://api.github.com/repos/o/r/actions/runners".into())
        );
    }

    #[test]
    fn bare_host_is_an_error() {
        assert!(RunnersApi::from_url(API, "https://github.com").is_err());
    }

    #[test]
    fn jit_request_runner_group_derivation() {
        assert_eq!(JitRequest::new("n", "a,b", None).runner_group_id, 1);
        assert_eq!(
            JitRequest::new("n", "l", Some(&json!("5"))).runner_group_id,
            5
        );
        assert_eq!(
            JitRequest::new("n", "l", Some(&json!("abc"))).runner_group_id,
            1
        );
        assert_eq!(
            JitRequest::new("n", "l", Some(&json!(7))).runner_group_id,
            7
        );
        assert_eq!(
            JitRequest::new("n", "l", Some(&json!(0))).runner_group_id,
            1
        );
        assert_eq!(
            JitRequest::new("n", "l", Some(&json!(-3))).runner_group_id,
            1
        );
        assert_eq!(
            JitRequest::new("n", "l", Some(&json!(null))).runner_group_id,
            1
        );
    }

    #[test]
    fn jit_request_filters_empty_labels_and_sets_work_folder() {
        let req = JitRequest::new("runner-x", "self-hosted,,linux,", None);
        assert_eq!(req.labels, ["self-hosted", "linux"]);
        assert_eq!(req.work_folder, "_work");
        assert_eq!(req.name, "runner-x");
    }
}
