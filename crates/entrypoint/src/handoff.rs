//! The SSM handoff mailbox, VM side: the dispatcher parks a run payload at
//! `{handoff_prefix}/{microvmId}` BEFORE resuming this VM; the VM polls its
//! parameter and DELETES it to claim — the delete IS the ack the dispatcher
//! watches for.

use crate::aws::CloudControl;
use crate::logfmt::{log, truncate_chars};
use crate::payload::RunConfig;
use serde_json::Value;
use std::time::Duration;

/// This VM's mailbox parameter name.
pub struct HandoffAddress(String);

impl HandoffAddress {
    /// Present only when the run carries both a handoff prefix and a
    /// microVM id — otherwise there is no mailbox to poll.
    pub fn from_run(cfg: &RunConfig) -> Option<Self> {
        let prefix = cfg.handoff_prefix.as_deref()?;
        let id = cfg.microvm_id.as_ref()?;
        Some(Self(format!("{prefix}/{id}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Fetch + delete this VM's parked payload, if any. A missing parameter is
/// the common case (nothing parked yet) and stays silent; fetch errors are
/// logged and the poll retries next tick. A payload that doesn't parse is
/// left in place (NOT deleted) so the dispatcher's unclaimed-handoff
/// recovery can run.
pub async fn claim(aws: &dyn CloudControl, addr: &HandoffAddress) -> Option<Value> {
    let name = addr.as_str();
    let text = match tokio::time::timeout(Duration::from_secs(15), aws.get_parameter(name)).await {
        Err(_) => {
            log("handoff: get raised: timed out after 15s");
            return None;
        }
        Ok(Err(e)) => {
            log(format!(
                "handoff: get failed: {}",
                truncate_chars(&e.to_string(), 200)
            ));
            return None;
        }
        Ok(Ok(None)) => return None,
        Ok(Ok(Some(text))) => text,
    };
    let parsed: Value = match serde_json::from_str(text.trim()) {
        Ok(v) => v,
        Err(e) => {
            log(format!("handoff: unparseable mailbox payload: {e}"));
            return None;
        }
    };
    // Best-effort delete-as-ack; the run proceeds even if the delete fails
    // (the dispatcher's mailbox GC cleans up eventually).
    let _ = tokio::time::timeout(Duration::from_secs(15), aws.delete_parameter(name)).await;
    log("handoff: claimed parked run payload");
    Some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::testsupport::FakeCloud;
    use serde_json::json;

    fn pool_cfg() -> RunConfig {
        RunConfig::from_value(json!({
            "handoff_prefix": "/gha-microvm/handoff/",
            "microvmId": "microvm-abc123",
        }))
    }

    const ADDR: &str = "/gha-microvm/handoff/microvm-abc123";

    #[test]
    fn address_requires_both_prefix_and_id() {
        // Trailing slash on the prefix is stripped before joining.
        assert_eq!(
            HandoffAddress::from_run(&pool_cfg()).unwrap().as_str(),
            ADDR
        );
        let no_prefix = RunConfig::from_value(json!({"microvmId": "microvm-1"}));
        let no_id = RunConfig::from_value(json!({"handoff_prefix": "/p"}));
        assert!(HandoffAddress::from_run(&no_prefix).is_none());
        assert!(HandoffAddress::from_run(&no_id).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn claim_fetches_deletes_and_returns_payload() {
        let fake = FakeCloud::default();
        fake.params.lock().unwrap().insert(
            ADDR.to_string(),
            Ok(Some(
                "{\"github_url\": \"u\", \"token\": \"t\"}".to_string(),
            )),
        );
        let addr = HandoffAddress::from_run(&pool_cfg()).unwrap();
        let got = claim(&fake, &addr).await.unwrap();
        assert_eq!(got["github_url"], "u");
        assert_eq!(fake.deletes.lock().unwrap().as_slice(), [ADDR]);
    }

    #[tokio::test(start_paused = true)]
    async fn claim_returns_none_when_parameter_missing() {
        let fake = FakeCloud::default();
        let addr = HandoffAddress::from_run(&pool_cfg()).unwrap();
        assert!(claim(&fake, &addr).await.is_none());
        assert!(fake.deletes.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn claim_tolerates_unparseable_mailbox_value() {
        let fake = FakeCloud::default();
        fake.params
            .lock()
            .unwrap()
            .insert(ADDR.to_string(), Ok(Some("not json".to_string())));
        let addr = HandoffAddress::from_run(&pool_cfg()).unwrap();
        assert!(claim(&fake, &addr).await.is_none());
        // The parse failure happens before the delete-as-ack.
        assert!(fake.deletes.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn claim_logs_and_retries_next_tick_on_fetch_error() {
        let fake = FakeCloud::default();
        fake.params.lock().unwrap().insert(
            ADDR.to_string(),
            Err(crate::aws::CloudError("throttled".to_string())),
        );
        let addr = HandoffAddress::from_run(&pool_cfg()).unwrap();
        assert!(claim(&fake, &addr).await.is_none());
        assert!(fake.deletes.lock().unwrap().is_empty());
    }
}
