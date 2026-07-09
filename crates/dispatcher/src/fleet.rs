//! Fleet view and control: normalized VM records, capacity counting,
//! image-version resolution, launch (with the IAM-propagation retry).

use serde_json::json;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use types::{MicrovmId, RunPayload};

use crate::aws::AwsApiError;
use crate::aws::microvm::{LaunchSpec, MicrovmApi};
use crate::clock::{Clock, Epoch};
use crate::config::Config;
use crate::dispatch::DispatchError;
use crate::oplog;

/// Resolved latest-ACTIVE image version is re-checked at most this often.
const IMAGE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicrovmState {
    Pending,
    Running,
    Suspending,
    Suspended,
    Terminating,
    Terminated,
    Other(String),
}

impl MicrovmState {
    pub fn parse(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "PENDING" => Self::Pending,
            "RUNNING" => Self::Running,
            "SUSPENDING" => Self::Suspending,
            "SUSPENDED" => Self::Suspended,
            "TERMINATING" => Self::Terminating,
            "TERMINATED" => Self::Terminated,
            other => Self::Other(other.to_string()),
        }
    }

    /// Counts against the concurrency cap.
    pub fn holds_capacity(&self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }

    /// Occupies a warm-pool slot.
    pub fn pooled(&self) -> bool {
        matches!(self, Self::Suspended | Self::Suspending)
    }

    pub fn gone(&self) -> bool {
        matches!(self, Self::Terminating | Self::Terminated)
    }
}

#[derive(Debug, Clone)]
pub struct VmRecord {
    pub id: MicrovmId,
    pub state: MicrovmState,
    /// Stringified version label; `None` when absent.
    pub image_version: Option<String>,
    /// `None` mirrors an absent/unparsable `startedAt`.
    pub started_at: Option<Epoch>,
}

impl VmRecord {
    /// Seconds since launch; `None` when `started_at` is absent or zero.
    pub fn age(&self, now: Epoch) -> Option<f64> {
        self.started_at
            .filter(|born| born.0 != 0.0)
            .map(|born| now.since(born))
    }

    /// Version known and different from the current one.
    pub fn stale_image(&self, current: &str) -> bool {
        self.image_version
            .as_deref()
            .is_some_and(|v| !v.is_empty() && v != current)
    }

    /// Old enough that a resumed job could die at max-duration mid-run.
    pub fn near_eol(&self, now: Epoch, threshold_secs: f64) -> bool {
        self.age(now).is_some_and(|age| age > threshold_secs)
    }
}

pub struct Fleet {
    api: Arc<dyn MicrovmApi>,
    cfg: Arc<Config>,
    clock: Arc<dyn Clock>,
    image_cache: Mutex<Option<(String, Epoch)>>,
    /// One-shot `vm_record_keys` canary.
    keys_logged: AtomicBool,
}

