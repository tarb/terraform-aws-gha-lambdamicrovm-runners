//! Operational log lines: one JSON object per line on **stdout**.
//!
//! These lines are a monitoring contract — dashboards grep the KEYS and
//! semantic values. Catalog of keys emitted by this binary:
//!
//! * `"pool"`: `suspended`, `resumed`, `handoff-put-failed`, `resume-failed`,
//!   `handoff-unclaimed-terminating`, `terminate-stale`, `terminate-near-eol`,
//!   `full-terminating`, `busy-check-failed`, `completed-intake-error`,
//!   `describe-failed`.
//! * `"sweep"`: `deadline-bail`, `installation-failed`, `runner-list-failed`,
//!   `repo-failed`, `stale-queued-job`, `dispatch-failed`,
//!   `zombie-reap-failed`, `handoff-gc-failed`, `pool-gc-failed`,
//!   `reap-zombie-vm`, `gc-handoff-param`, `gc-pool-vm`, `done`.
//! * `"dispatched"`, `"microvmId"`, `"handoff_seconds"` (1 decimal),
//!   `"vm_record_keys"` (one-shot, sorted), `"status"`/`"msg"` on every
//!   Function-URL response, `"event"` intake lines, `{"outcome": "defer"}` on
//!   the cap gate, `{"warn": "unknown microvm states (not counted)"}`,
//!   `{"sqs_item_failed", "messageId"}` per SQS record returned for retry
//!   via a partial batch response.
//!
//! `tracing` (stderr) is diagnostics only; operational lines never go through
//! its formatters — the key shape would drift.

/// Emit one operational line.
pub fn emit(line: serde_json::Value) {
    println!("{line}");
}

/// Truncate to `n` characters (error strings in log lines stay bounded).
pub fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
