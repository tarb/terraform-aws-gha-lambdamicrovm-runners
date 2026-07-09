//! RLIMIT_NOFILE handling — port of `_raise_nofile_rlimit` and
//! `_container_default_nofile`. The guest gives PID-1 a hard nofile of 1024;
//! dockerd and the runner inherit ours, so raise it before anything spawns.

use crate::config::Config;
use crate::util::log;

/// Raise our own RLIMIT_NOFILE before anything is spawned. Root can raise
/// the hard limit up to fs.nr_open; if that is somehow refused, still max
/// the soft limit under the existing cap.
pub fn raise_nofile_rlimit(cfg: &Config) {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } != 0 {
        log(format!(
            "WARN: could not read nofile rlimit: {}",
            std::io::Error::last_os_error()
        ));
        return;
    }
    let (soft, hard) = (rl.rlim_cur, rl.rlim_max);
    let want_soft = soft.max(cfg.nofile_soft as libc::rlim_t);
    let want_hard = hard.max(cfg.nofile_hard as libc::rlim_t);
    let mut last_err = String::new();
    for (ws, wh) in [(want_soft, want_hard), (hard, hard)] {
        let new = libc::rlimit {
            rlim_cur: ws.min(wh),
            rlim_max: wh,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) } == 0 {
            log(format!(
                "nofile rlimit {soft}:{hard} -> {}:{}",
                new.rlim_cur, new.rlim_max
            ));
            return;
        }
        last_err = std::io::Error::last_os_error().to_string();
    }
    log(format!(
        "WARN: could not raise nofile rlimit (still {soft}:{hard}): {last_err}"
    ));
}

/// `--default-ulimit` value for containers, clamped to our achieved hard
/// limit (what setrlimit accepted is the proven-safe ceiling — a too-high
/// value would fail every container start).
pub fn container_default_nofile(cfg: &Config) -> String {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let achieved_hard = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
        rl.rlim_max
    } else {
        cfg.nofile_hard
    };
    format_default_ulimit(cfg.nofile_soft, cfg.nofile_hard, achieved_hard)
}

/// Pure formatting/clamping half of `_container_default_nofile`, split out
/// for tests.
pub fn format_default_ulimit(cfg_soft: u64, cfg_hard: u64, achieved_hard: u64) -> String {
    let hard = if achieved_hard == libc::RLIM_INFINITY {
        cfg_hard
    } else {
        achieved_hard
    };
    format!("nofile={}:{}", cfg_soft.min(hard), hard)
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
            format_default_ulimit(65536, 1048576, libc::RLIM_INFINITY),
            "nofile=65536:1048576"
        );
    }
}
