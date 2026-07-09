//! GitHub REST seam: domain-level operations (fakes script typed responses,
//! not URL substrings) and the real reqwest-backed client with the App-JWT +
//! installation-token machinery inside.

pub mod jwt;
pub mod types;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::clock::{Clock, Epoch};
use crate::gtime;
use crate::secrets::{SecretsCache, SecretsError};
use jwt::JwtError;
use types::{
    InstallationId, InstallationToken, JobInfo, RepoRef, RunStatus, RunnerInfo, WorkflowRun,
};

#[derive(Debug, Clone, thiserror::Error)]
pub enum GithubError {
    #[error("github returned {status} for {endpoint}")]
    Status { status: u16, endpoint: String },
    #[error("github transport error: {0}")]
    Transport(String),
    #[error("unexpected github response shape: {0}")]
    Shape(&'static str),
    #[error(transparent)]
    Jwt(#[from] JwtError),
    #[error(transparent)]
    Secrets(#[from] SecretsError),
}

#[async_trait]
pub trait GithubApi: Send + Sync {
    /// Installation token for a repo; derives the installation via
    /// `/repos/{repo}/installation` when the webhook didn't carry one.
    async fn token_for_repo(
        &self,
        repo: &str,
        installation: Option<InstallationId>,
    ) -> Result<InstallationToken, GithubError>;
    /// All App installations (App JWT auth).
    async fn installations(&self) -> Result<Vec<InstallationId>, GithubError>;
    /// Mint (or reuse) the installation token and list its repos.
    async fn installation_repos(
        &self,
        id: InstallationId,
    ) -> Result<(InstallationToken, Vec<RepoRef>), GithubError>;
    async fn repo_runners(
        &self,
        repo: &str,
        token: &InstallationToken,
    ) -> Result<Vec<RunnerInfo>, GithubError>;
    async fn workflow_runs(
        &self,
        repo: &str,
        token: &InstallationToken,
        status: RunStatus,
    ) -> Result<Vec<WorkflowRun>, GithubError>;
    async fn run_jobs(
        &self,
        repo: &str,
        run_id: i64,
        token: &InstallationToken,
    ) -> Result<Vec<JobInfo>, GithubError>;
}

/// Installation-token cache: tokens live ~1h; reuse until 60 s before expiry.
pub(crate) struct TokenCache(Mutex<HashMap<InstallationId, (InstallationToken, Epoch)>>);

impl TokenCache {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }

    pub(crate) fn fresh(&self, id: InstallationId, now: Epoch) -> Option<InstallationToken> {
        self.0
            .lock()
            .unwrap()
            .get(&id)
            .filter(|(_, expires)| expires.0 - 60.0 > now.0)
            .map(|(token, _)| token.clone())
    }

    pub(crate) fn store(&self, id: InstallationId, token: InstallationToken, expires: Epoch) {
        self.0.lock().unwrap().insert(id, (token, expires));
    }
}

pub struct GithubClient {
    http: reqwest::Client,
    base: String,
    secrets: Arc<SecretsCache>,
    clock: Arc<dyn Clock>,
    tokens: TokenCache,
}

impl GithubClient {
    pub fn new(base: String, secrets: Arc<SecretsCache>, clock: Arc<dyn Clock>) -> Self {
        Self {
            // GitHub hard-403s requests without a User-Agent.
            http: reqwest::Client::builder()
                .user_agent("gha-microvm-dispatcher")
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("http client"),
            base,
            secrets,
            clock,
            tokens: TokenCache::new(),
        }
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        endpoint: &str,
        bearer: &str,
    ) -> Result<T, GithubError> {
        let url = format!("{}{}", self.base, endpoint);
        let resp = self
            .http
            .request(method, &url)
            .header("Authorization", format!("Bearer {bearer}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|e| GithubError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            return Err(GithubError::Status {
                status,
                endpoint: endpoint.to_string(),
            });
        }
        resp.json::<T>()
            .await
            .map_err(|_| GithubError::Shape("undecodable response body"))
    }

    async fn app_jwt(&self) -> Result<String, GithubError> {
        let bundle = self.secrets.bundle().await?;
        let app = bundle.app()?;
        Ok(jwt::sign_app_jwt(
            &app.app_id,
            secrecy::ExposeSecret::expose_secret(&app.private_key),
            self.clock.now(),
        )?)
    }

    async fn installation_token(
        &self,
        id: InstallationId,
    ) -> Result<InstallationToken, GithubError> {
        if let Some(token) = self.tokens.fresh(id, self.clock.now()) {
            return Ok(token);
        }
        #[derive(serde::Deserialize)]
        struct Minted {
            token: String,
            expires_at: String,
        }
        let app_jwt = self.app_jwt().await?;
        let minted: Minted = self
            .call(
                reqwest::Method::POST,
                &format!("/app/installations/{id}/access_tokens"),
                &app_jwt,
            )
            .await?;
        let expires = gtime::parse_gh_time(&minted.expires_at)
            .map_err(|_| GithubError::Shape("unparsable token expires_at"))?;
        let token = InstallationToken::new(minted.token);
        self.tokens.store(id, token.clone(), expires);
        Ok(token)
    }
}

#[async_trait]
impl GithubApi for GithubClient {
    async fn token_for_repo(
        &self,
        repo: &str,
        installation: Option<InstallationId>,
    ) -> Result<InstallationToken, GithubError> {
        let id = match installation {
            Some(id) => id,
            None => {
                #[derive(serde::Deserialize)]
                struct InstallationRef {
                    id: Option<InstallationId>,
                }
                let app_jwt = self.app_jwt().await?;
                let inst: InstallationRef = self
                    .call(
                        reqwest::Method::GET,
                        &format!("/repos/{repo}/installation"),
                        &app_jwt,
                    )
                    .await?;
                inst.id
                    .ok_or(GithubError::Shape("no installation id for repo"))?
            }
        };
        self.installation_token(id).await
    }

