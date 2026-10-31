//! Pure parts of `search.py`: neighbor expansion (`contextual_row`), citation
//! URLs and the MCP result item shape. No I/O here.

use crate::fault::Fault;
use crate::py::{path as pypath, text, url};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub const CONTEXT_CHARACTERS: usize = 6000;
pub const CHUNK_CHARACTERS: usize = 131072;

/// One row selected from `notes_rag.chunks JOIN notes_rag.pages`.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub page_key: String,
    pub ordinal: i64,
    pub title: String,
    pub relative_path: String,
    pub status: String,
    pub needs_visual_review: bool,
    pub rag_priority: String,
    pub content: String,
    pub similarity: Option<f64>,
    pub context: Option<Context>,
}

/// Adjacent chunk returned by the neighbor query.
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbor {
    pub page_key: String,
    pub ordinal: i64,
    pub rag_priority: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Context {
    pub matched_ordinal: i64,
    pub included_ordinals: Vec<i64>,
    pub omitted_ordinals: Vec<i64>,
    pub expanded: bool,
    pub character_limit: usize,
}

impl Context {
    fn to_json(&self) -> Value {
        json!({
            "matched_ordinal": self.matched_ordinal,
            "included_ordinals": self.included_ordinals,
            "omitted_ordinals": self.omitted_ordinals,
            "expanded": self.expanded,
            "character_limit": self.character_limit,
        })
    }
}

fn render(parts: &BTreeMap<i64, String>) -> String {
    if parts.len() == 1 {
        return parts.values().next().cloned().unwrap_or_default();
    }
    parts
        .values()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

/// `contextual_row(row, neighbors, maximum)`: keep the matched chunk intact,
/// then fit whole adjacent chunks (next first), rendered in source order.
pub fn contextual_row(row: &Hit, neighbors: &[Neighbor], maximum: usize) -> Hit {
    let center = row.ordinal;
    let mut candidates: BTreeMap<i64, &Neighbor> = BTreeMap::new();
    for n in neighbors {
        if n.page_key == row.page_key
            && n.rag_priority == row.rag_priority
            && (n.ordinal - center).abs() == 1
        {
            candidates.insert(n.ordinal, n);
        }
    }
    let mut included: BTreeMap<i64, String> = BTreeMap::from([(center, row.content.clone())]);
    for ordinal in [center + 1, center - 1] {
        if let Some(candidate) = candidates.get(&ordinal) {
            let mut proposed = included.clone();
            proposed.insert(ordinal, candidate.content.clone());
            if text::len(&render(&proposed)) <= maximum {
                included = proposed;
            }
        }
    }
    let context = Context {
        matched_ordinal: center,
        included_ordinals: included.keys().copied().collect(),
        omitted_ordinals: candidates
            .keys()
            .copied()
            .filter(|o| !included.contains_key(o))
            .collect(),
        expanded: included.len() > 1,
        character_limit: maximum,
    };
    Hit {
        content: render(&included),
        context: Some(context),
        ..row.clone()
    }
}

/// `citation_url(relative_path, base, root)`
pub fn citation_url(relative_path: &str, base: &str, root: &str) -> Result<String, Fault> {
    let refused = || Fault::search("Invalid source path; citation refused.");
    let path_parts = pypath::parts(relative_path);
    let root_parts = pypath::parts(root);
    if relative_path.starts_with('/')
        || root.starts_with('/')
        || path_parts.contains(&"..")
        || root_parts.contains(&"..")
        || pypath::suffix(relative_path) != ".md"
        || relative_path.contains('\\')
        || root.contains('\\')
        || relative_path.contains(':')
        || root.contains(':')
        || relative_path
            .chars()
            .chain(root.chars())
            .any(|c| (c as u32) < 32)
    {
        return Err(refused());
    }
    let parsed =
        url::urlsplit(base).ok_or_else(|| Fault::search("Invalid SilverBullet base URL."))?;
    let userinfo = text::rpartition(&parsed.netloc, "@").0;
    let host = text::partition(text::rpartition(&parsed.netloc, "@").2, ":").0;
    if parsed.scheme != "https"
        || host.is_empty()
        || !userinfo.is_empty()
        || !parsed.query.is_empty()
        || !parsed.fragment.is_empty()
        || !matches!(parsed.path.as_str(), "" | "/")
    {
        return Err(Fault::search("Invalid SilverBullet base URL."));
    }
    // str(PurePosixPath(root) / PurePosixPath(relative).with_suffix(''))
    let mut parts: Vec<&str> = root_parts;
    let last = path_parts.len() - 1;
    for (index, part) in path_parts.iter().enumerate() {
        parts.push(if index == last {
            pypath::stem(part)
        } else {
            part
        });
    }
    Ok(format!(
        "{}/{}",
        base.trim_end_matches('/'),
        url::quote(&parts.join("/"), "/")
    ))
}

/// `result_item(row, base, root, maximum)` → MCP result object.
pub fn result_item(row: &Hit, base: &str, root: &str, maximum: usize) -> Result<Value, Fault> {
    let omitted = row
        .context
        .as_ref()
        .is_some_and(|c| !c.omitted_ordinals.is_empty());
    let mut item = Map::new();
    item.insert("page_key".into(), row.page_key.clone().into());
    item.insert("ordinal".into(), row.ordinal.into());
    item.insert("title".into(), row.title.clone().into());
    item.insert("status".into(), row.status.clone().into());
    item.insert("rag_priority".into(), row.rag_priority.clone().into());
    item.insert("needs_visual_review".into(), row.needs_visual_review.into());
    item.insert(
        "source_url".into(),
        citation_url(&row.relative_path, base, root)?.into(),
    );
    item.insert("content".into(), text::prefix(&row.content, maximum).into());
    item.insert(
        "truncated".into(),
        (text::len(&row.content) > maximum || omitted).into(),
    );
    item.insert("evidence_only".into(), true.into());
    if let Some(similarity) = row.similarity {
        item.insert("similarity".into(), round5(similarity).into());
    }
    if let Some(context) = &row.context {
        item.insert("context".into(), context.to_json());
    }
    Ok(Value::Object(item))
}

/// Python `round(x, 5)` (correctly rounded, half-even on the exact binary value).
pub fn round5(value: f64) -> f64 {
    // CPython rounds the exact decimal expansion; formatting with 5 decimals
    // via Rust's correctly-rounded `{:.5}` gives the same digits, and parsing
    // them back yields the nearest double, exactly as CPython's round().
    format!("{value:.5}").parse().unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://notes.example.invalid";
    const ROOT: &str = "OneNote/markdown";

    fn row() -> Hit {
        Hit {
            page_key: "a".repeat(64),
            ordinal: 2,
            title: "Synthetic".into(),
            relative_path: "실습/dnf-conf.md".into(),
            status: "current".into(),
            needs_visual_review: true,
            rag_priority: "normal".into(),
            content: "matched procedure".into(),
            similarity: Some(0.8),
            context: None,
        }
    }

    fn neighbor(hit: &Hit, ordinal: i64, content: &str) -> Neighbor {
        Neighbor {
            page_key: hit.page_key.clone(),
            ordinal,
            rag_priority: hit.rag_priority.clone(),
            content: content.into(),
        }
    }

    #[test]
    fn neighbors_order_identity_priority_and_rank_preserved() {
        let hit = row();
        let mut candidates: Vec<Neighbor> = [0, 1, 3, 4]
            .iter()
            .map(|i| neighbor(&hit, *i, &format!("chunk{i}")))
            .collect();
        candidates.push(Neighbor {
            page_key: "b".repeat(64),
            ..neighbor(&hit, 3, "OTHER PAGE")
        });
        candidates.push(Neighbor {
            rag_priority: "fallback".into(),
            ..neighbor(&hit, 1, "OLD")
        });
        let expanded = contextual_row(&hit, &candidates, CONTEXT_CHARACTERS);
        assert_eq!(
            expanded.context.as_ref().unwrap().included_ordinals,
            [1, 2, 3]
        );
        let c = &expanded.content;
        assert!(
            c.find("chunk1") < c.find("matched procedure")
                && c.find("matched procedure") < c.find("chunk3")
        );
        for forbidden in ["chunk0", "chunk4", "OTHER PAGE", "OLD"] {
            assert!(!c.contains(forbidden));
        }
        assert_eq!(expanded.ordinal, 2);
        assert_eq!(expanded.similarity, hit.similarity);
        assert_eq!(hit.content, "matched procedure");
    }

    #[test]
    fn next_chunk_has_budget_priority_and_no_partial_neighbor() {
        let hit = row();
        let neighbors = [
            neighbor(&hit, 1, &"x".repeat(80)),
            neighbor(&hit, 3, &"x".repeat(80)),
        ];
        let expanded = contextual_row(&hit, &neighbors, 160);
        let context = expanded.context.as_ref().unwrap();
        assert_eq!(
            (
                context.included_ordinals.as_slice(),
                context.omitted_ordinals.as_slice()
            ),
            (&[2, 3][..], &[1][..])
        );
        assert!(text::len(&expanded.content) <= 160);
        assert_eq!(
            result_item(&expanded, BASE, ROOT, CONTEXT_CHARACTERS).unwrap()["truncated"],
            true
        );
    }

    #[test]
    fn large_atomic_center_keeps_truncation_and_reference() {
        let hit = Hit {
            content: "x".repeat(6001),
            ..row()
        };
        let expanded = contextual_row(&hit, &[neighbor(&hit, 3, "next")], CONTEXT_CHARACTERS);
        assert!(!expanded.context.as_ref().unwrap().expanded);
        let item = result_item(&expanded, BASE, ROOT, CONTEXT_CHARACTERS).unwrap();
        assert_eq!(item["truncated"], true);
        assert_eq!(text::len(item["content"].as_str().unwrap()), 6000);
    }

    #[test]
    fn history_hit_cannot_import_current_neighbor() {
        let hit = Hit {
            rag_priority: "fallback".into(),
            ..row()
        };
        let current = Neighbor {
            rag_priority: "normal".into(),
            ..neighbor(&hit, 1, "CURRENT")
        };
        let expanded = contextual_row(&hit, &[current], CONTEXT_CHARACTERS);
        assert_eq!(expanded.context.unwrap().included_ordinals, [2]);
        assert!(!expanded.content.contains("CURRENT"));
    }

    #[test]
    fn citation_encodes_special_characters_and_rejects_escape() {
        let result = citation_url("a (b)/한글 #%.md", BASE, ROOT).unwrap();
        assert!(result.contains("%23%25") && !result.contains("./"));
        assert!(result.starts_with("https://notes.example.invalid/OneNote/markdown/"));
        for path in ["../x.md", "/x.md", "https://evil/x.md", "x.txt", "a\\b.md"] {
            assert!(citation_url(path, BASE, ROOT).is_err(), "{path}");
        }
        assert!(citation_url("x.md", "http://notes.example.invalid", ROOT).is_err());
        assert!(citation_url("x.md", "https://notes.example.invalid/sub", ROOT).is_err());
    }

    #[test]
    fn result_item_shape_and_rounding() {
        let item = result_item(
            &Hit {
                similarity: Some(0.721544999),
                ..row()
            },
            BASE,
            ROOT,
            6000,
        )
        .unwrap();
        let keys: Vec<&String> = item.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "page_key",
                "ordinal",
                "title",
                "status",
                "rag_priority",
                "needs_visual_review",
                "source_url",
                "content",
                "truncated",
                "evidence_only",
                "similarity"
            ]
        );
        assert_eq!(item["similarity"], 0.72154);
        assert!(!item["source_url"].as_str().unwrap().contains(".md"));
    }
}
