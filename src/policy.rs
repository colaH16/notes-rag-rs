//! Ports of `preflight.load_allowlist` and `access_policy.load_public_notebooks`.
//!
//! Both files are private mounts. They are re-read on every reader permission
//! check (as in Python), so a policy rotation is observed without restart and
//! a mismatch fails closed.

use crate::fault::{Fault, Result};
use crate::py::{json as pyjson, sha256_hex};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

/// The validated private Markdown selection (`markdown.json`).
#[derive(Debug, Clone)]
pub struct Allowlist {
    pub ids: BTreeSet<String>,
    pub payload: Value,
}

impl Allowlist {
    /// `json.loads(selection.read_text())['silverBulletRoot']`
    pub fn silverbullet_root(&self) -> Option<&str> {
        self.payload.get("silverBulletRoot").and_then(Value::as_str)
    }
}

/// `preflight.load_allowlist(path)`; every failure is a `PreflightError`
/// (a plain `Exception` subclass, i.e. [`Fault::Other`]).
pub fn load_allowlist(path: &Path) -> Result<Allowlist> {
    let preflight = Fault::Other("invalid Markdown allowlist");
    let is_symlink = std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if path.file_name().and_then(|n| n.to_str()) != Some("markdown.json") || is_symlink {
        return Err(Fault::Other(
            "allowlist must be the private markdown.json, not export.json",
        ));
    }
    let raw = std::fs::read(path).map_err(|_| preflight.clone())?;
    let text = String::from_utf8(raw).map_err(|_| preflight.clone())?;
    let payload: Value = serde_json::from_str(&text).map_err(|_| preflight.clone())?;
    let Some(entries) = payload.get("notebooks").and_then(Value::as_array) else {
        return Err(preflight);
    };
    let nonblank = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !crate::py::text::strip(s).is_empty())
    };
    if payload.get("schemaVersion") != Some(&Value::from(1))
        || !nonblank("silverBulletRoot")
        || !nonblank("sourceArchiveRoot")
    {
        return Err(preflight);
    }
    let mut ids = BTreeSet::new();
    for entry in entries {
        let Some(id) = entry
            .as_object()
            .and_then(|e| e.get("id"))
            .and_then(Value::as_str)
        else {
            return Err(Fault::Other(
                "Markdown allowlist contains an empty or invalid notebook ID",
            ));
        };
        if id.is_empty() {
            return Err(Fault::Other(
                "Markdown allowlist contains an empty or invalid notebook ID",
            ));
        }
        if !ids.insert(id.to_string()) {
            return Err(Fault::Other(
                "Markdown allowlist contains duplicate notebook IDs",
            ));
        }
    }
    if ids.is_empty() {
        return Err(Fault::Other(
            "Markdown allowlist contains an empty or invalid notebook ID",
        ));
    }
    Ok(Allowlist { ids, payload })
}

/// `access_policy.load_public_notebooks(path, indexed)` → (sorted IDs, revision).
pub fn load_public_notebooks(
    path: &Path,
    indexed: &BTreeSet<String>,
) -> Result<(Vec<String>, String)> {
    let raw = std::fs::read(path).map_err(|_| Fault::Os)?;
    let text = String::from_utf8(raw).map_err(|_| Fault::Value("invalid UTF-8"))?;
    let data: Value =
        serde_json::from_str(&text).map_err(|_| Fault::Value("Invalid access policy"))?;
    if !data.is_object() || data.get("schemaVersion") != Some(&Value::from(1)) {
        return Err(Fault::Value("Invalid access policy"));
    }
    let invalid = Fault::Value("Invalid public notebook allowlist");
    let Some(values) = data.get("publicNotebookIds").and_then(Value::as_array) else {
        return Err(invalid);
    };
    let mut ids = Vec::with_capacity(values.len());
    for value in values {
        match value.as_str() {
            Some(id) if !id.is_empty() => ids.push(id.to_string()),
            _ => return Err(invalid),
        }
    }
    let unique: BTreeSet<&String> = ids.iter().collect();
    if unique.len() != ids.len() || !ids.iter().all(|id| indexed.contains(id)) {
        return Err(invalid);
    }
    ids.sort();
    Ok((ids.clone(), revision_of(&ids)))
}

/// `hashlib.sha256(json.dumps(tuple(sorted(ids))).encode()).hexdigest()`
pub fn revision_of(sorted_ids: &[String]) -> String {
    let list = Value::Array(sorted_ids.iter().cloned().map(Value::String).collect());
    sha256_hex(pyjson::dumps_ascii(&list))
}

/// `sha256(('owner:' + revision).encode())`: owner audience revision.
pub fn owner_revision(revision: &str) -> String {
    sha256_hex(format!("owner:{revision}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(dir: &Path, name: &str, value: &Value) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, value.to_string()).unwrap();
        path
    }

    #[test]
    fn explicit_allowlist_and_default_private() {
        let dir = tempfile::tempdir().unwrap();
        let indexed: BTreeSet<String> = ["public", "private", "new"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let path = write(
            dir.path(),
            "access.json",
            &json!({"schemaVersion": 1, "publicNotebookIds": ["public"]}),
        );
        let (ids, revision) = load_public_notebooks(&path, &indexed).unwrap();
        assert_eq!(ids, ["public"]);
        assert_eq!(revision.len(), 64);
        // json.dumps(("public",)) == '["public"]'
        assert_eq!(revision, sha256_hex("[\"public\"]"));
        let only_public: BTreeSet<String> = ["public".to_string()].into();
        for values in [
            json!(["unknown"]),
            json!(["public", "public"]),
            json!([""]),
            Value::Null,
        ] {
            let path = write(
                dir.path(),
                "access.json",
                &json!({"schemaVersion": 1, "publicNotebookIds": values}),
            );
            assert!(matches!(
                load_public_notebooks(&path, &only_public),
                Err(Fault::Value(_))
            ));
        }
        let path = write(
            dir.path(),
            "access.json",
            &json!({"schemaVersion": 1, "publicNotebookIds": []}),
        );
        assert!(
            load_public_notebooks(&path, &only_public)
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[test]
    fn allowlist_shape_is_strict() {
        let dir = tempfile::tempdir().unwrap();
        let good = json!({"schemaVersion": 1, "silverBulletRoot": "OneNote/markdown", "sourceArchiveRoot": "a",
                          "notebooks": [{"id": "b"}, {"id": "a"}]});
        let path = write(dir.path(), "markdown.json", &good);
        assert_eq!(
            load_allowlist(&path)
                .unwrap()
                .ids
                .into_iter()
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        let renamed = write(dir.path(), "export.json", &good);
        assert!(load_allowlist(&renamed).is_err());
        for bad in [
            json!({"schemaVersion": 1, "silverBulletRoot": " ", "sourceArchiveRoot": "a", "notebooks": [{"id": "a"}]}),
            json!({"schemaVersion": 1, "silverBulletRoot": "r", "sourceArchiveRoot": "a", "notebooks": [{"id": "a"}, {"id": "a"}]}),
            json!({"schemaVersion": 1, "silverBulletRoot": "r", "sourceArchiveRoot": "a", "notebooks": ["a"]}),
            json!({"schemaVersion": 1, "silverBulletRoot": "r", "sourceArchiveRoot": "a", "notebooks": []}),
        ] {
            let path = write(dir.path(), "markdown.json", &bad);
            assert!(load_allowlist(&path).is_err(), "{bad}");
        }
    }
}
