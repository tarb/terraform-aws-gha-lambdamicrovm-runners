//! AWS control-plane seam: terminate-self and the SSM handoff mailbox.
//!
//! One trait per external system so the pool/terminate logic is testable
//! with hand-rolled fakes. Per-call timeouts live at the call sites
//! (20 s terminate, 15 s SSM).

use async_trait::async_trait;

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct CloudError(pub String);

#[async_trait]
pub trait CloudControl: Send + Sync {
    async fn terminate_microvm(&self, id: &str) -> Result<(), CloudError>;
    /// `Ok(None)` means ParameterNotFound — silent in the mailbox polling
    /// path. `Err` is any other failure (logged by the caller).
    async fn get_parameter(&self, name: &str) -> Result<Option<String>, CloudError>;
    async fn delete_parameter(&self, name: &str) -> Result<(), CloudError>;
}

struct Clients {
    mv: aws_sdk_lambdamicrovms::Client,
    ssm: aws_sdk_ssm::Client,
}

/// SDK-backed implementation. Clients are built lazily on first use so
/// startup — and the snapshot point — does no AWS work.
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

#[async_trait]
impl CloudControl for RealAws {
    async fn terminate_microvm(&self, id: &str) -> Result<(), CloudError> {
        let c = self.clients().await;
        c.mv.terminate_microvm()
            .microvm_identifier(id)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| {
                CloudError(aws_sdk_lambdamicrovms::error::DisplayErrorContext(&e).to_string())
            })
    }

    async fn get_parameter(&self, name: &str) -> Result<Option<String>, CloudError> {
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
            Err(e) => Err(CloudError(
                aws_sdk_ssm::error::DisplayErrorContext(&e).to_string(),
            )),
        }
    }

    async fn delete_parameter(&self, name: &str) -> Result<(), CloudError> {
        let c = self.clients().await;
        c.ssm
            .delete_parameter()
            .name(name)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| CloudError(aws_sdk_ssm::error::DisplayErrorContext(&e).to_string()))
    }
}

#[cfg(test)]
pub mod testsupport {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Scripted SSM parameters plus recorded calls.
    #[derive(Default)]
    pub struct FakeCloud {
        /// name -> scripted get_parameter result.
        pub params: Mutex<HashMap<String, Result<Option<String>, CloudError>>>,
        pub gets: Mutex<Vec<String>>,
        pub deletes: Mutex<Vec<String>>,
        pub terminated: Mutex<Vec<String>>,
        /// Number of terminate calls that fail before one succeeds.
        pub terminate_failures: Mutex<u32>,
    }

    #[async_trait]
    impl CloudControl for FakeCloud {
        async fn terminate_microvm(&self, id: &str) -> Result<(), CloudError> {
            let mut left = self.terminate_failures.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(CloudError("throttled".to_string()));
            }
            self.terminated.lock().unwrap().push(id.to_string());
            Ok(())
        }

        async fn get_parameter(&self, name: &str) -> Result<Option<String>, CloudError> {
            self.gets.lock().unwrap().push(name.to_string());
            self.params
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .unwrap_or(Ok(None))
        }

        async fn delete_parameter(&self, name: &str) -> Result<(), CloudError> {
            self.deletes.lock().unwrap().push(name.to_string());
            Ok(())
        }
    }
}
