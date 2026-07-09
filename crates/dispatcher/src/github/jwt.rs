//! GitHub App JWT signing (RS256), standalone so the smoke test exercises the
//! REAL crypto path: jsonwebtoken's pluggable provider panics at SIGNING time
//! when the crate's crypto feature selection is ambiguous — invisible behind
//! fakes (live incident: every dispatcher invoke died on its first JWT).

use crate::clock::Epoch;

#[derive(Debug, Clone, thiserror::Error)]
pub enum JwtError {
    #[error("invalid App private key: {0}")]
    InvalidKey(String),
    #[error("JWT signing failed: {0}")]
    Sign(String),
}

/// Short-lived App JWT: `iat = now - 60` (clock skew), `exp = now + 540`
/// (well under GitHub's 10-minute cap), `iss = app_id` trimmed.
pub fn sign_app_jwt(app_id: &str, pem: &str, now: Epoch) -> Result<String, JwtError> {
    #[derive(serde::Serialize)]
    struct Claims {
        iat: i64,
        exp: i64,
        iss: String,
    }
    let now = now.0 as i64;
    let claims = Claims {
        iat: now - 60,
        exp: now + 540,
        iss: app_id.trim().to_string(),
    };
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|e| JwtError::InvalidKey(e.to_string()))?;
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &key,
    )
    .map_err(|e| JwtError::Sign(e.to_string()))
}

#[cfg(test)]
mod smoke {
    use super::*;

    const TEST_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQDgqsWx7DSptFza
X67Zzx+F3hjfY0pIZKGu0phDEdRcAQeYYyTwGLa5s3kpiIcXQpZdH/+A9HNys3Aa
fy0VjWXXkrXnBSK8+ZKCNPVQxIRxP9Hf+uJPjnrJAOzE9H1vmJhzLoT+1Hzqguuk
/G6xYv7Id6qj334oqRhV4uCLGOZVruL6dc18igGw01Bv4jeqPbFiJpaim58gqYMs
QqRebxVpnMV4InzvqllApgDV/HaixhEVYQruWeLr18xIfdNorVxKKb4zWkhZOlWg
FlhHQlCXAgEWRkdT6Apz4HgOs+AeITEe/ljM9HI8IwnWwqCHEL4z1WezRsPmwoIQ
6etOBafXAgMBAAECggEAB/IpYV/o9VuW6gOhb2biuKRZgRNUCLD8rLVvoBJOlXLj
bf39dpDgT1f2py5ma2nZmLFbzbQeuoJbuP0TKa8zH3W7AXt8dkOs+2fxpaYzAIXw
+/xulN2Fg8MW8gNyU9ZszQGjozOofutGPvoinBtFqnzAHX2h9WtNnEI/Y33Pn7KN
HC5D94Grztm9SP5Mw51VOhM1FATNamNMRmiY2LBlhu/rxMRxrKqr2hj+NLU2T71X
zZM/QbwvyzQc7LSsatJxLi/uDoSRQ/XxSf7iJpHCBPyihF8icO0q2NQu6Hj2C9/j
piu570Ilsq307s4VlamQKyJ9gCgk95r8EHU380dNYQKBgQD+ZjT1mMK0UVzQcinU
ICSelUo91V7dLRrnmbtYrXpCWrxFr6GlTd0hTKYcFf4pNz/bAT0hJTyG8zLmLuOr
EPHP8tpqeDB1vMVCPLsFW+lWwKrhHXyU9hL3acsJmFOojYsU2a0DIYL/j4UHtSl7
XUq6HuQl8oWU1+3e1I550Sy2UQKBgQDiFKwIWfIoN8oUb0E7msCQGS35PdTEsQbo
Wh4Jo0uvKdAcsveuiL3PM+2AZoX2xlxamJVPX2sqMOIlH3nCTD/iz8Tc8M7EG+mt
rL88eTTOuvzyIuTNE0af+vYuEvC+tOfLXnt57yyFycauyJoYnNdZmZBG6+eBxbV3
mQnm33vppwKBgQD7ZQ7yoDnQLRL2Hcr+B6GIYOkTv5XWJWuP8Ng1IoFNrxKcHpoz
m4VpEbCY0pbuLd3ZUxkQdxagGRZ0Z2OuOblsEIYMbqccwiWAdjkua4xjoVN70EK7
hYxqmE3/Nlt9lhoZyZ3yGRy15SLF4h2S/jcJQ9ubMFUXKGa1LAF7mdyAcQKBgQCp
ZHPBjiMynxp6VSG7VygQz8zygrF47msOjPcUoZWDmQClgDK0QyB0r6O0IR0e2WE5
QDofTo8s/ZNz3TGNszPq7WHDaWqC5acgyd4/oVE/1DrR8fMc9ORl2dO6kdZwDXvf
lNtPcTUaySRksUlER7/TEoxXl0nOoiRlh/UzVx+w4QKBgQClnK3KMXXO5u8qplt+
+GTlGedXVzl51a5BqXPgEUoWBtUGnf/bqap3/JeYlvVB6ZnZuzLoRQKdD1BJgY/f
MS85EvlZrOVKTh6C0lH1BC/DGeU93HOR6Yaw6mdXLKVkpGkuaR7imFk+9TYND2lN
/k4/GLu8lAqxKyRQUDCG3iH4xQ==
-----END PRIVATE KEY-----
"#;

    #[test]
    fn app_jwt_signs_with_the_selected_crypto_provider() {
        let jwt = sign_app_jwt("12345", TEST_KEY, Epoch(1_700_000_000.0)).expect("must sign");
        assert_eq!(jwt.split('.').count(), 3, "compact JWS has three segments");
    }
}
