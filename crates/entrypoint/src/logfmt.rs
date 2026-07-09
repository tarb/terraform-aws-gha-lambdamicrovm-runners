//! Plain-text stderr logging with the `[runner-microvm]` prefix.
//! Operational greps depend on several exact message texts (see the log
//! call sites); the prefix and stream must not change.

/// Emit one log line on stderr.
pub fn log(msg: impl AsRef<str>) {
    eprintln!("[runner-microvm] {}", msg.as_ref());
}

/// First `n` characters of `s` — bounds error text pasted into log lines.
pub fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
