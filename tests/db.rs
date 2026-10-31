//! Opt-in PostgreSQL + pgvector integration tests (search side of the
//! original `tests/test_index_store.py`). Like the original, they require
//! `RAG_TEST_DATABASE_URL` pointing at an *isolated localhost* server that
//! permits CREATE DATABASE; each test creates and drops its own database.
//! Only synthetic rows are written; the production schema DDL is the
//! original Python file, used here solely to build the throwaway database.

use notes_rag_rs::fault::Fault;
use notes_rag_rs::search::db::Database;
use notes_rag_rs::search::service::{Embedder, SearchRequest, SearchService};
use notes_rag_rs::search::store::SearchStore;
use serde_json::{Value, json};
use std::sync::Arc;

const DIMENSIONS: usize = 1536;

fn base_url() -> Option<String> {
    let url = std::env::var("RAG_TEST_DATABASE_URL").ok()?;
    let host_ok = ["@127.0.0.1", "@localhost", "@[::1]"]
        .iter()
        .any(|h| url.contains(h));
    assert!(
        host_ok,
        "integration tests require an isolated localhost DB"
    );
    Some(url)
}

fn with_database(url: &str, name: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map(|(b, q)| (b, Some(q)))
        .unwrap_or((url, None));
    let root = &base[..base.rfind('/').unwrap()];
    match query {
        Some(query) => format!("{root}/{name}?{query}"),
        None => format!("{root}/{name}"),
    }
}

struct TestDb {
    admin: tokio_postgres::Client,
    name: String,
    url: String,
    client: tokio_postgres::Client,
}

impl TestDb {
    async fn new() -> Option<Self> {
        let url = base_url()?;
        let admin = Database::from_dsn(&url).unwrap().connect().await.unwrap();
        let name = format!(
            "notes_rag_rs_test_{}",
            hex::encode(rand::random::<[u8; 8]>())
        );
        admin
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let url = with_database(&url, &name);
        let client = Database::from_dsn(&url).unwrap().connect().await.unwrap();
        for file in ["schema.sql", "access_schema.sql", "pruning_schema.sql"] {
            let ddl =
                std::fs::read_to_string(format!("tests/fixtures/python-schema/{file}")).unwrap();
            client
                .batch_execute(&format!("BEGIN; {ddl}; COMMIT"))
                .await
                .unwrap();
        }
        client
            .batch_execute("INSERT INTO notes_rag.generation(scope_signature, config_signature, updated_at) VALUES ('scope', 'config', '2026-10-31 15:16:17.123456+09')")
            .await
            .unwrap();
        Some(Self {
            admin,
            name,
            url,
            client,
        })
    }

