//! Optional guest IPv6 blackhole (`DISABLE_IPV6`), applied at supervisor
//! boot.
//!
//! Why: the fleet's egress connector can be IPv4-only (NetworkConnector
//! VpcEgressConfiguration `network_protocol = "IPv4"`), but the guest has no
//! way to know — dual-stack clients (observed: bun's highly concurrent
//! package fetches) burn a happy-eyeballs IPv6 attempt per connection
//! against a protocol that can never work, a per-connection tax that turned
//! a ~1min install into ~11min. When the operator knows the connector is
//! v4-only, `DISABLE_IPV6=1` (baked into the image env by the module's
//! `disable_guest_ipv6` variable) removes the entire class before anything
//! dials out.
//!
//! Mechanism — an UNREACHABLE default route, NOT a stack disable: we run
//! `ip -6 route replace unreachable default metric 1` and set
//! `net.ipv6.conf.{all,default}.accept_ra=0`. Global v6 destinations then
//! fail instantly with ENETUNREACH (happy-eyeballs falls back to v4 with
//! zero timeout) while link-local and loopback IPv6 keep working.
//!
//! DO NOT "simplify" this back to the `disable_ipv6` sysctls. v0.0.5 did
//! exactly that, and every image build with the flag on failed
//! NotStabilized: the platform's lifecycle READY probe (which boots a VM
//! from the candidate image and dials the hook server) depends on IPv6 —
//! most plausibly link-local — somewhere in its channel, so disabling the
//! whole v6 stack breaks the platform contract. The unreachable route only
//! kills GLOBAL v6 routing and leaves the stack (and the probe's channel)
//! intact.

use crate::config::{env_or, parse_flag};
use crate::logfmt::{log, truncate_chars};
use crate::supervisor::exit_code;
use std::path::Path;

/// Kernel sysctl root for the IPv6 knobs.
const SYSCTL_IPV6_BASE: &str = "/proc/sys/net/ipv6";

/// The blackhole: every global v6 destination resolves to an unreachable
/// route, so connects fail instantly with ENETUNREACH. `metric 1` outranks
/// any RA-installed default (those land around metric 1024) that might slip
/// in before the accept_ra writes below take effect; `replace` is idempotent
/// across warm-pool resumes.
const BLACKHOLE_ROUTE_ARGV: &[&str] = &[
    "ip",
    "-6",
    "route",
    "replace",
    "unreachable",
    "default",
    "metric",
    "1",
];

/// Blackhole global guest IPv6 when the `DISABLE_IPV6` env flag is truthy
/// (same lenient parsing as every other flag: `1`/`true`/`yes`). No-op when
/// the flag is unset or falsy. Called at boot, before any child process —
/// the runner and everything it spawns then see instant-fail global v6 and
/// fall back to v4, while link-local v6 stays up for the platform's hook
/// channel.
pub fn blackhole_ipv6_if_requested() {
    blackhole_ipv6(
        &env_or("DISABLE_IPV6", ""),
        &SystemCommands,
        Path::new(SYSCTL_IPV6_BASE),
    );
}

/// Gate + both mechanisms. Every failure warns and continues — a v4-only
/// guest must still boot — and exactly one success line is logged when both
/// the route and the accept_ra writes landed. Parameterised (flag value,
/// command seam, sysctl base) so tests drive it without touching the
/// process env, the real `ip`, or /proc.
fn blackhole_ipv6(flag: &str, commands: &dyn CommandRunner, sysctl_base: &Path) {
    if !parse_flag(flag) {
        return;
    }
    let route_ok = match commands.run(BLACKHOLE_ROUTE_ARGV) {
        Ok(()) => true,
        Err(e) => {
            log(format!(
                "WARN: could not install unreachable ipv6 default route: {e}"
            ));
            false
        }
    };
    let ra_ok = reject_router_advertisements(sysctl_base);
    if route_ok && ra_ok {
        log("ipv6 global routes blackholed (DISABLE_IPV6 set - v4-only egress)");
    }
}

/// Write `0` to `conf/{all,default}/accept_ra` under `base`, so a later
/// router advertisement can't install a real default route behind the
/// blackhole. These knobs do NOT disable the stack — link-local v6 stays
/// fully functional (unlike `disable_ipv6`; see the module docs for the
/// NotStabilized incident). Absent paths (kernel built without IPv6) and
/// write failures are tolerated with one WARN each. Returns true only when
/// every write landed. `base` is a parameter so tests run against a temp
/// dir.
fn reject_router_advertisements(base: &Path) -> bool {
    let mut all_ok = true;
    for scope in ["all", "default"] {
        let path = base.join("conf").join(scope).join("accept_ra");
        if let Err(e) = std::fs::write(&path, "0") {
            log(format!(
                "WARN: could not set accept_ra=0 at {}: {e}",
                path.display()
            ));
            all_ok = false;
        }
    }
    all_ok
}

