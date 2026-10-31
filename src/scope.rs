//! Port of `folder_scope.py`: literal, corpus-relative folder scopes; never
//! filesystem paths or SQL patterns.

use crate::fault::{Fault, Result};
use crate::py::text;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const MAX_FOLDER_LENGTH: usize = 2048;

/// `validate_folder(value)` for a value already known to be a string.
pub fn validate_folder(value: &str) -> Result<&str> {
    let invalid = Fault::Value("Invalid folder scope");
    if text::len(value) > MAX_FOLDER_LENGTH {
        return Err(invalid);
    }
    if !value.is_empty()
        && (value.split('/').any(|part| matches!(part, "" | "." | ".."))
            || value.contains('\\')
            || value.chars().any(|c| (c as u32) < 32 || c as u32 == 127))
    {
        return Err(invalid);
    }
    Ok(value)
}

/// `validate_folder(value)` for an arbitrary decoded JSON value.
pub fn validate_folder_value(value: &Value) -> Result<&str> {
    match value {
        Value::String(s) => validate_folder(s),
        _ => Err(Fault::Value("Invalid folder scope")),
    }
}

/// `folder_catalog(paths)`: descendant page counts for every folder prefix.
pub fn folder_catalog(paths: &[String]) -> Result<Value> {
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for path in paths {
        if !path.ends_with(".md") {
            return Err(Fault::Value("Invalid indexed path"));
        }
        validate_folder(path)?;
        let parts: Vec<&str> = path.split('/').collect();
        for depth in 1..parts.len() {
            *counts.entry(parts[..depth].join("/")).or_default() += 1;
        }
    }
    if counts.len() > 10000 {
        return Err(Fault::Value("Too many indexed folders"));
    }
    let mut ordered: Vec<(String, String, i64)> = counts
        .into_iter()
        .map(|(path, pages)| (text::casefold(&path), path, pages))
        .collect();
    // Python: sorted(counts, key=lambda p: (p.casefold(), p))
    ordered.sort();
    let folders: Vec<Value> = ordered
        .into_iter()
        .map(|(_, path, pages)| {
            let (parent, _, name) = text::rpartition(&path, "/");
            json!({"path": path, "parent": parent, "name": name, "pages": pages})
        })
        .collect();
    Ok(json!({"indexed_pages": paths.len(), "folders": folders}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_unicode_names_and_empty_root() {
        for value in ["", "실습 (test)/100%_O'Reilly", "a/%2e%2e"] {
            assert_eq!(validate_folder(value).unwrap(), value);
        }
    }

    #[test]
    fn invalid_paths() {
        let long = "x".repeat(2049);
        for value in [
            "/a",
            "a/",
            "a//b",
            ".",
            "..",
            "a/../b",
            "a\\b",
            "a\n",
            long.as_str(),
        ] {
            assert!(validate_folder(value).is_err(), "{value:?}");
        }
        for value in [Value::Null, json!([]), json!(1)] {
            assert!(validate_folder_value(&value).is_err());
        }
    }

    #[test]
    fn catalog_counts_descendants_only_and_no_root_files_as_folders() {
        let paths: Vec<String> = ["root.md", "노트/a.md", "노트/하위/b.md", "노트2/c.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let result = folder_catalog(&paths).unwrap();
        assert_eq!(result["indexed_pages"], 4);
        assert_eq!(
            result["folders"],
            json!([
                {"path": "노트", "parent": "", "name": "노트", "pages": 2},
                {"path": "노트/하위", "parent": "노트", "name": "하위", "pages": 1},
                {"path": "노트2", "parent": "", "name": "노트2", "pages": 1}
            ])
        );
    }

    #[test]
    fn casefold_order_then_exact() {
        let paths: Vec<String> = ["b/x.md", "A/x.md", "a/x.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let names: Vec<String> = folder_catalog(&paths).unwrap()["folders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["A", "a", "b"]);
    }

    #[test]
    fn invalid_indexed_paths_fail_closed() {
        for value in ["../x.md", "x.txt", "/x.md"] {
            assert!(folder_catalog(&[value.to_string()]).is_err());
        }
    }
}
