//! Runner registration as a typed state machine.
//!
//! Key design rule: the runner is registered with GitHub at /run time
//! (post-snapshot), NEVER at build time — every MicroVM boots from the same
//! snapshot, so baking a registration in would make all VMs share one
//! identity.

use crate::config::Config;
use crate::github::{GhError, JitRequest, RunnersApi, gh_api, mint_jitconfig};
use crate::logfmt::log;
use crate::payload::{RunConfig, lenient};
use crate::supervisor::{RunnerCommand, exit_code};
use reqwest::Method;
use secrecy::SecretString;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("config.sh exited rc={rc}")]
    ConfigSh { rc: i64 },
    #[error("config.sh: {0}")]
    ConfigShSpawn(String),
    #[error("launch runner: {0}")]
    Spawn(String),
    #[error("wait for runner: {0}")]
    Wait(String),
    #[error(transparent)]
    Github(#[from] GhError),
}

/// Everything a config.sh registration needs; also the fallback target of
/// a failed on-box JIT mint.
pub struct ConfigShSpec {
    pub url: String,
    pub token: SecretString,
    pub name: String,
    pub labels: String,
    pub ephemeral: bool,
    pub runner_group: Option<Value>,
}

/// How this run will obtain a runner identity.
pub enum LaunchPlan {
    /// The payload carried a pre-minted JIT config: run exactly that.
    JitDirect { jit: String },
    /// Ephemeral with url+token: mint a JIT config ON-BOX (keeps the big
    /// blob off the 4 KB runHookPayload); fall back to config.sh.
    MintJit(ConfigShSpec),
    /// Persistent with url+token: classic config.sh registration.
    ConfigSh(ConfigShSpec),
}

impl LaunchPlan {
    /// Decision table over the run config (env fallbacks already resolved).
    /// `None` means the run lacks credentials — the caller logs and no-ops;
    /// it is deliberately NOT a failure (no pool teardown; the zombie
    /// reaper / max-duration backstop reap the VM).
    pub fn decide(cfg: &RunConfig) -> Option<LaunchPlan> {
        if let Some(jit) = &cfg.jit {
            return Some(LaunchPlan::JitDirect { jit: jit.clone() });
        }
        let (Some(url), Some(token)) = (&cfg.github_url, &cfg.token) else {
            return None;
        };
        let spec = ConfigShSpec {
            url: url.clone(),
            token: token.clone(),
            name: cfg.runner_name(),
            labels: cfg.labels.clone(),
            ephemeral: cfg.ephemeral,
            runner_group: cfg.runner_group.clone(),
        };
        Some(if cfg.ephemeral {
            LaunchPlan::MintJit(spec)
        } else {
            LaunchPlan::ConfigSh(spec)
        })
    }

    /// Perform the registration this plan calls for.
    pub async fn prepare(
        self,
        http: &reqwest::Client,
        env: &Config,
    ) -> Result<PreparedRunner, LaunchError> {
        match self {
            LaunchPlan::JitDirect { jit } => {
                log("ephemeral mode: running one job via JIT config");
                Ok(PreparedRunner::Jit {
                    cmd: jit_command(env, jit),
                })
            }
            LaunchPlan::MintJit(spec) => {
                let api = RunnersApi::from_url(&env.gh_api, &spec.url)?;
                let req = JitRequest::new(&spec.name, &spec.labels, spec.runner_group.as_ref());
                match mint_jitconfig(http, &api, &spec.token, &req).await {
                    Some(jit) => {
                        log(format!(
                            "ephemeral JIT: on-box jitconfig, running one job (name={})",
                            spec.name
                        ));
                        Ok(PreparedRunner::Jit {
                            cmd: jit_command(env, jit),
                        })
                    }
                    // mint_jitconfig already logged the fallback reason.
                    None => config_sh(http, env, api, spec).await,
                }
            }
            LaunchPlan::ConfigSh(spec) => {
                let api = RunnersApi::from_url(&env.gh_api, &spec.url)?;
                config_sh(http, env, api, spec).await
            }
        }
    }
}

