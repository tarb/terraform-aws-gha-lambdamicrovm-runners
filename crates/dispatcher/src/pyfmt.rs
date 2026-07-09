//! Python-compatible formatting helpers.
//!
//! Operational log lines and response bodies must stay byte-compatible with
//! the Python dispatcher's `print(json.dumps(...))` output (`", "` / `": "`
//! separators, `ensure_ascii=True` escaping), and several user-visible
//! message strings embed Python `str()` / `repr()` conversions.

use serde::Serialize;
use serde_json::Value;
use std::io;

/// Error carrying a Python-exception-shaped identity: `kind` mirrors
/// `type(e).__name__` and `msg` mirrors `str(e)`. Only `RuntimeError` texts
/// are load-bearing for the contract; the rest are best-effort diagnostics.
#[derive(Debug, Clone)]
pub struct PyErr {
    pub kind: String,
    pub msg: String,
}

impl PyErr {
    pub fn new(kind: &str, msg: impl Into<String>) -> Self {
        Self {
            kind: kind.to_string(),
            msg: msg.into(),
        }
    }

    pub fn runtime(msg: impl Into<String>) -> Self {
        Self::new("RuntimeError", msg)
    }

    pub fn key_error(key: &str) -> Self {
        Self::new("KeyError", format!("'{key}'"))
    }

    pub fn type_error(msg: impl Into<String>) -> Self {
        Self::new("TypeError", msg)
    }

    pub fn value_error(msg: impl Into<String>) -> Self {
        Self::new("ValueError", msg)
    }

    pub fn json_error(e: serde_json::Error) -> Self {
        Self::new("JSONDecodeError", e.to_string())
    }
}

impl std::fmt::Display for PyErr {
    /// `str(e)` — message only, no kind prefix (matches Python).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for PyErr {}

/// `json.dumps(v)` with CPython's default separators and `ensure_ascii`.
pub fn dumps(v: &Value) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, PyJsonFormatter);
    v.serialize(&mut ser)
        .expect("serde_json::Value serialization cannot fail");
    String::from_utf8(out).expect("json output is valid utf-8")
}

/// Print one operational log line to stdout, exactly as Python's
/// `print(json.dumps({...}))` would.
pub fn logln(v: &Value) {
    println!("{}", dumps(v));
}

struct PyJsonFormatter;

impl serde_json::ser::Formatter for PyJsonFormatter {
    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        writer.write_all(b": ")
    }

    /// `ensure_ascii=True`: non-ASCII chars become `\uXXXX` (surrogate pairs
    /// for astral-plane chars), lowercase hex, like CPython's json module.
    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        let mut buf = [0u16; 2];
        for ch in fragment.chars() {
            if (ch as u32) < 0x80 {
                writer.write_all(&[ch as u8])?;
            } else {
                for unit in ch.encode_utf16(&mut buf).iter() {
                    write!(writer, "\\u{unit:04x}")?;
                }
            }
        }
        Ok(())
    }
}

/// Python `str()` of a JSON value, as used in f-strings.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => dumps(other), // approximation; not hit by real webhook payloads
    }
}

/// Python `repr()` of a str, as used by `{runner!r}`.
pub fn py_repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python `repr()` of a sorted list of strings, e.g. `['a', 'b']`.
pub fn py_list_repr(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| py_repr_str(s)).collect();
    format!("[{}]", inner.join(", "))
}

/// Python `str(e)[:n]` — truncation by code points, not bytes.
pub fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python truthiness of a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `v[key]`: KeyError on a missing key, TypeError on a non-dict.
pub fn v_index<'a>(v: &'a Value, key: &str) -> Result<&'a Value, PyErr> {
    match v {
        Value::Object(m) => m.get(key).ok_or_else(|| PyErr::key_error(key)),
        _ => Err(PyErr::type_error(format!("'{key}' lookup on non-object"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dumps_matches_python_separators_and_ascii() {
        let v =
            json!({"status": 200, "msg": "pong", "x": [1, 2], "f": 0.0, "n": null, "u": "café"});
        // reference output from CPython: json.dumps({...})
        assert_eq!(
            dumps(&v),
            "{\"status\": 200, \"msg\": \"pong\", \"x\": [1, 2], \"f\": 0.0, \"n\": null, \"u\": \"caf\\u00e9\"}"
        );
    }

    #[test]
    fn py_str_shapes() {
        assert_eq!(py_str(&Value::Null), "None");
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(123)), "123");
        assert_eq!(py_str(&json!("queued")), "queued");
    }

    #[test]
    fn py_repr_and_list_repr() {
        assert_eq!(py_repr_str("ubuntu-hosted-3"), "'ubuntu-hosted-3'");
        assert_eq!(py_repr_str("it's"), "\"it's\"");
        assert_eq!(
            py_list_repr(&["a".to_string(), "b".to_string()]),
            "['a', 'b']"
        );
    }

    #[test]
    fn truthiness_matches_python() {
        assert!(!truthy(&json!(null)));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!("")));
        assert!(!truthy(&json!([])));
        assert!(!truthy(&json!({})));
        assert!(truthy(&json!(true)));
        assert!(truthy(&json!(1)));
        assert!(truthy(&json!("x")));
    }

    #[test]
    fn trunc_is_by_chars() {
        assert_eq!(trunc("héllo", 2), "hé");
    }
}
