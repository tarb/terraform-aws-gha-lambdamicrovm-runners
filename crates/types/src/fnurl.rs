//! Lambda Function-URL response shape, shared by the dispatcher and the
//! webhook proxy.
//!
//! Function URLs expect `{"statusCode": <int>, "headers": {...}, "body":
//! "<string>"}`; both binaries answer GitHub with a JSON `{"msg": ...}` body
//! and `content-type: application/json`.

use serde::Serialize;

/// The full response object handed back to the Function-URL runtime.
#[derive(Debug, Clone, Serialize)]
pub struct FnUrlResponse {
    #[serde(rename = "statusCode")]
    pub status_code: u16,
    pub headers: FnUrlHeaders,
    /// JSON-encoded body, e.g. `{"msg": "pong"}`.
    pub body: String,
}

/// Response headers — always exactly `content-type: application/json`.
#[derive(Debug, Clone, Serialize)]
pub struct FnUrlHeaders {
    #[serde(rename = "content-type")]
    pub content_type: &'static str,
}

impl FnUrlResponse {
    /// Standard `{"msg": ...}` reply. Callers emit their own operational
    /// `{"status", "msg"}` log line alongside.
    pub fn msg(status_code: u16, msg: &str) -> Self {
        #[derive(Serialize)]
        struct Body<'a> {
            msg: &'a str,
        }
        Self {
            status_code,
            headers: FnUrlHeaders {
                content_type: "application/json",
            },
            body: serde_json::to_string(&Body { msg })
                .expect("a single string field always serializes"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_the_function_url_wire_shape() {
        let v = serde_json::to_value(FnUrlResponse::msg(202, "ok")).unwrap();
        assert_eq!(v["statusCode"], 202);
        assert_eq!(
            v["headers"],
            serde_json::json!({"content-type": "application/json"})
        );
        let body: serde_json::Value = serde_json::from_str(v["body"].as_str().unwrap()).unwrap();
        assert_eq!(body, serde_json::json!({"msg": "ok"}));
    }
}
