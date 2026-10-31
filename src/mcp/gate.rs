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
            return text(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
        }
        let body = match Limited::new(body, MAX_REQUEST_BODY).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => return text(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large"),
        };
        if parts.method == Method::POST {
            let content_type = parts
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok());
            if !content_type.is_some_and(|v| v.to_ascii_lowercase().starts_with("application/json"))
            {
                return text(StatusCode::BAD_REQUEST, "Invalid Content-Type header");
            }
        }
        if !self.host_allowed(
            parts
                .headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok()),
        ) {
            return text(StatusCode::MISDIRECTED_REQUEST, "Invalid Host header");
        }
        if parts
            .headers
            .get(header::ORIGIN)
            .is_some_and(|v| !v.is_empty())
        {
            return text(StatusCode::FORBIDDEN, "Invalid Origin header");
        }
        parts.extensions.insert(audience);
        let request = Request::from_parts(parts, Full::<Bytes>::new(body));
        match ServiceExt::<Request<Full<Bytes>>>::oneshot(self.mcp.clone(), request).await {
            Ok(response) => response.map(Body::new),
            Err(error) => match error {},
        }
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

fn text(status: StatusCode, body: &'static str) -> Response {
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}