    async fn page(
        &self,
        key: char,
        notebook: &str,
        path: &str,
        status: &str,
        chunks: &[(&str, &str, Vec<f32>)],
    ) {
        let page_key = key.to_string().repeat(64);
        self.client
            .execute(
                "INSERT INTO notes_rag.pages(page_key, notebook_id, source_sha256, relative_path, title, status, needs_visual_review) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[&page_key, &notebook, &"0".repeat(64), &path, &format!("Title {key}"), &status, &(key == 'e')],
            )
            .await
            .unwrap();
        for (ordinal, (priority, content, vector)) in chunks.iter().enumerate() {
            let literal = format!(
                "[{}]",
                vector
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            self.client
                .execute(
                    "INSERT INTO notes_rag.chunks(page_key, ordinal, rag_priority, content, token_count, embedding) \
                     VALUES ($1, $2, $3, $4, 1, $5::text::public.vector)",
                    &[&page_key, &(ordinal as i32), priority, content, &literal],
                )
                .await
                .unwrap();
        }
    }

    fn store(&self, notebooks: Option<&[&str]>) -> SearchStore {
        SearchStore {
            database: Database::from_dsn(&self.url).unwrap(),
            scope_signature: "scope".into(),
            generation_signature: "config".into(),
            allowed_notebooks: notebooks.map(|n| n.iter().map(|s| s.to_string()).collect()),
            access_revision: "public-revision".into(),
            visibility: None,
        }
    }

    async fn drop(self) {
        drop(self.client);
        self.admin
            .batch_execute(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .await
            .unwrap();
    }
}

/// Unit vector along `axis` mixed slightly with axis 0.
fn vector(axis: usize, weight: f32) -> Vec<f32> {
    let mut v = vec![0.0; DIMENSIONS];
    v[0] = weight;
    v[axis] = 1.0;
    v
}

fn query_vector() -> String {
    let mut v = vec!["0".to_string(); DIMENSIONS];
    v[0] = "1".into();
    format!("[{}]", v.join(","))
}

#[tokio::test]
async fn current_first_history_fill_and_neighbors_in_one_snapshot() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.page(
        'a',
        "public",
        "노트/a.md",
        "current",
        &[
            ("normal", "A0", vector(1, 0.5)),
            ("normal", "A1 match", vector(2, 3.0)),
            ("fallback", "A2 old", vector(3, 0.1)),
        ],
    )
    .await;
    db.page(
        'b',
        "public",
        "노트/b.md",
        "old",
        &[("fallback", "B0 old", vector(5, 9.0))],
    )
    .await;
    // cos = 0.2 / sqrt(1.04) < 0.35: below the default threshold.
    db.page(
        'c',
        "public",
        "다른/c.md",
        "current",
        &[("normal", "C0", vector(6, 0.2))],
    )
    .await;
    let store = db.store(Some(&["public"]));
    let (rows, info) = store
        .search(&query_vector(), 2, 0.35, true, "")
        .await
        .unwrap();
    let keys: Vec<(String, String)> = rows
        .iter()
        .map(|r| (r.page_key[..1].to_string(), r.rag_priority.clone()))
        .collect();
    // Current results first even though the history page is more similar.
    assert_eq!(
        keys,
        [
            ("a".into(), "normal".into()),
            ("b".into(), "fallback".into())
        ]
    );
    assert_eq!(
        (
            info["normal_results"].clone(),
            info["fallback_results"].clone()
        ),
        (json!(1), json!(1))
    );
    let a = &rows[0];
    assert_eq!(a.ordinal, 1);
    let context = a.context.as_ref().unwrap();
    assert_eq!(context.included_ordinals, [0, 1]);
    assert_eq!(a.content, "A0\n\n---\n\nA1 match");
    assert!(
        context.omitted_ordinals.is_empty(),
        "a history neighbor is not a candidate at all"
    );
    let (only_current, info) = store
        .search(&query_vector(), 3, 0.35, false, "")
        .await
        .unwrap();
    assert!(only_current.iter().all(|r| r.rag_priority == "normal"));
    assert_eq!(info["fallback_results"], 0);
    assert!(
        info["updated_at"]
            .as_str()
            .unwrap()
            .starts_with("2026-10-31T")
    );
    db.drop().await;
}

#[tokio::test]
async fn folder_filters_before_ranking_with_literal_boundaries() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.page(
        'a',
        "public",
        "노트/%_O'Reilly/a.md",
        "current",
        &[("normal", "inside", vector(1, 0.5))],
    )
    .await;
    db.page(
        'b',
        "public",
        "노트/%_O'Reilly2/b.md",
        "current",
        &[("normal", "sibling", vector(2, 5.0))],
    )
    .await;
    db.page(
        'c',
        "public",
        "노트/x_O'Reilly/c.md",
        "current",
        &[("normal", "pattern", vector(3, 5.0))],
    )
    .await;
    let store = db.store(Some(&["public"]));
    let (rows, info) = store
        .search(&query_vector(), 5, 0.0, true, "노트/%_O'Reilly")
        .await
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.content.as_str()).collect::<Vec<_>>(),
        ["inside"]
    );
    assert_eq!(
        (info["scoped_pages"].clone(), info["indexed_pages"].clone()),
        (json!(1), json!(3))
    );
    assert!(matches!(
        store.search(&query_vector(), 5, 0.0, true, "../x").await,
        Err(Fault::Value(_))
    ));
    db.drop().await;
}

