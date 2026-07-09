//! The AWS / GitHub / clock seam.
//!
//! All side effects go through the [`Platform`] trait so the pure logic can
//! be unit-tested against hand-rolled fakes. [`RealPlatform`] wraps the AWS
//! SDK clients (rustls only) and reqwest.

use crate::pyfmt::PyErr;
use serde_json::Value;

/// A boto3 `ClientError`-shaped failure. `service == true` corresponds to a
/// botocore ClientError (an API error response); `false` is a transport-level
/// failure, which Python's `except ClientError` handlers do NOT swallow.
#[derive(Debug, Clone)]
pub struct AwsErr {
    pub code: Option<String>,
    pub message: String,
    pub service: bool,
}

impl AwsErr {
    pub fn is_code(&self, code: &str) -> bool {
        self.service && self.code.as_deref() == Some(code)
    }
}

impl From<AwsErr> for PyErr {
    fn from(e: AwsErr) -> Self {
        PyErr::new(
            if e.service {
                "ClientError"
            } else {
                "ConnectionError"
            },
            e.message,
        )
    }
}

/// `try: ... except ClientError: pass` — swallow service errors only.
pub fn ignore_service(r: Result<(), AwsErr>) -> Result<(), PyErr> {
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.service => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// One un-normalized ListMicrovms item (the Python dict from botocore).
#[derive(Debug, Clone, Default)]
pub struct RawVm {
    pub microvm_id: Option<String>,
    pub state: Option<String>,
    pub image_version: Option<String>,
    /// `startedAt` as epoch seconds (Python keeps the datetime and calls
    /// `.timestamp()` later; parse failures there map to `None` here).
    pub started_at: Option<f64>,
    /// Response record keys, for the one-shot `vm_record_keys` canary log.
    pub raw_keys: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct VmPage {
    pub items: Vec<RawVm>,
    pub next_token: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ImageInfo {
    pub latest_active_image_version: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunVmRequest<'a> {
    pub image_arn: &'a str,
    pub image_version: &'a str,
    pub exec_role_arn: &'a str,
    pub egress: &'a str,
    pub max_duration: i64,
    pub run_hook_payload: &'a str,
    pub log_group: &'a str,
}

#[derive(Debug, Clone, Default)]
pub struct ParamMeta {
    pub name: Option<String>,
    pub last_modified: Option<f64>,
}

/// Every side effect the dispatcher performs, so tests can fake the world.
#[allow(async_fn_in_trait)]
pub trait Platform {
    // ── lambda-microvms ──────────────────────────────────────────────────
    async fn mv_list_page(
        &self,
        image_arn: &str,
        next_token: Option<&str>,
    ) -> Result<VmPage, AwsErr>;
    async fn mv_get_state(&self, id: &str) -> Result<String, AwsErr>;
    async fn mv_get_image(&self, image_arn: &str) -> Result<ImageInfo, AwsErr>;
    /// Returns the new microvmId.
    async fn mv_run(&self, req: RunVmRequest<'_>) -> Result<Option<String>, AwsErr>;
    async fn mv_resume(&self, id: &str) -> Result<(), AwsErr>;
    async fn mv_suspend(&self, id: &str) -> Result<(), AwsErr>;
    async fn mv_terminate(&self, id: &str) -> Result<(), AwsErr>;
    // ── ssm ──────────────────────────────────────────────────────────────
    async fn ssm_get_parameter(&self, name: &str, decrypt: bool) -> Result<String, AwsErr>;
    async fn ssm_put_secure(&self, name: &str, value: &str) -> Result<(), AwsErr>;
    async fn ssm_delete(&self, name: &str) -> Result<(), AwsErr>;
    async fn ssm_by_path(&self, path: &str) -> Result<Vec<ParamMeta>, AwsErr>;
    // ── secretsmanager ───────────────────────────────────────────────────
    async fn sm_get_secret(&self, arn: &str) -> Result<String, AwsErr>;
    // ── github rest ──────────────────────────────────────────────────────
    /// Mirrors Python `_gh`: returns (status, parsed body) for 2xx/3xx and
    /// errors (like urllib's HTTPError) for >= 400. Empty bodies parse to null.
    async fn gh_call(
        &self,
        method: &str,
        url: &str,
        token: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), PyErr>;
    // ── crypto ───────────────────────────────────────────────────────────
    /// Short-lived RS256 App JWT: iat now-60, exp now+540, iss = app_id.
    fn app_jwt(&self, app_id: &str, pem: &str) -> Result<String, PyErr>;
    // ── clock ────────────────────────────────────────────────────────────
    fn now(&self) -> f64;
    async fn sleep(&self, secs: f64);
}

// ─────────────────────────────────────────────────────────────────────────
// Real implementation
// ─────────────────────────────────────────────────────────────────────────

pub struct RealPlatform {
    mv: aws_sdk_lambdamicrovms::Client,
    ssm: aws_sdk_ssm::Client,
    sm: aws_sdk_secretsmanager::Client,
    http: reqwest::Client,
}

impl RealPlatform {
    pub fn new(shared: &aws_config::SdkConfig) -> Self {
        Self {
            mv: aws_sdk_lambdamicrovms::Client::new(shared),
            ssm: aws_sdk_ssm::Client::new(shared),
            sm: aws_sdk_secretsmanager::Client::new(shared),
            http: reqwest::Client::new(),
        }
    }
}

/// Map an SDK error to the botocore-style `AwsErr`, formatting service errors
/// like botocore does so `err` values in log lines line up across languages.
fn map_sdk_err<E, R>(op: &str, e: aws_sdk_ssm::error::SdkError<E, R>) -> AwsErr
where
    E: aws_sdk_ssm::error::ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
{
    match &e {
        aws_sdk_ssm::error::SdkError::ServiceError(ctx) => {
            let meta = ctx.err().meta();
            let code = meta.code().map(str::to_string);
            let message = format!(
                "An error occurred ({}) when calling the {} operation: {}",
                code.as_deref().unwrap_or("Unknown"),
                op,
                meta.message().unwrap_or("")
            );
            AwsErr {
                code,
                message,
                service: true,
            }
        }
        other => AwsErr {
            code: None,
            message: format!("{}", aws_sdk_ssm::error::DisplayErrorContext(other)),
            service: false,
        },
    }
}

impl Platform for RealPlatform {
    async fn mv_list_page(
        &self,
        image_arn: &str,
        next_token: Option<&str>,
    ) -> Result<VmPage, AwsErr> {
        let out = self
            .mv
            .list_microvms()
            .image_identifier(image_arn)
            .set_next_token(next_token.map(str::to_string))
            .send()
            .await
            .map_err(|e| map_sdk_err("ListMicrovms", e))?;
        let items = out
            .items()
            .iter()
            .map(|m| RawVm {
                microvm_id: Some(m.microvm_id().to_string()),
                state: Some(m.state().as_str().to_string()),
                image_version: Some(m.image_version().to_string()),
                started_at: Some(m.started_at().as_secs_f64()),
                // botocore record keys as the Python canary log would see them
                raw_keys: [
                    "imageArn",
                    "imageVersion",
                    "microvmId",
                    "startedAt",
                    "state",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            })
            .collect();
        Ok(VmPage {
            items,
            next_token: out.next_token().map(str::to_string),
        })
    }

    async fn mv_get_state(&self, id: &str) -> Result<String, AwsErr> {
        let out = self
            .mv
            .get_microvm()
            .microvm_identifier(id)
            .send()
            .await
            .map_err(|e| map_sdk_err("GetMicrovm", e))?;
        Ok(out.state().as_str().to_string())
    }

    async fn mv_get_image(&self, image_arn: &str) -> Result<ImageInfo, AwsErr> {
        let out = self
            .mv
            .get_microvm_image()
            .image_identifier(image_arn)
            .send()
            .await
            .map_err(|e| map_sdk_err("GetMicrovmImage", e))?;
        Ok(ImageInfo {
            latest_active_image_version: out.latest_active_image_version().map(str::to_string),
            state: Some(out.state().as_str().to_string()),
        })
    }

    async fn mv_run(&self, req: RunVmRequest<'_>) -> Result<Option<String>, AwsErr> {
        let logging = aws_sdk_lambdamicrovms::types::Logging::CloudWatch(
            aws_sdk_lambdamicrovms::types::CloudWatchLogging::builder()
                .log_group(req.log_group)
                .build(),
        );
        let out = self
            .mv
            .run_microvm()
            .image_identifier(req.image_arn)
            .image_version(req.image_version)
            .execution_role_arn(req.exec_role_arn)
            .egress_network_connectors(req.egress)
            .maximum_duration_in_seconds(req.max_duration as i32)
            .run_hook_payload(req.run_hook_payload)
            .logging(logging)
            .send()
            .await
            .map_err(|e| map_sdk_err("RunMicrovm", e))?;
        Ok(Some(out.microvm_id().to_string()))
    }

    async fn mv_resume(&self, id: &str) -> Result<(), AwsErr> {
        self.mv
            .resume_microvm()
            .microvm_identifier(id)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| map_sdk_err("ResumeMicrovm", e))
    }

    async fn mv_suspend(&self, id: &str) -> Result<(), AwsErr> {
        self.mv
            .suspend_microvm()
            .microvm_identifier(id)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| map_sdk_err("SuspendMicrovm", e))
    }

    async fn mv_terminate(&self, id: &str) -> Result<(), AwsErr> {
        self.mv
            .terminate_microvm()
            .microvm_identifier(id)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| map_sdk_err("TerminateMicrovm", e))
    }

    async fn ssm_get_parameter(&self, name: &str, decrypt: bool) -> Result<String, AwsErr> {
        let out = self
            .ssm
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

    async fn ssm_put_secure(&self, name: &str, value: &str) -> Result<(), AwsErr> {
        self.ssm
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

    async fn ssm_delete(&self, name: &str) -> Result<(), AwsErr> {
        self.ssm
            .delete_parameter()
            .name(name)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| map_sdk_err("DeleteParameter", e))
    }

    async fn ssm_by_path(&self, path: &str) -> Result<Vec<ParamMeta>, AwsErr> {
        // Python performs a single (non-paginated) GetParametersByPath call.
        let out = self
            .ssm
            .get_parameters_by_path()
            .path(path)
            .recursive(true)
            .send()
            .await
            .map_err(|e| map_sdk_err("GetParametersByPath", e))?;
        Ok(out
            .parameters()
            .iter()
            .map(|p| ParamMeta {
                name: p.name().map(str::to_string),
                last_modified: p.last_modified_date().map(|d| d.as_secs_f64()),
            })
            .collect())
    }

    async fn sm_get_secret(&self, arn: &str) -> Result<String, AwsErr> {
        let out = self
            .sm
            .get_secret_value()
            .secret_id(arn)
            .send()
            .await
            .map_err(|e| map_sdk_err("GetSecretValue", e))?;
        Ok(out.secret_string().unwrap_or_default().to_string())
    }

    async fn gh_call(
        &self,
        method: &str,
        url: &str,
        token: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), PyErr> {
        let m = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| PyErr::value_error(e.to_string()))?;
        let mut rb = self
            .http
            .request(m, url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("Content-Type", "application/json")
            .header("User-Agent", "microvm-runner-dispatcher")
            .timeout(std::time::Duration::from_secs(15));
        if let Some(b) = body {
            rb = rb.body(serde_json::to_string(b).map_err(PyErr::json_error)?);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| PyErr::new("URLError", format!("<urlopen error {e}>")))?;
        let status = resp.status();
        if status.as_u16() >= 400 {
            // urllib raises HTTPError for 4xx/5xx; str(e) == "HTTP Error N: Reason"
            return Err(PyErr::new(
                "HTTPError",
                format!(
                    "HTTP Error {}: {}",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("")
                ),
            ));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| PyErr::new("URLError", format!("<urlopen error {e}>")))?;
        let v: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).map_err(PyErr::json_error)?
        };
        Ok((status.as_u16(), v))
    }

    fn app_jwt(&self, app_id: &str, pem: &str) -> Result<String, PyErr> {
        #[derive(serde::Serialize)]
        struct Claims {
            iat: i64,
            exp: i64,
            iss: String,
        }
        let now = self.now() as i64;
        // iat backdated 60s for clock skew; exp well under GitHub's 10-min cap.
        let claims = Claims {
            iat: now - 60,
            exp: now + 540,
            iss: app_id.trim().to_string(),
        };
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())
            .map_err(|e| PyErr::new("InvalidKeyError", e.to_string()))?;
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            &key,
        )
        .map_err(|e| PyErr::new("PyJWTError", e.to_string()))
    }

    fn now(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    async fn sleep(&self, secs: f64) {
        tokio::time::sleep(std::time::Duration::from_secs_f64(secs.max(0.0))).await;
    }
}
