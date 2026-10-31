//! Port of `semantic_graph.py`. Node/edge similarity is computed entirely in
//! PostgreSQL (unchanged SQL); this module only assembles JSON, filters it for
//! the caller's live visibility, and caches it for one hour per service.
//! The browser Barnes-Hut layout (`static/graph-layout.js`) is untouched.

use crate::fault::{Fault, Result};
use crate::search::context::round5;
use crate::search::store::{Param, SearchStore, db, numbered};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};
use tokio_postgres::types::Type;

pub const MAX_PAGES: i64 = 4000;
pub const NEIGHBORS: i64 = 5;
pub const MIN_SIMILARITY: f64 = 0.45;
const CACHE_SECONDS: u64 = 3600;

pub struct PageRow {
    pub page_key: String,
    pub title: String,
    pub relative_path: String,
    pub status: String,
    pub chunks: i64,
}

pub struct LinkRow {
    pub source: String,
    pub target: String,
    pub similarity: Option<f64>,
}

/// `assemble_graph(pages, links, info)`
pub fn assemble_graph(
    pages: &[PageRow],
    links: &[LinkRow],
    info: Map<String, Value>,
    generated_at: String,
) -> Value {
    let indices: HashMap<&str, usize> = pages
        .iter()
        .enumerate()
        .map(|(i, p)| (p.page_key.as_str(), i))
        .collect();
    let nodes: Vec<Value> = pages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            json!({
                "id": i,
                "title": p.title,
                "path": p.relative_path,
                "notebook": p.relative_path.split('/').next().unwrap_or(""),
                "chunks": p.chunks,
                "history": p.status == "old",
            })
        })
        .collect();
    let mut edges: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    for link in links {
        let (Some(&a), Some(&b)) = (
            indices.get(link.source.as_str()),
            indices.get(link.target.as_str()),
        ) else {
            continue;
        };
        let Some(score) = link.similarity.filter(|s| s.is_finite()) else {
            continue;
        };
        if a == b || score < MIN_SIMILARITY {
            continue;
        }
        let key = (a.min(b), a.max(b));
        let entry = edges.entry(key).or_insert(-1.0);
        *entry = entry.max(score.min(1.0));
    }
    let edges: Vec<Value> = edges
        .into_iter()
        .map(|((a, b), score)| json!({"source": a, "target": b, "similarity": round5(score)}))
        .collect();
    json!({
        "nodes": nodes,
        "edges": edges,
        "index": info,
        "method": "mean-chunk-cosine-knn-v1",
        "dimensions": 1536,
        "neighbors": NEIGHBORS,
        "min_similarity": MIN_SIMILARITY,
        "generated_at": generated_at,
    })
}

/// `visible_graph(graph, paths)`: filter without mutating the cached graph or
/// leaving dangling edge indices. Works on arbitrary upstream JSON (the web
/// gateway applies it to the MCP response too).
pub fn visible_graph(graph: &Value, paths: &BTreeSet<String>) -> Result<Value> {
    let malformed = Fault::Other("malformed graph");
    let object = graph.as_object().ok_or(malformed.clone())?;
    let nodes = object
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or(malformed.clone())?;
    let mut kept = Vec::new();
    let mut indices: HashMap<String, usize> = HashMap::new();
    for node in nodes {
        let path = node
            .get("path")
            .and_then(Value::as_str)
            .ok_or(malformed.clone())?;
        if paths.contains(path) {
            let id = node.get("id").ok_or(malformed.clone())?;
            indices.insert(id.to_string(), kept.len());
            kept.push(node);
        }
    }
    let new_nodes: Vec<Value> = kept
        .iter()
        .map(|node| {
            let mut node = node.as_object().cloned().unwrap_or_default();
            let id = node.get("id").map(Value::to_string).unwrap_or_default();
            node.insert("id".into(), indices[&id].into());
            Value::Object(node)
        })
        .collect();
    let edges = object
        .get("edges")
        .and_then(Value::as_array)
        .ok_or(malformed.clone())?;
    let mut new_edges = Vec::new();
    for edge in edges {
        let endpoint = |name: &str| {
            edge.get(name)
                .map(Value::to_string)
                .and_then(|k| indices.get(&k).copied())
        };
        let (Some(source), Some(target)) = (endpoint("source"), endpoint("target")) else {
            continue;
        };
        let mut edge = edge.as_object().cloned().ok_or(malformed.clone())?;
        edge.insert("source".into(), source.into());
        edge.insert("target".into(), target.into());
        new_edges.push(Value::Object(edge));
    }
    let mut index = object
        .get("index")
        .and_then(Value::as_object)
        .cloned()
        .ok_or(malformed)?;
    index.insert("indexed_pages".into(), kept.len().into());
    let mut result = object.clone();
    result.insert("nodes".into(), new_nodes.into());
    result.insert("edges".into(), new_edges.into());
    result.insert("index".into(), index.into());
    Ok(Value::Object(result))
}

type CacheKey = (String, i64, String);

/// `SemanticGraph`: one cache per `SearchService` (public and owner never share).
#[derive(Default)]
pub struct SemanticGraph {
    state: tokio::sync::Mutex<Option<(CacheKey, Instant, Value)>>,
}