/// A registered runner, ready to spawn.
#[derive(Debug)]
pub enum PreparedRunner {
    /// JIT runners always run exactly one job.
    Jit { cmd: RunnerCommand },
    Registered {
        cmd: RunnerCommand,
        ephemeral: bool,
        dereg: Registration,
    },
}

impl PreparedRunner {
    pub fn is_ephemeral(&self) -> bool {
        match self {
            PreparedRunner::Jit { .. } => true,
            PreparedRunner::Registered { ephemeral, .. } => *ephemeral,
        }
    }

    pub fn command(&self) -> RunnerCommand {
        match self {
            PreparedRunner::Jit { cmd } | PreparedRunner::Registered { cmd, .. } => cmd.clone(),
        }
    }
}

/// What `/terminate` needs to de-register a config.sh-registered runner.
#[derive(Debug, Clone)]
pub struct Registration {
    pub api: RunnersApi,
    /// GitHub token for de-registration. Never logged.
    pub token: SecretString,
}

fn jit_command(env: &Config, jit: String) -> RunnerCommand {
    RunnerCommand {
        program: format!("{}/bin/Runner.Listener", env.runner_dir),
        args: vec!["run".into(), "--jitconfig".into(), jit],
    }
}

async fn config_sh(
    http: &reqwest::Client,
    env: &Config,
    api: RunnersApi,
    spec: ConfigShSpec,
) -> Result<PreparedRunner, LaunchError> {
    log(format!(
        "minting registration token: {api}/registration-token"
    ));
    let (_, tok) = gh_api(
        http,
        Method::POST,
        &api.url("registration-token"),
        &spec.token,
        None,
    )
    .await?;
    let reg_token = tok
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| GhError::Shape("registration-token response missing 'token'".into()))?
        .to_string();
    let mut argv: Vec<String> = vec![
        "--unattended".into(),
        "--disableupdate".into(),
        "--url".into(),
        spec.url.clone(),
        "--token".into(),
        reg_token,
        "--name".into(),
        spec.name.clone(),
        "--labels".into(),
        spec.labels.clone(),
        "--work".into(),
        "_work".into(),
        // A crashed predecessor may have left this name registered.
        "--replace".into(),
    ];
    if spec.ephemeral {
        argv.push("--ephemeral".into());
    }
    if let Some(group) = spec.runner_group.as_ref().filter(|v| lenient::truthy(v)) {
        argv.push("--runnergroup".into());
        argv.push(group_arg(group));
    }
    log(format!(
        "configuring runner '{}' labels={} ephemeral={}",
        spec.name, spec.labels, spec.ephemeral
    ));
    let status = tokio::process::Command::new(format!("{}/config.sh", env.runner_dir))
        .args(&argv)
        .current_dir(&env.runner_dir)
        .env("RUNNER_ALLOW_RUNASROOT", "1")
        .status()
        .await
        .map_err(|e| LaunchError::ConfigShSpawn(e.to_string()))?;
    if !status.success() {
        return Err(LaunchError::ConfigSh {
            rc: exit_code(status),
        });
    }
    Ok(PreparedRunner::Registered {
        cmd: RunnerCommand {
            program: format!("{}/run.sh", env.runner_dir),
            args: vec![],
        },
        ephemeral: spec.ephemeral,
        dereg: Registration {
            api,
            token: spec.token,
        },
    })
}

