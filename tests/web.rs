//! Ports of the original `tests/test_public_search.py`, the web part of
//! `tests/test_semantic_graph.py` and `tests/test_document_reader.py`.

mod common;

use axum::body::Body;
use common::*;
use http::{Method, StatusCode};
use notes_rag_rs::fault::Fault;
use notes_rag_rs::scope::folder_catalog;
use notes_rag_rs::web::app::WebApp;
use notes_rag_rs::web::gateway::Upstream;
use notes_rag_rs::web::state::State;
use notes_rag_rs::web::{auth::OwnerAuth, router};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;

struct Fixture {
    _dir: tempfile::TempDir,
    clock: TestClock,
    state: Arc<State>,
    gateway: Arc<MockUpstream>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (clock, source) = TestClock::new(2_000_000_000);
        let state = state(dir.path(), 3, 2, source);
        Self {
            _dir: dir,
            clock,
            state,
            gateway: MockUpstream::with_search(result_fixture()),
        }
    }

    fn app(
        &self,
        revision: Option<&str>,
        reader: Option<Arc<notes_rag_rs::reader::Reader>>,
        version: &str,
    ) -> axum::Router {
        let mut app = WebApp::new(
            self.state.clone(),
            self.gateway.clone() as Arc<dyn Upstream>,
            revision.map(str::to_string),
            None,
            None,
            reader,
            PREFIX.into(),
            NOTE_PREFIX.into(),
        )
        .unwrap();
        app.cache_version = version.into();
        router(Arc::new(app))
    }

    fn default_app(&self) -> axum::Router {
        self.app(None, None, notes_rag_rs::web::app::SEARCH_CACHE_VERSION)
    }
}

async fn search(router: &axum::Router, body: Value) -> Response {
    post_json(router, "/api/search", &body, &[]).await
}

#[tokio::test]
async fn no_auth_search_cache_and_fixed_fields() {
    let f = Fixture::new();
    let app = f.default_app();
    let first = search(&app, json!({"query": "test"})).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.json()["cached"], false);
    assert_eq!(
        search(&app, json!({"query": " test  "})).await.json()["cached"],
        true
    );
    assert_eq!(
        *f.gateway.search_calls.lock().unwrap(),
        [("test".to_string(), true, String::new())]
    );
    assert!(first.json()["results"][0].get("page_key").is_none());
    assert!(first.header("cache-control").unwrap().contains("no-store"));
    assert!(
        first
            .header("content-security-policy")
            .unwrap()
            .contains("script-src 'self'")
    );
    assert_eq!(first.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(first.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(first.header("x-robots-tag"), Some("noindex, nofollow"));
    // Key order and shape of the public response.
    let keys: Vec<String> = first.json().as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        [
            "results",
            "indexed_pages",
            "scoped_pages",
            "folder",
            "audience",
            "cached"
        ]
    );
}

