//! Per-container service wiring. All caches (secret TTL, image TTL, token
//! cache, resume ledger, record-keys canary) live inside their owning
//! service — no globals.

use std::sync::Arc;
use std::time::Duration;

use crate::aws::microvm::SdkMicrovmApi;
use crate::aws::params::SdkParamStore;
use crate::aws::secretsman::SdkSecretStore;
use crate::clock::{Clock, SystemClock};
use crate::config::Config;
use crate::dispatch::Dispatcher;
use crate::fleet::Fleet;
use crate::github::{GithubApi, GithubClient};
use crate::mailbox::Mailbox;
use crate::pool::{Pool, PoolPolicy, ResumeLedger};
use crate::secrets::SecretsCache;
use crate::sweep::Sweeper;

pub struct Services {
    pub cfg: Arc<Config>,
    pub secrets: Arc<SecretsCache>,
    pub pool: Arc<Pool>,
    pub dispatcher: Arc<Dispatcher>,
    pub sweeper: Arc<Sweeper>,
}

impl Services {
    pub fn real(cfg: Arc<Config>, sdk: &aws_config::SdkConfig) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let params = Arc::new(SdkParamStore::new(sdk));
        let secrets = Arc::new(SecretsCache::new(
            params.clone(),
            Arc::new(SdkSecretStore::new(sdk)),
            clock.clone(),
            cfg.param_name.clone(),
            cfg.app_secret_arn.clone(),
        ));
        let github: Arc<dyn GithubApi> = Arc::new(GithubClient::new(
            cfg.gh_api.clone(),
            secrets.clone(),
            clock.clone(),
        ));
        let fleet = Arc::new(Fleet::new(
            Arc::new(SdkMicrovmApi::new(sdk)),
            cfg.clone(),
            clock.clone(),
        ));
        let mailbox = Arc::new(Mailbox::new(
            params,
            cfg.handoff_prefix.clone(),
            clock.clone(),
        ));
        let ledger = Arc::new(ResumeLedger::new(clock.clone()));
        Self::wire(cfg, clock, secrets, github, fleet, mailbox, ledger)
    }

    /// Assemble the domain services around pre-built leaves (the test harness
    /// builds them over fakes and keeps its own handles).
    pub fn wire(
        cfg: Arc<Config>,
        clock: Arc<dyn Clock>,
        secrets: Arc<SecretsCache>,
        github: Arc<dyn GithubApi>,
        fleet: Arc<Fleet>,
        mailbox: Arc<Mailbox>,
        ledger: Arc<ResumeLedger>,
    ) -> Self {
        let policy = PoolPolicy {
            enabled: cfg.pool_enabled,
            max_size: cfg.pool_max_size,
            suspend_delay: Duration::from_secs(cfg.suspend_delay.max(0) as u64),
            handoff_window: Duration::from_secs(cfg.handoff_window.max(0) as u64),
            eol_threshold_secs: cfg.eol_threshold(),
        };
        let pool = Arc::new(Pool::new(
            fleet.clone(),
            mailbox.clone(),
            github.clone(),
            ledger.clone(),
            clock.clone(),
            policy,
        ));
        let dispatcher = Arc::new(Dispatcher::new(
            fleet.clone(),
            pool.clone(),
            github.clone(),
            cfg.clone(),
        ));
        let sweeper = Arc::new(Sweeper::new(
            github,
            fleet,
            mailbox,
            dispatcher.clone(),
            ledger,
            clock,
            cfg.clone(),
        ));
        Self {
            cfg,
            secrets,
            pool,
            dispatcher,
            sweeper,
        }
    }
}
