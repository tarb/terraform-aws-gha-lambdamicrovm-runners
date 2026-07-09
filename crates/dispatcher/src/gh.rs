//! GitHub App auth: secret bundle, App JWT, installation-token cache, and
//! webhook HMAC verification. Neither the private key nor any token is ever
//! logged.

use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;

use crate::app::App;
use crate::config::SECRET_TTL;
use crate::platform::Platform;
use crate::pyfmt::{PyErr, py_str, v_index};
use crate::timeparse::parse_gh_time;

impl<P: Platform> App<'_, P> {
    /// `_secrets()`: SSM SecureString (webhook secret + legacy App cred),
    /// overlaid with the Secrets Manager App credential when APP_SECRET_ARN
    /// is set. Re-read at most every SECRET_TTL seconds.
    pub async fn secrets(&self) -> Result<Value, PyErr> {
        let now = self.p.now() as i64;
        if let Some((data, ts)) = self.caches.secret.lock().unwrap().as_ref()
            && now - ts <= SECRET_TTL
        {
            return Ok(data.clone());
        }
        let raw = self
            .p
            .ssm_get_parameter(&self.cfg.param_name, true)
            .await
            .map_err(PyErr::from)?;
        let mut data: Value = serde_json::from_str(&raw).map_err(PyErr::json_error)?;
        if let Some(arn) = &self.cfg.app_secret_arn {
            let app_raw = self.p.sm_get_secret(arn).await.map_err(PyErr::from)?;
            let app: Value = serde_json::from_str(&app_raw).map_err(PyErr::json_error)?;
            let app_id = v_index(&app, "app_id")?.clone();
            // app.get("app_private_key") or app["private_key"]
            let pk = match app.get("app_private_key") {
                Some(v) if crate::pyfmt::truthy(v) => v.clone(),
                _ => v_index(&app, "private_key")?.clone(),
            };
            let obj = data
                .as_object_mut()
                .ok_or_else(|| PyErr::type_error("secret parameter is not a JSON object"))?;
            obj.insert("app_id".to_string(), app_id);
            obj.insert("app_private_key".to_string(), pk);
        }
        *self.caches.secret.lock().unwrap() = Some((data.clone(), now));
        Ok(data)
    }

    /// `_app_jwt(secret["app_id"], secret["app_private_key"])`.
    pub fn app_jwt_from_secret(&self, secret: &Value) -> Result<String, PyErr> {
        let app_id = py_str(v_index(secret, "app_id")?);
        let pem = v_index(secret, "app_private_key")?
            .as_str()
            .ok_or_else(|| PyErr::type_error("app_private_key is not a string"))?;
        self.p.app_jwt(&app_id, pem)
    }

    /// `_installation_token`: exchange the App JWT for an installation access
    /// token (~1h), cached per installation with a 60s safety margin.
    pub async fn installation_token(
        &self,
        secret: &Value,
        installation_id: i64,
    ) -> Result<String, PyErr> {
        let now = self.p.now() as i64;
        if let Some((tok, exp)) = self.caches.tok.lock().unwrap().get(&installation_id)
            && exp - 60 > now
        {
            return Ok(tok.clone());
        }
        let app_jwt = self.app_jwt_from_secret(secret)?;
        let url = format!(
            "{}/app/installations/{}/access_tokens",
            self.cfg.gh_api, installation_id
        );
        let (_, tok) = self.p.gh_call("POST", &url, &app_jwt, None).await?;
        let token = v_index(&tok, "token")?
            .as_str()
            .ok_or_else(|| PyErr::type_error("token is not a string"))?
            .to_string();
        let expires_at = v_index(&tok, "expires_at")?
            .as_str()
            .ok_or_else(|| PyErr::type_error("expires_at is not a string"))?
            .to_string();
        let expiry = parse_gh_time(&expires_at)?;
        self.caches
            .tok
            .lock()
            .unwrap()
            .insert(installation_id, (token.clone(), expiry));
        Ok(token)
    }

    /// `_token_for_repo`: derive installation_id from the repo when the
    /// webhook didn't include it, then mint/fetch the installation token.
    pub async fn token_for_repo(
        &self,
        secret: &Value,
        repo: &str,
        installation_id: Option<i64>,
    ) -> Result<String, PyErr> {
        // Python: `if not installation_id` — None and 0 are both falsy.
        let iid = match installation_id {
            Some(i) if i != 0 => i,
            _ => {
                let app_jwt = self.app_jwt_from_secret(secret)?;
                let url = format!("{}/repos/{}/installation", self.cfg.gh_api, repo);
                let (_, inst) = self.p.gh_call("GET", &url, &app_jwt, None).await?;
                inst.get("id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| PyErr::runtime(format!("no installation id for repo {repo}")))?
            }
        };
        self.installation_token(secret, iid).await
    }
}

/// `_verify`: constant-time check of the `X-Hub-Signature-256` header.
pub fn verify(body: &[u8], sig: Option<&str>, secret: &str) -> bool {
    let Some(sig) = sig else { return false };
    let Some(hex_sig) = sig.strip_prefix("sha256=") else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key size");
    mac.update(body);
    // Python compares "sha256=" + hexdigest with the raw header, exactly
    // (lowercase hex; an uppercase signature does NOT match there either).
    let expected = hex::encode(mac.finalize().into_bytes());
    ct_eq(expected.as_bytes(), hex_sig.as_bytes())
}

/// Constant-time equality for equal-length byte strings (hmac.compare_digest).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_accepts_reference_signature() {
        // reference: hmac.new(b"s3cret", b"hello world", sha256).hexdigest()
        let sig = "sha256=41b38be71f34aaebe51e690babb5cef0f58ccfe80e3b932c1caf0ad7945ec9e4";
        assert!(verify(b"hello world", Some(sig), "s3cret"));
    }

    #[test]
    fn verify_rejects_bad_or_missing_signatures() {
        assert!(!verify(b"hello world", None, "s3cret"));
        assert!(!verify(b"hello world", Some("md5=abc"), "s3cret"));
        assert!(!verify(
            b"hello world",
            Some("sha256=41b38be71f34aaebe51e690babb5cef0f58ccfe80e3b932c1caf0ad7945ec9e5"),
            "s3cret"
        ));
        assert!(!verify(
            b"tampered",
            Some("sha256=41b38be71f34aaebe51e690babb5cef0f58ccfe80e3b932c1caf0ad7945ec9e4"),
            "s3cret"
        ));
    }
}
