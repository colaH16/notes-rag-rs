//! Port of `native_notes.py`: opt-in virtual metadata for native SilverBullet
//! notes. Never writes the source.

use crate::fault::{Fault, Result};
use crate::py::{json as pyjson, path as pypath, sha256_hex, text};
use serde_json::{Value, json};
use std::path::Path;

/// `RAG_NATIVE_NOTES=1` is mandatory for web and MCP (current-folder ACL).
pub fn folder_acl_enabled() -> bool {
    std::env::var("RAG_NATIVE_NOTES").as_deref() == Ok("1")
}

/// `require_folder_acl()`
pub fn require_folder_acl() -> std::result::Result<(), String> {
    if folder_acl_enabled() {
        Ok(())
    } else {
        Err("Current-folder ACL requires RAG_NATIVE_NOTES=1".into())
    }
}

/// `folder_notebook(relative, selection)`: resolve only a configured,
/// unambiguous current notebook folder. Re-reads the selection every call.
pub fn folder_notebook(relative: &str, selection: &Path) -> Result<String> {
    let parts = pypath::parts(relative);
    if relative.starts_with('/') || parts.len() < 2 || parts.iter().any(|p| p.starts_with('.')) {
        return Err(Fault::Value("Note is outside a configured notebook folder"));
    }
    let raw = std::fs::read(selection).map_err(|_| Fault::Os)?;
    let data: Value =
        serde_json::from_slice(&raw).map_err(|_| Fault::Value("invalid selection"))?;
    let rows = data
        .get("notebooks")
        .and_then(Value::as_array)
        .ok_or(Fault::Other("invalid selection"))?;
    let matches: Vec<&Value> = rows
        .iter()
        .filter(|row| row.get("name").and_then(Value::as_str) == Some(parts[0]))
        .map(|row| row.get("id").unwrap_or(&Value::Null))
        .collect();
    match matches.as_slice() {
        [Value::String(id)] => Ok(id.clone()),
        [_] => Err(Fault::Other("invalid notebook id")),
        _ => Err(Fault::Value("Unknown or ambiguous notebook folder")),
    }
}

/// `normalize(raw, relative, selection)`; `enabled` is `RAG_NATIVE_NOTES=1`,
/// read once at startup (production web/MCP always run with it enabled).
pub fn normalize(raw: &[u8], relative: &str, selection: &Path, enabled: bool) -> Result<Vec<u8>> {
    if !enabled {
        return Ok(raw.to_vec());
    }
    let text_value = std::str::from_utf8(raw).map_err(|_| Fault::Value("invalid UTF-8"))?;
    let lines = text::splitlines_keepends(text_value);
    let mut body = text_value.to_string();
    if !lines.is_empty() && text::strip_chars(lines[0], "\u{feff}\r\n ") == "---" {
        let end = lines
            .iter()
            .enumerate()
            .skip(1)
            .take(100)
            .find(|(_, line)| text::strip(line) == "---")
            .map(|(index, _)| index)
            .ok_or(Fault::Value("Unterminated native frontmatter"))?;
        let header = &lines[1..end];
        let key_of = |line: &str| text::strip(text::partition(line, ":").0).to_string();
        if header.iter().any(|line| {
            matches!(
                key_of(line).as_str(),
                "source_system" | "onenote_notebook_id" | "onenote_id"
            )
        }) {
            let source = header
                .iter()
                .find(|line| key_of(line) == "source_system")
                .map(|line| text::strip(text::partition(line, ":").2));
            if source != Some("\"onenote\"") {
                return Err(Fault::Value("Reserved source identity in native note"));
            }
            if header.iter().any(|line| {
                key_of(line) == "generated" && text::strip(text::partition(line, ":").2) == "true"
            }) {
                return Ok(raw.to_vec());
            }
            let notebook = folder_notebook(relative, selection)?;
            let keys: Vec<usize> = (1..end)
                .filter(|&i| key_of(lines[i]) == "onenote_notebook_id")
                .collect();
            let [index] = keys.as_slice() else {
                return Err(Fault::Value("Missing or duplicate notebook identity"));
            };
            let existing: Value = serde_json::from_str(text::partition(lines[*index], ":").2)
                .map_err(|_| Fault::Value("invalid notebook identity"))?;
            if existing == Value::String(notebook.clone()) {
                return Ok(raw.to_vec());
            }
            let newline = if lines[*index].ends_with("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let replacement = format!(
                "onenote_notebook_id: {}{}",
                pyjson::dumps_unicode(&Value::String(notebook)),
                newline
            );
            let mut rebuilt = String::with_capacity(text_value.len());
            for (i, line) in lines.iter().enumerate() {
                rebuilt.push_str(if i == *index { &replacement } else { line });
            }
            return Ok(rebuilt.into_bytes());
        }
        body = lines[end + 1..].concat();
    }
    let parts = pypath::parts(relative);
    if parts.len() < 2 || parts.iter().any(|p| p.starts_with('.')) {
        return Ok(raw.to_vec());
    }
    // Python: `except ValueError: return raw`; other failures propagate.
    let notebook = match folder_notebook(relative, selection) {
        Ok(notebook) => notebook,
        Err(Fault::Value(_)) => return Ok(raw.to_vec()),
        Err(other) => return Err(other),
    };
    let old = parts[1..]
        .iter()
        .any(|p| p.starts_with("--") || *p == "old");
    let normalized_path = parts.join("/");
    let metadata = json!({
        "source_system": "silverbullet",
        "content_type": if text::strip(&body).is_empty() { "draft" } else { "note" },
        "onenote_notebook_id": notebook,
        "onenote_id": format!("silverbullet:{}", sha256_hex(&normalized_path)),
        "title": pypath::stem(relative),
        "status": if old { "old" } else { "current" },
        "rag_priority": if old { "fallback" } else { "normal" },
        "needs_visual_review": false,
    });
    let mut out = String::from("---\n");
    for (key, value) in metadata.as_object().into_iter().flatten() {
        out.push_str(&format!("{key}: {}\n", pyjson::dumps_unicode(value)));
    }
    out.push_str("---\n");
    out.push_str(&body);
    Ok(out.into_bytes())
}
