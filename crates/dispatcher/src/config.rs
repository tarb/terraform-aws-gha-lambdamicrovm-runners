//! Environment configuration — identical variable names, parsing semantics
//! and defaults to the Python dispatcher's module-level globals.

use crate::pyfmt::PyErr;
use std::collections::BTreeSet;

pub const SECRET_TTL: i64 = 60;
pub const IMG_TTL: i64 = 60;

/// States that occupy real capacity (see the Python comment: only
/// PENDING/RUNNING hold cap capacity).
pub const ACTIVE_STATES: [&str; 2] = ["PENDING", "RUNNING"];

#[derive(Debug, Clone)]
pub struct Config {
    #[allow(dead_code)] // required at startup like Python's os.environ["AWS_REGION"]
    pub region: String,
    pub image_arn: String,
    /// Optional pin; `None` => resolve latest at runtime (empty env == unset).
    pub image_version: Option<String>,
    pub exec_role_arn: String,
    pub egress: String,
    pub max_duration: i64,
    pub log_group: String,
    pub param_name: String,
    pub app_secret_arn: Option<String>,
    pub gh_api: String,
    pub required_labels: BTreeSet<String>,
    pub runner_labels: Vec<String>,
    pub max_concurrency: i64,
    pub pool_enabled: bool,
    pub handoff_window: i64,
    pub handoff_prefix: String,
    pub pool_max_size: i64,
    pub suspend_delay: i64,
    pub sweep_min_age: i64,
    pub pool_suspend_grace: i64,
}

