//! Python-compatibility helpers.
//!
//! Learning note (Python → Rust): the original relies on many small CPython
//! behaviours (code-point lengths, `json.dumps` spacing, `urllib.parse`
//! quoting, `posixpath`). Reimplementing them in one place keeps the ported
//! business logic readable and lets each behaviour be unit-tested against the
//! CPython oracle (`tests/oracle/`).

pub mod float;
pub mod json;
pub mod path;
pub mod text;
pub mod url;

/// `hashlib.sha256(data).hexdigest()`
pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data.as_ref()))
}

/// `html.escape(s, quote=True)`
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}
