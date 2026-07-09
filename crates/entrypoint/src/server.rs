//! The lifecycle-hook HTTP server: POST
//! /aws/lambda-microvms/runtime/v1/<hook> on 0.0.0.0:HOOK_PORT, for the
//! hooks `ready` `validate` `run` `resume` `suspend` `terminate`.
//!
//! Service-pinned contract: unknown hooks and the bare prefix 404, a
//! trailing slash is tolerated, query strings are ignored, GET anything is
//! the liveness probe (200 "ok"), and a malformed body is never a 400 —
//! the lifecycle service must not be failed on body shape.

use crate::config::HOOK_PREFIX;
use crate::logfmt::log;
use crate::payload::RunConfig;
use crate::state::AppState;
use crate::supervisor;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Map, Value};
use std::str::FromStr;
use std::sync::Arc;

pub fn router(app: Arc<AppState>) -> Router {
    Router::new()
        .route(
            &format!("{HOOK_PREFIX}/{{hook}}"),
            post(post_hook).get(get_ok),
        )
        .route(
            &format!("{HOOK_PREFIX}/{{hook}}/"),
            post(post_hook).get(get_ok),
        )
        .fallback(fallback)
        .with_state(app)
}

/// Bind and serve forever.
pub async fn serve(app: Arc<AppState>) -> std::io::Result<()> {
    let port = app.cfg.hook_port;
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    axum::serve(listener, router(app)).await
}

/// GET anything is the liveness probe.
async fn get_ok() -> &'static str {
    "ok"
}

async fn fallback(method: Method) -> Response {
    match method {
        Method::GET => (StatusCode::OK, "ok").into_response(),
        Method::POST => StatusCode::NOT_FOUND.into_response(),
        _ => StatusCode::NOT_IMPLEMENTED.into_response(),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Hook {
    Ready,
    Validate,
    Run,
    Resume,
    Suspend,
    Terminate,
}

impl FromStr for Hook {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "ready" => Hook::Ready,
            "validate" => Hook::Validate,
            "run" => Hook::Run,
            "resume" => Hook::Resume,
            "suspend" => Hook::Suspend,
            "terminate" => Hook::Terminate,
            _ => return Err(()),
        })
    }
}

/// Lenient body parse: invalid/absent JSON or a non-object becomes an empty
/// map — NEVER a 400.
fn lenient_object(body: &[u8]) -> Map<String, Value> {
    if body.is_empty() {
        return Map::new();
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

async fn post_hook(
    State(app): State<Arc<AppState>>,
    Path(hook): Path<String>,
    body: Bytes,
) -> Response {
    let payload = lenient_object(&body);
    {
        let mut keys: Vec<&str> = payload.keys().map(String::as_str).collect();
        keys.sort_unstable();
        log(format!("hook /{hook} payload-keys={keys:?}"));
    }
    let Ok(hook) = hook.parse::<Hook>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match hook {
        Hook::Ready => {
            // Build-time snapshot point: binaries present, server up, NOT
            // registered. dockerd is intentionally NOT started here.
            let listener = format!("{}/bin/Runner.Listener", app.cfg.runner_dir);
            if std::path::Path::new(&listener).exists() {
                StatusCode::OK.into_response()
            } else {
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            }
        }
        Hook::Validate => StatusCode::OK.into_response(),
        Hook::Run => {
            let cfg = RunConfig::from_hook_body(&payload);
            log(format!(
                "/run microvmId={:?} config-keys={:?}",
                cfg.microvm_id.as_ref().map(types::MicrovmId::as_str),
                cfg.raw_keys
            ));
            supervisor::spawn_run_task(app, cfg);
            StatusCode::OK.into_response()
        }
        // Freeze/thaw are transparent to the guest; pool logic lives in the
        // idle waiter's clock-jump detection and the mailbox poll.
        Hook::Resume | Hook::Suspend => StatusCode::OK.into_response(),
        Hook::Terminate => {
            tokio::spawn(supervisor::deregister(app));
            StatusCode::OK.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testsupport::{temp_dir, test_app};
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::util::ServiceExt;

    fn test_router(runner_dir: &str) -> Router {
        let (app, _fake) = test_app(runner_dir);
        router(app)
    }

    async fn send(router: &Router, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn known_hooks_answer_200() {
        let r = test_router("/nonexistent");
        for hook in ["validate", "run", "resume", "suspend", "terminate"] {
            let (status, _) = send(&r, "POST", &format!("{HOOK_PREFIX}/{hook}"), "{}").await;
            assert_eq!(status, StatusCode::OK, "hook {hook}");
        }
    }

    #[tokio::test]
    async fn ready_gates_on_the_listener_binary() {
        let r = test_router("/nonexistent");
        let (status, _) = send(&r, "POST", &format!("{HOOK_PREFIX}/ready"), "").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let dir = temp_dir("server-ready");
        std::fs::create_dir_all(format!("{dir}/bin")).unwrap();
        std::fs::write(format!("{dir}/bin/Runner.Listener"), b"").unwrap();
        let r = test_router(&dir);
        let (status, _) = send(&r, "POST", &format!("{HOOK_PREFIX}/ready"), "").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn query_strings_are_ignored() {
        let r = test_router("/nonexistent");
        let (status, _) = send(&r, "POST", &format!("{HOOK_PREFIX}/validate?x=1&y=2"), "{}").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn trailing_slash_is_tolerated() {
        let r = test_router("/nonexistent");
        let (status, _) = send(&r, "POST", &format!("{HOOK_PREFIX}/validate/"), "{}").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn wrong_prefix_bare_prefix_and_unknown_hook_are_404() {
        let r = test_router("/nonexistent");
        for uri in [
            "/other/v1/run".to_string(),
            "/".to_string(),
            HOOK_PREFIX.to_string(),
            format!("{HOOK_PREFIX}/"),
            format!("{HOOK_PREFIX}/nope"),
            format!("{HOOK_PREFIX}/run/extra"),
        ] {
            let (status, _) = send(&r, "POST", &uri, "{}").await;
            assert_eq!(status, StatusCode::NOT_FOUND, "POST {uri}");
        }
    }

    #[tokio::test]
    async fn get_anything_is_the_liveness_probe() {
        let r = test_router("/nonexistent");
        for uri in [
            "/".to_string(),
            "/anything".to_string(),
            // Even the ready path: GET never runs the readiness gate.
            format!("{HOOK_PREFIX}/ready"),
        ] {
            let (status, body) = send(&r, "GET", &uri, "").await;
            assert_eq!(status, StatusCode::OK, "GET {uri}");
            assert_eq!(body, "ok");
        }
    }

    #[tokio::test]
    async fn malformed_bodies_are_never_a_400() {
        let r = test_router("/nonexistent");
        for body in ["{not json", "[1, 2]", "\"str\"", ""] {
            let (status, _) = send(&r, "POST", &format!("{HOOK_PREFIX}/validate"), body).await;
            assert_eq!(status, StatusCode::OK, "body {body:?}");
        }
    }

    #[tokio::test]
    async fn unsupported_methods_are_501_on_unrouted_paths() {
        let r = test_router("/nonexistent");
        let (status, _) = send(&r, "PUT", "/elsewhere", "").await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    }
}