impl Config {
    pub fn from_env() -> Result<Self, PyErr> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Build from a lookup function (pure; unit-testable without touching
    /// process env).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, PyErr> {
        let required = |k: &str| get(k).ok_or_else(|| PyErr::key_error(k));
        let int = |k: &str, default: &str| -> Result<i64, PyErr> {
            let raw = get(k).unwrap_or_else(|| default.to_string());
            raw.trim().parse::<i64>().map_err(|_| {
                PyErr::value_error(format!("invalid literal for int() with base 10: '{raw}'"))
            })
        };

        let required_labels: BTreeSet<String> = get("REQUIRED_LABELS")
            .unwrap_or_else(|| "self-hosted,microvm".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let runner_labels: Vec<String> = get("RUNNER_LABELS")
            .unwrap_or_else(|| "self-hosted,linux,arm64,microvm".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();

        Ok(Self {
            region: required("AWS_REGION")?,
            image_arn: required("IMAGE_ARN")?,
            image_version: get("IMAGE_VERSION").filter(|s| !s.is_empty()),
            exec_role_arn: required("EXEC_ROLE_ARN")?,
            egress: required("EGRESS_CONNECTOR")?,
            max_duration: int("MAX_DURATION", "1200")?,
            log_group: get("LOG_GROUP")
                .unwrap_or_else(|| "/aws/lambda-microvms/github-actions-runner".to_string()),
            param_name: required("PARAM_NAME")?,
            // Python reads APP_SECRET_ARN; GITHUB_APP_SECRET_ARN accepted as
            // an alias for new deployments (see contract notes).
            app_secret_arn: get("APP_SECRET_ARN")
                .filter(|s| !s.is_empty())
                .or_else(|| get("GITHUB_APP_SECRET_ARN").filter(|s| !s.is_empty())),
            gh_api: get("GH_API_URL").unwrap_or_else(|| "https://api.github.com".to_string()),
            required_labels,
            runner_labels,
            max_concurrency: int("MAX_CONCURRENCY", "0")?,
            pool_enabled: matches!(
                get("POOL_ENABLED")
                    .unwrap_or_else(|| "false".to_string())
                    .to_lowercase()
                    .as_str(),
                "1" | "true" | "yes"
            ),
            handoff_window: int("HANDOFF_WINDOW_SECONDS", "90")?,
            handoff_prefix: get("HANDOFF_PREFIX")
                .unwrap_or_else(|| "/gha-microvm/handoff".to_string())
                .trim_end_matches('/')
                .to_string(),
            pool_max_size: int("POOL_MAX_SIZE", "4")?,
            suspend_delay: int("SUSPEND_DELAY_SECONDS", "20")?,
            sweep_min_age: int("SWEEP_MIN_AGE_SECONDS", "360")?,
            pool_suspend_grace: int("POOL_SUSPEND_GRACE_SECONDS", "300")?,
        })
    }

    /// The near-EOL threshold: `max(MAX_DURATION - 900, MAX_DURATION * 0.5)`.
    pub fn eol_threshold(&self) -> f64 {
        ((self.max_duration - 900) as f64).max(self.max_duration as f64 * 0.5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn base_env() -> HashMap<String, String> {
        [
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
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn cfg_with(extra: &[(&str, &str)]) -> Config {
        let mut env = base_env();
        for (k, v) in extra {
            env.insert(k.to_string(), v.to_string());
        }
        Config::from_lookup(|k| env.get(k).cloned()).unwrap()
    }

    #[test]
    fn defaults_match_python() {
        let c = cfg_with(&[]);
        assert_eq!(c.max_duration, 1200);
        assert_eq!(c.max_concurrency, 0);
        assert!(!c.pool_enabled);
        assert_eq!(c.handoff_window, 90);
        assert_eq!(c.handoff_prefix, "/gha-microvm/handoff");
        assert_eq!(c.pool_max_size, 4);
        assert_eq!(c.suspend_delay, 20);
        assert_eq!(c.sweep_min_age, 360);
        assert_eq!(c.pool_suspend_grace, 300);
        assert_eq!(c.gh_api, "https://api.github.com");
        assert_eq!(c.log_group, "/aws/lambda-microvms/github-actions-runner");
        assert!(c.image_version.is_none());
        assert!(c.app_secret_arn.is_none());
        assert_eq!(
            c.required_labels.iter().cloned().collect::<Vec<_>>(),
            vec!["microvm", "self-hosted"]
        );
        assert_eq!(
            c.runner_labels,
            vec!["self-hosted", "linux", "arm64", "microvm"]
        );
    }

    #[test]
    fn pool_enabled_accepts_python_truthy_spellings() {
        for v in ["1", "true", "TRUE", "True", "yes", "YES"] {
            assert!(cfg_with(&[("POOL_ENABLED", v)]).pool_enabled, "{v}");
        }
        for v in ["0", "false", "no", "on", ""] {
            assert!(!cfg_with(&[("POOL_ENABLED", v)]).pool_enabled, "{v:?}");
        }
    }

    #[test]
    fn label_lists_strip_and_drop_empties() {
        let c = cfg_with(&[("REQUIRED_LABELS", " a , ,b,"), ("RUNNER_LABELS", "x, y ,")]);
        assert_eq!(
            c.required_labels.iter().cloned().collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(c.runner_labels, vec!["x", "y"]);
    }

    #[test]
    fn handoff_prefix_rstrips_slashes() {
        assert_eq!(
            cfg_with(&[("HANDOFF_PREFIX", "/p/handoff//")]).handoff_prefix,
            "/p/handoff"
        );
    }

    #[test]
    fn empty_image_version_means_unpinned() {
        assert!(cfg_with(&[("IMAGE_VERSION", "")]).image_version.is_none());
        assert_eq!(
            cfg_with(&[("IMAGE_VERSION", "7")]).image_version.as_deref(),
            Some("7")
        );
    }

    #[test]
    fn empty_app_secret_arn_is_falsy() {
        assert!(cfg_with(&[("APP_SECRET_ARN", "")]).app_secret_arn.is_none());
        assert_eq!(
            cfg_with(&[("APP_SECRET_ARN", "arn:x")])
                .app_secret_arn
                .as_deref(),
            Some("arn:x")
        );
    }

    #[test]
    fn missing_required_env_errors() {
        let mut env = base_env();
        env.remove("IMAGE_ARN");
        let e = Config::from_lookup(|k| env.get(k).cloned()).unwrap_err();
        assert_eq!(e.kind, "KeyError");
    }

    #[test]
    fn eol_threshold_matches_python_max() {
        assert_eq!(cfg_with(&[]).eol_threshold(), 600.0); // max(300, 600)
        assert_eq!(
            cfg_with(&[("MAX_DURATION", "3600")]).eol_threshold(),
            2700.0
        ); // max(2700, 1800)
    }
}
