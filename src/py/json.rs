//! Python `json.dumps` and value conversions used for signatures, cache keys and
//! response shaping.
//!
//! Signatures must be byte-identical to what the Python indexer stored, so the
//! serializer reproduces CPython's defaults: `", "`/`": "` separators, optional
//! `sort_keys`, and `ensure_ascii` `\uXXXX` escapes (with surrogate pairs).
//! `serde_json` is still used for parsing and for HTTP response bodies, whose
//! compact form matches Starlette's `JSONResponse` already.

use serde_json::Value;

#[derive(Clone, Copy)]
pub struct DumpOptions {
    pub ensure_ascii: bool,
    pub sort_keys: bool,
}

/// `json.dumps(value)` with default separators.
pub fn dumps(value: &Value, options: DumpOptions) -> String {
    let mut out = String::new();
    write_value(&mut out, value, options);
    out
}

/// `json.dumps(value, ensure_ascii=False)`
pub fn dumps_unicode(value: &Value) -> String {
    dumps(
        value,
        DumpOptions {
            ensure_ascii: false,
            sort_keys: false,
        },
    )
}

/// `json.dumps(value)` (ensure_ascii=True)
pub fn dumps_ascii(value: &Value) -> String {
    dumps(
        value,
        DumpOptions {
            ensure_ascii: true,
            sort_keys: false,
        },
    )
}

fn write_value(out: &mut String, value: &Value, options: DumpOptions) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if let Some(f) = number.as_f64().filter(|_| number.is_f64()) {
                out.push_str(&super::float::repr(f));
            } else {
                out.push_str(&number.to_string());
            }
        }
        Value::String(s) => write_string(out, s, options.ensure_ascii),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_value(out, item, options);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            if options.sort_keys {
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_string(out, key, options.ensure_ascii);
                out.push_str(": ");
                write_value(out, item, options);
            }
            out.push('}');
        }
    }
}

/// CPython `encode_basestring(_ascii)`.
pub fn write_string(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ensure_ascii && (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python truthiness of a decoded JSON value (`bool(value)`).
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `str(value)` for a decoded JSON value.
pub fn to_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => repr(other),
    }
}

/// `repr(value)` for a decoded JSON value (dict/list/str/int/float/bool/None).
pub fn repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) if n.is_f64() => super::float::repr(n.as_f64().unwrap_or(f64::NAN)),
        Value::Number(n) => n.to_string(),
        Value::String(s) => super::text::repr_str(s),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", super::text::repr_str(k), repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `int(value)`; `None` for every input where Python raises.
pub fn to_int(value: &Value) -> Option<i64> {
    match value {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Some(i),
            None => n
                .as_f64()
                .filter(|f| f.is_finite())
                .map(|f| f.trunc() as i64),
        },
        Value::String(s) => parse_python_int(s),
        _ => None,
    }
}

/// Python `int(str)` for base 10: surrounding whitespace, a sign, and single
/// underscores between digits.
fn parse_python_int(s: &str) -> Option<i64> {
    let s = super::text::strip(s);
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    let mut value: i64 = 0;
    for c in digits.chars().filter(|c| *c != '_') {
        let digit = c.to_digit(10)? as i64;
        value = value.checked_mul(10)?.checked_add(digit)?;
    }
    Some(if negative { -value } else { value })
}

/// `type(value) is int` for JSON input (booleans and floats excluded).
pub fn is_int(value: &Value) -> bool {
    matches!(value, Value::Number(n) if !n.is_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn signature_json_matches_python() {
        let value = json!({"model": "m", "dimensions": 1536, "a": "한"});
        assert_eq!(
            dumps(
                &value,
                DumpOptions {
                    ensure_ascii: true,
                    sort_keys: true
                }
            ),
            "{\"a\": \"\\ud55c\", \"dimensions\": 1536, \"model\": \"m\"}"
        );
        assert_eq!(
            dumps_unicode(&json!(["v", "한", null, true])),
            r#"["v", "한", null, true]"#
        );
        assert_eq!(dumps_ascii(&json!(["\u{1F600}"])), "[\"\\ud83d\\ude00\"]");
        assert_eq!(dumps_ascii(&json!(["\u{7f}\u{1}"])), "[\"\\u007f\\u0001\"]");
        assert_eq!(dumps_unicode(&json!(["\u{7f}\u{1}"])), "[\"\u{7f}\\u0001\"]");
    }

    #[test]
    fn python_conversions() {
        assert_eq!(to_str(&json!(["a", 1, null])), "['a', 1, None]");
        assert_eq!(to_str(&json!(true)), "True");
        assert_eq!(to_int(&json!(" 1_2 ")), Some(12));
        assert_eq!(to_int(&json!(3.9)), Some(3));
        assert_eq!(to_int(&json!("1__2")), None);
        assert_eq!(to_int(&json!(null)), None);
        assert!(!truthy(&json!(0.0)) && truthy(&json!("x")) && !truthy(&json!({})));
    }
}
