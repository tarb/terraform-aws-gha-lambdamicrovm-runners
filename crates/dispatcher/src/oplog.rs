//! Operational log lines: one JSON object per line on **stdout**.
//!
//! These lines are a monitoring contract — dashboards grep the KEYS and
//! semantic values. Catalog of keys emitted by this binary:
//!
//! * `"pool"`: `suspended`, `resumed`, `handoff-put-failed`, `resume-failed`,
//!   `handoff-unclaimed-terminating`, `terminate-stale`, `terminate-near-eol`,
//!   `full-terminating`, `busy-check-failed`, `completed-intake-error`,
//!   `describe-failed`, `idle-report`, `idle-skip`, `idle-intake-error`,
//!   `throttled` (a dispatch-path control-plane call needed throttle
//!   retries; `calls` = throttled attempts in the burst).
//! * `"sweep"`: `deadline-bail`, `installation-failed`, `runner-list-failed`,
//!   `repo-failed`, `stale-queued-job`, `dispatch-failed`,
//!   `zombie-reap-failed`, `handoff-gc-failed`, `pool-gc-failed`,
//!   `reap-zombie-vm`, `gc-handoff-param`, `gc-pool-vm`,
//!   `scan-failed-everywhere` (every attempted repo scan failed — a loud
//!   canary for e.g. a missing GitHub App permission 403ing everything while
//!   `done` still reports 0), `done`.
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
    #[cfg(test)]
    capture::LINES.with_borrow_mut(|lines| lines.push(line.clone()));
    println!("{line}");
}

/// Truncate to `n` characters (error strings in log lines stay bounded).
pub fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Test-only capture of emitted lines, so behavior tests can assert the
/// monitoring contract. Thread-local: each test (single-threaded tokio
/// runtime on its own libtest thread) sees only its own lines.
#[cfg(test)]
pub mod capture {
    use std::cell::RefCell;

    thread_local! {
        pub(super) static LINES: RefCell<Vec<serde_json::Value>> =
            const { RefCell::new(Vec::new()) };
    }

    /// Every line emitted on this thread so far.
    pub fn lines() -> Vec<serde_json::Value> {
        LINES.with_borrow(|lines| lines.clone())
    }
}
