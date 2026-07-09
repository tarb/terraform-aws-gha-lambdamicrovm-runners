//! The secret bundle: webhook secret + optional GitHub App credentials,
//! cached per warm container.
//!
//! Source of truth is the SSM SecureString at `PARAM_NAME` (webhook secret,
//! and legacy inline App credential). When `APP_SECRET_ARN` is configured the
//! App credential is fetched from Secrets Manager and overlays the inline one.

use secrecy::SecretString;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::aws::AwsApiError;
use crate::aws::params::ParamStore;
use crate::aws::secretsman::SecretStore;
use crate::clock::{Clock, Epoch};

/// Re-read the bundle at most this often.
const SECRET_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, thiserror::Error)]
pub enum SecretsError {
    #[error(transparent)]
    Aws(#[from] AwsApiError),
    #[error("malformed secret material: {0}")]
    Malformed(&'static str),
    #[error("GitHub App credentials are not configured")]
    MissingAppCredentials,
}

// SecretString's own Debug is redacted, so these are safe to derive.
#[derive(Debug, Clone)]
pub struct SecretBundle {
    pub webhook_secret: SecretString,
    pub app: Option<AppCredentials>,
}

#[derive(Debug, Clone)]
pub struct AppCredentials {
    /// Number-or-string in the JSON; normalized to its string spelling.
    pub app_id: String,
    pub private_key: SecretString,
}

impl SecretBundle {
    pub fn app(&self) -> Result<&AppCredentials, SecretsError> {
        self.app.as_ref().ok_or(SecretsError::MissingAppCredentials)
    }
}

/// JSON shape of the SSM parameter and the Secrets Manager secret.
#[derive(serde::Deserialize)]
struct RawSecret {
    webhook_secret: Option<String>,
    #[serde(default)]
    app_id: Option<AppId>,
    #[serde(default)]
    app_private_key: Option<String>,
    /// Secrets Manager fallback key for the PEM.
    #[serde(default)]
    private_key: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum AppId {
    Num(i64),
    Str(String),
}

impl AppId {
    fn into_string(self) -> String {
        match self {
            AppId::Num(n) => n.to_string(),
            AppId::Str(s) => s,
        }
    }
}

pub struct SecretsCache {
    params: Arc<dyn ParamStore>,
    secretsman: Arc<dyn SecretStore>,
    clock: Arc<dyn Clock>,
    param_name: String,
    app_secret_arn: Option<String>,
    cached: Mutex<Option<(SecretBundle, Epoch)>>,
}

impl SecretsCache {
    pub fn new(
        params: Arc<dyn ParamStore>,
        secretsman: Arc<dyn SecretStore>,
        clock: Arc<dyn Clock>,
        param_name: String,
        app_secret_arn: Option<String>,
    ) -> Self {
        Self {
            params,
            secretsman,
            clock,
            param_name,
            app_secret_arn,
            cached: Mutex::new(None),
        }
    }

    pub async fn bundle(&self) -> Result<SecretBundle, SecretsError> {
        let now = self.clock.now();
        if let Some((bundle, at)) = self.cached.lock().unwrap().as_ref()
            && now.since(*at) <= SECRET_TTL.as_secs_f64()
        {
            return Ok(bundle.clone());
        }
        let bundle = self.fetch().await?;
        *self.cached.lock().unwrap() = Some((bundle.clone(), now));
        Ok(bundle)
    }

    async fn fetch(&self) -> Result<SecretBundle, SecretsError> {
        let raw = self.params.get(&self.param_name, true).await?;
        let base: RawSecret = serde_json::from_str(&raw)
            .map_err(|_| SecretsError::Malformed("secret parameter is not the expected JSON"))?;
        let webhook_secret = base.webhook_secret.ok_or(SecretsError::Malformed(
            "secret parameter has no webhook_secret",
        ))?;

        let app = match &self.app_secret_arn {
            Some(arn) => Some(Self::app_from_secretsman(
                &self.secretsman.secret_string(arn).await?,
            )?),
            // Legacy: App credential inline in the parameter.
            None => match (base.app_id, base.app_private_key) {
                (Some(id), Some(pem)) => Some(AppCredentials {
                    app_id: id.into_string(),
                    private_key: pem.into(),
                }),
                _ => None,
            },
        };

        Ok(SecretBundle {
            webhook_secret: webhook_secret.into(),
            app,
        })
    }

    fn app_from_secretsman(raw: &str) -> Result<AppCredentials, SecretsError> {
        let parsed: RawSecret = serde_json::from_str(raw)
            .map_err(|_| SecretsError::Malformed("app secret is not the expected JSON"))?;
        let app_id = parsed
            .app_id
            .ok_or(SecretsError::Malformed("app secret has no app_id"))?
            .into_string();
        // `app_private_key` preferred; `private_key` accepted as the
        // Secrets Manager spelling.
        let pem = parsed
            .app_private_key
            .filter(|s| !s.is_empty())
            .or(parsed.private_key)
            .ok_or(SecretsError::Malformed("app secret has no private key"))?;
        Ok(AppCredentials {
            app_id,
            private_key: pem.into(),
        })
    }
}
