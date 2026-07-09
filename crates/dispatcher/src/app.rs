//! Shared per-container state, mirroring the Python module globals.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

use crate::config::Config;
use crate::platform::Platform;

/// Warm-container caches (the Python module-level `_secret`, `_tok_cache`,
/// `_img` dicts and the `_list_vms.logged` flag).
#[derive(Default)]
pub struct Caches {
    /// `_secret`: parsed secret bundle + fetch timestamp.
    pub secret: Mutex<Option<(Value, i64)>>,
    /// `_tok_cache`: installation_id -> (token, epoch_expiry).
    pub tok: Mutex<HashMap<i64, (String, i64)>>,
    /// `_img`: resolved latest ACTIVE image version + fetch timestamp.
    pub img: Mutex<Option<(String, i64)>>,
    /// `_list_vms.logged`: one-shot record-keys canary.
    pub vm_keys_logged: AtomicBool,
}

/// `_recently_resumed`: VM ids this container resumed recently; the sweep's
/// zombie reaper skips them. Per-container memory, so a process-wide static.
pub fn recently_resumed() -> &'static Mutex<HashMap<String, f64>> {
    static MAP: OnceLock<Mutex<HashMap<String, f64>>> = OnceLock::new();
    MAP.get_or_init(Default::default)
}

/// Everything a handler invocation needs: platform seam, config, caches.
pub struct App<'a, P> {
    pub p: &'a P,
    pub cfg: &'a Config,
    pub caches: &'a Caches,
}

impl<'a, P: Platform> App<'a, P> {
    pub fn new(p: &'a P, cfg: &'a Config, caches: &'a Caches) -> Self {
        Self { p, cfg, caches }
    }
}
