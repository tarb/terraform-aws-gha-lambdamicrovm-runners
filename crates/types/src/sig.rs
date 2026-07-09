//! GitHub webhook signature verification — one implementation shared by the
//! webhook proxy and the dispatcher's legacy Function-URL path, so the two
//! front doors can never drift on what counts as a valid signature.

use hmac::{Hmac, KeyInit, Mac};
use secrecy::{ExposeSecret, SecretString};
use sha2::Sha256;

/// Verify `X-Hub-Signature-256: sha256=<hex>` against the shared secret.
/// Constant-time comparison via [`Mac::verify_slice`]. GitHub sends lowercase
/// hex; an uppercase digest must NOT verify (`hex::decode` would otherwise
/// accept it, silently widening what counts as a valid signature).
pub fn verify(body: &[u8], signature: Option<&str>, secret: &SecretString) -> bool {
    let Some(hex_part) = signature.and_then(|s| s.strip_prefix("sha256=")) else {
        return false;
    };
    if hex_part.bytes().any(|b| b.is_ascii_uppercase()) {
        return false;
    }
    let Ok(sig_bytes) = hex::decode(hex_part) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.expose_secret().as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(&sig_bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GitHub's documented example webhook secret / payload / signature.
    const GOLDEN_SECRET: &str = "It's a Secret to Everybody";
    const GOLDEN_BODY: &[u8] = b"Hello, World!";
    const GOLDEN_SIG: &str =
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    fn secret(s: &str) -> SecretString {
        SecretString::from(s.to_string())
    }

    #[test]
    fn verify_golden_vector() {
        assert!(verify(
            GOLDEN_BODY,
            Some(GOLDEN_SIG),
            &secret(GOLDEN_SECRET)
        ));
    }

    #[test]
    fn verify_rejects_bad_inputs() {
        let golden = secret(GOLDEN_SECRET);
        // wrong body
        assert!(!verify(b"Hello, World?", Some(GOLDEN_SIG), &golden));
        // wrong secret
        assert!(!verify(GOLDEN_BODY, Some(GOLDEN_SIG), &secret("nope")));
        // missing / empty / unprefixed
        assert!(!verify(GOLDEN_BODY, None, &golden));
        assert!(!verify(GOLDEN_BODY, Some(""), &golden));
        assert!(!verify(
            GOLDEN_BODY,
            Some("sha1=757107ea0eb2509fc211221cce984b8a37570b6d"),
            &golden
        ));
        // truncated / non-hex digests
        assert!(!verify(GOLDEN_BODY, Some("sha256=757107"), &golden));
        assert!(!verify(GOLDEN_BODY, Some("sha256=zz"), &golden));
        // uppercase hex must fail — only the lowercase digest is accepted
        let upper = format!("sha256={}", GOLDEN_SIG[7..].to_ascii_uppercase());
        assert!(!verify(GOLDEN_BODY, Some(&upper), &golden));
    }
}
