//! `RAG_ROLE=web`: public search/graph/reader gateway (port of
//! `public_search.py`, `web_auth.py`).

pub mod app;
pub mod auth;
pub mod gateway;
pub mod http;
pub mod state;

use crate::config::{Env, WebConfig};
use crate::reader::{Reader, ReaderConfig};
use app::WebApp;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Wrap the application with the security-header middleware (outermost
/// after the concurrency limiter, like Starlette's `BaseHTTPMiddleware`).
pub fn router(app: Arc<WebApp>) -> axum::Router {
    axum::Router::new()
        .fallback(move |request: axum::extract::Request| {
            let app = app.clone();
            async move { app.route(request).await }
        })
        .layer(axum::middleware::from_fn(http::security_headers))
}

pub async fn run(env: &Env) -> Result<(), String> {
    crate::native::require_folder_acl()?;
    let config = WebConfig::from_env(env)?;
    let state = Arc::new(
        state::State::new(
            &config.state_db,
            config.daily_limit,
            config.hourly_limit,
            state::system_clock(),
        )
        .map_err(|e| format!("state: {e}"))?,
    );
    let gateway = Arc::new(gateway::McpGateway::new(
        &config.mcp_url,
        &config.mcp_token,
    )?);
    let auth = Arc::new(auth::OwnerAuth::new(
        state.clone(),
        &config.oidc_client_id,
        &config.oidc_client_secret,
        &config.session_key,
        &config.owner_subject,
        &config.public_origin,
        &config.oidc_issuer,
    )?);
    let owner_gateway = Arc::new(gateway::McpGateway::new(
        &config.mcp_url,
        &config.owner_mcp_token,
    )?);
    let reader = Arc::new(
        Reader::new(ReaderConfig {
            root: config.corpus_root.clone(),
            selection: config.selection.clone(),
            policy: config.policy.clone(),
            revision: config.public_access_revision.clone(),
            note_prefix: config.note_prefix.clone(),
            silverbullet_origin: config.silverbullet_url.clone(),
            vision_state: config.vision_state.clone(),
            native_notes: crate::native::folder_acl_enabled(),
        })
        .map_err(|e| format!("reader: {e}"))?,
    );
    let app = WebApp::new(
        state,
        gateway,
        Some(config.public_access_revision.clone()),
        Some(auth),
        Some(owner_gateway),
        Some(reader),
        config.citation_prefix(),
        config.note_prefix.clone(),
    )
    .map_err(|e| format!("web: {e}"))?;
    println!("Notes RAG web ready.");
    crate::serve::serve(
        config.listen,
        router(app),
        Some(32),
        config.shutdown_grace,
        CancellationToken::new(),
    )
    .await
}