/// A `--runnergroup` argv value from a loose JSON field.
fn group_arg(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testsupport::{temp_dir, test_config};
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn run_cfg(v: serde_json::Value) -> RunConfig {
        RunConfig::from_value(v)
    }

    #[test]
    fn decide_table() {
        assert!(matches!(
            LaunchPlan::decide(&run_cfg(json!({"encoded_jit_config": "abc"}))),
            Some(LaunchPlan::JitDirect { .. })
        ));
        // A pre-minted JIT config wins over url+token.
        assert!(matches!(
            LaunchPlan::decide(&run_cfg(json!({
                "encoded_jit_config": "abc",
                "github_url": "https://github.com/o/r",
                "token": "t",
                "ephemeral": true
            }))),
            Some(LaunchPlan::JitDirect { .. })
        ));
        assert!(matches!(
            LaunchPlan::decide(&run_cfg(json!({
                "github_url": "https://github.com/o/r", "token": "t", "ephemeral": true
            }))),
            Some(LaunchPlan::MintJit(_))
        ));
        assert!(matches!(
            LaunchPlan::decide(&run_cfg(json!({
                "github_url": "https://github.com/o/r", "token": "t"
            }))),
            Some(LaunchPlan::ConfigSh(_))
        ));
        // Missing credentials: a no-op, not a failure.
        assert!(LaunchPlan::decide(&run_cfg(json!({}))).is_none());
        assert!(LaunchPlan::decide(&run_cfg(json!({"github_url": "u"}))).is_none());
        assert!(LaunchPlan::decide(&run_cfg(json!({"token": "t"}))).is_none());
    }

    #[tokio::test]
    async fn jit_direct_prepares_the_listener_command() {
        let env = test_config("/opt/actions-runner");
        let plan = LaunchPlan::decide(&run_cfg(json!({"encoded_jit_config": "blob"}))).unwrap();
        let prepared = plan.prepare(&reqwest::Client::new(), &env).await.unwrap();
        assert!(prepared.is_ephemeral());
        let cmd = prepared.command();
        assert_eq!(cmd.program, "/opt/actions-runner/bin/Runner.Listener");
        assert_eq!(cmd.args, ["run", "--jitconfig", "blob"]);
    }

    /// A local GitHub stand-in: generate-jitconfig fails, registration-token
    /// succeeds. Returns (api base URL, request-path journal).
    async fn fake_github() -> (String, Arc<Mutex<Vec<String>>>) {
        let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let record = |hits: &Arc<Mutex<Vec<String>>>, path: &str| {
            hits.lock().unwrap().push(path.to_string());
        };
        let h1 = hits.clone();
        let h2 = hits.clone();
        let router = Router::new()
            .route(
                "/repos/o/r/actions/runners/generate-jitconfig",
                post(move || {
                    record(&h1, "generate-jitconfig");
                    async { StatusCode::INTERNAL_SERVER_ERROR.into_response() }
                }),
            )
            .route(
                "/repos/o/r/actions/runners/registration-token",
                post(move || {
                    record(&h2, "registration-token");
                    async { (StatusCode::OK, r#"{"token": "REG"}"#).into_response() }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (base, hits)
    }

    fn fake_runner_dir(tag: &str, config_sh: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir(tag);
        let path = format!("{dir}/config.sh");
        std::fs::write(&path, config_sh).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[tokio::test]
    async fn mint_jit_failure_falls_back_to_config_sh() {
        let (base, hits) = fake_github().await;
        let mut env = test_config(&fake_runner_dir("reg-fallback", "#!/bin/sh\nexit 0\n"));
        env.gh_api = base;
        let plan = LaunchPlan::decide(&run_cfg(json!({
            "github_url": "https://github.com/o/r", "token": "t", "ephemeral": true
        })))
        .unwrap();
        let prepared = plan.prepare(&reqwest::Client::new(), &env).await.unwrap();
        assert_eq!(
            hits.lock().unwrap().as_slice(),
            ["generate-jitconfig", "registration-token"]
        );
        assert!(matches!(prepared, PreparedRunner::Registered { .. }));
        assert!(prepared.is_ephemeral());
        assert!(prepared.command().program.ends_with("/run.sh"));
    }

    #[tokio::test]
    async fn config_sh_nonzero_exit_is_an_error() {
        let (base, _hits) = fake_github().await;
        let mut env = test_config(&fake_runner_dir("reg-rc", "#!/bin/sh\nexit 3\n"));
        env.gh_api = base;
        let plan = LaunchPlan::decide(&run_cfg(json!({
            "github_url": "https://github.com/o/r", "token": "t"
        })))
        .unwrap();
        let err = plan
            .prepare(&reqwest::Client::new(), &env)
            .await
            .unwrap_err();
        assert!(matches!(err, LaunchError::ConfigSh { rc: 3 }));
    }
}
