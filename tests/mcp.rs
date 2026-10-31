//! Port of the original `tests/test_mcp.py` and the internal-endpoint part
//! of `tests/test_semantic_graph.py`, run through the real gate and the
//! official rmcp Streamable HTTP service with a mock retrieval service.

mod common;

use axum::body::Body;
use common::*;
use http::{Method, StatusCode};
use notes_rag_rs::fault::{Fault, Result};
use notes_rag_rs::mcp::{build_gate, router};
use notes_rag_rs::search::service::{BoxFuture, Retrieval, SearchRequest};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "synthetic-test-token-synthetic-test-token-synthetic-test-token-";

#[derive(Default)]
struct MockService {
    name: &'static str,
    searches: Mutex<Vec<SearchRequest>>,
    fail_search: Mutex<Option<String>>,
}

impl Retrieval for MockService {
    fn search(&self, request: SearchRequest) -> BoxFuture<'_, Result<Value>> {
        self.searches.lock().unwrap().push(request);
        let failure = self.fail_search.lock().unwrap().clone();
        Box::pin(async move {
            match failure {
                Some(message) => Err(Fault::Search(message)),
                None => Ok(json!({"results": [], "index": {"indexed_pages": 0}})),
            }
        })
    }
    fn folders(&self) -> BoxFuture<'_, Result<Value>> {
        let pages = if self.name == "owner" { 2 } else { 1 };
        Box::pin(async move { Ok(json!({"indexed_pages": pages, "folders": []})) })
    }
    fn graph(&self) -> BoxFuture<'_, Result<Value>> {
        let name = self.name;
        Box::pin(async move { Ok(json!({"nodes": [name]})) })
    }
    fn chunk(&self, _page_key: String, _ordinal: i64) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async { Ok(json!({"result": null})) })
    }
}

struct Server {
    router: axum::Router,
    public: Arc<MockService>,
}

fn server(owner_token: Option<&str>) -> Server {
    let public = Arc::new(MockService {
        name: "public",
        ..Default::default()
    });
    let owner = owner_token.map(|_| {
        Arc::new(MockService {
            name: "owner",
            ..Default::default()
        }) as Arc<dyn Retrieval>
    });
    let gate = build_gate(
        TOKEN,
        owner_token,
        vec!["testserver".into()],
        public.clone(),
        owner,
        CancellationToken::new(),
    )
    .unwrap();
    Server {
        router: router(gate),
        public,
    }
}

async fn rpc(
    router: &axum::Router,
    method: &str,
    params: Value,
    headers: &[(&str, &str)],
) -> Response {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let bearer = format!("Bearer {TOKEN}");
    let mut all = vec![
        ("host", "testserver"),
        ("authorization", bearer.as_str()),
        ("accept", "application/json, text/event-stream"),
    ];
    for (name, value) in headers {
        all.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        all.push((name, value));
    }
    post_json(router, "/mcp", &body, &all).await
}

async fn initialized(router: &axum::Router) {
    let init = rpc(
        router,
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name": "synthetic-test", "version": "1"}}),
        &[],
    )
    .await;
    assert_eq!(init.status, StatusCode::OK, "{}", init.text());
}

#[tokio::test]
async fn auth_missing_wrong_and_duplicate() {
    let s = server(None);
    for auth in ["", "Bearer wrong"] {
        let response = rpc(
            &s.router,
            "tools/list",
            json!({}),
            &[("authorization", auth)],
        )
        .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
        assert_eq!(response.header("www-authenticate"), Some("Bearer"));
        assert_eq!(response.json(), json!({"error": "unauthorized"}));
    }
    let bearer = format!("Bearer {TOKEN}");
    let mut request = http::Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header("host", "testserver");
    request = request
        .header("authorization", &bearer)
        .header("authorization", &bearer);
    let response =
        tower::ServiceExt::oneshot(s.router.clone(), request.body(Body::empty()).unwrap())
            .await
            .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(s.public.searches.lock().unwrap().is_empty());
}

