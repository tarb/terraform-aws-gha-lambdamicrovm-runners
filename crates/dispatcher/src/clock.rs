//! Wall-clock seam: an epoch-seconds newtype plus a fakeable `Clock`.

use async_trait::async_trait;
use std::time::Duration;

/// Unix time in seconds (fractional).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Epoch(pub f64);

impl Epoch {
    /// Seconds elapsed since `earlier` (negative if `earlier` is later).
    pub fn since(self, earlier: Epoch) -> f64 {
        self.0 - earlier.0
    }
}

#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> Epoch;
    async fn sleep(&self, d: Duration);
}

pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now(&self) -> Epoch {
        Epoch(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        )
    }

    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}