impl Fleet {
    pub fn new(api: Arc<dyn MicrovmApi>, cfg: Arc<Config>, clock: Arc<dyn Clock>) -> Self {
        Self {
            api,
            cfg,
            clock,
            image_cache: Mutex::new(None),
            keys_logged: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    pub fn silence_record_keys_canary(&self) {
        self.keys_logged.store(true, Ordering::SeqCst);
    }

    /// Full pagination. On the first non-empty result ever, logs the record
    /// keys once so schema drift in the service response is visible.
    pub async fn list(&self) -> Result<Vec<VmRecord>, AwsApiError> {
        let mut vms: Vec<VmRecord> = Vec::new();
        let mut first_keys: Option<Vec<String>> = None;
        let mut token: Option<String> = None;
        loop {
            let page = self
                .api
                .list_page(&self.cfg.image_arn, token.as_deref())
                .await?;
            if first_keys.is_none() && !page.items.is_empty() {
                first_keys = Some(page.record_keys);
            }
            vms.extend(page.items);
            token = page.next;
            if token.is_none() {
                break;
            }
        }
        if !vms.is_empty() && !self.keys_logged.swap(true, Ordering::SeqCst) {
            let mut keys = first_keys.unwrap_or_default();
            keys.sort();
            oplog::emit(json!({ "vm_record_keys": keys }));
        }
        Ok(vms)
    }

    /// PENDING/RUNNING hold capacity; unknown states are logged, not counted.
    pub async fn running_count(&self) -> Result<i64, AwsApiError> {
        let mut n = 0i64;
        let mut unknown: BTreeSet<String> = BTreeSet::new();
        for vm in self.list().await? {
            if vm.state.holds_capacity() {
                n += 1;
            } else if let MicrovmState::Other(s) = &vm.state
                && !s.is_empty()
            {
                unknown.insert(s.clone());
            }
        }
        if !unknown.is_empty() {
            let states: Vec<&String> = unknown.iter().collect();
            oplog::emit(json!({
                "warn": "unknown microvm states (not counted)",
                "states": states,
            }));
        }
        Ok(n)
    }

    /// The version to launch/resume against: the env pin wins; otherwise the
    /// image's latest ACTIVE version, cached for [`IMAGE_TTL`].
    pub async fn current_image_version(&self) -> Result<String, DispatchError> {
        if let Some(pin) = &self.cfg.image_version {
            return Ok(pin.clone());
        }
        let now = self.clock.now();
        if let Some((version, at)) = self.image_cache.lock().unwrap().as_ref()
            && now.since(*at) <= IMAGE_TTL.as_secs_f64()
        {
            return Ok(version.clone());
        }
        let info = self.api.image(&self.cfg.image_arn).await?;
        let version = info
            .latest_active
            .filter(|v| !v.is_empty())
            .ok_or_else(|| DispatchError::NoActiveImage {
                arn: self.cfg.image_arn.clone(),
                state: info.state,
            })?;
        *self.image_cache.lock().unwrap() = Some((version.clone(), now));
        Ok(version)
    }

    /// Cold launch. IAM PassRole propagation can briefly deny RunMicrovm
    /// right after a policy edit: retry `AccessDeniedException` up to 3 times
    /// (4 attempts total) with 1.5 s sleeps.
    pub async fn launch(&self, payload: &RunPayload) -> Result<MicrovmId, DispatchError> {
        let image_version = self.current_image_version().await?;
        let run_hook_payload =
            serde_json::to_string(payload).expect("RunPayload always serializes");
        let mut retries = 0;
        loop {
            let spec = LaunchSpec {
                image_arn: &self.cfg.image_arn,
                image_version: &image_version,
                exec_role_arn: &self.cfg.exec_role_arn,
                egress: &self.cfg.egress,
                max_duration_secs: self.cfg.max_duration,
                run_hook_payload: run_hook_payload.clone(),
                log_group: &self.cfg.log_group,
            };
            match self.api.run(&spec).await {
                Ok(id) => return Ok(id),
                Err(e) if e.is_code("AccessDeniedException") && retries < 3 => {
                    self.clock.sleep(Duration::from_millis(1500)).await;
                    retries += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub async fn state_of(&self, id: &MicrovmId) -> Result<MicrovmState, AwsApiError> {
        self.api.state(id).await
    }

    pub async fn resume(&self, id: &MicrovmId) -> Result<(), AwsApiError> {
        self.api.resume(id).await
    }

    pub async fn suspend(&self, id: &MicrovmId) -> Result<(), AwsApiError> {
        self.api.suspend(id).await
    }

    pub async fn terminate(&self, id: &MicrovmId) -> Result<(), AwsApiError> {
        self.api.terminate(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_capacity_counts_only_pending_and_running() {
        let table = [
            ("PENDING", true),
            ("RUNNING", true),
            ("SUSPENDING", false),
            ("SUSPENDED", false),
            ("TERMINATING", false),
            ("TERMINATED", false),
            ("SOMETHING_NEW", false),
        ];
        for (state, expected) in table {
            assert_eq!(
                MicrovmState::parse(state).holds_capacity(),
                expected,
                "{state}"
            );
        }
    }

    #[test]
    fn state_parse_uppercases_and_keeps_unknowns() {
        assert_eq!(MicrovmState::parse("running"), MicrovmState::Running);
        assert_eq!(
            MicrovmState::parse("weird"),
            MicrovmState::Other("WEIRD".into())
        );
    }

    #[test]
    fn record_age_and_staleness() {
        let now = Epoch(1_000_000.0);
        let vm = VmRecord {
            id: MicrovmId::new("microvm-x"),
            state: MicrovmState::Suspended,
            image_version: Some("8".into()),
            started_at: Some(Epoch(999_000.0)),
        };
        assert_eq!(vm.age(now), Some(1000.0));
        assert!(vm.stale_image("9"));
        assert!(!vm.stale_image("8"));
        assert!(vm.near_eol(now, 600.0));
        assert!(!vm.near_eol(now, 1000.0));

        let unborn = VmRecord {
            started_at: Some(Epoch(0.0)),
            ..vm.clone()
        };
        assert_eq!(unborn.age(now), None, "startedAt == 0 means unknown");
        assert!(!unborn.near_eol(now, 0.0));
    }
}
