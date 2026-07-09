//! Hand-rolled fakes for the Platform seam (no mocking frameworks), plus the
//! `vm_item`/`client_error` helpers mirroring tests/conftest.py.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::app::Caches;
use crate::config::Config;
use crate::platform::{AwsErr, ImageInfo, ParamMeta, Platform, RawVm, RunVmRequest, VmPage};
use crate::pyfmt::PyErr;

pub const NOW: f64 = 1_700_000_000.0;
pub const DEFAULT_VM_ID: &str = "microvm-aaaa1111-2222-3333-4444-555566667777";

/// conftest.vm_item(): one raw fleet record.
pub fn vm_item(vmid: &str, state: &str, version: &str, started_at: Option<f64>) -> RawVm {
    RawVm {
        microvm_id: Some(vmid.to_string()),
        state: Some(state.to_string()),
        image_version: Some(version.to_string()),
        started_at: Some(started_at.unwrap_or(NOW)),
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
    }
}

pub fn vm(state: &str) -> RawVm {
    vm_item(DEFAULT_VM_ID, state, "9", None)
}

/// conftest.client_error(): a botocore-shaped service error.
pub fn client_error(code: &str) -> AwsErr {
    AwsErr {
        code: Some(code.to_string()),
        message: format!("An error occurred ({code}) when calling the Op operation: {code}"),
        service: true,
    }
}

/// The conftest env, as a Config.
pub fn test_cfg() -> Config {
    let env: HashMap<&str, &str> = [
        ("AWS_REGION", "us-east-1"),
        (
            "IMAGE_ARN",
            "arn:aws:lambda:us-east-1:111122223333:microvm-image:test",
        ),
        ("EXEC_ROLE_ARN", "arn:aws:iam::111122223333:role/test-exec"),
        (
            "EGRESS_CONNECTOR",
            "arn:aws:lambda:us-east-1:111122223333:network-connector:test",
        ),
        ("PARAM_NAME", "/test/dispatcher"),
    ]
    .into_iter()
    .collect();
    Config::from_lookup(|k| env.get(k).map(|s| s.to_string())).unwrap()
}

pub fn quiet_caches() -> Caches {
    let caches = Caches::default();
    // conftest: silence the record-keys canary in tests
    caches.vm_keys_logged.store(true, Ordering::SeqCst);
    caches
}

/// A canned GitHub response, matched by URL substring.
pub struct GhRule {
    pub url_contains: String,
    pub response: Result<(u16, Value), String>,
}

#[derive(Default)]
pub struct FakePlatform {
    /// Pages consumed one per ListMicrovms call; when exhausted, the last
    /// page repeats (MagicMock return_value semantics).
    pub list_pages: Mutex<Vec<VmPage>>,
    pub list_error: Option<AwsErr>,
    pub list_calls: AtomicUsize,
    pub get_state: Mutex<Option<Result<String, AwsErr>>>,
    pub image_latest: Mutex<Option<String>>,
    pub run_result: Mutex<Option<Result<Option<String>, AwsErr>>>,
    pub run_payloads: Mutex<Vec<String>>,
    pub resume_calls: Mutex<Vec<String>>,
    pub resume_error: Option<AwsErr>,
    pub suspend_calls: Mutex<Vec<String>>,
    pub terminate_calls: Mutex<Vec<String>>,
    /// SSM parameter store; `put` writes here, `delete` removes.
    pub params: Mutex<HashMap<String, String>>,
    /// Forced per-name get_parameter results, overriding the store.
    pub forced_get: Mutex<HashMap<String, Result<String, AwsErr>>>,
    pub put_calls: Mutex<Vec<(String, String, String)>>, // (name, value, type)
    pub delete_calls: Mutex<Vec<String>>,
    pub by_path: Mutex<Vec<ParamMeta>>,
    pub secrets: Mutex<HashMap<String, String>>,
    pub gh_rules: Mutex<Vec<GhRule>>,
    pub gh_calls: Mutex<Vec<(String, String)>>, // (method, url)
    /// Ordered side-effect log for park-before-resume style assertions.
    pub events: Mutex<Vec<(String, String)>>,
    pub sleeps: Mutex<Vec<f64>>,
    /// Fake clock: `sleep` advances it, so polling loops always terminate.
    pub now_v: Mutex<f64>,
}

impl FakePlatform {
    pub fn new() -> Self {
        let f = Self::default();
        *f.image_latest.lock().unwrap() = Some("9".to_string());
        *f.now_v.lock().unwrap() = NOW;
        f
    }

    pub fn with_vms(self, items: Vec<RawVm>) -> Self {
        *self.list_pages.lock().unwrap() = vec![VmPage {
            items,
            next_token: None,
        }];
        self
    }

