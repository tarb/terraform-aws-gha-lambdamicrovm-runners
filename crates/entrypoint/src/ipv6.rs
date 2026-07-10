//! Optional guest IPv6 disable (`DISABLE_IPV6`), applied at supervisor boot.
//!
//! Why: the fleet's egress connector can be IPv4-only (NetworkConnector
//! VpcEgressConfiguration `network_protocol = "IPv4"`), but the guest has no
//! way to know — dual-stack clients (observed: bun's highly concurrent
//! package fetches) burn a happy-eyeballs IPv6 attempt per connection
//! against a protocol that can never work, a per-connection tax that turned
//! a ~1min install into ~11min. When the operator knows the connector is
//! v4-only, `DISABLE_IPV6=1` (baked into the image env by the module's
//! `disable_guest_ipv6` variable) removes the entire class by flipping the
//! kernel's `disable_ipv6` sysctls before anything dials out.

use crate::config::{env_or, parse_flag};
use crate::logfmt::log;
use std::path::Path;

/// Kernel sysctl root for the IPv6 knobs.
const SYSCTL_IPV6_BASE: &str = "/proc/sys/net/ipv6";

/// Disable guest IPv6 when the `DISABLE_IPV6` env flag is truthy (same
/// lenient parsing as every other flag: `1`/`true`/`yes`). No-op when the
/// flag is unset or falsy. Called at boot, before any child process — the
/// runner and everything it spawns then see an IPv6-less stack.
pub fn disable_ipv6_if_requested() {
    if !parse_flag(&env_or("DISABLE_IPV6", "")) {
        return;
    }
    if disable_ipv6(Path::new(SYSCTL_IPV6_BASE)) {
        log("ipv6 disabled (DISABLE_IPV6 set - v4-only egress)");
    }
}

/// Write `1` to `conf/{all,default}/disable_ipv6` under `base`. Absent paths
/// (kernel built without IPv6) and write failures are tolerated with one
/// WARN each — a v4-only guest must still boot. Returns true only when every
/// write landed. `base` is a parameter so tests run against a temp dir.
fn disable_ipv6(base: &Path) -> bool {
    let mut all_ok = true;
    for scope in ["all", "default"] {
        let path = base.join("conf").join(scope).join("disable_ipv6");
        if let Err(e) = std::fs::write(&path, "1") {
            log(format!(
                "WARN: could not disable ipv6 at {}: {e}",
                path.display()
            ));
            all_ok = false;
        }
    }
    all_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testsupport::temp_dir;
    use std::path::PathBuf;

    #[test]
    fn writes_one_to_both_scopes() {
        let base = PathBuf::from(temp_dir("ipv6-set"));
        for scope in ["all", "default"] {
            std::fs::create_dir_all(base.join("conf").join(scope)).unwrap();
        }
        assert!(disable_ipv6(&base));
        for scope in ["all", "default"] {
            let path = base.join("conf").join(scope).join("disable_ipv6");
            assert_eq!(std::fs::read_to_string(path).unwrap(), "1");
        }
    }

    #[test]
    fn absent_sysctl_tree_is_tolerated() {
        // Kernel without IPv6: the conf tree simply isn't there.
        let base = PathBuf::from(temp_dir("ipv6-absent")).join("missing");
        assert!(!disable_ipv6(&base));
    }

    #[test]
    fn unwritable_scope_warns_but_still_writes_the_other() {
        let base = PathBuf::from(temp_dir("ipv6-unwritable"));
        // A directory where the file should be fails the write (EISDIR)
        // regardless of privileges — unlike a chmod, this also fails as root.
        std::fs::create_dir_all(base.join("conf/all/disable_ipv6")).unwrap();
        std::fs::create_dir_all(base.join("conf/default")).unwrap();
        assert!(!disable_ipv6(&base));
        let default = base.join("conf/default/disable_ipv6");
        assert_eq!(std::fs::read_to_string(default).unwrap(), "1");
    }
}
