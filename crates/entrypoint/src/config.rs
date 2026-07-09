//! Environment configuration for the in-guest supervisor.
//!
//! An unparsable numeric env var is fatal at startup: a wrong `HOOK_PORT`
//! would leave the lifecycle service unable to reach the VM at all, and the
//! rlimit/watchdog numbers gate everything spawned afterwards.

/// Hook route prefix. Service-pinned: the MicroVM lifecycle service POSTs
/// its hooks here. Not configurable.
pub const HOOK_PREFIX: &str = "/aws/lambda-microvms/runtime/v1";

/// Runner labels used when neither the /run payload nor `RUNNER_LABELS`
/// provides any.
pub const DEFAULT_LABELS: &str = "self-hosted,linux,arm64,microvm";

#[derive(Debug, Clone)]
pub struct Config {
    pub hook_port: u16,
    pub runner_dir: String,
    pub gh_api: String,
    pub enable_docker: bool,
    pub docker_storage_driver: String,
    pub nofile_soft: u64,
    pub nofile_hard: u64,
    pub idle_grace_seconds: i64,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            hook_port: env_or("HOOK_PORT", "9000")
                .parse()
                .expect("HOOK_PORT must be an integer"),
            runner_dir: env_or("RUNNER_DIR", "/opt/actions-runner"),
            gh_api: env_or("GH_API_URL", "https://api.github.com"),
            enable_docker: parse_flag(&env_or("ENABLE_DOCKER", "true")),
            docker_storage_driver: env_or("DOCKER_STORAGE_DRIVER", "overlay2"),
            nofile_soft: env_or("NOFILE_SOFT", "65536")
                .parse()
                .expect("NOFILE_SOFT must be an integer"),
            nofile_hard: env_or("NOFILE_HARD", "1048576")
                .parse()
                .expect("NOFILE_HARD must be an integer"),
            idle_grace_seconds: env_or("IDLE_GRACE_SECONDS", "120")
                .parse()
                .expect("IDLE_GRACE_SECONDS must be an integer"),
        }
    }
}

/// The env var's value, or `default` when unset. Present-but-empty stays
/// empty — only [`env_nonempty`] treats empty as absent.
pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// The env var's value, with empty treated as absent (run-time fallbacks
/// like `GH_URL`/`GH_PAT` use this so an empty export doesn't shadow the
/// payload).
pub fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Flag parsing: `1`/`true`/`yes`, case-insensitive; everything else false.
pub fn parse_flag(value: &str) -> bool {
    matches!(value.to_lowercase().as_str(), "1" | "true" | "yes")
}

/// Region for AWS calls and log lines: `AWS_REGION`, then
/// `AWS_DEFAULT_REGION`, then `us-east-1`. Empty values fall through.
pub fn region_label() -> String {
    for key in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Ok(v) = std::env::var(key)
            && !v.is_empty()
        {
            return v;
        }
    }
    "us-east-1".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_parsing_accepts_only_the_documented_spellings() {
        for v in ["1", "true", "TRUE", "True", "yes", "YES", "Yes"] {
            assert!(parse_flag(v), "{v} should be true");
        }
        for v in ["0", "false", "no", "", "on", "enabled", "y"] {
            assert!(!parse_flag(v), "{v} should be false");
        }
    }
}
