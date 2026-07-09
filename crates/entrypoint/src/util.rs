//! Shared helpers: the Python-parity logger, an async port of
//! `threading.Event`, and Python-semantics coercions (truthiness, `int()`,
//! `repr` of strings/lists) so log lines and parsing stay byte-compatible
//! with `microvm/entrypoint.py`.

use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// Mirror of the Python `log()`: plain-text lines on stderr with the
/// `[runner-microvm]` prefix. Operational greps depend on these exact texts.
pub fn log(msg: impl AsRef<str>) {
    eprintln!("[runner-microvm] {}", msg.as_ref());
}

/// Async port of `threading.Event` with the exact semantics the pool
/// handshake relies on: `set()` is level-triggered state (not an edge), so a
/// `new_run` set by a fresh run is seen by a waiter even if it registered
/// late — and `clear()` really can wipe a racing `set()`, which is why the
/// idle-waiter re-checks `is_set()` at grace expiry (see `pool_idle_wait`).
#[derive(Default)]
pub struct Event {
    flag: AtomicBool,
    notify: Notify,
}

impl Event {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn clear(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// `Event.wait(timeout=...)`: returns the flag state, waking early when
    /// the flag is set. Registers interest before checking the flag so a
    /// concurrent `set()` cannot be lost.
    pub async fn wait_timeout(&self, dur: Duration) -> bool {
        let notified = self.notify.notified();
        if self.is_set() {
            return true;
        }
        tokio::select! {
            _ = notified => {}
            _ = tokio::time::sleep(dur) => {}
        }
        self.is_set()
    }
}

/// Python truthiness of a JSON value (`bool(x)`).
pub fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `int(x)` over a JSON value: bools are 0/1, floats truncate,
/// strings are trimmed and parsed; anything else is an error (which the
/// Python code would raise into `start_runner`'s catch-all).
pub fn python_int(v: &Value) -> Result<i64, String> {
    match v {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .ok_or_else(|| format!("int() cannot convert {n}")),
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("invalid literal for int(): {s:?}")),
        other => Err(format!(
            "int() argument must be a number or string: {other}"
        )),
    }
}

/// Python `repr()` of a `bool` ("True"/"False") — several log lines
/// interpolate raw Python bools.
pub fn py_bool(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

/// Python `repr()` of a sorted list of strings: `['a', 'b']`.
pub fn py_str_list(items: &[&str]) -> String {
    let inner = items
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

/// Python `f"{x!r}"` of an optional JSON scalar (used for the `/run
/// microvmId=...` log line): strings quote, missing/null prints `None`.
pub fn py_repr_json(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => format!("'{s}'"),
        Some(Value::Bool(b)) => py_bool(*b).to_string(),
        Some(other) => other.to_string(),
    }
}

/// Python `s[:n]` (chars, like the str slices in the error logs).
pub fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python `subprocess.returncode`: exit code, or negative signal number.
pub fn exit_code(status: std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .map(i64::from)
        .unwrap_or_else(|| -i64::from(status.signal().unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truthiness_matches_python() {
        assert!(!py_truthy(&json!(null)));
        assert!(!py_truthy(&json!(false)));
        assert!(!py_truthy(&json!(0)));
        assert!(!py_truthy(&json!("")));
        assert!(!py_truthy(&json!([])));
        assert!(!py_truthy(&json!({})));
        assert!(py_truthy(&json!("false"))); // non-empty string is truthy
        assert!(py_truthy(&json!(1)));
        assert!(py_truthy(&json!({"a": 1})));
    }

    #[test]
    fn python_int_semantics() {
        assert_eq!(python_int(&json!("300")).unwrap(), 300);
        assert_eq!(python_int(&json!(" 42 ")).unwrap(), 42);
        assert_eq!(python_int(&json!(300.9)).unwrap(), 300);
        assert_eq!(python_int(&json!(true)).unwrap(), 1);
        assert!(python_int(&json!("abc")).is_err());
        assert!(python_int(&json!([1])).is_err());
    }

    #[test]
    fn py_list_and_repr_render_like_python() {
        assert_eq!(py_str_list(&["a", "b"]), "['a', 'b']");
        assert_eq!(py_str_list(&[]), "[]");
        assert_eq!(py_repr_json(Some(&json!("m-1"))), "'m-1'");
        assert_eq!(py_repr_json(None), "None");
        assert_eq!(py_repr_json(Some(&json!(null))), "None");
    }

    #[tokio::test(start_paused = true)]
    async fn event_set_before_wait_returns_immediately() {
        let e = Event::new();
        e.set();
        assert!(e.wait_timeout(Duration::from_secs(5)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn event_times_out_false_when_unset() {
        let e = Event::new();
        assert!(!e.wait_timeout(Duration::from_millis(100)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn event_set_during_wait_wakes_waiter() {
        let e = std::sync::Arc::new(Event::new());
        let e2 = e.clone();
        let waiter = tokio::spawn(async move { e2.wait_timeout(Duration::from_secs(60)).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        e.set();
        assert!(waiter.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn event_clear_can_wipe_a_set_like_threading_event() {
        // The documented pool race: clear() after a set() loses the signal;
        // is_set() is the recovery check.
        let e = Event::new();
        e.set();
        e.clear();
        assert!(!e.is_set());
        assert!(!e.wait_timeout(Duration::from_millis(50)).await);
    }
}
