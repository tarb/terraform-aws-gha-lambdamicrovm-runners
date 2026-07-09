//! Secrets Manager seam (GitHub App credential).

use async_trait::async_trait;

use crate::aws::{AwsApiError, map_sdk_err};

#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn secret_string(&self, arn: &str) -> Result<String, AwsApiError>;
}

pub struct SdkSecretStore {
    client: aws_sdk_secretsmanager::Client,
}

impl SdkSecretStore {
    pub fn new(shared: &aws_config::SdkConfig) -> Self {
        Self {
            client: aws_sdk_secretsmanager::Client::new(shared),
        }
    }
}

#[async_trait]
impl SecretStore for SdkSecretStore {
    async fn secret_string(&self, arn: &str) -> Result<String, AwsApiError> {
        let out = self
            .client
            .get_secret_value()
            .secret_id(arn)
            .send()
            .await
            .map_err(|e| map_sdk_err("GetSecretValue", e))?;
        Ok(out.secret_string().unwrap_or_default().to_string())
    }
}
