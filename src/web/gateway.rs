//! Port of `public_search.MCPGateway`: the web role's only upstream.
//!
//! Python opened a fresh official-SDK client per call in `mode='legacy'`
//! (initialize handshake, protocol 2025-11-25). The Rust gateway does the
//! same with the official rmcp client, so it works against either the Python
//! or the Rust MCP role during a mixed rollout. The browser never sees the
//! MCP token and there is no configurable upstream beyond `RAG_MCP_URL`.

use crate::fault::{Fault, Result};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Map, Value, json};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Upstream seam (Python tests used `AsyncMock`).
pub trait Upstream: Send + Sync {
    fn search(&self, query: String, history: bool, folder: String) -> BoxFuture<'_, Result<Value>>;
    fn folders(&self) -> BoxFuture<'_, Result<Value>>;
    fn graph(&self) -> BoxFuture<'_, Result<Value>>;
}

pub struct McpGateway {
    url: String,
    token: String,
    mcp_http: reqwest::Client,
    graph_http: reqwest::Client,
}

fn upstream_failed<E>(_: E) -> Fault {
    Fault::Other("Upstream retrieval failed")
}

impl McpGateway {
    pub fn new(url: &str, token: &str) -> std::result::Result<Self, String> {
        // httpx2.AsyncClient(timeout=40 / 35, trust_env=False); no pooling,
        // because Python created a new client per call as well.
        let client = |timeout: u64| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(timeout))
                .pool_max_idle_per_host(0)
                .build()
                .map_err(|_| "cannot build the MCP HTTP client".to_string())
        };
        Ok(Self {
            url: url.to_string(),
            token: token.to_string(),
            mcp_http: client(40)?,
            graph_http: client(35)?,
        })
    }

    /// `call(tool, arguments)`: errors, empty or `error` structured content
    /// are all failures; nothing upstream is echoed.
    async fn call(&self, tool: &'static str, arguments: Map<String, Value>) -> Result<Value> {
        let config = StreamableHttpClientTransportConfig::with_uri(self.url.clone())
            .auth_header(self.token.clone());
        let transport = StreamableHttpClientTransport::with_client(self.mcp_http.clone(), config);
        let info: ClientConfig = serde_json::from_value(json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "notes-rag-web", "version": env!("CARGO_PKG_VERSION")},
        }))
        .map_err(upstream_failed)?;
        let client = info.serve(transport).await.map_err(upstream_failed)?;
        let outcome = client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
            .await;
        let _ = client.cancel().await;
        let result = outcome.map_err(upstream_failed)?;
        let structured = match (result.is_error, result.structured_content) {
            (Some(true), _) | (_, None) => return Err(upstream_failed(())),
            (_, Some(structured)) => structured,
        };
        let empty = structured.as_object().is_none_or(|map| map.is_empty());
        let has_error = structured.get("error").is_some_and(crate::py::json::truthy);
        if empty || has_error {
            return Err(upstream_failed(()));
        }
        Ok(structured)
    }
}

impl Upstream for McpGateway {
    fn search(&self, query: String, history: bool, folder: String) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            let arguments =
                json!({"query": query, "limit": 5, "include_history": history, "folder": folder});
            self.call(
                "search_notes",
                arguments.as_object().cloned().unwrap_or_default(),
            )
            .await
        })
    }

    fn folders(&self) -> BoxFuture<'_, Result<Value>> {
        Box::pin(self.call("list_note_folders", Map::new()))
    }

    fn graph(&self) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            let url = format!(
                "{}/graph",
                self.url.strip_suffix("/mcp").unwrap_or(&self.url)
            );
            let response = self
                .graph_http
                .get(url)
                .header("Authorization", format!("Bearer {}", self.token))
                .send()
                .await
                .map_err(upstream_failed)?;
            if !response.status().is_success() {
                return Err(upstream_failed(()));
            }
            let body = response.bytes().await.map_err(upstream_failed)?;
            serde_json::from_slice(&body).map_err(|_| Fault::Value("invalid graph JSON"))
        })
    }
}
