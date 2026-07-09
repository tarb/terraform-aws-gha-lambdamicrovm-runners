//! AWS control-plane calls behind a trait seam so pool/terminate logic is
//! testable with hand-rolled fakes.
//!
//! Where the Python shelled out to the AWS CLI, this uses the typed SDKs
//! directly: `aws-sdk-lambdamicrovms` TerminateMicrovm and `aws-sdk-ssm`
//! GetParameter(with_decryption)/DeleteParameter. Per-call timeouts mirror
//! the Python subprocess timeouts (20s terminate, 15s SSM).

use crate::payload::Payload;
use crate::util::{log, truncate_chars};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait AwsApi: Send + Sync {
    fn terminate_microvm<'a>(&'a self, microvm_id: &'a str) -> BoxFut<'a, Result<(), String>>;
    /// `Ok(None)` means ParameterNotFound (silent in the polling path);
    /// `Err` is any other failure (logged).
    fn ssm_get_parameter<'a>(&'a self, name: &'a str)
    -> BoxFut<'a, Result<Option<String>, String>>;
    fn ssm_delete_parameter<'a>(&'a self, name: &'a str) -> BoxFut<'a, Result<(), String>>;
}

struct Clients {
    mv: aws_sdk_lambdamicrovms::Client,
    ssm: aws_sdk_ssm::Client,
}

/// Real SDK-backed implementation; clients are built lazily on first use so
/// startup (and the snapshot point) does no AWS work — the Python only ever
/// invoked the CLI lazily, too.
pub struct RealAws {
    region: String,
    clients: tokio::sync::OnceCell<Clients>,
}

impl RealAws {
    pub fn new(region: String) -> Self {
        Self {
            region,
            clients: tokio::sync::OnceCell::new(),
        }
    }

    async fn clients(&self) -> &Clients {
        self.clients
            .get_or_init(|| async {
                let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .region(aws_config::Region::new(self.region.clone()))
                    .load()
                    .await;
                Clients {
                    mv: aws_sdk_lambdamicrovms::Client::new(&conf),
                    ssm: aws_sdk_ssm::Client::new(&conf),
                }
            })
            .await
    }
}

impl AwsApi for RealAws {
    fn terminate_microvm<'a>(&'a self, microvm_id: &'a str) -> BoxFut<'a, Result<(), String>> {
        Box::pin(async move {
            let c = self.clients().await;
            c.mv.terminate_microvm()
                .microvm_identifier(microvm_id)
                .send()
                .await
                .map(|_| ())
                .map_err(|e| format!("{}", aws_sdk_lambdamicrovms::error::DisplayErrorContext(&e)))
        })
    }

    fn ssm_get_parameter<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFut<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            let c = self.clients().await;
            match c
                .ssm
                .get_parameter()
                .name(name)
                .with_decryption(true)
                .send()
                .await
            {
                Ok(out) => Ok(out.parameter().and_then(|p| p.value()).map(str::to_string)),
                Err(aws_sdk_ssm::error::SdkError::ServiceError(se))
                    if se.err().is_parameter_not_found() =>
                {
                    Ok(None)
                }
                Err(e) => Err(format!("{}", aws_sdk_ssm::error::DisplayErrorContext(&e))),
            }
        })
    }

    fn ssm_delete_parameter<'a>(&'a self, name: &'a str) -> BoxFut<'a, Result<(), String>> {
        Box::pin(async move {
            let c = self.clients().await;
            c.ssm
                .delete_parameter()
                .name(name)
                .send()
                .await
                .map(|_| ())
                .map_err(|e| format!("{}", aws_sdk_ssm::error::DisplayErrorContext(&e)))
        })
    }
}

/// Port of `terminate_self`: terminate THIS MicroVM now so billing stops
/// immediately. LOUD + retried (3 attempts, 2s/4s/6s backoff) — a silently
/// failing call leaves the VM billing until max-duration.
pub async fn terminate_self(aws: &dyn AwsApi, payload: &Payload, region: &str) {
    let Some(mvid) = payload.microvm_id() else {
        log("no microvmId in /run payload; relying on max-duration backstop");
        return;
    };
    log(format!(
        "job done - self-terminating microvm {mvid} (region {region})"
    ));
    for attempt in 0u32..3 {
        match tokio::time::timeout(Duration::from_secs(20), aws.terminate_microvm(&mvid)).await {
            Ok(Ok(())) => {
                log("self-terminate accepted - teardown imminent");
                return;
            }
            Ok(Err(e)) => log(format!(
                "self-terminate attempt {} raised: {}",
                attempt + 1,
                truncate_chars(&e, 300)
            )),
            Err(_) => log(format!(
                "self-terminate attempt {} raised: timed out after 20s",
                attempt + 1
            )),
        }
        // Python sleeps after every failed attempt, including the last.
        tokio::time::sleep(Duration::from_secs(2 * (u64::from(attempt) + 1))).await;
    }
    log("self-terminate FAILED after 3 attempts - sweep reaper / max-duration will reap");
}

