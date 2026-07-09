//! GitHub API calls — port of `gh_api`, `runners_api_for` and
//! `mint_jitconfig`. Same headers, same 30s timeout, and like
//! `urllib.request` a non-2xx status is an error (callers rely on that to
//! fall back / bubble into `start_runner`'s catch-all). Tokens are never
//! logged and never appear in error strings.

use crate::util::log;
use serde_json::Value;
use std::time::Duration;

pub async fn gh_api(
    client: &reqwest::Client,
    method: &str,
    url: &str,
    token: &str,
    body: Option<Value>,
) -> Result<(u16, Value), String> {
    let m = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| format!("invalid method {method}"))?;
    let mut req = client
        .request(m, url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Content-Type", "application/json")
        .header("User-Agent", "lambda-microvm-runner")
        .timeout(Duration::from_secs(30));
    if let Some(b) = &body {
        req = req.body(serde_json::to_vec(b).map_err(|e| e.to_string())?);
    }
    // reqwest errors never include request headers, so the token can't leak.
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        // urllib raises HTTPError on >= 400; message shape approximated.
        return Err(format!("HTTP Error {status}: {url}"));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let value = if bytes.is_empty() {
        Value::Null // json.loads(r.read() or b"null")
    } else {
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())?
    };
    Ok((status, value))
}

/// Map `https://github.com/OWNER/REPO` -> repo runners API; `.../ORG` -> org.
pub fn runners_api_for(gh_api_base: &str, github_url: &str) -> Result<String, String> {
    // github_url.split("://", 1)[-1]
    let rest = github_url.split_once("://").map_or(github_url, |(_, r)| r);
    // .split("/", 1)[1] — IndexError (an error here) when there is no path.
    let path = rest
        .split_once('/')
        .ok_or_else(|| format!("no owner/repo path in github url {github_url}"))?
        .1
        .trim_matches('/');
    if path.contains('/') {
        Ok(format!("{gh_api_base}/repos/{path}/actions/runners"))
    } else {
        Ok(format!("{gh_api_base}/orgs/{path}/actions/runners"))
    }
}

/// Request body for `generate-jitconfig`, with the Python's
/// `int(runner_group) if str(runner_group or "").isdigit() else 1`
/// runner-group derivation and empty-label filtering.
pub fn jit_request_body(name: &str, labels: &str, runner_group: Option<&Value>) -> Value {
    let group_id: i64 = match runner_group {
        Some(Value::String(s)) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
            s.parse().unwrap_or(1)
        }
        // JSON integers: str(5) == "5" is all digits; negatives/floats are not.
        Some(Value::Number(n)) => n.as_i64().filter(|v| *v > 0).unwrap_or(1),
        _ => 1,
    };
    let label_list: Vec<&str> = labels.split(',').filter(|s| !s.is_empty()).collect();
    serde_json::json!({
        "name": name,
        "runner_group_id": group_id,
        "labels": label_list,
        "work_folder": "_work",
    })
}

/// Create a JIT runner config via `generate-jitconfig`. Returns the
/// `encoded_jit_config` blob, or `None` on any failure so the caller falls
/// back to config.sh registration.
pub async fn mint_jitconfig(
    client: &reqwest::Client,
    runners_api: &str,
    token: &str,
    name: &str,
    labels: &str,
    runner_group: Option<&Value>,
) -> Option<String> {
    let body = jit_request_body(name, labels, runner_group);
    match gh_api(
        client,
        "POST",
        &format!("{runners_api}/generate-jitconfig"),
        token,
        Some(body),
    )
    .await
    {
        Ok((_, cfg)) => cfg
            .get("encoded_jit_config")
            .and_then(Value::as_str)
            .map(str::to_string),
        Err(e) => {
            log(format!(
                "generate-jitconfig failed ({e}); falling back to config.sh registration"
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const API: &str = "https://api.github.com";

    #[test]
    fn repo_url_maps_to_repo_runners_api() {
        assert_eq!(
            runners_api_for(API, "https://github.com/octo/repo").unwrap(),
            "https://api.github.com/repos/octo/repo/actions/runners"
        );
    }

    #[test]
    fn org_url_maps_to_org_runners_api() {
        assert_eq!(
            runners_api_for(API, "https://github.com/octo-org").unwrap(),
            "https://api.github.com/orgs/octo-org/actions/runners"
        );
        // Trailing slash is stripped like Python's .strip("/").
        assert_eq!(
            runners_api_for(API, "https://github.com/octo-org/").unwrap(),
            "https://api.github.com/orgs/octo-org/actions/runners"
        );
    }

    #[test]
    fn schemeless_url_still_parses() {
        assert_eq!(
            runners_api_for(API, "github.com/o/r").unwrap(),
            "https://api.github.com/repos/o/r/actions/runners"
        );
    }

    #[test]
    fn bare_host_is_an_error() {
        assert!(runners_api_for(API, "https://github.com").is_err());
    }

    #[test]
    fn jit_body_runner_group_derivation() {
        let b = jit_request_body("n", "a,b", None);
        assert_eq!(b["runner_group_id"], 1);
        assert_eq!(
            jit_request_body("n", "l", Some(&json!("5")))["runner_group_id"],
            5
        );
        assert_eq!(
            jit_request_body("n", "l", Some(&json!("abc")))["runner_group_id"],
            1
        );
        assert_eq!(
            jit_request_body("n", "l", Some(&json!(7)))["runner_group_id"],
            7
        );
        assert_eq!(
            jit_request_body("n", "l", Some(&json!(0)))["runner_group_id"],
            1
        );
        assert_eq!(
            jit_request_body("n", "l", Some(&json!(-3)))["runner_group_id"],
            1
        );
        assert_eq!(
            jit_request_body("n", "l", Some(&json!(null)))["runner_group_id"],
            1
        );
    }

    #[test]
    fn jit_body_filters_empty_labels_and_sets_work_folder() {
        let b = jit_request_body("runner-x", "self-hosted,,linux,", None);
        assert_eq!(b["labels"], json!(["self-hosted", "linux"]));
        assert_eq!(b["work_folder"], "_work");
        assert_eq!(b["name"], "runner-x");
    }
}
