//! SSM Parameter Store seam (secret bundle + handoff mailbox).

use async_trait::async_trait;

use crate::aws::{AwsApiError, map_sdk_err};
use crate::clock::Epoch;

#[derive(Debug, Clone)]
pub struct ParamMeta {
    pub name: String,
    pub last_modified: Option<Epoch>,
}

#[async_trait]
pub trait ParamStore: Send + Sync {
    async fn get(&self, name: &str, decrypt: bool) -> Result<String, AwsApiError>;
    async fn put_secure(&self, name: &str, value: &str) -> Result<(), AwsApiError>;
    async fn delete(&self, name: &str) -> Result<(), AwsApiError>;
    /// Single non-paginated GetParametersByPath call (the mailbox holds at
    /// most a handful of parameters).
    async fn list_by_path(&self, path: &str) -> Result<Vec<ParamMeta>, AwsApiError>;
}

pub struct SdkParamStore {
    client: aws_sdk_ssm::Client,
}

impl SdkParamStore {
    pub fn new(shared: &aws_config::SdkConfig) -> Self {
        Self {
            client: aws_sdk_ssm::Client::new(shared),
        }
    }
}

#[async_trait]
impl ParamStore for SdkParamStore {
    async fn get(&self, name: &str, decrypt: bool) -> Result<String, AwsApiError> {
        let out = self
            .client
            .get_parameter()
            .name(name)
            .with_decryption(decrypt)
            .send()
            .await
            .map_err(|e| map_sdk_err("GetParameter", e))?;
        Ok(out
            .parameter()
            .and_then(|p| p.value())
            .unwrap_or_default()
            .to_string())
    }

    async fn put_secure(&self, name: &str, value: &str) -> Result<(), AwsApiError> {
        self.client
            .put_parameter()
            .name(name)
            .value(value)
            .r#type(aws_sdk_ssm::types::ParameterType::SecureString)
            .overwrite(true)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| map_sdk_err("PutParameter", e))
    }

    async fn delete(&self, name: &str) -> Result<(), AwsApiError> {
        self.client
            .delete_parameter()
            .name(name)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| map_sdk_err("DeleteParameter", e))
    }

    async fn list_by_path(&self, path: &str) -> Result<Vec<ParamMeta>, AwsApiError> {
        let out = self
            .client
            .get_parameters_by_path()
            .path(path)
            .recursive(true)
            .send()
            .await
            .map_err(|e| map_sdk_err("GetParametersByPath", e))?;
        Ok(out
            .parameters()
            .iter()
            .filter_map(|p| {
                Some(ParamMeta {
                    name: p.name()?.to_string(),
                    last_modified: p.last_modified_date().map(|d| Epoch(d.as_secs_f64())),
                })
            })
            .collect())
    }
}
