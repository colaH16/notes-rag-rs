//! MCP tool handler on the official Rust SDK (`rmcp::ServerHandler`).
//!
//! Python used `MCPServer` decorators that derived schemas from type hints.
//! To keep tool names, descriptions, input/output schemas, annotations,
//! instructions and capabilities byte-for-byte identical, they are served from
//! `compat/mcp-contract.json`, which `tests/oracle/dump_mcp_contract.py`
//! captures from the pinned Python server. The protocol itself (initialize,
//! version negotiation, JSON-RPC framing, 2026-07-28 stateless requests) is
//! handled entirely by rmcp; nothing here hand-rolls MCP.

use super::args;
use super::gate::Audience;
use crate::fault::Fault;
use crate::search::service::{Retrieval, SearchRequest};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Value, json};
use std::sync::Arc;

pub const CONTRACT_JSON: &str = include_str!("../../compat/mcp-contract.json");

/// The served contract, parsed once at startup.
pub struct Contract {
    pub server: ServerConfig,
    pub tools: ListToolsResult,
}

impl Contract {
    pub fn embedded() -> Result<Self, String> {
        let raw: Value =
            serde_json::from_str(CONTRACT_JSON).map_err(|_| "MCP contract is not valid JSON")?;
        let server = serde_json::from_value(raw["initialize"].clone())
            .map_err(|_| "invalid MCP initialize contract")?;
        let tools =
            serde_json::from_value(json!({"resultType": "complete", "tools": raw["tools"]}))
                .map_err(|_| "invalid MCP tool contract")?;
        Ok(Self { server, tools })
    }
}

#[derive(Clone)]
pub struct NotesHandler {
    pub contract: Arc<Contract>,
    pub public: Arc<dyn Retrieval>,
    pub owner: Option<Arc<dyn Retrieval>>,
}

impl NotesHandler {
    /// `selected()`: owner service only for a verified owner bearer token.
    fn selected(&self, context: &RequestContext<RoleServer>) -> &Arc<dyn Retrieval> {
        let owner = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Audience>())
            .is_some_and(|audience| *audience == Audience::Owner);
        match (&self.owner, owner) {
            (Some(service), true) => service,
            _ => &self.public,
        }
    }

    async fn run_tool(
        &self,
        request: CallToolRequestParams,
        service: &dyn Retrieval,
    ) -> CallToolResult {
        let arguments = request.arguments.clone().unwrap_or_default();
        let name = request.name.as_ref();
        let outcome = match name {
            "search_notes" => match args::search_notes(&arguments) {
                Err(errors) => Err(format!("Error executing tool search_notes: {}", errors.message())),
                Ok(parsed) => {
                    let effective = parsed.limit.min(8);
                    let request = SearchRequest {
                        query: parsed.query,
                        limit: effective,
                        include_history: parsed.include_history,
                        min_similarity: parsed.min_similarity,
                        folder: parsed.folder,
                    };
                    match service.search(request).await {
                        Ok(mut result) => {
                            let found = result["results"].as_array().is_some_and(|r| !r.is_empty());
                            if let Some(map) = result.as_object_mut() {
                                map.insert("retrieval_status".into(), (if found { "found" } else { "no_matches" }).into());
                                map.insert("requested_limit".into(), parsed.limit.into());
                                map.insert("effective_limit".into(), effective.into());
                            }
                            Ok(result)
                        }
                        Err(fault) => Err(format!(
                            "Error executing tool search_notes: Search failed, not an empty search result. {} \
                             Retry once with a plain query and default arguments. If it still fails, report the \
                             failure; do not answer from general knowledge.",
                            search_message(&fault)
                        )),
                    }
                }
            },
            "list_note_folders" => service
                .folders()
                .await
                .map_err(|fault| format!("Error executing tool list_note_folders: Folder listing failed. {}", search_message(&fault))),
            "get_note_chunk" => match args::get_note_chunk(&arguments) {
                Err(errors) => Err(format!("Error executing tool get_note_chunk: {}", errors.message())),
                Ok((page_key, ordinal)) => service.chunk(page_key, ordinal).await.map_err(|fault| {
                    format!(
                        "Error executing tool get_note_chunk: Chunk retrieval failed. {} Do not invent the missing note content.",
                        search_message(&fault)
                    )
                }),
            },
            other => Err(format!("Unknown tool: {other}")),
        };
        tool_result(outcome)
    }
}

/// `str(SearchError)`; anything else is already generic inside the service.
fn search_message(fault: &Fault) -> String {
    match fault {
        Fault::Search(message) => message.clone(),
        _ => "Retrieval failed; check service connectivity/configuration.".into(),
    }
}

/// Results carry `resultType: "complete"` (SEP-2322, required by 2026-07-28
/// clients); rmcp strips it again for peers on handshake-era versions.
///
/// `FuncMetadata.convert_result` for a `dict[str, Any]` return value, or the
/// `isError` result the SDK builds from a `ToolError`.
pub fn tool_result(outcome: Result<Value, String>) -> CallToolResult {
    let value = match outcome {
        Ok(structured) => {
            let text = serde_json::to_string_pretty(&structured).unwrap_or_default();
            json!({"resultType": "complete", "content": [{"type": "text", "text": text}], "structuredContent": structured, "isError": false})
        }
        Err(message) => {
            json!({"resultType": "complete", "content": [{"type": "text", "text": message}], "isError": true})
        }
    };
    serde_json::from_value(value).expect("static CallToolResult shape")
}

impl ServerHandler for NotesHandler {
    fn get_info(&self) -> ServerConfig {
        self.contract.server.clone()
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(self.contract.tools.clone())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let service = self.selected(&context).clone();
        Ok(CallToolResponse::Complete(
            self.run_tool(request, service.as_ref()).await,
        ))
    }

    /// Python: `{"code": -32601, "message": "Method not found", "data": <method>}`.
    async fn on_custom_request(
        &self,
        request: rmcp::model::CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CustomResult, McpError> {
        Err(McpError::new(
            rmcp::model::ErrorCode::METHOD_NOT_FOUND,
            "Method not found",
            Some(Value::String(request.method.to_string())),
        ))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(
            serde_json::from_value(json!({"resultType": "complete", "prompts": []}))
                .expect("static shape"),
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(
            serde_json::from_value(json!({"resultType": "complete", "resources": []}))
                .expect("static shape"),
        )
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(
            serde_json::from_value(json!({"resultType": "complete", "resourceTemplates": []}))
                .expect("static shape"),
        )
    }
}
