//! Identifiers shared by the dispatcher and the VM entrypoint.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Opaque service-issued microVM id, e.g. `microvm-aaaa1111-...`. Accepts any
/// string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MicrovmId(String);

impl MicrovmId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MicrovmId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// GitHub runner name. THE fleet-wide derivation: `gha-mvm-` + first 18 chars
/// of the id with a leading `microvm-` prefix stripped, whole name capped at
/// 64 chars. Both the entrypoint (registration) and the dispatcher (suspend
/// intake, zombie reaper) derive it; it must never diverge between crates —
/// that is why it lives here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunnerName(String);

impl RunnerName {
    pub const PREFIX: &'static str = "gha-mvm-";

    /// The deterministic name a VM registers under.
    pub fn for_vm(id: &MicrovmId) -> Self {
        let bare = id.as_str().strip_prefix("microvm-").unwrap_or(id.as_str());
        let suffix: String = bare.chars().take(18).collect();
        let name: String = format!("{}{}", Self::PREFIX, suffix)
            .chars()
            .take(64)
            .collect();
        Self(name)
    }

    /// Fallback name when a VM does not know its own id: `gha-mvm-` + 8 hex
    /// chars from `/dev/urandom` (nanos^pid if the read fails).
    pub fn random() -> Self {
        Self(format!("{}{}", Self::PREFIX, random_hex8()))
    }

    /// Recognize one of OUR runner names in a webhook. `Some` iff `name`
    /// starts with [`Self::PREFIX`].
    pub fn parse(name: &str) -> Option<OurRunner<'_>> {
        name.strip_prefix(Self::PREFIX)
            .map(|fragment| OurRunner { fragment })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunnerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A webhook `runner_name` recognized as ours; `fragment` is the id substring
/// after the prefix, used to find the owning VM
/// (`vm.id.as_str().contains(fragment)`).
pub struct OurRunner<'a> {
    pub fragment: &'a str,
}

fn random_hex8() -> String {
    let bytes = read_urandom4().unwrap_or_else(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        (nanos ^ std::process::id()).to_be_bytes()
    });
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn read_urandom4() -> Option<[u8; 4]> {
    use std::io::Read;
    let mut buf = [0u8; 4];
    std::fs::File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut buf)
        .ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_name_matches_python_derivation() {
        let id = MicrovmId::new("microvm-aaaa1111-2222-3333-4444-555566667777");
        assert_eq!(
            RunnerName::for_vm(&id).as_str(),
            "gha-mvm-aaaa1111-2222-3333"
        );
        let bare = MicrovmId::new("deadbeef");
        assert_eq!(RunnerName::for_vm(&bare).as_str(), "gha-mvm-deadbeef");
    }

    #[test]
    fn runner_name_caps_at_64_chars() {
        let id = MicrovmId::new("x".repeat(200));
        assert!(RunnerName::for_vm(&id).as_str().len() <= 64);
    }

    #[test]
    fn random_names_are_prefixed_8_hex() {
        let name = RunnerName::random();
        let frag = name.as_str().strip_prefix(RunnerName::PREFIX).unwrap();
        assert_eq!(frag.len(), 8);
        assert!(frag.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_recognizes_only_our_prefix() {
        let ours = RunnerName::parse("gha-mvm-aaaa1111-2222-33").unwrap();
        assert_eq!(ours.fragment, "aaaa1111-2222-33");
        assert!(RunnerName::parse("ubuntu-hosted-3").is_none());
    }
}
