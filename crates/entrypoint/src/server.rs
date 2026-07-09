//! The lifecycle-hook HTTP server — port of the `Hooks` handler: a minimal
//! HTTP/1.0-style server (one request per connection, exactly like Python's
//! default `BaseHTTPRequestHandler`) on 0.0.0.0:HOOK_PORT serving
//! POST /aws/lambda-microvms/runtime/v1/<hook>.

use crate::config::HOOK_PREFIX;
use crate::payload::unwrap_run_payload;
use crate::state::Sup;
use crate::util::{log, py_repr_json, py_str_list};
use serde_json::{Map, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Bind and serve forever (`ThreadingHTTPServer.serve_forever`).
pub async fn serve(sup: Arc<Sup>) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", sup.cfg.hook_port)).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let sup = sup.clone();
        tokio::spawn(async move {
            let _ = handle_conn(sup, stream).await;
        });
    }
}

/// Strip the query string, require the hook prefix, return the hook name
/// (`path[len(HOOK_PREFIX):].strip("/")`). `None` is a 404.
pub fn route_hook(path: &str, prefix: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or(path);
    let rest = path.strip_prefix(prefix)?;
    Some(rest.trim_matches('/').to_string())
}

async fn handle_conn(sup: Arc<Sup>, mut stream: TcpStream) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.split();
    let mut reader = BufReader::new(read_half);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await? == 0 {
            break;
        }
        let line = header.trim();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    // The service caps runHookPayload at 4KB; 16MB is a generous DoS bound.
    let mut body = vec![0u8; content_length.min(16 * 1024 * 1024)];
    if !body.is_empty() {
        reader.read_exact(&mut body).await?;
    }

    let (code, resp_body) = dispatch(&sup, &method, &path, &body);
    let reason = match code {
        200 => "OK",
        404 => "Not Found",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp_body.len()
    );
    write_half.write_all(head.as_bytes()).await?;
    if !resp_body.is_empty() {
        write_half.write_all(resp_body).await?;
    }
    write_half.shutdown().await
}

fn dispatch(sup: &Arc<Sup>, method: &str, path: &str, body: &[u8]) -> (u16, &'static [u8]) {
    match method {
        "GET" => (200, b"ok"),
        "POST" => dispatch_post(sup, path, body),
        _ => (501, b""),
    }
}

fn dispatch_post(sup: &Arc<Sup>, path: &str, body: &[u8]) -> (u16, &'static [u8]) {
    let Some(hook) = route_hook(path, HOOK_PREFIX) else {
        return (404, b"");
    };
    let payload: Map<String, Value> = if body.is_empty() {
        Map::new()
    } else {
        serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .unwrap_or_default()
    };
    {
        let mut keys: Vec<&str> = payload.keys().map(String::as_str).collect();
        keys.sort_unstable();
        log(format!("hook /{hook} payload-keys={}", py_str_list(&keys)));
    }

    match hook.as_str() {
        "ready" => {
            // Build-time snapshot point: binaries present, server up, NOT
            // registered. dockerd is intentionally NOT started here.
            let listener = format!("{}/bin/Runner.Listener", sup.cfg.runner_dir);
            if std::path::Path::new(&listener).exists() {
                (200, b"")
            } else {
                (503, b"")
            }
        }
        "validate" => (200, b""),
        "run" => {
            let run_payload = unwrap_run_payload(&payload);
            log(format!(
                "/run microvmId={} config-keys={}",
                py_repr_json(run_payload.get("microvmId")),
                py_str_list(&run_payload.keys_sorted_excluding("token"))
            ));
            crate::runner::spawn_start_runner(sup.clone(), run_payload);
            (200, b"")
        }
        "resume" | "suspend" => (200, b""),
        "terminate" => {
            tokio::spawn(crate::runner::deregister(sup.clone()));
            (200, b"")
        }
        _ => (404, b""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hooks_route() {
        for hook in ["ready", "validate", "run", "resume", "suspend", "terminate"] {
            assert_eq!(
                route_hook(&format!("{HOOK_PREFIX}/{hook}"), HOOK_PREFIX).as_deref(),
                Some(hook)
            );
        }
    }

    #[test]
    fn query_strings_are_stripped() {
        assert_eq!(
            route_hook(&format!("{HOOK_PREFIX}/run?x=1&y=2"), HOOK_PREFIX).as_deref(),
            Some("run")
        );
    }

    #[test]
    fn trailing_slash_is_tolerated() {
        assert_eq!(
            route_hook(&format!("{HOOK_PREFIX}/run/"), HOOK_PREFIX).as_deref(),
            Some("run")
        );
    }

    #[test]
    fn wrong_prefix_is_none() {
        assert!(route_hook("/other/v1/run", HOOK_PREFIX).is_none());
        assert!(route_hook("/", HOOK_PREFIX).is_none());
    }

    #[test]
    fn bare_prefix_routes_to_empty_hook() {
        // Python: path == prefix gives hook "" which then 404s at match time.
        assert_eq!(route_hook(HOOK_PREFIX, HOOK_PREFIX).as_deref(), Some(""));
    }

    #[test]
    fn unknown_suffix_still_routes_for_the_404_match() {
        assert_eq!(
            route_hook(&format!("{HOOK_PREFIX}/nope"), HOOK_PREFIX).as_deref(),
            Some("nope")
        );
    }
}