#[tokio::test]
async fn initialize_tools_and_search() {
    let s = server(None);
    let init = rpc(
        &s.router,
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name": "synthetic-test", "version": "1"}}),
        &[],
    )
    .await;
    assert_eq!(init.status, StatusCode::OK);
    let result = &init.json()["result"];
    assert_eq!(result["serverInfo"]["name"], "notes-rag");
    assert_eq!(result["protocolVersion"], "2025-11-25");
    assert!(
        result["instructions"]
            .as_str()
            .unwrap()
            .contains("untrusted evidence")
    );
    let tools =
        rpc(&s.router, "tools/list", json!({}), &[]).await.json()["result"]["tools"].clone();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["search_notes", "list_note_folders", "get_note_chunk"]
    );
    assert!(
        tools
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["annotations"]["readOnlyHint"] == true)
    );
    let contract: Value = serde_json::from_str(notes_rag_rs::mcp::handler::CONTRACT_JSON).unwrap();
    assert_eq!(
        tools, contract["tools"],
        "tools/list must equal the captured Python contract"
    );
    let response = rpc(
        &s.router,
        "tools/call",
        json!({"name": "search_notes", "arguments": {"query": "synthetic"}}),
        &[],
    )
    .await;
    let result = &response.json()["result"];
    assert_eq!(result["structuredContent"]["results"], json!([]));
    assert_eq!(
        result["structuredContent"]["retrieval_status"],
        "no_matches"
    );
    assert_eq!(result["isError"], false);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap(),
        result["structuredContent"]
    );
    assert_eq!(s.public.searches.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn untrusted_origin_host_and_size() {
    let s = server(None);
    let origin = rpc(
        &s.router,
        "tools/list",
        json!({}),
        &[("origin", "https://evil.invalid")],
    )
    .await;
    assert_eq!(
        (origin.status, origin.text().as_str()),
        (StatusCode::FORBIDDEN, "Invalid Origin header")
    );
    let host = rpc(
        &s.router,
        "tools/list",
        json!({}),
        &[("host", "evil.invalid")],
    )
    .await;
    assert_eq!(
        (host.status, host.text().as_str()),
        (StatusCode::MISDIRECTED_REQUEST, "Invalid Host header")
    );
    let big = rpc(
        &s.router,
        "tools/call",
        json!({"name": "search_notes", "arguments": {"query": "x".repeat(40000)}}),
        &[],
    )
    .await;
    assert_eq!(big.status, StatusCode::PAYLOAD_TOO_LARGE);
    let bearer = format!("Bearer {TOKEN}");
    let wrong_type = send(
        &s.router,
        Method::POST,
        "/mcp",
        &[
            ("host", "testserver"),
            ("authorization", &bearer),
            ("content-type", "text/plain"),
        ],
        Body::from("{}"),
    )
    .await;
    assert_eq!(wrong_type.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        get(&s.router, "/other", &[("authorization", &bearer)])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn oversized_limit_is_capped_and_schema_explains_bounds() {
    let s = server(None);
    let tools =
        rpc(&s.router, "tools/list", json!({}), &[]).await.json()["result"]["tools"].clone();
    let schema = &tools[0]["inputSchema"]["properties"];
    assert_eq!(schema["limit"]["minimum"], 1);
    assert!(
        schema["limit"]["description"]
            .as_str()
            .unwrap()
            .contains("capped")
    );
    assert_eq!(schema["min_similarity"]["maximum"], 1);
    let response = rpc(
        &s.router,
        "tools/call",
        json!({"name": "search_notes", "arguments": {
        "query": "zypper update 방법 OR 사용법 OR 명령어", "limit": 10, "min_similarity": 0.3}}),
        &[],
    )
    .await;
    let result = &response.json()["result"];
    assert_eq!(result["isError"], false);
    assert_eq!(
        s.public.searches.lock().unwrap().as_slice(),
        [SearchRequest {
            query: "zypper update 방법 OR 사용법 OR 명령어".into(),
            limit: 8,
            include_history: true,
            min_similarity: 0.3,
            folder: String::new()
        }]
    );
    assert_eq!(result["structuredContent"]["effective_limit"], 8);
    assert_eq!(result["structuredContent"]["requested_limit"], 10);
}

#[tokio::test]
async fn search_failure_is_mcp_error_not_empty_results() {
    let s = server(None);
    *s.public.fail_search.lock().unwrap() = Some("Search is busy; retry shortly.".into());
    let result = rpc(
        &s.router,
        "tools/call",
        json!({"name": "search_notes", "arguments": {"query": "test"}}),
        &[],
    )
    .await
    .json()["result"]
        .clone();
    assert_eq!(result["isError"], true);
    assert!(result.get("structuredContent").is_none());
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Error executing tool search_notes: Search failed, not an empty search result. Search is busy; retry shortly."));
}

#[tokio::test]
async fn invalid_limits_rejected_before_service() {
    let s = server(None);
    for args in [
        json!({"limit": 0}),
        json!({"limit": -1}),
        json!({"min_similarity": 1.1}),
    ] {
        let mut arguments = args.as_object().unwrap().clone();
        arguments.insert("query".into(), json!("x"));
        let result = rpc(
            &s.router,
            "tools/call",
            json!({"name": "search_notes", "arguments": arguments}),
            &[],
        )
        .await
        .json()["result"]
            .clone();
        assert_eq!(result["isError"], true, "{args}");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("validation error for search_notesArguments")
        );
    }
    assert!(s.public.searches.lock().unwrap().is_empty());
    let unknown = rpc(
        &s.router,
        "tools/call",
        json!({"name": "nope", "arguments": {}}),
        &[],
    )
    .await
    .json()["result"]
        .clone();
    assert_eq!(
        (
            unknown["isError"].clone(),
            unknown["content"][0]["text"].clone()
        ),
        (json!(true), json!("Unknown tool: nope"))
    );
}

#[tokio::test]
async fn health_is_no_content_and_no_auth() {
    let s = server(None);
    let response = get(&s.router, "/healthz", &[]).await;
    assert_eq!(response.json(), json!({"status": "ok"}));
    assert!(s.public.searches.lock().unwrap().is_empty());
}

#[test]
fn weak_token_refused() {
    let public: Arc<dyn Retrieval> = Arc::new(MockService::default());
    assert!(
        build_gate(
            "weak",
            None,
            vec![],
            public.clone(),
            None,
            CancellationToken::new()
        )
        .is_err()
    );
    let same = Some(TOKEN);
    assert!(
        build_gate(
            TOKEN,
            same,
            vec![],
            public.clone(),
            Some(public.clone()),
            CancellationToken::new()
        )
        .is_err()
    );
    assert!(
        build_gate(
            TOKEN,
            None,
            vec![],
            public.clone(),
            Some(public),
            CancellationToken::new()
        )
        .is_err()
    );
}

#[tokio::test]
async fn owner_token_selects_service_per_request_without_leaking_context() {
    let owner_token = "owner-secret-owner-secret-owner-secret-owner-secret-";
    let s = server(Some(owner_token));
    for (token, count) in [(TOKEN, 1), (owner_token, 2), (TOKEN, 1)] {
        let bearer = format!("Bearer {token}");
        let response = rpc(
            &s.router,
            "tools/call",
            json!({"name": "list_note_folders", "arguments": {}}),
            &[("authorization", &bearer)],
        )
        .await;
        assert_eq!(
            response.json()["result"]["structuredContent"]["indexed_pages"],
            count
        );
        let graph = get(&s.router, "/graph", &[("authorization", &bearer)]).await;
        assert_eq!(
            graph.json()["nodes"],
            json!([if count == 2 { "owner" } else { "public" }])
        );
        assert_eq!(graph.header("cache-control"), Some("no-store"));
    }
    assert_eq!(
        get(&s.router, "/graph", &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn folder_tool_and_search_argument() {
    let s = server(None);
    initialized(&s.router).await;
    let result = rpc(
        &s.router,
        "tools/call",
        json!({"name": "list_note_folders", "arguments": {}}),
        &[],
    )
    .await
    .json()["result"]
        .clone();
    assert_eq!(result["structuredContent"]["indexed_pages"], 1);
    assert!(s.public.searches.lock().unwrap().is_empty());
    rpc(
        &s.router,
        "tools/call",
        json!({"name": "search_notes", "arguments": {"query": "test", "folder": "노트/하위"}}),
        &[],
    )
    .await;
    assert_eq!(s.public.searches.lock().unwrap()[0].folder, "노트/하위");
}

/// HTTP-level answers of the Python legacy stateless transport, as observed
/// against the pinned server by `tests/oracle/e2e.py`.
#[tokio::test]
async fn legacy_transport_http_answers_match_python() {
    let s = server(None);
    let bearer = format!("Bearer {TOKEN}");
    let base = [("host", "testserver"), ("authorization", bearer.as_str())];
    let with = |extra: &[(&'static str, &'static str)]| -> Vec<(&str, &str)> {
        let mut headers = base.to_vec();
        headers.extend_from_slice(extra);
        headers
    };
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();
    // JSON-only Accept is enough in JSON-response mode.
    let json_only = send(
        &s.router,
        Method::POST,
        "/mcp",
        &with(&[
            ("content-type", "application/json"),
            ("accept", "application/json"),
        ]),
        Body::from(list.clone()),
    )
    .await;
    assert_eq!(json_only.status, StatusCode::OK);
    assert_eq!(
        json_only.json()["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let no_accept = send(
        &s.router,
        Method::POST,
        "/mcp",
        &with(&[("content-type", "application/json")]),
        Body::from(list.clone()),
    )
    .await;
    assert_eq!(no_accept.status, StatusCode::NOT_ACCEPTABLE);
    assert_eq!(
        no_accept.json(),
        json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": "Not Acceptable: Client must accept application/json"}})
    );
    let accept = [
        ("content-type", "application/json"),
        ("accept", "application/json, text/event-stream"),
    ];
    let parse = send(
        &s.router,
        Method::POST,
        "/mcp",
        &with(&accept),
        Body::from("{not json"),
    )
    .await;
    assert_eq!(
        (parse.status, parse.json()["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!(-32700))
    );
    assert!(
        parse.json()["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Parse error: key must be a string at line 1 column 2")
    );
    let batch = send(
        &s.router,
        Method::POST,
        "/mcp",
        &with(&accept),
        Body::from(format!("[{list}]")),
    )
    .await;
    assert_eq!(
        (batch.status, batch.json()["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!(-32602))
    );
    let patch_type = send(
        &s.router,
        Method::POST,
        "/mcp",
        &with(&[
            ("content-type", "application/json-patch+json"),
            ("accept", "application/json"),
        ]),
        Body::from(list.clone()),
    )
    .await;
    assert_eq!(patch_type.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let notification = send(
        &s.router,
        Method::POST,
        "/mcp",
        &with(&accept),
        Body::from(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string()),
    )
    .await;
    assert_eq!(
        (notification.status, notification.header("content-type")),
        (StatusCode::ACCEPTED, Some("application/json"))
    );
    let unknown = send(
        &s.router,
        Method::POST,
        "/mcp",
        &with(&accept),
        Body::from(json!({"jsonrpc": "2.0", "id": 7, "method": "nope/x"}).to_string()),
    )
    .await;
    assert_eq!(
        unknown.json()["error"],
        json!({"code": -32601, "message": "Method not found", "data": "nope/x"})
    );
    let get = send(&s.router, Method::GET, "/mcp", &base, Body::empty()).await;
    assert_eq!(get.status, StatusCode::NOT_ACCEPTABLE);
    assert_eq!(
        get.json()["error"]["message"],
        "Not Acceptable: Client must accept text/event-stream"
    );
    let delete = send(&s.router, Method::DELETE, "/mcp", &base, Body::empty()).await;
    assert_eq!(delete.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        delete.json()["error"]["message"],
        "Method Not Allowed: Session termination not supported"
    );
    let put = send(&s.router, Method::PUT, "/mcp", &base, Body::empty()).await;
    assert_eq!(
        (put.status, put.header("allow")),
        (StatusCode::METHOD_NOT_ALLOWED, Some("GET, POST, DELETE"))
    );
    let host = send(
        &s.router,
        Method::POST,
        "/mcp",
        &[
            ("host", "evil.invalid"),
            ("authorization", bearer.as_str()),
            ("content-type", "application/json"),
        ],
        Body::from(list),
    )
    .await;
    assert_eq!(
        (
            host.status,
            host.header("content-type"),
            host.text().as_str()
        ),
        (StatusCode::MISDIRECTED_REQUEST, None, "Invalid Host header")
    );
}
