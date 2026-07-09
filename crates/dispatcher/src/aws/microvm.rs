//! The lambda-microvms control plane, typed but 1:1 with API calls so retry
//! decisions stay visible in the services that own them.

use async_trait::async_trait;
use types::MicrovmId;

use crate::aws::{AwsApiError, map_sdk_err};
use crate::clock::Epoch;
use crate::fleet::{MicrovmState, VmRecord};

/// One ListMicrovms page.
#[derive(Debug, Clone, Default)]
pub struct VmPage {
    pub items: Vec<VmRecord>,
    pub next: Option<String>,
    /// Response record keys (for the one-shot `vm_record_keys` canary).
    pub record_keys: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ImageInfo {
    pub latest_active: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LaunchSpec<'a> {
    pub image_arn: &'a str,
    pub image_version: &'a str,
    pub exec_role_arn: &'a str,
    pub egress: &'a str,
    pub max_duration_secs: i64,
    /// Serialized `types::RunPayload`.
    pub run_hook_payload: String,
    pub log_group: &'a str,
}

#[async_trait]
pub trait MicrovmApi: Send + Sync {
    async fn list_page(&self, image_arn: &str, token: Option<&str>) -> Result<VmPage, AwsApiError>;
    async fn state(&self, id: &MicrovmId) -> Result<MicrovmState, AwsApiError>;
    async fn image(&self, image_arn: &str) -> Result<ImageInfo, AwsApiError>;
    async fn run(&self, spec: &LaunchSpec<'_>) -> Result<MicrovmId, AwsApiError>;
    async fn resume(&self, id: &MicrovmId) -> Result<(), AwsApiError>;
    async fn suspend(&self, id: &MicrovmId) -> Result<(), AwsApiError>;
    async fn terminate(&self, id: &MicrovmId) -> Result<(), AwsApiError>;
}

pub struct SdkMicrovmApi {
    client: aws_sdk_lambdamicrovms::Client,
}

impl SdkMicrovmApi {
    pub fn new(shared: &aws_config::SdkConfig) -> Self {
        Self {
            client: aws_sdk_lambdamicrovms::Client::new(shared),
        }
    }
}

/// The response record keys as the canary log reports them.
const RECORD_KEYS: [&str; 5] = [
    "imageArn",
    "imageVersion",
    "microvmId",
    "startedAt",
    "state",
];

#[async_trait]
impl MicrovmApi for SdkMicrovmApi {
    async fn list_page(&self, image_arn: &str, token: Option<&str>) -> Result<VmPage, AwsApiError> {
        let out = self
            .client
            .list_microvms()
            .image_identifier(image_arn)
            .set_next_token(token.map(str::to_string))
            .send()
            .await
            .map_err(|e| map_sdk_err("ListMicrovms", e))?;
        let items = out
            .items()
            .iter()
            .map(|m| VmRecord {
                id: MicrovmId::new(m.microvm_id()),
                state: MicrovmState::parse(m.state().as_str()),
                image_version: Some(m.image_version().to_string()).filter(|v| !v.is_empty()),
                started_at: Some(Epoch(m.started_at().as_secs_f64())),
            })
            .collect();
        Ok(VmPage {
            items,
            next: out.next_token().map(str::to_string),
            record_keys: RECORD_KEYS.iter().map(|s| s.to_string()).collect(),
        })
    }

    async fn state(&self, id: &MicrovmId) -> Result<MicrovmState, AwsApiError> {
        let out = self
            .client
            .get_microvm()
            .microvm_identifier(id.as_str())
            .send()
            .await
            .map_err(|e| map_sdk_err("GetMicrovm", e))?;
        Ok(MicrovmState::parse(out.state().as_str()))
    }

    async fn image(&self, image_arn: &str) -> Result<ImageInfo, AwsApiError> {
        let out = self
            .client
            .get_microvm_image()
            .image_identifier(image_arn)
            .send()
            .await
            .map_err(|e| map_sdk_err("GetMicrovmImage", e))?;
        Ok(ImageInfo {
            latest_active: out.latest_active_image_version().map(str::to_string),
            state: Some(out.state().as_str().to_string()),
        })
    }

    async fn run(&self, spec: &LaunchSpec<'_>) -> Result<MicrovmId, AwsApiError> {
        let logging = aws_sdk_lambdamicrovms::types::Logging::CloudWatch(
            aws_sdk_lambdamicrovms::types::CloudWatchLogging::builder()
                .log_group(spec.log_group)
                .build(),
        );
        let out = self
            .client
            .run_microvm()
            .image_identifier(spec.image_arn)
            .image_version(spec.image_version)
            .execution_role_arn(spec.exec_role_arn)
            .egress_network_connectors(spec.egress)
            .maximum_duration_in_seconds(spec.max_duration_secs as i32)
            .run_hook_payload(&spec.run_hook_payload)
            .logging(logging)
            .send()
            .await
            .map_err(|e| map_sdk_err("RunMicrovm", e))?;
        Ok(MicrovmId::new(out.microvm_id()))
    }

    async fn resume(&self, id: &MicrovmId) -> Result<(), AwsApiError> {
        self.client
            .resume_microvm()
            .microvm_identifier(id.as_str())
            .send()
            .await
            .map(|_| ())
            .map_err(|e| map_sdk_err("ResumeMicrovm", e))
    }

    async fn suspend(&self, id: &MicrovmId) -> Result<(), AwsApiError> {
        self.client
            .suspend_microvm()
            .microvm_identifier(id.as_str())
            .send()
            .await
            .map(|_| ())
            .map_err(|e| map_sdk_err("SuspendMicrovm", e))
    }

    async fn terminate(&self, id: &MicrovmId) -> Result<(), AwsApiError> {
        self.client
            .terminate_microvm()
            .microvm_identifier(id.as_str())
            .send()
            .await
            .map(|_| ())
            .map_err(|e| map_sdk_err("TerminateMicrovm", e))
    }
}