#[tokio::test]
async fn cached_excerpt_hidden_after_move_without_reembedding() {
    let f = Fixture::new();
    let corpus = Corpus::new();
    corpus.note("example.md", "public", "synthetic", &[]);
    let app = f.app(
        None,
        Some(corpus.reader()),
        notes_rag_rs::web::app::SEARCH_CACHE_VERSION,
    );
    assert_eq!(
        search(&app, json!({"query": "test"})).await.json()["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    std::fs::rename(
        corpus.corpus.join("example.md"),
        corpus.corpus.join("moved.md"),
    )
    .unwrap();
    let second = search(&app, json!({"query": "test"})).await;
    assert_eq!(second.json()["cached"], true);
    assert_eq!(second.json()["results"], json!([]));
    assert!(!second.text().contains("synthetic excerpt"));
    assert_eq!(f.gateway.searches(), 1);
}

#[tokio::test]
async fn cached_graph_hides_moved_nodes_and_remaps_edges() {
    let f = Fixture::new();
    let corpus = Corpus::new();
    for name in ["b.md", "c.md"] {
        corpus.note(name, "public", "x", &[]);
    }
    let graph = json!({"nodes": [{"id": 0, "path": "a.md", "title": "a.md"}, {"id": 1, "path": "b.md", "title": "b.md"},
                                 {"id": 2, "path": "c.md", "title": "c.md"}],
                       "edges": [{"source": 0, "target": 1}, {"source": 1, "target": 2}], "index": {"indexed_pages": 3}});
    *f.gateway.graph_result.lock().unwrap() = Some(graph);
    let app = f.app(
        None,
        Some(corpus.reader()),
        notes_rag_rs::web::app::SEARCH_CACHE_VERSION,
    );
    let result = get(&app, "/api/graph", &[]).await.json();
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
    assert_eq!(result["audience"], "public");
}

#[tokio::test]
async fn budget_persists_and_cached_results_remain_available() {
    let f = Fixture::new();
    let app = f.default_app();
    search(&app, json!({"query": "one"})).await;
    search(&app, json!({"query": "two"})).await;
    assert_eq!(
        search(&app, json!({"query": "three"})).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    let restarted =
        State::new(f._dir.path().join("state.db"), 3, 2, f.state.clock.clone()).unwrap();
    assert!(matches!(restarted.reserve(), Err(Fault::Denied { .. })));
    assert_eq!(
        search(&app, json!({"query": "one"})).await.json()["cached"],
        true
    );
    f.clock.advance(3601);
    search(&app, json!({"query": "three"})).await;
    f.clock.advance(3601);
    let denied = search(&app, json!({"query": "four"})).await;
    assert_eq!(denied.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(denied.header("retry-after").is_some());
    assert_eq!(f.gateway.searches(), 3);
}

#[tokio::test]
async fn failed_search_consumes_budget_not_cached_or_retried() {
    let f = Fixture::new();
    *f.gateway.search_result.lock().unwrap() = None;
    let app = f.default_app();
    let response = search(&app, json!({"query": "test"})).await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!response.text().contains("PRIVATE"));
    search(&app, json!({"query": "test"})).await;
    assert_eq!(
        search(&app, json!({"query": "test"})).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(f.gateway.searches(), 2);
}

#[tokio::test]
async fn invalid_inputs_never_reach_gateway() {
    let f = Fixture::new();
    let app = f.default_app();
    for body in [
        json!({"query": ""}),
        json!({"query": "x".repeat(301)}),
        json!({"query": "x", "limit": 999}),
        json!({"query": "x", "include_history": "yes"}),
        json!([]),
    ] {
        assert_eq!(
            search(&app, body.clone()).await.status,
            StatusCode::BAD_REQUEST,
            "{body}"
        );
    }
    let large = send(
        &app,
        Method::POST,
        "/api/search",
        &[("content-type", "application/json")],
        Body::from("x".repeat(9000)),
    )
    .await;
    assert_eq!(large.status, StatusCode::PAYLOAD_TOO_LARGE);
    let cross = post_json(
        &app,
        "/api/search",
        &json!({"query": "x"}),
        &[("sec-fetch-site", "cross-site")],
    )
    .await;
    assert_eq!(cross.status, StatusCode::FORBIDDEN);
    let form = send(
        &app,
        Method::POST,
        "/api/search",
        &[("content-type", "text/plain")],
        Body::from("{}"),
    )
    .await;
    assert_eq!(form.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(f.gateway.searches(), 0);
}

#[tokio::test]
async fn cache_expires_and_does_not_cache_failures() {
    let f = Fixture::new();
    let app = f.default_app();
    search(&app, json!({"query": "test"})).await;
    f.clock.advance(601);
    assert_eq!(
        search(&app, json!({"query": "test"})).await.json()["cached"],
        true
    );
    f.clock.advance(10800 - 601 - 1);
    assert_eq!(
        search(&app, json!({"query": "test"})).await.json()["cached"],
        true
    );
    assert_eq!(f.gateway.searches(), 1);
    f.clock.advance(1);
    assert_eq!(
        search(&app, json!({"query": "test"})).await.json()["cached"],
        false
    );
    assert_eq!(f.gateway.searches(), 2);
}

#[tokio::test]
async fn context_revision_invalidates_cache_without_resetting_budget() {
    let f = Fixture::new();
    let current = f.default_app();
    assert_eq!(
        search(&current, json!({"query": "test"})).await.json()["cached"],
        false
    );
    let future = f.app(None, None, "future-context-version");
    assert_eq!(
        search(&future, json!({"query": "test"})).await.json()["cached"],
        false
    );
    assert_eq!(
        search(&future, json!({"query": "test"})).await.json()["cached"],
        true
    );
    assert_eq!(
        search(&future, json!({"query": "another"})).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(f.gateway.searches(), 2);
}

#[tokio::test]
async fn static_pages_and_no_public_mcp_proxy() {
    let f = Fixture::new();
    let app = f.default_app();
    for path in [
        "/",
        "/app.js",
        "/style.css",
        "/healthz",
        "/robots.txt",
        "/graph",
        "/graph.js",
        "/graph.css",
        "/graph-layout.js",
        "/reader.css",
        "/reader.js",
        "/cola-theme.css",
        "/cola-theme-init.js",
        "/cola-theme.js",
        "/cola-header.css",
    ] {
        let response = get(&app, path, &[]).await;
        assert_eq!(response.status, StatusCode::OK, "{path}");
        assert!(
            response
                .header("content-security-policy")
                .unwrap()
                .contains("worker-src 'self'")
        );
    }
    assert_eq!(
        get(&app, "/", &[]).await.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        get(&app, "/app.js", &[]).await.header("content-type"),
        Some("application/javascript")
    );
    assert_eq!(
        get(&app, "/style.css", &[]).await.header("content-type"),
        Some("text/css; charset=utf-8")
    );
    assert_eq!(
        post_json(&app, "/mcp", &json!({}), &[]).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&app, "/api/search", &[]).await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        send(&app, Method::POST, "/read", &[], Body::empty())
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert!(!get(&app, "/app.js", &[]).await.text().contains("innerHTML"));
    assert!(
        !get(&app, "/graph.js", &[])
            .await
            .text()
            .contains("innerHTML")
    );
    assert_eq!(
        get(&app, "/robots.txt", &[]).await.text(),
        "User-agent: *\nDisallow: /\n"
    );
    assert_eq!(
        get(&app, "/healthz", &[]).await.json(),
        json!({"status": "ok"})
    );
    let head = send(&app, Method::HEAD, "/", &[], Body::empty()).await;
    assert_eq!((head.status, head.body.len()), (StatusCode::OK, 0));
    assert_eq!(
        get(&app, "/api/session", &[]).await.json(),
        json!({"login_available": false, "owner": false})
    );
}

#[tokio::test]
async fn traffic_limit_cannot_be_bypassed_by_forwarded_ip() {
    let f = Fixture::new();
    let app = f.default_app();
    for _ in 0..60 {
        f.state.traffic("public").unwrap();
    }
    let response = post_json(
        &app,
        "/api/search",
        &json!({"query": "x"}),
        &[("x-forwarded-for", "1.2.3.4")],
    )
    .await;
    assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(f.gateway.searches(), 0);
}

#[tokio::test]
async fn folder_browser_has_native_buttons_and_no_dropdown() {
    let f = Fixture::new();
    let app = f.default_app();
    let html = get(&app, "/", &[]).await.text();
    for marker in [
        "id=\"folder-columns\"",
        "id=\"folder-up\"",
        "id=\"folder-all\"",
    ] {
        assert!(html.contains(marker));
    }
    let scope = html
        .split("<section class=\"folder-scope\"")
        .nth(1)
        .unwrap()
        .split("</section>")
        .next()
        .unwrap();
    assert!(!scope.contains("<select"));
    let css = get(&app, "/style.css", &[]).await.text();
    assert!(
        css.contains("repeat(3,minmax(0,1fr))")
            && css.contains(".folder-column-current{display:block}")
    );
}

/// The shared `<cola-header>` contract: one header before `<main>`, the provider
/// script loaded async with only a local fallback stylesheet, and the page's
/// account controls (if any) in `slot="account"`. Returns the `<main>` markup.
fn shared_header(html: &str, current: &str) -> String {
    assert_eq!(html.matches("<cola-header ").count(), 1);
    assert!(html.contains(&format!("<cola-header current=\"{current}\" ")));
    assert!(html.contains(
        "<script src=\"https://apps.cola16.app/branding/v1/cola-header.js\" async></script>"
    ));
    assert!(html.contains("<link rel=\"stylesheet\" href=\"/cola-header.css\">"));
    for link in html.split("<link ").skip(1) {
        assert!(link.contains("href=\"/"), "cross-origin stylesheet: {link}");
    }
    for old in [
        "cola-spaces",
        "data-cola-theme-select",
        "cola-skip",
        "지식 지도",
        "검색으로 돌아가기",
    ] {
        assert!(!html.contains(old), "{old}");
    }
    let (header, main) = html.split_once("</cola-header>").unwrap();
    assert_eq!(header.matches(" aria-current=\"page\"").count(), 1);
    let main = main.trim_start();
    assert!(main.starts_with("<main id=\"main\">"));
    main.to_string()
}

#[tokio::test]
async fn search_and_graph_share_the_cola_header() {
    let f = Fixture::new();
    let app = f.default_app();
    for (path, current, badge, login) in [
        ("/", "notes", "id=\"access-badge\"", "href=\"/auth/login\""),
        (
            "/graph",
            "graph",
            "id=\"audience\"",
            "href=\"/auth/login?next=/graph\"",
        ),
    ] {
        let html = get(&app, path, &[]).await.text();
        let main = shared_header(&html, current);
        // Library access is page content. Login/logout stay host-owned, start hidden until the
        // session check, and are direct account-slot children so the header styles them as
        // its shared account buttons (no wrapper, no page-specific button styles).
        assert!(main.contains(badge) && !html.split("<main").next().unwrap().contains(badge));
        let account = html
            .split("</header>")
            .nth(1)
            .unwrap()
            .split("</cola-header>")
            .next()
            .unwrap();
        assert_eq!(
            account,
            format!(
                "<a id=\"login\" slot=\"account\" {login} hidden>로그인</a><button id=\"logout\" slot=\"account\" type=\"button\" hidden>로그아웃</button>"
            )
        );
        assert!(!html.contains("cola-account"));
    }
    let init = get(&app, "/cola-theme-init.js", &[]).await.text();
    assert!(init.contains("'#191719':'#F4F0F1'"));
    let theme = get(&app, "/cola-theme.js", &[]).await.text();
    assert!(theme.contains("cola-theme-change") && !theme.contains("select"));
}

#[tokio::test]
async fn policy_mismatch_never_serves_catalog_or_results() {
    let f = Fixture::new();
    *f.gateway.folders_result.lock().unwrap() =
        Some(folder_catalog(&["private/a.md".to_string()]).unwrap());
    let revision = "a".repeat(64);
    let app = f.app(
        Some(&revision),
        None,
        notes_rag_rs::web::app::SEARCH_CACHE_VERSION,
    );
    assert_eq!(
        get(&app, "/api/folders", &[]).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let result = search(&app, json!({"query": "test"})).await;
    assert_eq!(result.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!result.text().contains("synthetic excerpt"));
    f.gateway.folders_result.lock().unwrap().as_mut().unwrap()["access_revision"] = json!(revision);
    assert_eq!(get(&app, "/api/folders", &[]).await.status, StatusCode::OK);
}

#[tokio::test]
async fn folder_catalog_cached_without_embedding_budget() {
    let f = Fixture::new();
    *f.gateway.folders_result.lock().unwrap() =
        Some(folder_catalog(&["example/child/a.md".to_string()]).unwrap());
    let app = f.default_app();
    for _ in 0..3 {
        let result = get(&app, "/api/folders", &[]).await;
        assert_eq!(result.status, StatusCode::OK);
        assert_eq!(result.json()["folders"].as_array().unwrap().len(), 2);
        assert_eq!(result.json()["audience"], "public");
    }
    assert_eq!(f.gateway.folder_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.gateway.searches(), 0);
    assert_eq!(
        search(&app, json!({"query": "one"})).await.status,
        StatusCode::OK
    );
    assert_eq!(
        search(&app, json!({"query": "two"})).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn folder_cache_isolation_and_descendant_results() {
    let f = Fixture::new();
    *f.gateway.folders_result.lock().unwrap() =
        Some(folder_catalog(&["example/child/a.md".to_string()]).unwrap());
    *f.gateway.search_result.lock().unwrap() = Some(
        json!({"index": {"indexed_pages": 10, "scoped_pages": 1},
        "results": [{"title": "t", "content": "c", "source_url": format!("{PREFIX}example/child/a"), "rag_priority": "normal"}]}),
    );
    let app = f.default_app();
    assert_eq!(
        search(&app, json!({"query": "test"})).await.json()["cached"],
        false
    );
    let response = search(&app, json!({"query": "test", "folder": "example"})).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["cached"], false);
    assert_eq!(response.json()["scoped_pages"], 1);
    assert_eq!(
        search(&app, json!({"query": "test", "folder": "example"}))
            .await
            .json()["cached"],
        true
    );
    assert_eq!(
        f.gateway.search_calls.lock().unwrap().last().unwrap(),
        &("test".to_string(), true, "example".to_string())
    );
}

#[tokio::test]
async fn missing_and_malformed_folders_do_not_use_search_budget() {
    let f = Fixture::new();
    *f.gateway.folders_result.lock().unwrap() =
        Some(folder_catalog(&["example/a.md".to_string()]).unwrap());
    let app = f.default_app();
    for folder in [
        json!("absent"),
        json!("/example"),
        json!("example/"),
        json!("example/../other"),
        Value::Null,
    ] {
        assert_eq!(
            search(&app, json!({"query": "test", "folder": folder}))
                .await
                .status,
            StatusCode::BAD_REQUEST,
            "{folder}"
        );
    }
    assert_eq!(f.gateway.searches(), 0);
    assert_eq!(
        search(&app, json!({"query": "test"})).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn folder_failure_redacted_and_unscoped_search_still_works() {
    let f = Fixture::new();
    let app = f.default_app();
    let response = get(&app, "/api/folders", &[]).await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!response.text().contains("PRIVATE"));
    assert_eq!(
        search(&app, json!({"query": "test"})).await.status,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_upstream_at_a_time() {
    // `lock.locked()` → 429 with Retry-After: 3 while another search is in flight.
    let f = Fixture::new();
    let app = Arc::new(f.default_app());
    let gate = Arc::new(tokio::sync::Notify::new());
    let (entered, release) = (gate.clone(), Arc::new(std::sync::Barrier::new(2)));
    let barrier = release.clone();
    *f.gateway.during_search.lock().unwrap() = Some(Box::new(move || {
        entered.notify_one();
        barrier.wait();
    }));
    let background = {
        let app = app.clone();
        tokio::spawn(async move { search(&app, json!({"query": "slow"})).await })
    };
    gate.notified().await;
    let busy = search(&app, json!({"query": "other"})).await;
    release.wait();
    assert_eq!(busy.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(busy.header("retry-after"), Some("3"));
    assert_eq!(background.await.unwrap().status, StatusCode::OK);
}

// ----- web graph (tests/test_semantic_graph.py: WebGraphTests) -------------

struct GraphFixture {
    _dir: tempfile::TempDir,
    state: Arc<State>,
    auth: Arc<OwnerAuth>,
    public: Arc<MockUpstream>,
    private: Arc<MockUpstream>,
    router: axum::Router,
}

fn graph_fixture() -> GraphFixture {
    let dir = tempfile::tempdir().unwrap();
    let (_clock, source) = TestClock::new(2_000_000_000);
    let state = state(dir.path(), 300, 60, source);
    let revision = "a".repeat(64);
    let owner_revision = notes_rag_rs::policy::owner_revision(&revision);
    let public = Arc::new(MockUpstream::default());
    let private = Arc::new(MockUpstream::default());
    *public.graph_result.lock().unwrap() =
        Some(json!({"nodes": ["public"], "index": {"access_revision": revision}}));
    *private.graph_result.lock().unwrap() =
        Some(json!({"nodes": ["private"], "index": {"access_revision": owner_revision}}));
    let auth = Arc::new(
        OwnerAuth::new(
            state.clone(),
            "synthetic-client",
            &"c".repeat(48),
            &"s".repeat(48),
            "owner-uuid",
            "https://rag.cola16.app",
            "http://127.0.0.1:9/application/o/notes-rag/",
        )
        .unwrap(),
    );
    let app = WebApp::new(
        state.clone(),
        public.clone(),
        Some(revision),
        Some(auth.clone()),
        Some(private.clone()),
        None,
        PREFIX.into(),
        NOTE_PREFIX.into(),
    )
    .unwrap();
    GraphFixture {
        _dir: dir,
        state,
        auth,
        public,
        private,
        router: router(Arc::new(app)),
    }
}

#[tokio::test]
async fn graph_public_owner_public_isolation_and_no_embedding_budget() {
    let g = graph_fixture();
    let token = g.auth.issue().unwrap();
    let owner_cookie = format!("__Host-notes-owner={token}");
    for (cookie, expected) in [
        (None, "public"),
        (Some(owner_cookie.as_str()), "private"),
        (None, "public"),
    ] {
        let headers: Vec<(&str, &str)> = cookie.map(|c| vec![("cookie", c)]).unwrap_or_default();
        let response = get(&g.router, "/api/graph", &headers).await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.json()["nodes"], json!([expected]));
        assert_eq!(response.header("cache-control"), Some("no-store"));
    }
    assert_eq!(g.public.searches() + g.private.searches(), 0);
    let db = g.state.connect().unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM attempts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn graph_policy_mismatch_and_revocation_during_build() {
    let g = graph_fixture();
    g.public.graph_result.lock().unwrap().as_mut().unwrap()["index"]["access_revision"] =
        json!("wrong");
    assert_eq!(
        get(&g.router, "/api/graph", &[]).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let token = g.auth.issue().unwrap();
    let state = g.state.clone();
    *g.private.during_graph.lock().unwrap() = Some(Box::new(move || {
        state
            .connect()
            .unwrap()
            .execute("DELETE FROM owner_sessions", [])
            .unwrap();
    }));
    let cookie = format!("__Host-notes-owner={token}");
    assert_eq!(
        get(&g.router, "/api/graph", &[("cookie", &cookie)])
            .await
            .status,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn graph_cross_site_denied_and_fixed_errors() {
    let g = graph_fixture();
    assert_eq!(
        get(&g.router, "/api/graph", &[("sec-fetch-site", "cross-site")])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(g.public.graph_calls.load(Ordering::SeqCst), 0);
    *g.public.graph_result.lock().unwrap() = None;
    let response = get(&g.router, "/api/graph", &[]).await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!response.text().contains("PRIVATE"));
}

// ----- reader (tests/test_document_reader.py) -------------------------------

fn reader_app(corpus: &Corpus, dir: &tempfile::TempDir) -> (Arc<State>, axum::Router) {
    let (_clock, source) = TestClock::new(2_000_000_000);
    let state = state(dir.path(), 100, 100, source);
    let app = WebApp::new(
        state.clone(),
        Arc::new(MockUpstream::default()),
        Some(corpus.revision.clone()),
        None,
        None,
        Some(corpus.reader()),
        PREFIX.into(),
        NOTE_PREFIX.into(),
    )
    .unwrap();
    (state, router(Arc::new(app)))
}

#[test]
fn reader_public_owner_and_unknown() {
    let corpus = Corpus::new();
    let reader = corpus.reader();
    assert!(
        reader
            .document("public/note.md", false)
            .unwrap()
            .contains("SilverBullet에서 편집")
    );
    assert!(
        reader
            .document("private/note.md", true)
            .unwrap()
            .contains("OWNER")
    );
    for (path, owner) in [
        ("private/note.md", false),
        ("unknown/note.md", false),
        ("unknown/note.md", true),
    ] {
        assert_eq!(reader.document(path, owner).err(), Some(Fault::NotReadable));
    }
    corpus.note(
        "public/generated.md",
        "public",
        "private titles",
        &[("generated", json!(true))],
    );
    assert_eq!(
        reader.document("public/generated.md", true).err(),
        Some(Fault::NotReadable)
    );
}

#[tokio::test]
async fn reader_routes_headers_and_no_writes() {
    let corpus = Corpus::new();
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = reader_app(&corpus, &dir);
    let result = get(&app, "/read?path=public%2Fnote.md", &[]).await;
    assert_eq!(result.status, StatusCode::OK);
    assert_eq!(result.header("cache-control"), Some("no-store"));
    assert_eq!(result.header("x-content-type-options"), Some("nosniff"));
    assert!(
        result
            .header("content-security-policy")
            .unwrap()
            .contains("script-src 'self'")
    );
    assert!(
        result
            .text()
            .contains("<pre><code class=\"language-fish\"># comment")
    );
    assert!(!result.text().contains("<script>title"));
    // The reader has no account controls; its back/audience/edit links sit by the article.
    let main = shared_header(&result.text(), "notes");
    assert!(!result.text().contains("slot=\"account\""));
    assert!(main.starts_with(
        "<main id=\"main\"><div class=\"reader-tools\"><a href=\"/\">← 노트 검색</a>\n<span>PUBLIC · 읽기 전용</span>"
    ));
    corpus.note(
        "public/note.md",
        "public",
        "# Updated original\n\nLive file contents.",
        &[],
    );
    let refreshed = get(&app, "/read?path=public/note.md", &[]).await;
    assert_eq!(refreshed.status, StatusCode::OK);
    assert!(
        refreshed.text().contains("Live file contents.")
            && !refreshed.text().contains("echo hello")
    );
    for path in ["private%2Fnote.md", "missing.md"] {
        let response = get(
            &app,
            &format!("/read?path={path}&owner=true"),
            &[("x-user", "ldap.cola16")],
        )
        .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert!(
            response
                .text()
                .contains("문서가 없거나 읽을 권한이 없습니다.")
        );
    }
}

#[tokio::test]
async fn reader_asset_throttle_happens_before_reading_source() {
    let corpus = Corpus::new();
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = reader_app(&corpus, &dir);
    for _ in 0..120 {
        state.traffic("public-assets").unwrap();
    }
    std::fs::remove_file(corpus.corpus.join("public/note.md")).unwrap();
    let response = get(
        &app,
        "/read-asset?path=public%2Fnote.md&asset=public%2Fnote.assets%2Fimage.png",
        &[],
    )
    .await;
    assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(response.header("retry-after").is_some());
}

#[tokio::test]
async fn reader_assets_direct_acl_and_attachment_disposition() {
    let corpus = Corpus::new();
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = reader_app(&corpus, &dir);
    let asset = |book: &str, name: &str| {
        format!("/read-asset?path={book}%2Fnote.md&asset={book}%2Fnote.assets%2F{name}")
    };
    let image = get(&app, &asset("public", "image.png"), &[]).await;
    assert_eq!(image.body.as_ref(), b"synthetic-image");
    assert!(
        image
            .header("content-disposition")
            .unwrap()
            .starts_with("inline")
    );
    assert_eq!(image.header("content-type"), Some("image/png"));
    assert_eq!(image.header("cache-control"), Some("no-store"));
    assert_eq!(
        get(&app, &asset("private", "image.png"), &[]).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&app, &asset("public", "secret.png"), &[]).await.status,
        StatusCode::NOT_FOUND
    );
    let svg = get(&app, &asset("public", "file.svg"), &[]).await;
    assert!(
        svg.header("content-disposition")
            .unwrap()
            .starts_with("attachment")
    );
    assert_eq!(svg.header("content-type"), Some("application/octet-stream"));
    let range = get(
        &app,
        &asset("public", "image.png"),
        &[("range", "bytes=0-8")],
    )
    .await;
    assert_eq!(
        (range.status, range.body.as_ref()),
        (StatusCode::PARTIAL_CONTENT, b"synthetic".as_slice())
    );
    assert!(
        corpus
            .reader()
            .asset("private/note.md", "private/note.assets/image.png", true)
            .is_ok()
    );
}

#[test]
fn reader_traversal_symlink_fifo_and_size() {
    let corpus = Corpus::new();
    let reader = corpus.reader();
    for path in [
        "../public/note.md",
        "/public/note.md",
        "public/../private/note.md",
        "public//note.md",
        "public\\note.md",
        ".secret.md",
        "public/note.md\0",
    ] {
        assert_eq!(
            reader.read_note(path, true).err(),
            Some(Fault::NotReadable),
            "{path:?}"
        );
    }
    let root = &corpus.corpus;
    std::os::unix::fs::symlink(root.join("private/note.md"), root.join("public/link.md")).unwrap();
    std::os::unix::fs::symlink(root.join("public"), root.join("alias")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        root.join("public/fifo.md"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    std::fs::File::create(root.join("public/large.md"))
        .unwrap()
        .set_len(notes_rag_rs::reader::MAX_NOTE + 1)
        .unwrap();
    for path in [
        "public/link.md",
        "alias/note.md",
        "public/fifo.md",
        "public/large.md",
    ] {
        assert_eq!(
            reader.read_note(path, true).err(),
            Some(Fault::NotReadable),
            "{path}"
        );
    }
    let asset = root.join("public/note.assets/image.png");
    std::fs::remove_file(&asset).unwrap();
    std::os::unix::fs::symlink(root.join("private/note.assets/image.png"), &asset).unwrap();
    assert!(matches!(
        reader.asset("public/note.md", "public/note.assets/image.png", true),
        Err(Fault::NotReadable)
    ));
}

#[test]
fn reader_sanitizer_tables_links_and_no_cross_notebook_assets() {
    let corpus = Corpus::new();
    let reader = corpus.reader();
    let (safe, assets) = reader.render_body(
        "public/note.md",
        "# 제목\n# 제목\n<script>alert(1)</script><img src=\"https://evil.invalid/pixel\" onerror=\"alert(1)\">\n\
         <iframe src=\"https://evil.invalid\"></iframe><a href=\"javascript:alert(1)\">x</a>\n\
         <table><tr><td onclick=\"evil()\">preserved</td></tr></table>\n\n\
         ![private](../private/note.assets/image.png)\n\
         [next](https://notes.cola16.app/OneNote/markdown/public/next)\n\
         ![local](note.assets/image.png)\n",
    );
    for value in [
        "alert(",
        "onerror",
        "onclick",
        "<script",
        "<iframe",
        "evil.invalid",
        "../private",
    ] {
        assert!(!safe.contains(value), "{value} in {safe}");
    }
    assert!(safe.contains("<table>"));
    assert!(safe.contains("id=\"제목-1\""));
    assert!(safe.contains("/read?path=public%2Fnext.md"));
    assert_eq!(
        assets.into_iter().collect::<Vec<_>>(),
        ["public/note.assets/image.png"]
    );
}

#[tokio::test]
async fn reader_policy_change_fails_closed() {
    let corpus = Corpus::new();
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = reader_app(&corpus, &dir);
    std::fs::write(
        &corpus.policy,
        json!({"schemaVersion": 1, "publicNotebookIds": []}).to_string(),
    )
    .unwrap();
    assert_eq!(
        get(&app, "/read?path=public%2Fnote.md", &[]).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn reader_no_asset_fd_leak() {
    let corpus = Corpus::new();
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = reader_app(&corpus, &dir);
    let before = std::fs::read_dir("/proc/self/fd").unwrap().count();
    for _ in 0..20 {
        let response = get(
            &app,
            "/read-asset?path=public%2Fnote.md&asset=public%2Fnote.assets%2Fimage.png",
            &[],
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
    }
    // Other tests run concurrently in this process; allow small jitter.
    assert!(std::fs::read_dir("/proc/self/fd").unwrap().count() <= before + 8);
}

#[tokio::test]
async fn wordpress_preview_and_cached_excerpts_recheck_public_revisions() {
    use axum::Json;
    use axum::routing::get as route_get;
    use std::sync::Mutex;
    notes_rag_rs::install_crypto_provider();
    let public = Arc::new(Mutex::new(true));
    let catalog_public = public.clone();
    let post_public = public.clone();
    let post = json!({"id":1,"title":"Synthetic blog","url":"https://cola16.app/synthetic/","published":"2026-01-01","modified":"2026-01-02","revision":"a".repeat(64)});
    let cpost = post.clone();
    let mut doc = post.clone();
    doc["markdown"] =
        "# Synthetic\n\n![image](/wp-content/uploads/synthetic.png)\n<script>bad()</script>".into();
    let upstream=axum::Router::new().route("/catalog",route_get(move ||{let public=catalog_public.clone();let post=cpost.clone();async move {Json(json!({"schema_version":1,"revision":"c".repeat(64),"posts":if *public.lock().unwrap(){vec![post]}else{vec![]}}))}}))
        .route("/posts/1",route_get(move ||{let public=post_public.clone();let doc=doc.clone();async move {if *public.lock().unwrap(){(StatusCode::OK,Json(doc))}else{(StatusCode::NOT_FOUND,Json(json!({"error":"not found"})))}}}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let fixture = Fixture::new();
    *fixture.gateway.search_result.lock().unwrap() = Some(
        json!({"index":{"indexed_pages":1},"results":[{"title":"Synthetic blog","content":"Previously public evidence","source_url":"https://cola16.app/synthetic/","source_kind":"wordpress","post_id":1,"source_revision":"a".repeat(64),"rag_priority":"normal"}]}),
    );
    let mut app = WebApp::new(
        fixture.state.clone(),
        fixture.gateway.clone(),
        None,
        None,
        None,
        None,
        PREFIX.into(),
        NOTE_PREFIX.into(),
    )
    .unwrap();
    app.blog = Some(notes_rag_rs::blog::Source::new(&base).unwrap());
    let app = router(Arc::new(app));
    let response = post_json(&app, "/api/search", &json!({"query":"synthetic"}), &[]).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["results"].as_array().unwrap().len(), 1);
    let response = get(&app, "/read-blog?id=1", &[]).await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(
        response
            .header("content-security-policy")
            .unwrap()
            .contains("img-src 'self' https://cola16.app")
    );
    assert!(response.text().contains("WordPress에서 전문 보기"));
    assert!(!response.text().contains("<script>bad()"));
    *public.lock().unwrap() = false;
    let response = post_json(&app, "/api/search", &json!({"query":"synthetic"}), &[]).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["cached"], true);
    assert!(response.json()["results"].as_array().unwrap().is_empty());
    assert_eq!(
        fixture.gateway.searches(),
        1,
        "revoked result filtered without a paid query"
    );
    assert_eq!(
        get(&app, "/read-blog?id=1", &[]).await.status,
        StatusCode::NOT_FOUND
    );
    task.abort();
}