#[tokio::test]
async fn notebook_acl_applies_to_counts_folders_ranking_and_chunks() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.page(
        'a',
        "public",
        "pub/a.md",
        "current",
        &[("normal", "public text", vector(1, 0.5))],
    )
    .await;
    db.page(
        'p',
        "private",
        "priv/p.md",
        "current",
        &[("normal", "private text", vector(2, 9.0))],
    )
    .await;
    let public = db.store(Some(&["public"]));
    let owner = db.store(Some(&["private", "public"]));
    assert_eq!(public.check("").await.unwrap().indexed_pages, 1);
    assert_eq!(owner.check("").await.unwrap().indexed_pages, 2);
    let folders = public.folders().await.unwrap();
    assert_eq!(
        folders["folders"],
        json!([{"path": "pub", "parent": "", "name": "pub", "pages": 1}])
    );
    assert_eq!(folders["access_revision"], "public-revision");
    let (rows, _) = public
        .search(&query_vector(), 5, 0.0, true, "")
        .await
        .unwrap();
    assert!(rows.iter().all(|r| r.content != "private text"));
    assert!(public.chunk(&"p".repeat(64), 0).await.unwrap().0.is_none());
    assert_eq!(
        owner
            .chunk(&"p".repeat(64), 0)
            .await
            .unwrap()
            .0
            .unwrap()
            .content,
        "private text"
    );
    assert!(public.check("priv").await.unwrap().scoped_pages == 0);
    db.drop().await;
}

#[tokio::test]
async fn absent_pages_hidden_and_signature_mismatch_fails_closed() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.page(
        'a',
        "public",
        "pub/a.md",
        "current",
        &[("normal", "present", vector(1, 0.5))],
    )
    .await;
    db.page(
        'm',
        "public",
        "pub/m.md",
        "current",
        &[("normal", "missing", vector(2, 9.0))],
    )
    .await;
    db.client
        .execute(
            "INSERT INTO notes_rag.missing_pages VALUES ($1, $2, now())",
            &[&"m".repeat(64), &"0".repeat(64)],
        )
        .await
        .unwrap();
    let store = db.store(Some(&["public"]));
    let (rows, info) = store
        .search(&query_vector(), 5, 0.0, true, "")
        .await
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.content.as_str()).collect::<Vec<_>>(),
        ["present"]
    );
    assert_eq!(info["indexed_pages"], 1);
    let mismatch =
        || Fault::search("Index scope or embedding generation mismatch; search disabled.");
    let mut wrong = db.store(Some(&["public"]));
    wrong.generation_signature = "0".repeat(64);
    assert_eq!(wrong.check("").await.err(), Some(mismatch()));
    let mut wrong = db.store(Some(&["public"]));
    wrong.scope_signature = "other".into();
    assert_eq!(wrong.check("").await.err(), Some(mismatch()));
    db.client
        .batch_execute("DELETE FROM notes_rag.generation")
        .await
        .unwrap();
    assert_eq!(store.check("").await.err(), Some(mismatch()));
    db.drop().await;
}

#[tokio::test]
async fn snapshot_is_read_only() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let store = db.store(None);
    let snapshot = store.snapshot("").await.unwrap();
    let denied = snapshot
        .client
        .batch_execute("UPDATE notes_rag.generation SET config_signature = 'x'")
        .await;
    assert!(
        denied.is_err(),
        "the snapshot transaction must be READ ONLY"
    );
    drop(snapshot);
    assert_eq!(store.check("").await.unwrap().indexed_pages, 0);
    db.drop().await;
}