/// Seam for the `ip` shell-out so a unit test asserts the exact argv without
/// running anything — the same hand-rolled-fake pattern as
/// [`crate::aws::CloudControl`].
trait CommandRunner {
    /// Run `argv[0]` with the remaining args. `Err` carries a one-line
    /// human-readable reason: spawn failure (including a missing binary,
    /// e.g. an image built without iproute) or a non-zero exit.
    fn run(&self, argv: &[&str]) -> Result<(), String>;
}

/// Real implementation: std::process, capturing stderr for the warn line.
struct SystemCommands;

impl CommandRunner for SystemCommands {
    fn run(&self, argv: &[&str]) -> Result<(), String> {
        let (program, args) = argv.split_first().expect("argv must be non-empty");
        match std::process::Command::new(program).args(args).output() {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(format!(
                "{program} exited rc={}: {}",
                exit_code(out.status),
                truncate_chars(String::from_utf8_lossy(&out.stderr).trim(), 200)
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(format!("{program} not found in image - skipping"))
            }
            Err(e) => Err(format!("could not spawn {program}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testsupport::temp_dir;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Recorded argv per call, with an optional scripted failure.
    #[derive(Default)]
    struct FakeCommands {
        runs: Mutex<Vec<Vec<String>>>,
        fail_with: Option<String>,
    }

    impl CommandRunner for FakeCommands {
        fn run(&self, argv: &[&str]) -> Result<(), String> {
            self.runs
                .lock()
                .unwrap()
                .push(argv.iter().map(ToString::to_string).collect());
            match &self.fail_with {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
    }

    /// A fake /proc/sys/net/ipv6 with both conf scopes present.
    fn sysctl_tree(tag: &str) -> PathBuf {
        let base = PathBuf::from(temp_dir(tag));
        for scope in ["all", "default"] {
            std::fs::create_dir_all(base.join("conf").join(scope)).unwrap();
        }
        base
    }

    fn read_accept_ra(base: &Path, scope: &str) -> String {
        std::fs::read_to_string(base.join("conf").join(scope).join("accept_ra")).unwrap()
    }

    #[test]
    fn truthy_flag_runs_exact_ip_argv_and_rejects_ras() {
        let base = sysctl_tree("ipv6-blackhole");
        let fake = FakeCommands::default();
        blackhole_ipv6("1", &fake, &base);
        // Exactly one `ip` invocation, argv verbatim: the unreachable
        // default route at metric 1 (NOT a disable_ipv6 sysctl — see the
        // module docs for the NotStabilized incident).
        assert_eq!(
            fake.runs.lock().unwrap().as_slice(),
            [[
                "ip",
                "-6",
                "route",
                "replace",
                "unreachable",
                "default",
                "metric",
                "1"
            ]
            .map(String::from)]
        );
        for scope in ["all", "default"] {
            assert_eq!(read_accept_ra(&base, scope), "0");
        }
    }

    #[test]
    fn unset_or_falsy_flag_is_a_complete_noop() {
        // "" is what the env_or default yields when DISABLE_IPV6 is unset.
        for flag in ["", "0", "false", "off"] {
            let base = sysctl_tree(&format!("ipv6-noop-{flag}"));
            let fake = FakeCommands::default();
            blackhole_ipv6(flag, &fake, &base);
            assert!(fake.runs.lock().unwrap().is_empty(), "flag {flag:?} ran ip");
            for scope in ["all", "default"] {
                assert!(
                    !base.join("conf").join(scope).join("accept_ra").exists(),
                    "flag {flag:?} wrote accept_ra"
                );
            }
        }
    }

    #[test]
    fn route_failure_warns_but_still_rejects_ras() {
        // e.g. an image built without iproute: warn-and-continue, never
        // block boot — and the RA rejection still lands.
        let base = sysctl_tree("ipv6-route-fail");
        let fake = FakeCommands {
            fail_with: Some("ip not found in image - skipping".to_string()),
            ..Default::default()
        };
        blackhole_ipv6("true", &fake, &base);
        for scope in ["all", "default"] {
            assert_eq!(read_accept_ra(&base, scope), "0");
        }
    }

    #[test]
    fn absent_sysctl_tree_is_tolerated() {
        // Kernel without IPv6: the conf tree simply isn't there.
        let base = PathBuf::from(temp_dir("ipv6-absent")).join("missing");
        assert!(!reject_router_advertisements(&base));
    }

    #[test]
    fn unwritable_scope_warns_but_still_writes_the_other() {
        let base = PathBuf::from(temp_dir("ipv6-unwritable"));
        // A directory where the file should be fails the write (EISDIR)
        // regardless of privileges — unlike a chmod, this also fails as root.
        std::fs::create_dir_all(base.join("conf/all/accept_ra")).unwrap();
        std::fs::create_dir_all(base.join("conf/default")).unwrap();
        assert!(!reject_router_advertisements(&base));
        assert_eq!(read_accept_ra(&base, "default"), "0");
    }
}
