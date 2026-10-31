//! Port of `mcp_server.BearerGate` plus the SDK's transport security, in the
//! same order the Python ASGI stack applied them:
//!
//! 1. `GET /healthz` → `{"status":"ok"}` without authentication;
//! 2. exactly one `Authorization` header equal (constant time) to the public
//!    or the distinct owner bearer token, else 401 + `WWW-Authenticate`;
//! 3. `GET /graph` → internal graph JSON for the web role (not an MCP tool);
//! 4. only `/mcp` exists (other paths 404); body > 32 KiB → 413;
//! 5. POST `Content-Type` must start with `application/json` → else 400;
//! 6. `Host` must match `RAG_MCP_HOSTS` (exact or `host:*`) → else 421;
//! 7. any non-empty `Origin` is rejected (no browser origins) → 403;
//! 8. the request is handed to rmcp's Streamable HTTP service, stateless,
//!    JSON responses, with the verified audience attached as an extension.

use crate::search::service::Retrieval;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::{HeaderValue, Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tower::ServiceExt;

pub const MAX_REQUEST_BODY: usize = 32768;

/// `mcp_types.version.HANDSHAKE_PROTOCOL_VERSIONS`: a request whose
/// `MCP-Protocol-Version` header is anything else (including unknown values)
/// was routed to the 2026-07-28 single-exchange entry by Python.
const HANDSHAKE_PROTOCOL_VERSIONS: [&str; 4] =
    ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

/// Which bearer token authenticated the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Public,
    Owner,
}

/// `BearerGate.__init__` token rules.
pub fn validate_tokens(token: &str, owner_token: Option<&str>) -> Result<(), String> {
    let strong = |t: &str| t.len() >= 40 && t.bytes().all(|b| (0x20..0x7f).contains(&b));
    if !strong(token) {
        return Err("A strong MCP-only bearer token is required".into());
    }
    if let Some(owner) = owner_token
        && (!strong(owner) || owner == token)
    {
        return Err("A distinct strong owner-only token is required".into());
    }
    Ok(())
}

pub type McpService = rmcp::transport::streamable_http_server::StreamableHttpService<
    super::handler::NotesHandler,
    rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
>;

pub struct Gate {
    expected: Vec<u8>,
    owner_expected: Option<Vec<u8>>,
    allowed_hosts: Vec<String>,
    pub public: Arc<dyn Retrieval>,
    pub owner: Option<Arc<dyn Retrieval>>,
    pub mcp: McpService,
}

impl Gate {
    pub fn new(
        token: &str,
        owner_token: Option<&str>,
        allowed_hosts: Vec<String>,
        public: Arc<dyn Retrieval>,
        owner: Option<Arc<dyn Retrieval>>,
        mcp: McpService,
    ) -> Result<Self, String> {
        validate_tokens(token, owner_token)?;
        if owner_token.is_some() != owner.is_some() {
            return Err("Owner service and credential must be configured together".into());
        }
        Ok(Self {
            expected: format!("Bearer {token}").into_bytes(),
            owner_expected: owner_token.map(|t| format!("Bearer {t}").into_bytes()),
            allowed_hosts,
            public,
            owner,
            mcp,
        })
    }

    fn authenticate(&self, request: &Request<Body>) -> Option<Audience> {
        let values: Vec<&HeaderValue> = request
            .headers()
            .get_all(header::AUTHORIZATION)
            .iter()
            .collect();
        let [value] = values.as_slice() else {
            return None;
        };
        let value = value.as_bytes();
        let matches =
            |expected: &[u8]| value.len() == expected.len() && bool::from(value.ct_eq(expected));
        // Evaluate both comparisons so timing does not reveal which token matched.
        let owner = self.owner_expected.as_deref().is_some_and(matches);
        let public = matches(&self.expected);
        if owner {
            Some(Audience::Owner)
        } else if public {
            Some(Audience::Public)
        } else {
            None
        }
    }

