//! The SSM handoff mailbox — the dispatcher⇄VM job-handoff protocol.
//!
//! Parameter name: `{HANDOFF_PREFIX}/{microvmId}`, SecureString (the value
//! carries a GitHub installation token). Protocol: the dispatcher parks the
//! payload BEFORE resuming; the woken VM polls its parameter (≤5 s tick) and
//! DELETES it to claim; the dispatcher polls for disappearance as the ack.

use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use types::{MicrovmId, RunPayload};

use crate::aws::params::ParamStore;
use crate::aws::{AwsApiError, ignore_service};
use crate::clock::Clock;
use crate::oplog;

pub struct Mailbox {
    store: Arc<dyn ParamStore>,
    prefix: String,
    clock: Arc<dyn Clock>,
}

pub enum ClaimStatus {
    Claimed { seconds: f64 },
    Unclaimed,
}

impl Mailbox {
    pub fn new(store: Arc<dyn ParamStore>, prefix: String, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            prefix,
            clock,
        }
    }

    pub fn address(&self, id: &MicrovmId) -> String {
        format!("{}/{}", self.prefix, id)
    }

    /// Park the payload for one VM. Callers park BEFORE resume — the
    /// money-risky step must come after the mailbox is loaded.
    pub async fn park(&self, id: &MicrovmId, payload: &RunPayload) -> Result<Parked, AwsApiError> {
        let name = self.address(id);
        let value =
            serde_json::to_string(&payload.addressed_to(id)).expect("RunPayload always serializes");
        self.store.put_secure(&name, &value).await?;
        Ok(Parked {
            name,
            store: Arc::clone(&self.store),
            clock: Arc::clone(&self.clock),
        })
    }

    /// Sweep GC: delete parameters under the prefix older than `older_than`
    /// — written but never claimed AND never cleaned. They must not
    /// accumulate or shadow a future VM id.
    pub async fn gc(&self, older_than: Duration) -> Result<(), AwsApiError> {
        let now = self.clock.now();
        for param in self.store.list_by_path(&self.prefix).await? {
            let Some(modified) = param.last_modified else {
                continue;
            };
            if now.since(modified) > older_than.as_secs_f64() {
                oplog::emit(json!({"sweep": "gc-handoff-param", "name": param.name}));
                ignore_service(self.store.delete(&param.name).await)?;
            }
        }
        Ok(())
    }
}

/// A parked payload awaiting its claim.
pub struct Parked {
    name: String,
    store: Arc<dyn ParamStore>,
    clock: Arc<dyn Clock>,
}

impl Parked {
    /// Poll (3 s tick, no decrypt) until the VM DELETES the parameter
    /// (ParameterNotFound == claimed) or `window` elapses. Service errors
    /// other than not-found keep polling; transport errors propagate. Each
    /// `get` is bounded by the SDK operation timeout configured in `main` —
    /// a hung read cannot pin this loop to the Lambda deadline.
    pub async fn await_claim(&self, window: Duration) -> Result<ClaimStatus, AwsApiError> {
        let start = self.clock.now();
        while self.clock.now().since(start) < window.as_secs_f64() {
            match self.store.get(&self.name, false).await {
                Err(e) if e.is_code("ParameterNotFound") => {
                    return Ok(ClaimStatus::Claimed {
                        seconds: self.clock.now().since(start),
                    });
                }
                Err(e) if !e.is_service() => return Err(e),
                // Present, or a transient read error — keep polling.
                _ => {}
            }
            self.clock.sleep(Duration::from_secs(3)).await;
        }
        Ok(ClaimStatus::Unclaimed)
    }

    /// Best-effort delete (unclaimed / resume-failed cleanup).
    pub async fn cancel(&self) -> Result<(), AwsApiError> {
        ignore_service(self.store.delete(&self.name).await)
    }
}
