//! Lifecycle-hook server + GitHub Actions runner supervisor for AWS Lambda
//! MicroVMs.
//!
//! Boots an HTTP server on HOOK_PORT (default 9000, bound 0.0.0.0)
//! implementing the MicroVM lifecycle hooks at
//! POST /aws/lambda-microvms/runtime/v1/<hook>.
//!
//! Key design rule: the runner is registered with GitHub at /run time
//! (post-snapshot), NEVER at build time — every MicroVM boots from the same
//! snapshot, so baking a registration in would make all VMs share one
//! identity. The per-VM config arrives in the /run body (runHookPayload),
//! with microvmId auto-injected by the platform.
//!
//! Two modes, chosen per-run by the /run payload:
//! * ephemeral : `{"encoded_jit_config": "<base64>"}` -> one job, then exit
//! * persistent: `{"github_url": "...", "token": "<PAT>", "labels": "...",
//!   "ephemeral": false}` -> many jobs (up to 8h)

mod aws;
mod config;
mod docker;
mod gate;
mod github;
mod handoff;
mod logfmt;
mod payload;
mod pool;
mod registration;
mod report;
mod rlimit;
mod server;
mod state;
mod supervisor;
mod terminate;

use logfmt::log;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let cfg = config::Config::from_env();
    log(format!(
        "hook server on 0.0.0.0:{} (runner dir {}, docker={})",
        cfg.hook_port, cfg.runner_dir, cfg.enable_docker
    ));
    // Before any child: dockerd + runner inherit these.
    rlimit::raise_nofile_rlimit(&cfg);
    // dockerd is NOT started here: it comes up fresh per job in the run
    // task so its bridge/NAT/DNS reflect the live MicroVM network.
    let region = config::region_label();
    let aws: Arc<dyn aws::CloudControl> = Arc::new(aws::RealAws::new(region.clone()));
    let app = state::AppState::new(cfg, aws, region);
    server::serve(app).await.expect("hook server failed");
}
