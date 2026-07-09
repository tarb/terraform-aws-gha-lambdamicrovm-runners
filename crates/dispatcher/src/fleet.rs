//! Fleet view: ListMicrovms normalization, capacity counting, image-version
//! resolution.

use serde_json::json;

use crate::app::App;
use crate::config::{ACTIVE_STATES, IMG_TTL};
use crate::platform::Platform;
use crate::pyfmt::{PyErr, logln};

/// One normalized fleet record (the Python `_list_vms` dict).
#[derive(Debug, Clone)]
pub struct Vm {
    /// May be empty when the record lacked a microvmId.
    pub id: String,
    /// Uppercased; empty when absent.
    pub state: String,
    /// Stringified; empty when absent.
    pub image_version: String,
    /// Epoch seconds; `None` mirrors an unparsable/absent `startedAt`.
    pub started_at: Option<f64>,
}

impl Vm {
    pub fn is_pooled_state(&self) -> bool {
        self.state == "SUSPENDED" || self.state == "SUSPENDING"
    }
}

impl<P: Platform> App<'_, P> {
    /// `_list_vms()`: full pagination + normalization, with the one-shot
    /// record-keys canary log.
    pub async fn list_vms(&self) -> Result<Vec<Vm>, PyErr> {
        let mut vms: Vec<Vm> = Vec::new();
        let mut first_raw_keys: Option<Vec<String>> = None;
        let mut token: Option<String> = None;
        loop {
            let page = self
                .p
                .mv_list_page(&self.cfg.image_arn, token.as_deref())
                .await
                .map_err(PyErr::from)?;
            for m in page.items {
                if first_raw_keys.is_none() {
                    first_raw_keys = Some(m.raw_keys.clone());
                }
                vms.push(Vm {
                    id: m.microvm_id.unwrap_or_default(),
                    state: m.state.unwrap_or_default().to_uppercase(),
                    image_version: m.image_version.unwrap_or_default(),
                    started_at: m.started_at,
                });
            }
            token = page.next_token;
            if token.is_none() {
                break;
            }
        }
        if !vms.is_empty()
            && !self
                .caches
                .vm_keys_logged
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let mut keys = first_raw_keys.unwrap_or_default();
            keys.sort();
            logln(&json!({"vm_record_keys": keys}));
        }
        Ok(vms)
    }

    /// `_running_count()`: PENDING/RUNNING hold capacity; unknown states are
    /// logged, not counted.
    pub async fn running_count(&self) -> Result<i64, PyErr> {
        let mut n = 0i64;
        let mut unknown: std::collections::BTreeSet<String> = Default::default();
        for v in self.list_vms().await? {
            if ACTIVE_STATES.contains(&v.state.as_str()) {
                n += 1;
            } else if !matches!(
                v.state.as_str(),
                "SUSPENDING" | "SUSPENDED" | "TERMINATING" | "TERMINATED" | ""
            ) {
                unknown.insert(v.state);
            }
        }
        if !unknown.is_empty() {
            let states: Vec<&String> = unknown.iter().collect();
            logln(&json!({"warn": "unknown microvm states (not counted)", "states": states}));
        }
        Ok(n)
    }

    /// `_image_version()`: env pin wins; otherwise the image's latest ACTIVE
    /// version, cached for IMG_TTL seconds.
    pub async fn image_version(&self) -> Result<String, PyErr> {
        if let Some(v) = &self.cfg.image_version {
            return Ok(v.clone());
        }
        let now = self.p.now() as i64;
        if let Some((version, ts)) = self.caches.img.lock().unwrap().as_ref()
            && now - ts <= IMG_TTL
        {
            return Ok(version.clone());
        }
        let img = self
            .p
            .mv_get_image(&self.cfg.image_arn)
            .await
            .map_err(PyErr::from)?;
        let version = img
            .latest_active_image_version
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                PyErr::runtime(format!(
                    "image {} has no ACTIVE version (state {})",
                    self.cfg.image_arn,
                    img.state.as_deref().unwrap_or("None")
                ))
            })?;
        *self.caches.img.lock().unwrap() = Some((version.clone(), now));
        Ok(version)
    }
}
