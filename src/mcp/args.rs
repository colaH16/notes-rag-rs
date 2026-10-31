//! Tool argument validation equivalent to the Python SDK's pydantic models.
//!
//! Python declared typed function parameters and let pydantic (lax mode,
//! `limit` strict) coerce/validate them; `FuncMetadata.pre_parse_json` first
//! JSON-decodes string values for non-`str` parameters unless they decode to a
//! scalar. The same rules are applied here explicitly. Unknown argument names
//! are ignored (pydantic `extra='ignore'`).
//!
//! Intentional difference: validation messages keep pydantic's layout
//! (`N validation error(s) for <tool>Arguments`, field, message, `[type=...]`)
//! but never echo the rejected input value, so caller text is not reflected
//! into logs or LLM transcripts.

use serde_json::{Map, Value};

pub struct FieldError {
    pub field: &'static str,
    pub kind: &'static str,
    pub message: String,
}

pub struct ValidationErrors {
    pub model: &'static str,
    pub errors: Vec<FieldError>,
}

impl ValidationErrors {
    pub fn message(&self) -> String {
        let count = self.errors.len();
        let mut out = format!(
            "{count} validation error{} for {}",
            if count == 1 { "" } else { "s" },
            self.model
        );
        for error in &self.errors {
            out.push_str(&format!(
                "\n{}\n  {} [type={}]",
                error.field, error.message, error.kind
            ));
        }
        out
    }
}

/// `pre_parse_json` for a non-`str` field.
fn pre_parse(value: &Value) -> Value {
    if let Value::String(s) = value
        && let Ok(parsed) = serde_json::from_str::<Value>(s)
        && !matches!(parsed, Value::String(_) | Value::Number(_) | Value::Bool(_))
    {
        return parsed;
    }
    value.clone()
}

struct Validator<'a> {
    arguments: &'a Map<String, Value>,
    errors: Vec<FieldError>,
}

impl<'a> Validator<'a> {
    fn fail(&mut self, field: &'static str, kind: &'static str, message: impl Into<String>) {
        self.errors.push(FieldError {
            field,
            kind,
            message: message.into(),
        });
    }

    fn string(
        &mut self,
        field: &'static str,
        default: Option<&str>,
        min: usize,
        max: usize,
    ) -> Option<String> {
        let value = match self.arguments.get(field) {
            None => match default {
                Some(default) => return Some(default.to_string()),
                None => {
                    self.fail(field, "missing", "Field required");
                    return None;
                }
            },
            Some(value) => value,
        };
        let Value::String(s) = value else {
            self.fail(field, "string_type", "Input should be a valid string");
            return None;
        };
        let length = crate::py::text::len(s);
        if length < min {
            self.fail(
                field,
                "string_too_short",
                format!("String should have at least {min} character{}", plural(min)),
            );
            return None;
        }
        if length > max {
            self.fail(
                field,
                "string_too_long",
                format!("String should have at most {max} character{}", plural(max)),
            );
            return None;
        }
        Some(s.clone())
    }

    /// pydantic strict `int` (no bool, float or string coercion).
    fn strict_int(&mut self, field: &'static str, default: i64, minimum: i64) -> Option<i64> {
        let Some(value) = self.arguments.get(field) else {
            return Some(default);
        };
        let parsed = pre_parse(value);
        let Some(number) = parsed.as_i64().filter(|_| crate::py::json::is_int(&parsed)) else {
            self.fail(field, "int_type", "Input should be a valid integer");
            return None;
        };
        if number < minimum {
            self.fail(
                field,
                "greater_than_equal",
                format!("Input should be greater than or equal to {minimum}"),
            );
            return None;
        }
        Some(number)
    }

    /// pydantic lax `int`.
    fn lax_int(&mut self, field: &'static str) -> Option<i64> {
        let Some(value) = self.arguments.get(field) else {
            self.fail(field, "missing", "Field required");
            return None;
        };
        let parsed = pre_parse(value);
        let number = match &parsed {
            Value::Bool(b) => Some(i64::from(*b)),
            Value::Number(n) if !n.is_f64() => n.as_i64(),
            Value::Number(n) => n
                .as_f64()
                .filter(|f| f.fract() == 0.0 && f.abs() < 9.0e18)
                .map(|f| f as i64),
            Value::String(s) => {
                let s = crate::py::text::strip(s);
                let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
                if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                    s.parse().ok()
                } else {
                    None
                }
            }
            _ => None,
        };
        if number.is_none() {
            let message = match parsed {
                Value::String(_) => (
                    "int_parsing",
                    "Input should be a valid integer, unable to parse string as an integer",
                ),
                Value::Number(_) => (
                    "int_from_float",
                    "Input should be a valid integer, got a number with a fractional part",
                ),
                _ => ("int_type", "Input should be a valid integer"),
            };
            self.fail(field, message.0, message.1);
        }
        number
    }

    /// pydantic lax `bool`.
    fn lax_bool(&mut self, field: &'static str, default: bool) -> Option<bool> {
        let Some(value) = self.arguments.get(field) else {
            return Some(default);
        };
        let parsed = pre_parse(value);
        let result = match &parsed {
            Value::Bool(b) => Some(*b),
            Value::Number(n) => match n.as_f64() {
                Some(0.0) => Some(false),
                Some(1.0) => Some(true),
                _ => None,
            },
            Value::String(s) => match s.to_ascii_lowercase().as_str() {
                "0" | "off" | "f" | "false" | "n" | "no" => Some(false),
                "1" | "on" | "t" | "true" | "y" | "yes" => Some(true),
                _ => None,
            },
            _ => None,
        };
        if result.is_none() {
            self.fail(field, "bool_parsing", "Input should be a valid boolean");
        }
        result
    }

    /// pydantic lax `float` with `ge`/`le`.
    fn lax_float(
        &mut self,
        field: &'static str,
        default: f64,
        minimum: f64,
        maximum: f64,
    ) -> Option<f64> {
        let Some(value) = self.arguments.get(field) else {
            return Some(default);
        };
        let parsed = pre_parse(value);
        let number = match &parsed {
            Value::Number(n) => n.as_f64(),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::String(s) => crate::py::text::strip(s).parse::<f64>().ok(),
            _ => None,
        };
        let Some(number) = number else {
            self.fail(field, "float_type", "Input should be a valid number");
            return None;
        };
        // NaN fails both bounds, as in pydantic.
        if number.is_nan() || number < minimum {
            self.fail(
                field,
                "greater_than_equal",
                format!(
                    "Input should be greater than or equal to {}",
                    fmt_bound(minimum)
                ),
            );
            return None;
        }
        if number > maximum {
            self.fail(
                field,
                "less_than_equal",
                format!(
                    "Input should be less than or equal to {}",
                    fmt_bound(maximum)
                ),
            );
            return None;
        }
        Some(number)
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn fmt_bound(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        crate::py::float::repr(value)
    }
}