impl SemanticGraph {
    /// `SemanticGraph.get()`: never queue builds; generation/access are checked
    /// in a fresh snapshot even on cache hits.
    pub async fn get(&self, store: &SearchStore) -> Result<Value> {
        let Ok(mut cache) = self.state.try_lock() else {
            return Err(Fault::search("Graph is building; retry shortly."));
        };
        let snapshot = store.snapshot("").await?;
        let info = snapshot.info.clone();
        let key: CacheKey = (
            info.updated_at.clone(),
            info.indexed_pages,
            info.access_revision.clone(),
        );
        if let Some((cached_key, until, value)) = cache.as_ref()
            && *cached_key == key
            && Instant::now() < *until
        {
            let value = value.clone();
            snapshot.finish().await?;
            return Ok(value);
        }
        if info.indexed_pages > MAX_PAGES {
            return Err(Fault::search("Graph exceeds the configured size limit."));
        }
        snapshot
            .client
            .batch_execute("SET LOCAL statement_timeout = '25s'")
            .await
            .map_err(db)?;
        let (access, bound) = store.access_clause("p.");
        let notebooks = store.notebooks();
        let mut params: Vec<Param> = Vec::new();
        if bound {
            params.push((&notebooks, Type::TEXT_ARRAY));
        }
        // All metadata and neighbors use the SAME ACL and read snapshot.
        let pages_sql = numbered(&format!(
            "SELECT p.page_key, p.title, p.relative_path,
                    p.status, count(c.ordinal)::int AS chunks
                    FROM notes_rag.pages p JOIN notes_rag.chunks c USING(page_key)
                    WHERE {access} GROUP BY p.page_key ORDER BY p.page_key"
        ));
        let pages = snapshot
            .rows(&pages_sql, &params)
            .await?
            .iter()
            .map(|row| {
                Ok(PageRow {
                    page_key: row.try_get("page_key").map_err(db)?,
                    title: row.try_get("title").map_err(db)?,
                    relative_path: row.try_get("relative_path").map_err(db)?,
                    status: row.try_get("status").map_err(db)?,
                    chunks: i64::from(row.try_get::<_, i32>("chunks").map_err(db)?),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let links_sql = numbered(&format!(
            "WITH centroids AS MATERIALIZED (
                    SELECT p.page_key,
                      coalesce(avg(c.embedding) FILTER (WHERE c.rag_priority = 'normal'
                               AND p.status = 'current'), avg(c.embedding)) AS vector
                    FROM notes_rag.pages p JOIN notes_rag.chunks c USING(page_key)
                    WHERE {access} GROUP BY p.page_key
                  ) SELECT a.page_key AS source, neighbor.* FROM centroids a
                  CROSS JOIN LATERAL (
                    SELECT b.page_key AS target, 1 - (a.vector <=> b.vector) AS similarity
                    FROM centroids b WHERE a.page_key <> b.page_key
                    ORDER BY a.vector <=> b.vector, b.page_key LIMIT 5
                  ) neighbor"
        ));
        let links = snapshot
            .rows(&links_sql, &params)
            .await?
            .iter()
            .map(|row| {
                Ok(LinkRow {
                    source: row.try_get("source").map_err(db)?,
                    target: row.try_get("target").map_err(db)?,
                    similarity: row.try_get::<_, Option<f64>>("similarity").map_err(db)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        snapshot.finish().await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let result = assemble_graph(
            &pages,
            &links,
            info.to_json(),
            crate::timefmt::gmtime_z(now as i64),
        );
        *cache = Some((
            key,
            Instant::now() + Duration::from_secs(CACHE_SECONDS),
            result.clone(),
        ));
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(key: &str) -> PageRow {
        PageRow {
            page_key: key.into(),
            title: "<script>title</script>".into(),
            relative_path: "public/note.md".into(),
            status: "current".into(),
            chunks: 2,
        }
    }

    fn link(a: &str, b: &str, s: f64) -> LinkRow {
        LinkRow {
            source: a.into(),
            target: b.into(),
            similarity: Some(s),
        }
    }

    #[test]
    fn deduplicates_edges_excludes_missing_invalid_and_weak() {
        let links = [
            link("a", "b", 0.8),
            link("b", "a", 0.8),
            link("a", "a", 1.0),
            link("a", "secret", 0.99),
            link("a", "c", f64::NAN),
            link("b", "c", 0.44),
        ];
        let data = assemble_graph(
            &[page("a"), page("b"), page("c")],
            &links,
            Map::new(),
            "t".into(),
        );
        assert_eq!(
            data["edges"],
            json!([{"source": 0, "target": 1, "similarity": 0.8}])
        );
        assert!(
            data["nodes"][0].get("page_key").is_none() && data["nodes"][0].get("vector").is_none()
        );
        assert_eq!(data["nodes"].as_array().unwrap().len(), 3);
        let keys: Vec<&String> = data.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "nodes",
                "edges",
                "index",
                "method",
                "dimensions",
                "neighbors",
                "min_similarity",
                "generated_at"
            ]
        );
    }

    #[test]
    fn visible_graph_remaps_and_does_not_mutate() {
        let graph = json!({"nodes": [{"id": 0, "path": "a.md", "title": "a"}, {"id": 1, "path": "b.md", "title": "b"},
                                     {"id": 2, "path": "c.md", "title": "c"}],
                           "edges": [{"source": 0, "target": 1}, {"source": 1, "target": 2}],
                           "index": {"indexed_pages": 3}});
        let visible: BTreeSet<String> = ["b.md".to_string(), "c.md".to_string()].into();
        let result = visible_graph(&graph, &visible).unwrap();
        assert_eq!(
            result["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n["id"].clone())
                .collect::<Vec<_>>(),
            [json!(0), json!(1)]
        );
        assert_eq!(result["edges"], json!([{"source": 0, "target": 1}]));
        assert_eq!(result["index"]["indexed_pages"], 2);
        assert_eq!(graph["nodes"].as_array().unwrap().len(), 3);
    }
}
