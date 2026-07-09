//! RLIMIT_NOFILE handling. The guest gives PID-1 a hard nofile of 1024;
//! dockerd and the runner inherit ours, so raise it before anything spawns.

use crate::config::Config;
use crate::logfmt::log;
use nix::sys::resource::{RLIM_INFINITY, Resource, getrlimit, setrlimit};

/// Raise our own RLIMIT_NOFILE before anything is spawned. Root can raise
/// the hard limit up to fs.nr_open; if that is somehow refused, still max
/// the soft limit under the existing cap.
pub fn raise_nofile_rlimit(cfg: &Config) {
    let (soft, hard) = match getrlimit(Resource::RLIMIT_NOFILE) {
        Ok(v) => v,
        Err(e) => {
            log(format!("WARN: could not read nofile rlimit: {e}"));
            return;
        }
    };
    let want_soft = soft.max(cfg.nofile_soft);
    let want_hard = hard.max(cfg.nofile_hard);
    let mut last_err = String::new();
    for (ws, wh) in [(want_soft, want_hard), (hard, hard)] {
        let cur = ws.min(wh);
        match setrlimit(Resource::RLIMIT_NOFILE, cur, wh) {
            Ok(()) => {
                log(format!("nofile rlimit {soft}:{hard} -> {cur}:{wh}"));
                return;
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    log(format!(
        "WARN: could not raise nofile rlimit (still {soft}:{hard}): {last_err}"
    ));
}

/// `--default-ulimit` value for containers, clamped to our achieved hard
/// limit (what setrlimit accepted is the proven-safe ceiling — a too-high
/// value would fail every container start).
pub fn container_default_nofile(nofile_soft: u64, nofile_hard: u64) -> String {
    let achieved_hard = getrlimit(Resource::RLIMIT_NOFILE)
        .map(|(_, hard)| hard)
        .unwrap_or(nofile_hard);
    format_default_ulimit(nofile_soft, nofile_hard, achieved_hard)
}

/// Pure formatting/clamping half of [`container_default_nofile`], split out
/// for tests.
pub fn format_default_ulimit(cfg_soft: u64, cfg_hard: u64, achieved_hard: u64) -> String {
    let hard = if achieved_hard == RLIM_INFINITY {
        cfg_hard
    } else {
        achieved_hard
    };
    format!("nofile={}:{hard}", cfg_soft.min(hard))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ulimit_clamps_to_achieved_hard() {
        assert_eq!(
            format_default_ulimit(65536, 1048576, 1048576),
            "nofile=65536:1048576"
        );
        // Hard raise refused: both values clamp to the real ceiling.
        assert_eq!(
            format_default_ulimit(65536, 1048576, 1024),
            "nofile=1024:1024"
        );
        // Infinity falls back to the configured hard value.
        assert_eq!(
            format_default_ulimit(65536, 1048576, RLIM_INFINITY),
            "nofile=65536:1048576"
        );
    }
}