    /// `TransportSecurityMiddleware._validate_host`
    fn host_allowed(&self, host: Option<&str>) -> bool {
        let Some(host) = host.filter(|h| !h.is_empty()) else {
            return false;
        };
        self.allowed_hosts.iter().any(|allowed| {
            allowed == host
                || allowed
                    .strip_suffix(":*")
                    .is_some_and(|base| host.starts_with(&format!("{base}:")))
        })
    }

    pub async fn handle(self: Arc<Self>, request: Request<Body>) -> Response {
        let path = request.uri().path().to_string();
        if path == "/healthz" && request.method() == Method::GET {
            return json(StatusCode::OK, r#"{"status":"ok"}"#);
        }
        let Some(audience) = self.authenticate(&request) else {
            let mut response = json(StatusCode::UNAUTHORIZED, r#"{"error":"unauthorized"}"#);
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return response;
        };
        if path == "/graph" && request.method() == Method::GET {
            let service = match (audience, &self.owner) {
                (Audience::Owner, Some(owner)) => owner.clone(),
                _ => self.public.clone(),
            };
            return match service.graph().await {
                Ok(graph) => {
                    let mut response = json(
                        StatusCode::OK,
                        &serde_json::to_string(&graph).unwrap_or_default(),
                    );
                    response
                        .headers_mut()
                        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                    response
                }
                Err(_) => json(
                    StatusCode::SERVICE_UNAVAILABLE,
                    r#"{"error":"Graph unavailable; retry shortly."}"#,
                ),
            };
        }
        if path != "/mcp" {
            return text(StatusCode::NOT_FOUND, "Not Found");
        }
        self.forward(request, audience).await
    }

    async fn forward(&self, request: Request<Body>, audience: Audience) -> Response {
        let (mut parts, body) = request.into_parts();
        let declared = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        if declared.is_some_and(|size| size > MAX_REQUEST_BODY as u64) {
            return bare(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
        }
        let body = match Limited::new(body, MAX_REQUEST_BODY).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => return bare(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large"),
        };
        let content_type = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if parts.method == Method::POST
            && !content_type
                .to_ascii_lowercase()
                .starts_with("application/json")
        {
            return bare(StatusCode::BAD_REQUEST, "Invalid Content-Type header");
        }
        if !self.host_allowed(
            parts
                .headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok()),
        ) {
            return bare(StatusCode::MISDIRECTED_REQUEST, "Invalid Host header");
        }
        if parts
            .headers
            .get(header::ORIGIN)
            .is_some_and(|v| !v.is_empty())
        {
            return bare(StatusCode::FORBIDDEN, "Invalid Origin header");
        }
        // Requests that name the 2026-07-28 revision are single-exchange and
        // handled entirely by rmcp (Python routed them to its modern entry).
        let modern = parts.headers.get("mcp-protocol-version").is_some_and(|v| {
            !HANDSHAKE_PROTOCOL_VERSIONS
                .iter()
                .any(|known| v.as_bytes() == known.as_bytes())
        });
        if !modern && let Some(response) = legacy_transport_checks(&mut parts, &content_type, &body)
        {
            return response;
        }
        parts.extensions.insert(audience);
        let request = Request::from_parts(parts, Full::<Bytes>::new(body));
        match ServiceExt::<Request<Full<Bytes>>>::oneshot(self.mcp.clone(), request).await {
            Ok(response) => {
                let mut response = response.map(Body::new);
                // Python acknowledges notifications with an empty JSON response.
                if response.status() == StatusCode::ACCEPTED
                    && !response.headers().contains_key(header::CONTENT_TYPE)
                {
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                }
                response
            }
            Err(error) => match error {},
        }
    }
}

/// `check_accept_headers`: (has_json, has_sse) with RFC 7231 wildcards.
fn accepts(headers: &http::HeaderMap) -> (bool, bool) {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let types: Vec<String> = accept
        .split(',')
        .map(|t| {
            t.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .collect();
    let any = types.iter().any(|t| t == "*/*");
    let json = any
        || types
            .iter()
            .any(|t| t == "application/json" || t == "application/*");
    let sse = any
        || types
            .iter()
            .any(|t| t == "text/event-stream" || t == "text/*");
    (json, sse)
}

/// `StreamableHTTPServerTransport._create_error_response`
fn jsonrpc_error(status: StatusCode, code: i32, message: &str) -> Response {
    let body = serde_json::json!({"jsonrpc": "2.0", "id": null, "error": {"code": code, "message": message}});
    json(status, &body.to_string())
}

/// The Python legacy (handshake-era) stateless transport answered several
/// HTTP-level cases itself before any JSON-RPC dispatch; reproduce those
/// answers exactly, and adapt `Accept` so rmcp serves JSON-only clients the
/// way Python's `json_response=True` mode did.
fn legacy_transport_checks(
    parts: &mut http::request::Parts,
    content_type: &str,
    body: &Bytes,
) -> Option<Response> {
    let (has_json, has_sse) = accepts(&parts.headers);
    match parts.method {
        Method::POST => {
            if !has_json {
                return Some(jsonrpc_error(
                    StatusCode::NOT_ACCEPTABLE,
                    -32600,
                    "Not Acceptable: Client must accept application/json",
                ));
            }
            let exact = content_type
                .split(';')
                .next()
                .unwrap_or("")
                .split(',')
                .any(|part| part.trim() == "application/json");
            if !exact {
                return Some(jsonrpc_error(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    -32600,
                    "Unsupported Media Type: Content-Type must be application/json",
                ));
            }
            let message: serde_json::Value = match serde_json::from_slice(body) {
                Ok(message) => message,
                Err(error) => {
                    return Some(jsonrpc_error(
                        StatusCode::BAD_REQUEST,
                        -32700,
                        &format!("Parse error: {error}"),
                    ));
                }
            };
            if !is_jsonrpc_message(&message) {
                return Some(jsonrpc_error(
                    StatusCode::BAD_REQUEST,
                    -32602,
                    "Validation error: not a single JSON-RPC 2.0 message",
                ));
            }
            if !has_sse {
                // JSON-response mode only needs application/json (Python);
                // rmcp's acceptance check wants both, and still answers JSON.
                parts.headers.insert(
                    header::ACCEPT,
                    HeaderValue::from_static("application/json, text/event-stream"),
                );
            }
            None
        }
        Method::GET if !has_sse => Some(jsonrpc_error(
            StatusCode::NOT_ACCEPTABLE,
            -32600,
            "Not Acceptable: Client must accept text/event-stream",
        )),
        // Stateless: Python opened an idle SSE stream that never carries a
        // message; rmcp answers 405 (allowed by the specification).
        Method::GET => None,
        Method::DELETE => Some(jsonrpc_error(
            StatusCode::METHOD_NOT_ALLOWED,
            -32600,
            "Method Not Allowed: Session termination not supported",
        )),
        _ => {
            let mut response =
                jsonrpc_error(StatusCode::METHOD_NOT_ALLOWED, -32600, "Method Not Allowed");
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, POST, DELETE"));
            Some(response)
        }
    }
}

/// Structural JSON-RPC 2.0 message check (request, notification, response, error).
fn is_jsonrpc_message(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return false;
    }
    let id_ok = |id: &serde_json::Value| id.is_string() || (id.is_i64() || id.is_u64());
    match (object.get("method"), object.get("id")) {
        (Some(method), None) => method.is_string(),
        (Some(method), Some(id)) => method.is_string() && id_ok(id),
        (None, Some(id)) => {
            id_ok(id) && (object.contains_key("result") || object.contains_key("error"))
        }
        (None, None) => false,
    }
}

fn json(status: StatusCode, body: &str) -> Response {
    let mut response = (status, body.to_string()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Starlette `Response(text, status_code)` without a media type: no
/// `Content-Type` header, exactly like the Python transport-security errors.
fn bare(status: StatusCode, body: &'static str) -> Response {
    let mut response = (status, body).into_response();
    response.headers_mut().remove(header::CONTENT_TYPE);
    response
}

fn text(status: StatusCode, body: &'static str) -> Response {
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}
