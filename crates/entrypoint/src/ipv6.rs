//! Optional guest IPv6 egress fast-fail (`DISABLE_IPV6`), applied at
//! supervisor boot.
//!
//! Why: the fleet's egress connector can be IPv4-only (NetworkConnector
//! VpcEgressConfiguration `network_protocol = "IPv4"`), but the guest has no
//! way to know — dual-stack clients (observed: bun's highly concurrent
//! package fetches) burn a happy-eyeballs IPv6 attempt per connection
//! against a protocol that can never work, a per-connection tax that turned
//! a ~1min install into ~11min. When the operator knows the connector is
//! v4-only, `DISABLE_IPV6=1` (baked into the image env by the module's
//! `disable_guest_ipv6` variable) makes those doomed attempts fail instantly
//! instead.
//!
//! Mechanism — a direction-aware ip6tables REJECT, and NOTHING else:
//! guest-initiated TCP connects to global-unicast v6 (`2000::/3`) are
//! answered with a local RST (`--reject-with tcp-reset`), so happy-eyeballs
//! falls back to v4 in microseconds. `--syn` matches only SYN-without-ACK,
//! i.e. only connections the GUEST initiates — reply traffic to inbound
//! connections (SYN-ACKs carry ACK) passes untouched.
//!
//! DO NOT "simplify" this to route or sysctl mutation. Both were shipped and
//! both broke the platform, discovered via image builds failing
//! NotStabilized ("Ready hook invocation timed out after PT2M"):
//!
//! - v0.0.5 set the `disable_ipv6` sysctls. That deletes the guest's IPv6
//!   address entirely.
//! - v0.0.6 installed `ip -6 route replace unreachable default metric 1`
//!   (+ `accept_ra=0`, which turned out to be a no-op — the platform's own
//!   base config already sets it). The unreachable route outranked the
//!   platform's static `default via fe80::1` and silently dropped REPLY
//!   packets to off-link peers.
//!
//! What a live-fleet network fingerprint established (2026-07-10): the guest
//! gets a statically installed global /128 (no SLAAC, no link-local, RA
//! already off), a static `default via fe80::1`, and a hidden platform agent
//! listening on `*:8443` over that address which forwards lifecycle hooks to
//! our server via `127.0.0.1:9000`. The control plane dials the agent from
//! an OFF-LINK global-unicast source, so the agent's replies need both the
//! /128 address and the v6 default route. Any mechanism that removes the
//! address or beats that route breaks the hook channel and every image
//! build. The `--syn` REJECT touches neither: inbound flows and their
//! replies are not SYN-only packets.

use crate::config::{env_or, parse_flag};
use crate::logfmt::{log, truncate_chars};
use crate::supervisor::exit_code;

/// The fast-fail rule. `-w` waits on the shared xtables lock instead of
/// racing dockerd's iptables calls; `-I OUTPUT` needs no pre-existing chain
/// setup. Scoped to global unicast (`2000::/3`) so loopback, link-local and
/// any platform-internal ULA traffic are never touched. Runs once per
/// process start (suspend/resume does not restart pid 1), so duplicate
/// inserts don't accumulate — and a duplicate would be behaviorally
/// identical anyway.
const REJECT_RULE_ARGV: &[&str] = &[
    "ip6tables",
    "-w",
    "-I",
    "OUTPUT",
    "-d",
    "2000::/3",
    "-p",
    "tcp",
    "--syn",
    "-j",
    "REJECT",
    "--reject-with",
    "tcp-reset",
];

/// Install the v6 egress fast-fail when the `DISABLE_IPV6` env flag is
/// truthy (same lenient parsing as every other flag: `1`/`true`/`yes`).
/// No-op when the flag is unset or falsy. Called at boot, before any child
/// process — the runner and everything it spawns then see instant-RST global
/// v6 connects and fall back to v4, while the platform's inbound hook
/// channel keeps working.
pub fn restrict_ipv6_egress_if_requested() {
    restrict_ipv6_egress(&env_or("DISABLE_IPV6", ""), &SystemCommands);
}

/// Gate + mechanism. Failure warns and continues — a guest without
/// ip6tables (or without the kernel modules) must still boot; it just keeps
/// the happy-eyeballs tax. Parameterised (flag value, command seam) so tests
/// drive it without touching the process env or the real ip6tables.
fn restrict_ipv6_egress(flag: &str, commands: &dyn CommandRunner) {
    if !parse_flag(flag) {
        return;
    }
    match commands.run(REJECT_RULE_ARGV) {
        Ok(()) => log(
            "ipv6 egress fast-fail installed (DISABLE_IPV6 set - guest-initiated v6 TCP gets RST)",
        ),
        Err(e) => log(format!(
            "WARN: could not install ipv6 egress fast-fail rule: {e}"
        )),
    }
}

/// Seam for the `ip6tables` shell-out so a unit test asserts the exact argv
/// without running anything — the same hand-rolled-fake pattern as
/// [`crate::aws::CloudControl`].
trait CommandRunner {
    /// Run `argv[0]` with the remaining args. `Err` carries a one-line
    /// human-readable reason: spawn failure (including a missing binary,
    /// e.g. an image built without iptables) or a non-zero exit.
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

    #[test]
    fn truthy_flag_runs_exact_ip6tables_argv() {
        let fake = FakeCommands::default();
        restrict_ipv6_egress("1", &fake);
        // Exactly one ip6tables invocation, argv verbatim: a --syn REJECT
        // scoped to global unicast. NOT a route, NOT a sysctl — both broke
        // the platform's hook channel (see the module docs for the
        // v0.0.5/v0.0.6 NotStabilized incidents).
        assert_eq!(
            fake.runs.lock().unwrap().as_slice(),
            [[
                "ip6tables",
                "-w",
                "-I",
                "OUTPUT",
                "-d",
                "2000::/3",
                "-p",
                "tcp",
                "--syn",
                "-j",
                "REJECT",
                "--reject-with",
                "tcp-reset"
            ]
            .map(String::from)]
        );
    }

    #[test]
    fn unset_or_falsy_flag_is_a_complete_noop() {
        // "" is what the env_or default yields when DISABLE_IPV6 is unset.
        for flag in ["", "0", "false", "off"] {
            let fake = FakeCommands::default();
            restrict_ipv6_egress(flag, &fake);
            assert!(
                fake.runs.lock().unwrap().is_empty(),
                "flag {flag:?} ran ip6tables"
            );
        }
    }

    #[test]
    fn rule_failure_warns_and_continues() {
        // e.g. an image built without iptables, or missing kernel modules:
        // warn-and-continue, never block boot — the guest just keeps the
        // happy-eyeballs tax.
        let fake = FakeCommands {
            fail_with: Some("ip6tables not found in image - skipping".to_string()),
            ..Default::default()
        };
        restrict_ipv6_egress("true", &fake);
        assert_eq!(fake.runs.lock().unwrap().len(), 1);
    }
}