pub struct SearchArgs {
    pub query: String,
    pub limit: i64,
    pub include_history: bool,
    pub min_similarity: f64,
    pub folder: String,
}

pub fn search_notes(arguments: &Map<String, Value>) -> Result<SearchArgs, ValidationErrors> {
    let mut v = Validator {
        arguments,
        errors: Vec::new(),
    };
    let query = v.string("query", None, 1, 4000);
    let limit = v.strict_int("limit", 5, 1);
    let include_history = v.lax_bool("include_history", true);
    let min_similarity = v.lax_float("min_similarity", 0.35, 0.0, 1.0);
    let folder = v.string("folder", Some(""), 0, 2048);
    match (query, limit, include_history, min_similarity, folder) {
        (Some(query), Some(limit), Some(include_history), Some(min_similarity), Some(folder))
            if v.errors.is_empty() =>
        {
            Ok(SearchArgs {
                query,
                limit,
                include_history,
                min_similarity,
                folder,
            })
        }
        _ => Err(ValidationErrors {
            model: "search_notesArguments",
            errors: v.errors,
        }),
    }
}

pub fn get_note_chunk(arguments: &Map<String, Value>) -> Result<(String, i64), ValidationErrors> {
    let mut v = Validator {
        arguments,
        errors: Vec::new(),
    };
    let page_key = v.string("page_key", None, 0, usize::MAX);
    let ordinal = v.lax_int("ordinal");
    match (page_key, ordinal) {
        (Some(page_key), Some(ordinal)) if v.errors.is_empty() => Ok((page_key, ordinal)),
        _ => Err(ValidationErrors {
            model: "get_note_chunkArguments",
            errors: v.errors,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn defaults_and_extra_arguments() {
        let parsed = search_notes(&args(json!({"query": "x", "owner": true})))
            .ok()
            .unwrap();
        assert_eq!(
            (
                parsed.limit,
                parsed.include_history,
                parsed.min_similarity,
                parsed.folder.as_str()
            ),
            (5, true, 0.35, "")
        );
    }

    #[test]
    fn invalid_limits_rejected() {
        for bad in [
            json!({"query": "x", "limit": 0}),
            json!({"query": "x", "limit": -1}),
            json!({"query": "x", "min_similarity": 1.1}),
            json!({"query": "x", "limit": true}),
            json!({"query": "x", "limit": "5"}),
            json!({"query": ""}),
            json!({"query": "x".repeat(4001)}),
            json!({}),
            json!({"query": 1}),
        ] {
            assert!(search_notes(&args(bad.clone())).is_err(), "{bad}");
        }
        assert_eq!(
            search_notes(&args(json!({"query": "x", "limit": 10})))
                .ok()
                .unwrap()
                .limit,
            10
        );
    }

    #[test]
    fn lax_coercions() {
        let parsed = search_notes(&args(
            json!({"query": "x", "include_history": "no", "min_similarity": "0.3"}),
        ))
        .ok()
        .unwrap();
        assert!(!parsed.include_history);
        assert_eq!(parsed.min_similarity, 0.3);
        assert_eq!(
            get_note_chunk(&args(json!({"page_key": "a", "ordinal": 2.0})))
                .ok()
                .unwrap()
                .1,
            2
        );
        assert!(get_note_chunk(&args(json!({"page_key": "a", "ordinal": 2.5}))).is_err());
    }

    #[test]
    fn message_layout() {
        let error = search_notes(&args(json!({"query": "x", "limit": 0})))
            .err()
            .unwrap();
        assert_eq!(
            error.message(),
            "1 validation error for search_notesArguments\nlimit\n  Input should be greater than or equal to 1 [type=greater_than_equal]"
        );
    }
}