#[tokio::test]
async fn semantic_graph_uses_same_acl_and_caches() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.page(
        'a',
        "public",
        "pub/a.md",
        "current",
        &[("normal", "a", vector(1, 1.0))],
    )
    .await;
    db.page(
        'b',
        "public",
        "pub/b.md",
        "current",
        &[("normal", "b", vector(1, 1.1))],
    )
    .await;
    db.page(
        'c',
        "public",
        "pub/c.md",
        "old",
        &[("fallback", "c", vector(2, 0.01))],
    )
    .await;
    db.page(
        'p',
        "private",
        "priv/p.md",
        "current",
        &[("normal", "p", vector(1, 1.0))],
    )
    .await;
    let store = db.store(Some(&["public"]));
    let graph = notes_rag_rs::graph::SemanticGraph::default();
    let first = graph.get(&store).await.unwrap();
    let paths: Vec<&str> = first["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["pub/a.md", "pub/b.md", "pub/c.md"]);
    assert_eq!(first["nodes"][2]["history"], true);
    assert_eq!(first["edges"][0]["source"], 0);
    assert_eq!(first["edges"][0]["target"], 1);
    assert!(
        first["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["similarity"].as_f64().unwrap() >= 0.45)
    );
    let second = graph.get(&store).await.unwrap();
    assert_eq!(first["generated_at"], second["generated_at"]);
    assert_eq!(first, second);
    db.drop().await;
}

struct FixedEmbedder;
impl Embedder for FixedEmbedder {
    fn embed_one(
        &self,
        _text: String,
    ) -> notes_rag_rs::search::service::BoxFuture<'_, notes_rag_rs::fault::Result<Vec<f64>>> {
        Box::pin(async {
            let mut v = vec![0.0; DIMENSIONS];
            v[0] = 1.0;
            Ok(v)
        })
    }
}

#[tokio::test]
async fn service_shapes_mcp_results_and_rejects_missing_folder_before_embedding() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.page(
        'e',
        "public",
        "실습/dnf conf.md",
        "current",
        &[("normal", "Synthetic evidence", vector(1, 1.0))],
    )
    .await;
    let manifest = notes_rag_rs::compat::ProducerManifest::embedded().unwrap();
    let service = SearchService::new(
        db.store(Some(&["public"])),
        Arc::new(FixedEmbedder),
        Arc::new(|text: &str| Ok(text.len() / 4)),
        manifest,
        "https://notes.example.invalid".into(),
        "OneNote/markdown".into(),
        None,
        false,
    )
    .unwrap();
    let request = |folder: &str| SearchRequest {
        query: "  테스트  ".into(),
        limit: 5,
        include_history: true,
        min_similarity: 0.35,
        folder: folder.into(),
    };
    let result = service.search(request("")).await.unwrap();
    let item = &result["results"][0];
    assert_eq!(
        item["source_url"],
        "https://notes.example.invalid/OneNote/markdown/%EC%8B%A4%EC%8A%B5/dnf%20conf"
    );
    assert_eq!(item["needs_visual_review"], true);
    assert_eq!(item["evidence_only"], true);
    assert_eq!(result["min_similarity"], 0.35);
    let keys: Vec<&String> = result.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["index", "results", "policy", "min_similarity"]);
    let missing = service.search(request("없음")).await;
    assert_eq!(
        missing.err(),
        Some(Fault::search(
            "Folder is not in the current index. Refresh the folder list."
        ))
    );
    let chunk = service.chunk(&"e".repeat(64), 0).await.unwrap();
    assert_eq!(chunk["result"]["content"], "Synthetic evidence");
    assert_eq!(
        service.chunk("arbitrary/path", 0).await.err(),
        Some(Fault::search("Invalid chunk reference."))
    );
    let value: Value = service.folders().await.unwrap();
    assert_eq!(value["indexed_pages"], 1);
    db.drop().await;
}
