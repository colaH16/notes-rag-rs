//! Shared fixtures for the integration tests (synthetic data only).
#![allow(dead_code)]

use axum::body::Body;
use bytes::Bytes;
use http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use notes_rag_rs::fault::{Fault, Result};
use notes_rag_rs::reader::{Reader, ReaderConfig};
use notes_rag_rs::web::gateway::{BoxFuture, Upstream};
use notes_rag_rs::web::state::{Clock, State};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

pub const PREFIX: &str = "https://notes.cola16.app/OneNote/markdown/";
pub const NOTE_PREFIX: &str = "/OneNote/markdown/";

pub type Hook = Box<dyn Fn() + Send + Sync>;

/// `AsyncMock` gateway: canned responses, call records and an optional hook
/// that runs *during* the upstream call (e.g. to revoke a session).
#[derive(Default)]
pub struct MockUpstream {
    pub search_result: Mutex<Option<Value>>,
    pub folders_result: Mutex<Option<Value>>,
    pub graph_result: Mutex<Option<Value>>,
    pub search_calls: Mutex<Vec<(String, bool, String)>>,
    pub folder_calls: AtomicUsize,
    pub graph_calls: AtomicUsize,
    pub during_search: Mutex<Option<Hook>>,
    pub during_graph: Mutex<Option<Hook>>,
}

fn take(slot: &Mutex<Option<Value>>) -> Result<Value> {
    slot.lock()
        .unwrap()
        .clone()
        .ok_or(Fault::Other("synthetic PRIVATE failure"))
}

impl Upstream for MockUpstream {
    fn search(&self, query: String, history: bool, folder: String) -> BoxFuture<'_, Result<Value>> {
        self.search_calls
            .lock()
            .unwrap()
            .push((query, history, folder));
        if let Some(hook) = self.during_search.lock().unwrap().as_ref() {
            hook();
        }
        let result = take(&self.search_result);
        Box::pin(async move { result })
    }

    fn folders(&self) -> BoxFuture<'_, Result<Value>> {
        self.folder_calls.fetch_add(1, Ordering::SeqCst);
        let result = take(&self.folders_result);
        Box::pin(async move { result })
    }

    fn graph(&self) -> BoxFuture<'_, Result<Value>> {
        self.graph_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = self.during_graph.lock().unwrap().as_ref() {
            hook();
        }
        let result = take(&self.graph_result);
        Box::pin(async move { result })
    }
}

impl MockUpstream {
    pub fn with_search(value: Value) -> Arc<Self> {
        let mock = Self::default();
        *mock.search_result.lock().unwrap() = Some(value);
        Arc::new(mock)
    }

    pub fn searches(&self) -> usize {
        self.search_calls.lock().unwrap().len()
    }
}

/// The `RESULT` fixture of `tests/test_public_search.py`.
pub fn result_fixture() -> Value {
    json!({"index": {"indexed_pages": 2178}, "results": [{"title": "<script>test</script>",
        "content": "synthetic excerpt", "source_url": format!("{PREFIX}example"), "rag_priority": "normal"}]})
}

pub struct TestClock(pub Arc<AtomicI64>);

impl TestClock {
    pub fn new(start: i64) -> (Self, Clock) {
        let now = Arc::new(AtomicI64::new(start));
        let reader = now.clone();
        (
            Self(now),
            Arc::new(move || reader.load(Ordering::SeqCst) as f64),
        )
    }

    pub fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}

pub fn state(dir: &Path, daily: i64, hourly: i64, clock: Clock) -> Arc<State> {
    Arc::new(State::new(dir.join("state.db"), daily, hourly, clock).unwrap())
}

pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Response {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn cookies(&self) -> Vec<String> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect()
    }
}

pub async fn send(
    router: &axum::Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> Response {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    Response {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes(),
    }
}

pub async fn get(router: &axum::Router, uri: &str, headers: &[(&str, &str)]) -> Response {
    send(router, Method::GET, uri, headers, Body::empty()).await
}

pub async fn post_json(
    router: &axum::Router,
    uri: &str,
    value: &Value,
    headers: &[(&str, &str)],
) -> Response {
    let mut all = vec![("content-type", "application/json")];
    all.extend_from_slice(headers);
    send(
        router,
        Method::POST,
        uri,
        &all,
        Body::from(value.to_string()),
    )
    .await
}

/// `ReaderTests.setUp`: three notebooks (public, private, unknown), each
/// with a note, a referenced image, a referenced SVG and an unreferenced file.
pub struct Corpus {
    pub dir: tempfile::TempDir,
    pub corpus: PathBuf,
    pub selection: PathBuf,
    pub policy: PathBuf,
    pub revision: String,
}

impl Corpus {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let corpus = dir.path().join("corpus");
        std::fs::create_dir(&corpus).unwrap();
        let selection = dir.path().join("markdown.json");
        std::fs::write(
            &selection,
            json!({"schemaVersion": 1, "sourceArchiveRoot": "archive", "silverBulletRoot": "notes",
                   "notebooks": [{"id": "public"}, {"id": "private"}]})
            .to_string(),
        )
        .unwrap();
        let policy = dir.path().join("access.json");
        std::fs::write(
            &policy,
            json!({"schemaVersion": 1, "publicNotebookIds": ["public"]}).to_string(),
        )
        .unwrap();
        let revision = notes_rag_rs::policy::revision_of(&["public".to_string()]);
        let corpus_fixture = Self {
            dir,
            corpus,
            selection,
            policy,
            revision,
        };
        for book in ["public", "private", "unknown"] {
            corpus_fixture.note(
                &format!("{book}/note.md"),
                book,
                "# 제목\n\n```fish\n# comment\necho hello\n```\n\n![test](note.assets/image.png)\n[download](note.assets/file.svg)\n",
                &[],
            );
            let assets = corpus_fixture.corpus.join(book).join("note.assets");
            std::fs::create_dir(&assets).unwrap();
            std::fs::write(assets.join("image.png"), b"synthetic-image").unwrap();
            std::fs::write(assets.join("file.svg"), "<svg onload=\"evil()\"/>").unwrap();
            std::fs::write(assets.join("secret.png"), b"not-referenced").unwrap();
        }
        corpus_fixture
    }

    /// `ReaderTests.note(path, book, body, **extra)` (`json.dumps` values).
    pub fn note(&self, path: &str, book: &str, body: &str, extra: &[(&str, Value)]) {
        let target = self.corpus.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let mut meta = vec![
            ("source_system", json!("onenote")),
            ("content_type", json!("note")),
            ("onenote_notebook_id", json!(book)),
            ("title", json!("test <script>title</script>")),
        ];
        meta.extend(extra.iter().cloned());
        let mut text = String::from("---\n");
        for (key, value) in meta {
            text.push_str(&format!(
                "{key}: {}\n",
                notes_rag_rs::py::json::dumps_ascii(&value)
            ));
        }
        text.push_str("---\n");
        text.push_str(body);
        std::fs::write(target, text).unwrap();
    }

    pub fn reader(&self) -> Arc<Reader> {
        Arc::new(
            Reader::new(ReaderConfig {
                root: self.corpus.clone(),
                selection: self.selection.clone(),
                policy: self.policy.clone(),
                revision: self.revision.clone(),
                note_prefix: NOTE_PREFIX.into(),
                silverbullet_origin: "https://notes.cola16.app".into(),
                vision_state: None,
                native_notes: false,
            })
            .unwrap(),
        )
    }
}
