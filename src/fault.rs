//! Failure classes mirroring the Python exception hierarchy.
//!
//! Learning note (Python → Rust): the original handlers catch by exception
//! *class* (`except (NotReadable, OSError, ValueError)` → 404, any other
//! `Exception` → 503). Rust has no exception hierarchy, so every fallible
//! operation returns a `Fault` whose variant records the Python class that the
//! original would have raised. Handlers then `match` on it, which keeps HTTP
//! status codes identical without leaking private error text.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// `document_reader.NotReadable`
    NotReadable,
    /// `ValueError` (also `json.JSONDecodeError`, `UnicodeError`).
    Value(&'static str),
    /// `OSError` (file system, `TimeoutError`).
    Os,
    /// `search.SearchError`: fixed, non-sensitive message that may reach MCP callers.
    Search(String),
    /// `request_limits.Denied`: user-visible Korean message plus `Retry-After`.
    Denied { message: &'static str, retry: u64 },
    /// Any other exception (`RuntimeError`, `KeyError`, `PreflightError`, ...).
    Other(&'static str),
}

impl Fault {
    pub fn denied(message: &'static str) -> Self {
        Fault::Denied { message, retry: 60 }
    }

    pub fn search(message: impl Into<String>) -> Self {
        Fault::Search(message.into())
    }

    /// `except (NotReadable, OSError, ValueError)` as used by the reader routes.
    pub fn is_reader_not_found(&self) -> bool {
        matches!(self, Fault::NotReadable | Fault::Os | Fault::Value(_))
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::NotReadable => f.write_str("not readable"),
            Fault::Value(m) | Fault::Other(m) => f.write_str(m),
            Fault::Os => f.write_str("operating system error"),
            Fault::Search(m) => f.write_str(m),
            Fault::Denied { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for Fault {}

pub type Result<T, E = Fault> = std::result::Result<T, E>;
