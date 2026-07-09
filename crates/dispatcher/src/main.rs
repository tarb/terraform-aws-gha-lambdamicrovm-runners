//! GitHub Actions runner autoscaler — webhook dispatcher.
//!
//! One Lambda, four intake shapes (SQS-delivered EventBridge envelopes,
//! direct EventBridge, the scheduled sweep marker, the legacy Function-URL
//! webhook), routed by [`intake::Intake`] into the domain services in
//! [`services::Services`]. The invariant everything here serves: every
//! failure degrades to a cold launch or a terminate — never a stuck job.

mod aws;
mod clock;
mod config;
mod dispatch;
mod fleet;
mod github;
mod gtime;
mod handler;
mod intake;
mod mailbox;
mod oplog;
mod pool;
mod secrets;
mod services;
mod sweep;

#[cfg(test)]
mod behavior;
#[cfg(test)]
mod testsupport;

use lambda_runtime::{LambdaEvent, service_fn};
use serde_json::Value;
use std::sync::Arc;

use crate::config::Config;
use crate::services::Services;
use crate::sweep::Deadline;

#[tokio::main]
async fn main() -> Result<(), lambda_runtime::Error> {
    // Diagnostics only — operational lines go to stdout via `oplog`.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let cfg = Arc::new(Config::from_env()?);
    // Per-call timeouts on every AWS SDK client: the defaults bound only the
    // connect, not a stalled read — which would otherwise ride to the Lambda
    // deadline (worst in the mailbox claim poll, turning one hung SSM `get`
    // into a failed batch). Mirrors the entrypoint's 15/20 s call discipline.
    let timeouts = aws_config::timeout::TimeoutConfig::builder()
        .operation_attempt_timeout(std::time::Duration::from_secs(15))
        .operation_timeout(std::time::Duration::from_secs(30))
        .build();
    let sdk = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .timeout_config(timeouts)
        .load()
        .await;
    let services = Arc::new(Services::real(cfg, &sdk));

    lambda_runtime::run(service_fn(move |ev: LambdaEvent<Value>| {
        let svc = Arc::clone(&services);
        async move {
            let deadline = Deadline::from_lambda(ev.context.deadline as i64);
            handler::handle(&svc, ev.payload, deadline)
                .await
                .map_err(|e| lambda_runtime::Error::from(e.to_string()))
        }
    }))
    .await
}