/// Port of `_claim_handoff`: fetch + DELETE this VM's parked handoff payload,
/// if any. The delete IS the ack — the dispatcher polls for disappearance.
pub async fn claim_handoff(aws: &dyn AwsApi, payload: &Payload) -> Option<Value> {
    let prefix = match payload.get("handoff_prefix") {
        Some(Value::String(s)) => s.trim_end_matches('/').to_string(),
        _ => String::new(),
    };
    let mvid = payload.microvm_id();
    let (prefix, mvid) = match (prefix.is_empty(), mvid) {
        (false, Some(m)) => (prefix, m),
        _ => return None,
    };
    let name = format!("{prefix}/{mvid}");
    let text =
        match tokio::time::timeout(Duration::from_secs(15), aws.ssm_get_parameter(&name)).await {
            Err(_) => {
                log("handoff: get raised: timed out after 15s");
                return None;
            }
            Ok(Err(e)) => {
                log(format!("handoff: get failed: {}", truncate_chars(&e, 200)));
                return None;
            }
            Ok(Ok(None)) => return None, // ParameterNotFound: silent, next tick retries
            Ok(Ok(Some(text))) => text,
        };
    let parsed: Value = match serde_json::from_str(text.trim()) {
        Ok(v) => v,
        Err(e) => {
            log(format!("handoff: get raised: {e}"));
            return None;
        }
    };
    // Best-effort delete-as-ack; Python ignores the delete result entirely.
    let _ = tokio::time::timeout(Duration::from_secs(15), aws.ssm_delete_parameter(&name)).await;
    log("handoff: claimed parked run payload");
    Some(parsed)
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Hand-rolled fake: scripted SSM parameters and recorded calls.
    #[derive(Default)]
    pub struct FakeAws {
        /// name -> scripted result for get_parameter.
        pub params: Mutex<HashMap<String, Result<Option<String>, String>>>,
        pub gets: Mutex<Vec<String>>,
        pub deletes: Mutex<Vec<String>>,
        pub terminated: Mutex<Vec<String>>,
        /// Number of terminate calls that fail before one succeeds.
        pub terminate_failures: Mutex<u32>,
    }

    impl AwsApi for FakeAws {
        fn terminate_microvm<'a>(&'a self, id: &'a str) -> BoxFut<'a, Result<(), String>> {
            Box::pin(async move {
                let mut left = self.terminate_failures.lock().unwrap();
                if *left > 0 {
                    *left -= 1;
                    return Err("throttled".to_string());
                }
                self.terminated.lock().unwrap().push(id.to_string());
                Ok(())
            })
        }

        fn ssm_get_parameter<'a>(
            &'a self,
            name: &'a str,
        ) -> BoxFut<'a, Result<Option<String>, String>> {
            Box::pin(async move {
                self.gets.lock().unwrap().push(name.to_string());
                self.params
                    .lock()
                    .unwrap()
                    .get(name)
                    .cloned()
                    .unwrap_or(Ok(None))
            })
        }

        fn ssm_delete_parameter<'a>(&'a self, name: &'a str) -> BoxFut<'a, Result<(), String>> {
            Box::pin(async move {
                self.deletes.lock().unwrap().push(name.to_string());
                Ok(())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::FakeAws;
    use super::*;
    use crate::payload::Payload;
    use serde_json::json;

    fn pool_payload() -> Payload {
        Payload::from_value(json!({
            "handoff_prefix": "/gha-microvm/handoff/",
            "microvmId": "microvm-abc123",
        }))
    }

    #[tokio::test(start_paused = true)]
    async fn claim_fetches_deletes_and_returns_payload() {
        let fake = FakeAws::default();
        fake.params.lock().unwrap().insert(
            "/gha-microvm/handoff/microvm-abc123".to_string(),
            Ok(Some(
                "{\"github_url\": \"u\", \"token\": \"t\"}".to_string(),
            )),
        );
        let got = claim_handoff(&fake, &pool_payload()).await.unwrap();
        assert_eq!(got["github_url"], "u");
        // Trailing slash on the prefix is stripped before joining.
        assert_eq!(
            fake.deletes.lock().unwrap().as_slice(),
            ["/gha-microvm/handoff/microvm-abc123"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn claim_returns_none_when_parameter_missing() {
        let fake = FakeAws::default();
        assert!(claim_handoff(&fake, &pool_payload()).await.is_none());
        assert!(fake.deletes.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn claim_skips_when_prefix_or_id_absent() {
        let fake = FakeAws::default();
        let no_prefix = Payload::from_value(json!({"microvmId": "microvm-1"}));
        let no_id = Payload::from_value(json!({"handoff_prefix": "/p"}));
        assert!(claim_handoff(&fake, &no_prefix).await.is_none());
        assert!(claim_handoff(&fake, &no_id).await.is_none());
        assert!(fake.gets.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn claim_tolerates_unparseable_mailbox_value() {
        let fake = FakeAws::default();
        fake.params.lock().unwrap().insert(
            "/gha-microvm/handoff/microvm-abc123".to_string(),
            Ok(Some("not json".to_string())),
        );
        assert!(claim_handoff(&fake, &pool_payload()).await.is_none());
        // Parse failure happens before the delete-as-ack.
        assert!(fake.deletes.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn terminate_self_retries_then_succeeds() {
        let fake = FakeAws::default();
        *fake.terminate_failures.lock().unwrap() = 2;
        terminate_self(&fake, &pool_payload(), "us-east-1").await;
        assert_eq!(
            fake.terminated.lock().unwrap().as_slice(),
            ["microvm-abc123"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn terminate_self_without_id_is_a_noop() {
        let fake = FakeAws::default();
        terminate_self(&fake, &Payload::default(), "us-east-1").await;
        assert!(fake.terminated.lock().unwrap().is_empty());
    }
}