    async fn installations(&self) -> Result<Vec<InstallationId>, GithubError> {
        #[derive(serde::Deserialize)]
        struct InstallationRef {
            id: InstallationId,
        }
        let app_jwt = self.app_jwt().await?;
        let list: Vec<InstallationRef> = self
            .call(reqwest::Method::GET, "/app/installations", &app_jwt)
            .await?;
        Ok(list.into_iter().map(|i| i.id).collect())
    }

    async fn installation_repos(
        &self,
        id: InstallationId,
    ) -> Result<(InstallationToken, Vec<RepoRef>), GithubError> {
        #[derive(serde::Deserialize)]
        struct Repos {
            #[serde(default)]
            repositories: Vec<RepoRef>,
        }
        let token = self.installation_token(id).await?;
        let repos: Repos = self
            .call(
                reqwest::Method::GET,
                "/installation/repositories?per_page=100",
                token.reveal(),
            )
            .await?;
        Ok((token, repos.repositories))
    }

    async fn repo_runners(
        &self,
        repo: &str,
        token: &InstallationToken,
    ) -> Result<Vec<RunnerInfo>, GithubError> {
        #[derive(serde::Deserialize)]
        struct Runners {
            #[serde(default)]
            runners: Vec<RunnerInfo>,
        }
        let runners: Runners = self
            .call(
                reqwest::Method::GET,
                &format!("/repos/{repo}/actions/runners?per_page=100"),
                token.reveal(),
            )
            .await?;
        Ok(runners.runners)
    }

    async fn workflow_runs(
        &self,
        repo: &str,
        token: &InstallationToken,
        status: RunStatus,
    ) -> Result<Vec<WorkflowRun>, GithubError> {
        #[derive(serde::Deserialize)]
        struct Runs {
            #[serde(default)]
            workflow_runs: Vec<WorkflowRun>,
        }
        let runs: Runs = self
            .call(
                reqwest::Method::GET,
                &format!(
                    "/repos/{repo}/actions/runs?status={}&per_page=30",
                    status.as_query()
                ),
                token.reveal(),
            )
            .await?;
        Ok(runs.workflow_runs)
    }

    async fn run_jobs(
        &self,
        repo: &str,
        run_id: i64,
        token: &InstallationToken,
    ) -> Result<Vec<JobInfo>, GithubError> {
        #[derive(serde::Deserialize)]
        struct Jobs {
            #[serde(default)]
            jobs: Vec<JobInfo>,
        }
        let jobs: Jobs = self
            .call(
                reqwest::Method::GET,
                &format!("/repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"),
                token.reveal(),
            )
            .await?;
        Ok(jobs.jobs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_cache_reuses_until_60s_before_expiry() {
        let cache = TokenCache::new();
        let id = InstallationId(1);
        let now = Epoch(1_700_000_000.0);
        cache.store(id, InstallationToken::new("tok"), Epoch(now.0 + 61.0));
        assert!(cache.fresh(id, now).is_some(), "61s of margin is fresh");
        cache.store(id, InstallationToken::new("tok"), Epoch(now.0 + 60.0));
        assert!(
            cache.fresh(id, now).is_none(),
            "the 60s safety margin must force a re-mint"
        );
        assert!(cache.fresh(InstallationId(2), now).is_none());
    }
}
