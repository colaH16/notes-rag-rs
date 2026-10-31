//! `RAG_ROLE=mcp`: internal Streamable HTTP MCP with bearer auth and the
//! internal `/graph` endpoint (port of `mcp_server.py` + `search.from_environment`).

pub mod args;
pub mod gate;
pub mod handler;

use crate::compat::{ProducerManifest, scope_signature};
use crate::config::{Env, McpConfig};
use crate::embedding::EmbeddingClient;
use crate::policy::{load_allowlist, load_public_notebooks, owner_revision};
use crate::reader::{Reader, ReaderConfig};
use crate::search::service::{RemoteEmbedder, Retrieval, SearchService};
use crate::search::store::SearchStore;
use crate::tokenizer::QwenTokenizer;
use gate::Gate;
use handler::{Contract, NotesHandler};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// `search.from_environment(owner=...)` for both audiences.
async fn services(
    config: &McpConfig,
    manifest: &ProducerManifest,
) -> Result<(Arc<SearchService>, Option<Arc<SearchService>>), String> {
    crate::native::require_folder_acl()?;
    let allowlist = load_allowlist(&config.allowlist).map_err(|e| format!("allowlist: {e}"))?;
    let root = allowlist
        .silverbullet_root()
        .ok_or("allowlist has no silverBulletRoot")?
        .trim_matches('/')
        .to_string();
    let scope = scope_signature(&allowlist.ids);
    let generation = manifest.config_signature();
    let tokenizer = Arc::new(QwenTokenizer::load(&config.tokenizer_path, manifest)?);
    let database = crate::search::db::Database::from_options(&config.database)?;
    let blog = crate::blog::Source::from_env()
        .map_err(|e| format!("blog source: {e}"))?
        .map(|source| {
            Arc::new(crate::blog::Store::new(
                source,
                database.clone(),
                generation.clone(),
            ))
        });
    let (public_ids, public_revision) =
        load_public_notebooks(&config.access_policy, &allowlist.ids)
            .map_err(|e| format!("access policy: {e}"))?;
    // Explicit corpus mount required; never silently omit fresh ACL checks.
    let reader = Arc::new(
        Reader::new(ReaderConfig {
            root: config.corpus_root.clone(),
            selection: config.allowlist.clone(),
            policy: config.access_policy.clone(),
            revision: public_revision.clone(),
            note_prefix: config.note_prefix.clone(),
            silverbullet_origin: config.silverbullet_url.clone(),
            vision_state: config.vision_state.clone(),
            native_notes: crate::native::folder_acl_enabled(),
        })
        .map_err(|e| format!("reader: {e}"))?,
    );
    let build = |owner: bool| -> Result<Arc<SearchService>, String> {
        let (ids, revision) = if owner {
            (
                allowlist.ids.iter().cloned().collect::<Vec<_>>(),
                owner_revision(&public_revision),
            )
        } else {
            (public_ids.clone(), public_revision.clone())
        };
        let visible_reader = reader.clone();
        let store = SearchStore {
            database: database.clone(),
            scope_signature: scope.clone(),
            generation_signature: generation.clone(),
            allowed_notebooks: Some(ids),
            access_revision: revision,
            visibility: Some(Arc::new(move |paths: &[String]| {
                visible_reader.visible_paths(paths, owner)
            })),
        };
        let client = EmbeddingClient::new(
            &config.litellm_base_url,
            &config.litellm_api_key,
            config.allow_internal_http,
            Duration::from_secs(30),
            manifest,
            config.alert_dir.clone(),
        )?;
        let embedder = Arc::new(RemoteEmbedder {
            client,
            tokenizer: tokenizer.clone(),
        });
        let counter = tokenizer.clone();
        SearchService::new(
            store,
            embedder,
            Arc::new(move |text: &str| counter.count(text)),
            manifest.clone(),
            config.silverbullet_url.clone(),
            root.clone(),
            Some(reader.clone()),
            owner,
        )
        .and_then(|service| service.with_blog(blog.clone()))
        .map(Arc::new)
        .map_err(|e| format!("search service: {e}"))
    };
    let public = build(false)?;
    public
        .store
        .check("")
        .await
        .map_err(|e| format!("database readiness: {e}"))?;
    let owner = match config.owner_token {
        Some(_) => {
            let owner = build(true)?;
            owner
                .store
                .check("")
                .await
                .map_err(|e| format!("database readiness: {e}"))?;
            Some(owner)
        }
        None => None,
    };
    Ok((public, owner))
}

/// Build the gate around rmcp's stateless JSON Streamable HTTP service.
pub fn gate(
    config: &McpConfig,
    public: Arc<dyn Retrieval>,
    owner: Option<Arc<dyn Retrieval>>,
    shutdown: CancellationToken,
) -> Result<Arc<Gate>, String> {
    build_gate(
        &config.token,
        config.owner_token.as_deref(),
        config.allowed_hosts.clone(),
        public,
        owner,
        shutdown,
    )
}

/// The MCP HTTP surface for any retrieval implementation (tests use mocks).
pub fn build_gate(
    token: &str,
    owner_token: Option<&str>,
    allowed_hosts: Vec<String>,
    public: Arc<dyn Retrieval>,
    owner: Option<Arc<dyn Retrieval>>,
    shutdown: CancellationToken,
) -> Result<Arc<Gate>, String> {
    let contract = Arc::new(Contract::embedded()?);
    let handler = NotesHandler {
        contract,
        public: public.clone(),
        owner: owner.clone(),
    };
    let http_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(gate::MAX_REQUEST_BODY)
        // Host and Origin are validated by the gate with the Python rules.
        .disable_allowed_hosts()
        .enforce_origin_validation()
        .with_cancellation_token(shutdown);
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        http_config,
    );
    Gate::new(token, owner_token, allowed_hosts, public, owner, service).map(Arc::new)
}

pub fn router(gate: Arc<Gate>) -> axum::Router {
    axum::Router::new().fallback(move |request: http::Request<axum::body::Body>| {
        let gate = gate.clone();
        async move { gate.handle(request).await }
    })
}

pub async fn run(env: &Env) -> Result<(), String> {
    let config = McpConfig::from_env(env)?;
    let manifest = ProducerManifest::embedded()?;
    let (public, owner) = services(&config, &manifest).await?;
    let shutdown = CancellationToken::new();
    let owner: Option<Arc<dyn Retrieval>> = owner.map(|o| o as Arc<dyn Retrieval>);
    let gate = gate(&config, public, owner, shutdown.clone())?;
    let app = router(gate);
    println!("Read-only notes MCP ready; HTTP bearer authentication required.");
    crate::serve::serve(config.listen, app, None, config.shutdown_grace, shutdown).await
}