    pub fn event_kinds(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub fn add_gh_rule(&self, url_contains: &str, response: Result<(u16, Value), String>) {
        self.gh_rules.lock().unwrap().push(GhRule {
            url_contains: url_contains.to_string(),
            response,
        });
    }

    /// Arm the standard secret bundle + token mint so `_secrets` /
    /// `_token_for_repo` work end-to-end against the fake.
    pub fn arm_github_auth(&self) {
        self.params.lock().unwrap().insert(
            "/test/dispatcher".to_string(),
            r#"{"webhook_secret": "x", "app_id": 1, "app_private_key": "test-pem"}"#.to_string(),
        );
        self.add_gh_rule(
            "/access_tokens",
            Ok((
                201,
                serde_json::json!({"token": "tok", "expires_at": "2099-01-01T00:00:00Z"}),
            )),
        );
    }
}

impl Platform for FakePlatform {
    async fn mv_list_page(
        &self,
        _image_arn: &str,
        next_token: Option<&str>,
    ) -> Result<VmPage, AwsErr> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(e) = &self.list_error {
            return Err(e.clone());
        }
        let pages = self.list_pages.lock().unwrap();
        if pages.is_empty() {
            return Ok(VmPage::default());
        }
        // Sequential pages keyed by the token round-trip.
        let idx = next_token
            .and_then(|t| t.parse::<usize>().ok())
            .unwrap_or(0);
        let mut page = pages[idx.min(pages.len() - 1)].clone();
        page.next_token = if idx + 1 < pages.len() {
            Some((idx + 1).to_string())
        } else {
            None
        };
        Ok(page)
    }

    async fn mv_get_state(&self, _id: &str) -> Result<String, AwsErr> {
        self.get_state
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Err(client_error("ResourceNotFoundException")))
    }

    async fn mv_get_image(&self, _image_arn: &str) -> Result<ImageInfo, AwsErr> {
        Ok(ImageInfo {
            latest_active_image_version: self.image_latest.lock().unwrap().clone(),
            state: Some("ACTIVE".to_string()),
        })
    }

    async fn mv_run(&self, req: RunVmRequest<'_>) -> Result<Option<String>, AwsErr> {
        self.run_payloads
            .lock()
            .unwrap()
            .push(req.run_hook_payload.to_string());
        self.events
            .lock()
            .unwrap()
            .push(("run".to_string(), String::new()));
        self.run_result
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(Ok(Some("microvm-new".to_string())))
    }

    async fn mv_resume(&self, id: &str) -> Result<(), AwsErr> {
        self.events
            .lock()
            .unwrap()
            .push(("resume".to_string(), id.to_string()));
        self.resume_calls.lock().unwrap().push(id.to_string());
        match &self.resume_error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    async fn mv_suspend(&self, id: &str) -> Result<(), AwsErr> {
        self.events
            .lock()
            .unwrap()
            .push(("suspend".to_string(), id.to_string()));
        self.suspend_calls.lock().unwrap().push(id.to_string());
        Ok(())
    }

    async fn mv_terminate(&self, id: &str) -> Result<(), AwsErr> {
        self.events
            .lock()
            .unwrap()
            .push(("terminate".to_string(), id.to_string()));
        self.terminate_calls.lock().unwrap().push(id.to_string());
        Ok(())
    }

    async fn ssm_get_parameter(&self, name: &str, _decrypt: bool) -> Result<String, AwsErr> {
        if let Some(r) = self.forced_get.lock().unwrap().get(name) {
            return r.clone();
        }
        self.params
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| client_error("ParameterNotFound"))
    }

    async fn ssm_put_secure(&self, name: &str, value: &str) -> Result<(), AwsErr> {
        self.events
            .lock()
            .unwrap()
            .push(("put".to_string(), name.to_string()));
        self.put_calls.lock().unwrap().push((
            name.to_string(),
            value.to_string(),
            "SecureString".to_string(),
        ));
        self.params
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
        Ok(())
    }

    async fn ssm_delete(&self, name: &str) -> Result<(), AwsErr> {
        self.events
            .lock()
            .unwrap()
            .push(("delete".to_string(), name.to_string()));
        self.delete_calls.lock().unwrap().push(name.to_string());
        self.params.lock().unwrap().remove(name);
        Ok(())
    }

    async fn ssm_by_path(&self, _path: &str) -> Result<Vec<ParamMeta>, AwsErr> {
        Ok(self.by_path.lock().unwrap().clone())
    }

    async fn sm_get_secret(&self, arn: &str) -> Result<String, AwsErr> {
        self.secrets
            .lock()
            .unwrap()
            .get(arn)
            .cloned()
            .ok_or_else(|| client_error("ResourceNotFoundException"))
    }

    async fn gh_call(
        &self,
        method: &str,
        url: &str,
        _token: &str,
        _body: Option<&Value>,
    ) -> Result<(u16, Value), PyErr> {
        self.gh_calls
            .lock()
            .unwrap()
            .push((method.to_string(), url.to_string()));
        let rules = self.gh_rules.lock().unwrap();
        for rule in rules.iter() {
            if url.contains(&rule.url_contains) {
                return rule
                    .response
                    .clone()
                    .map_err(|m| PyErr::new("HTTPError", m));
            }
        }
        Err(PyErr::new(
            "HTTPError",
            format!("HTTP Error 404: Not Found ({url})"),
        ))
    }

    fn app_jwt(&self, _app_id: &str, _pem: &str) -> Result<String, PyErr> {
        Ok("fake-jwt".to_string())
    }

    fn now(&self) -> f64 {
        *self.now_v.lock().unwrap()
    }

    async fn sleep(&self, secs: f64) {
        self.sleeps.lock().unwrap().push(secs);
        *self.now_v.lock().unwrap() += secs;
    }
}
