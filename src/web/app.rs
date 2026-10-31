//! Port of `public_search.create_app`: anonymous, budget-bounded search UI,
//! owner login, the semantic graph adapter and the read-only source viewer.
//! Never exposes MCP or LLM credentials.
//!
//! Each handler keeps the original decision order (traffic accounting before
//! cross-site checks, origin checks before body parsing, reservation before
//! any paid upstream call, session re-checks after slow upstream work) and
//! the original status codes and Korean messages.

use super::auth::{self, CallbackOutcome, OwnerAuth};
use super::gateway::Upstream;
use super::http::{self, cookie, header_str};
use super::state::State;
use crate::fault::{Fault, Result};
use crate::graph::visible_graph;
use crate::py::{json as pyjson, sha256_hex, text, url};
use crate::reader::Reader;
use crate::scope;
use crate::search::service::{SourceScope, scope_arguments};
use ::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use http_body_util::{BodyExt, Limited};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SEARCH_CACHE_VERSION: &str = "adjacent-context-v1";

const MSG_SEARCH_HERE: &str = "이 페이지에서 검색해 주세요.";
const MSG_INVALID: &str = "검색어(1~300자)와 폴더 선택을 확인해 주세요.";
const MSG_UNAVAILABLE: &str = "검색을 잠시 사용할 수 없습니다.";
const MSG_UPSTREAM: &str = "검색 서비스가 잠시 응답하지 않습니다. 문서가 없다는 뜻은 아닙니다. 잠시 후 다시 시도해 주세요.";
const MSG_SESSION_CHANGED: &str = "로그인 상태가 바뀌었습니다. 다시 검색해 주세요.";
const NOT_READABLE_PAGE: &str = "<!doctype html><html lang=\"ko\"><meta charset=\"utf-8\"><title>문서를 볼 수 없습니다</title><p>문서가 없거나 읽을 권한이 없습니다.</p><a href=\"/\">검색으로 돌아가기</a> · <a href=\"/auth/login\">본인 로그인</a></html>";

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Audience {
    Public,
    Owner,
}

impl Audience {
    pub fn name(self) -> &'static str {
        match self {
            Audience::Public => "public",
            Audience::Owner => "owner",
        }
    }
}

pub struct WebApp {
    pub state: Arc<State>,
    pub gateway: Arc<dyn Upstream>,
    pub owner_gateway: Option<Arc<dyn Upstream>>,
    pub access_revision: Option<String>,
    pub auth: Option<Arc<OwnerAuth>>,
    pub reader: Option<Arc<Reader>>,
    /// `https://notes.cola16.app/OneNote/markdown/` (citation allowlist)
    pub citation_prefix: String,
    /// `/OneNote/markdown/`
    pub note_prefix: String,
    pub cache_version: String,
    pub blog: Option<crate::blog::Source>,
    pub catalog_ttl: Duration,
    lock: tokio::sync::Mutex<()>,
    folder_lock: tokio::sync::Mutex<()>,
    catalogs: Mutex<HashMap<Audience, (Value, Instant)>>,
}

struct Selected {
    audience: Audience,
    gateway: Arc<dyn Upstream>,
    revision: Option<String>,
}

impl WebApp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: Arc<State>,
        gateway: Arc<dyn Upstream>,
        access_revision: Option<String>,
        auth: Option<Arc<OwnerAuth>>,
        owner_gateway: Option<Arc<dyn Upstream>>,
        reader: Option<Arc<Reader>>,
        citation_prefix: String,
        note_prefix: String,
    ) -> Result<Self> {
        if auth.is_some() != owner_gateway.is_some() {
            return Err(Fault::Value(
                "Owner login and gateway must be configured together",
            ));
        }
        if let Some(revision) = &access_revision {
            if revision.chars().count() != 64 {
                return Err(Fault::Value("Invalid access policy revision"));
            }
            state.activate_policy(revision)?;
        }
        Ok(Self {
            state,
            gateway,
            owner_gateway,
            access_revision,
            auth,
            reader,
            citation_prefix,
            note_prefix,
            cache_version: SEARCH_CACHE_VERSION.into(),
            blog: None,
            catalog_ttl: Duration::from_secs(60),
            lock: tokio::sync::Mutex::new(()),
            folder_lock: tokio::sync::Mutex::new(()),
            catalogs: Mutex::new(HashMap::new()),
        })
    }

    // ----- shared helpers -------------------------------------------------

    fn owner(&self, headers: &HeaderMap) -> Result<bool> {
        match &self.auth {
            None => Ok(false),
            Some(auth) => auth.owner(cookie(headers, auth::COOKIE).as_deref()),
        }
    }

    /// `context(request)` → (audience, gateway, revision).
    fn context(&self, headers: &HeaderMap) -> Result<Selected> {
        let owner = self.owner(headers)?;
        let revision = match (&self.access_revision, owner) {
            (Some(revision), true) => Some(crate::policy::owner_revision(revision)),
            (revision, _) => revision.clone(),
        };
        let (audience, gateway) = match (owner, &self.owner_gateway) {
            (true, Some(owner)) => (Audience::Owner, owner.clone()),
            _ => (Audience::Public, self.gateway.clone()),
        };
        Ok(Selected {
            audience,
            gateway,
            revision,
        })
    }

    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|_| Fault::Other("blocking task failed"))?
    }

    async fn traffic(&self, bucket: &'static str) -> Result<()> {
        let state = self.state.clone();
        self.blocking(move || state.traffic(bucket)).await
    }

    async fn visible(&self, paths: Vec<String>, owner: bool) -> Result<BTreeSet<String>> {
        let reader = self.reader.clone().ok_or(Fault::Other("no reader"))?;
        self.blocking(move || reader.visible_paths(&paths, owner))
            .await
    }

    /// Recheck both filesystem ACLs and live WordPress revisions on cache hits.
    async fn visible_result(
        &self,
        result: Value,
        audience: Audience,
        source: SourceScope,
        category: Option<i64>,
    ) -> Result<Value> {
        let rows = result
            .get("results")
            .and_then(Value::as_array)
            .ok_or(Fault::Other("malformed result"))?;
        let mut paths = HashMap::new();
        for row in rows
            .iter()
            .filter(|r| source != SourceScope::WordPress && !crate::blog::is_blog(r))
        {
            let link = row["url"]
                .as_str()
                .ok_or(Fault::Other("malformed result"))?;
            let path = url::unquote(
                &url::urlsplit(link)
                    .ok_or(Fault::Value("Invalid IPv6 URL"))?
                    .path,
            );
            let relative = path
                .strip_prefix(self.note_prefix.as_str())
                .unwrap_or(&path);
            paths.insert(link.to_string(), format!("{relative}.md"));
        }
        let visible = if self.reader.is_some() && source != SourceScope::WordPress {
            Some(
                self.visible(
                    paths.values().cloned().collect(),
                    audience == Audience::Owner,
                )
                .await?,
            )
        } else {
            None
        };
        let mut failed = false;
        let catalog = if source != SourceScope::Notes && rows.iter().any(crate::blog::is_blog) {
            match &self.blog {
                Some(source) => match source.catalog().await {
                    Ok(c) => c,
                    Err(_) => {
                        failed = true;
                        Default::default()
                    }
                },
                None => {
                    failed = true;
                    Default::default()
                }
            }
        } else {
            Default::default()
        };
        let kept: Vec<Value> = rows
            .iter()
            .filter(|row| {
                if crate::blog::is_blog(row) {
                    return source != SourceScope::Notes
                        && crate::blog::visible(row, &catalog)
                        && row["post_id"]
                            .as_i64()
                            .and_then(|id| catalog.get(&id))
                            .is_some_and(|p| crate::blog::in_category(p, category));
                }
                if source == SourceScope::WordPress {
                    return false;
                }
                visible.as_ref().is_none_or(|set| {
                    row["url"]
                        .as_str()
                        .and_then(|u| paths.get(u))
                        .is_some_and(|p| set.contains(p))
                })
            })
            .map(|row| {
                let mut row = row.clone();
                if crate::blog::is_blog(&row)
                    && let Some(post) = row["post_id"].as_i64().and_then(|id| catalog.get(&id))
                {
                    row["category_path"] =
                        serde_json::to_value(&post.category_path).expect("public path");
                }
                row
            })
            .collect();
        let mut result = result.as_object().cloned().unwrap_or_default();
        result.insert("results".into(), kept.into());
        if failed {
            result.insert("blog_unavailable".into(), true.into());
        }
        Ok(Value::Object(result))
    }

    /// `get_catalog(audience, gateway, revision)`: 60 s per-audience cache.
    async fn catalog(&self, selected: &Selected) -> Result<Value> {
        let fresh = |catalogs: &Mutex<HashMap<Audience, (Value, Instant)>>| {
            catalogs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&selected.audience)
                .filter(|(_, until)| Instant::now() < *until)
                .map(|(catalog, _)| catalog.clone())
        };
        if let Some(catalog) = fresh(&self.catalogs) {
            return Ok(catalog);
        }
        let _guard = self.folder_lock.lock().await;
        if let Some(catalog) = fresh(&self.catalogs) {
            return Ok(catalog);
        }
        let upstream = tokio::time::timeout(Duration::from_secs(10), selected.gateway.folders())
            .await
            .map_err(|_| Fault::Os)??;
        let upstream = upstream
            .as_object()
            .ok_or(Fault::Other("malformed catalog"))?;
        if let Some(revision) = &selected.revision
            && upstream.get("access_revision").and_then(Value::as_str) != Some(revision.as_str())
        {
            return Err(Fault::Value("Access policy mismatch"));
        }
        let mut catalog = public_folders(upstream)?;
        catalog.insert("audience".into(), selected.audience.name().into());
        let catalog = Value::Object(catalog);
        self.catalogs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                selected.audience,
                (catalog.clone(), Instant::now() + self.catalog_ttl),
            );
        Ok(catalog)
    }

    fn still(&self, headers: &HeaderMap, audience: Audience) -> bool {
        self.context(headers)
            .map(|s| s.audience == audience)
            .unwrap_or(false)
    }

    // ----- /api/folders, /api/graph --------------------------------------

    async fn folders(&self, request: Request) -> Response {
        let headers = request.headers().clone();
        let outcome = async {
            let selected = self.context(&headers)?;
            self.traffic(selected.audience.name()).await?;
            self.catalog(&selected).await
        }
        .await;
        match outcome {
            Ok(catalog) => http::json(StatusCode::OK, &catalog),
            Err(Fault::Denied { message, retry }) => {
                http::error(StatusCode::TOO_MANY_REQUESTS, message, Some(retry))
            }
            Err(_) => http::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "폴더 목록을 불러오지 못했습니다. 잠시 후 다시 시도해 주세요.",
                None,
            ),
        }
    }

    async fn graph_data(&self, request: Request) -> Response {
        let headers = request.headers().clone();
        let outcome: Result<Response> = async {
            let selected = self.context(&headers)?;
            self.traffic(selected.audience.name()).await?;
            if header_str(&headers, "sec-fetch-site") == Some("cross-site") {
                return Ok(http::error(
                    StatusCode::FORBIDDEN,
                    "이 페이지에서 열어 주세요.",
                    None,
                ));
            }
            let mut result =
                tokio::time::timeout(Duration::from_secs(36), selected.gateway.graph())
                    .await
                    .map_err(|_| Fault::Os)??;
            if let Some(revision) = &selected.revision
                && result
                    .get("index")
                    .and_then(|i| i.get("access_revision"))
                    .and_then(Value::as_str)
                    != Some(revision.as_str())
            {
                return Err(Fault::Value("Access policy mismatch"));
            }
            if self.reader.is_some() {
                let paths: Vec<String> = result
                    .get("nodes")
                    .and_then(Value::as_array)
                    .ok_or(Fault::Other("malformed graph"))?
                    .iter()
                    .map(|node| {
                        node.get("path")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .ok_or(Fault::Other("malformed graph"))
                    })
                    .collect::<Result<_>>()?;
                let visible = self
                    .visible(paths, selected.audience == Audience::Owner)
                    .await?;
                result = visible_graph(&result, &visible)?;
            }
            // Re-check the session after a potentially slow build: a revoked or
            // expired owner must not receive an owner graph started before logout.
            if !self.still(&headers, selected.audience) {
                return Ok(http::error(
                    StatusCode::CONFLICT,
                    "로그인 상태가 바뀌었습니다. 다시 열어 주세요.",
                    None,
                ));
            }
            let mut result = result
                .as_object()
                .cloned()
                .ok_or(Fault::Other("malformed graph"))?;
            result.insert("audience".into(), selected.audience.name().into());
            Ok(http::json(StatusCode::OK, &Value::Object(result)))
        }
        .await;
        match outcome {
            Ok(response) => response,
            Err(Fault::Denied { message, retry }) => {
                http::error(StatusCode::TOO_MANY_REQUESTS, message, Some(retry))
            }
            Err(_) => http::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "지도를 준비하지 못했습니다. 잠시 후 다시 시도해 주세요.",
                None,
            ),
        }
    }

    // ----- /api/search ----------------------------------------------------

    async fn search(&self, request: Request) -> Response {
        match self.search_inner(request).await {
            Ok(response) => response,
            Err(Fault::Denied { message, retry }) => {
                http::error(StatusCode::TOO_MANY_REQUESTS, message, Some(retry))
            }
            Err(Fault::Value(_)) => http::error(StatusCode::BAD_REQUEST, MSG_INVALID, None),
            // Counter failures fail closed; never call upstream without reservation.
            Err(_) => http::error(StatusCode::SERVICE_UNAVAILABLE, MSG_UNAVAILABLE, None),
        }
    }

    async fn search_inner(&self, request: Request) -> Result<Response> {
        let (parts, body) = request.into_parts();
        let headers = parts.headers;
        let selected = self.context(&headers)?;
        let audience = selected.audience;
        self.traffic(audience.name()).await?;
        if audience == Audience::Owner
            && header_str(&headers, "origin") != self.auth.as_ref().map(|a| a.origin.as_str())
        {
            return Ok(http::error(StatusCode::FORBIDDEN, MSG_SEARCH_HERE, None));
        }
        if header_str(&headers, "sec-fetch-site") == Some("cross-site") {
            return Ok(http::error(StatusCode::FORBIDDEN, MSG_SEARCH_HERE, None));
        }
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .map(|v| v.as_bytes())
            .unwrap_or(b"");
        if content_type.split(|b| *b == b';').next() != Some(b"application/json".as_slice()) {
            return Ok(http::error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "JSON 요청이 필요합니다.",
                None,
            ));
        }
        let Ok(collected) = Limited::new(body, 8192).collect().await else {
            return Ok(http::error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "검색어가 너무 깁니다.",
                None,
            ));
        };
        let raw = collected.to_bytes();
        let raw = raw.strip_prefix(b"\xef\xbb\xbf".as_slice()).unwrap_or(&raw);
        let data: Value = serde_json::from_slice(raw).map_err(|_| Fault::Value("invalid JSON"))?;
        let data = data.as_object().ok_or(Fault::Value("Invalid arguments"))?;
        let allowed: HashSet<&str> = [
            "query",
            "include_history",
            "folder",
            "source",
            "blog_category",
        ]
        .into();
        if data.keys().any(|k| !allowed.contains(k.as_str())) {
            return Err(Fault::Value("Invalid arguments"));
        }
        let history = data
            .get("include_history")
            .cloned()
            .unwrap_or(Value::Bool(true));
        let folder = scope::validate_folder_value(
            data.get("folder").unwrap_or(&Value::String(String::new())),
        )?
        .to_string();
        let query = match data.get("query") {
            Some(Value::String(q)) if (1..=300).contains(&text::len(text::strip(q))) => q.clone(),
            _ => return Err(Fault::Value("Invalid query")),
        };
        let Value::Bool(history) = history else {
            return Err(Fault::Value("Invalid query"));
        };
        let query = text::normalize_whitespace(&query);
        let (source, category) = scope_arguments(data, &folder)?;
        if let Some(category) = category {
            let listing = self.catalog(&selected).await?;
            if listing.get("blog_unavailable").and_then(Value::as_bool) == Some(true) {
                return Ok(http::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "블로그 분류를 확인하지 못했습니다. 잠시 후 다시 시도해 주세요.",
                    None,
                ));
            }
            if !listing
                .get("blog_categories")
                .and_then(Value::as_array)
                .is_some_and(|rows| rows.iter().any(|row| row["id"].as_i64() == Some(category)))
            {
                return Ok(http::error(
                    StatusCode::BAD_REQUEST,
                    "검색할 수 없는 블로그 분류입니다. 목록을 새로고침해 주세요.",
                    None,
                ));
            }
        }
        if !folder.is_empty() {
            let listing = self.catalog(&selected).await?;
            let known = listing["folders"]
                .as_array()
                .is_some_and(|f| f.iter().any(|item| item["path"] == folder.as_str()));
            if !known {
                return Ok(http::error(
                    StatusCode::BAD_REQUEST,
                    "검색할 수 없는 폴더입니다. 폴더 목록을 새로고침해 주세요.",
                    None,
                ));
            }
        }
        let blog_revision =
            if self.blog.is_some() && folder.is_empty() && source != SourceScope::Notes {
                match tokio::time::timeout(Duration::from_secs(2), self.catalog(&selected)).await {
                    Ok(Ok(catalog)) => catalog.get("blog_index_revision").cloned(),
                    _ => self
                        .catalogs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&audience)
                        .and_then(|(catalog, _)| catalog.get("blog_index_revision").cloned()),
                }
            } else {
                None
            };
        let key_material = json!([
            self.cache_version,
            audience.name(),
            selected.revision,
            query,
            history,
            folder,
        ]);
        let key_material = if self.blog.is_some() {
            json!([
                key_material,
                blog_revision,
                "wordpress-scopes-v2",
                source.as_str(),
                category
            ])
        } else if source != SourceScope::All || category.is_some() {
            json!([key_material, source.as_str(), category])
        } else {
            key_material
        };
        let key = sha256_hex(pyjson::dumps_unicode(&key_material));
        let state = self.state.clone();
        let lookup = key.clone();
        if let Some(cached) = self.blocking(move || state.cached(&lookup)).await? {
            let cached = self
                .visible_result(cached, audience, source, category)
                .await?;
            if !self.still(&headers, audience) {
                return Ok(http::error(StatusCode::CONFLICT, MSG_SESSION_CHANGED, None));
            }
            return Ok(http::json(StatusCode::OK, &with_flag(cached, true)));
        }
        // A single upstream request at a time: storms cannot amplify cost.
        let Ok(_guard) = self.lock.try_lock() else {
            return Err(Fault::Denied {
                message: "다른 검색을 처리 중입니다. 잠시 후 다시 검색해 주세요.",
                retry: 3,
            });
        };
        let state = self.state.clone();
        self.blocking(move || state.reserve()).await?;
        let fresh: Result<Value> = async {
            let result = tokio::time::timeout(
                Duration::from_secs(45),
                selected.gateway.search_scoped(
                    query.clone(),
                    history,
                    folder.clone(),
                    source,
                    category,
                ),
            )
            .await
            .map_err(|_| Fault::Os)??;
            if let Some(revision) = &selected.revision
                && result
                    .get("index")
                    .and_then(|i| i.get("access_revision"))
                    .and_then(Value::as_str)
                    != Some(revision.as_str())
            {
                return Err(Fault::Value("Access policy mismatch"));
            }
            let mut result =
                public_result(&result, &folder, &self.citation_prefix, &self.note_prefix)?;
            result.insert("audience".into(), audience.name().into());
            self.visible_result(Value::Object(result), audience, source, category)
                .await
        }
        .await;
        let Ok(result) = fresh else {
            // Failures consume budget; no automatic upstream retries.
            return Ok(http::error(
                StatusCode::SERVICE_UNAVAILABLE,
                MSG_UPSTREAM,
                None,
            ));
        };
        if !self.still(&headers, audience) {
            return Ok(http::error(StatusCode::CONFLICT, MSG_SESSION_CHANGED, None));
        }
        let state = self.state.clone();
        let saved = result.clone();
        let stable_blog_epoch = self.blog.is_none()
            || source == SourceScope::Notes
            || !folder.is_empty()
            || blog_revision
                .as_ref()
                .and_then(Value::as_str)
                .is_some_and(crate::blog::valid_revision);
        if result.get("blog_unavailable").and_then(Value::as_bool) != Some(true)
            && stable_blog_epoch
        {
            self.blocking(move || state.save(&key, &saved)).await?;
        }
        Ok(http::json(StatusCode::OK, &with_flag(result, false)))
    }

    // ----- reader -------------------------------------------------------

    async fn read_blog(&self, request: Request) -> Response {
        let headers = request.headers().clone();
        let query = request.uri().query().unwrap_or("").as_bytes().to_vec();
        drop(request);
        let outcome = async {
            let owner = self.owner(&headers)?;
            self.traffic(if owner { "owner" } else { "public" }).await?;
            let id = url::query_param(&query, "id")
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or(Fault::NotReadable)?;
            self.blog.as_ref().ok_or(Fault::NotReadable)?.page(id).await
        }
        .await;
        match outcome {
            Ok(page) => http::html(StatusCode::OK, page),
            Err(Fault::NotReadable) => http::html(StatusCode::NOT_FOUND, NOT_READABLE_PAGE),
            Err(Fault::Denied { message, .. }) => {
                http::text(StatusCode::TOO_MANY_REQUESTS, message)
            }
            Err(_) => http::text(
                StatusCode::SERVICE_UNAVAILABLE,
                "블로그 미리보기를 잠시 사용할 수 없습니다.",
            ),
        }
    }

    async fn read_document(&self, request: Request) -> Response {
        let headers = request.headers().clone();
        let query = request.uri().query().unwrap_or("").as_bytes().to_vec();
        let outcome: Result<String> = async {
            let reader = self.reader.clone().ok_or(Fault::NotReadable)?;
            let owner = self.owner(&headers)?;
            self.traffic(if owner { "owner" } else { "public" }).await?;
            let path = url::query_param(&query, "path").unwrap_or_default();
            self.blocking(move || reader.document(&path, owner)).await
        }
        .await;
        match outcome {
            Ok(page) => http::html(StatusCode::OK, page),
            Err(Fault::Denied { message, .. }) => {
                http::text(StatusCode::TOO_MANY_REQUESTS, message)
            }
            Err(fault) if fault.is_reader_not_found() => {
                http::html(StatusCode::NOT_FOUND, NOT_READABLE_PAGE)
            }
            Err(_) => http::text(
                StatusCode::SERVICE_UNAVAILABLE,
                "원문 보기를 잠시 사용할 수 없습니다.",
            ),
        }
    }

    async fn read_asset(&self, request: Request) -> Response {
        let range = header_str(request.headers(), "range").map(str::to_string);
        let headers = request.headers().clone();
        let query = request.uri().query().unwrap_or("").as_bytes().to_vec();
        let outcome = async {
            let reader = self.reader.clone().ok_or(Fault::NotReadable)?;
            let owner = self.owner(&headers)?;
            self.traffic(if owner {
                "owner-assets"
            } else {
                "public-assets"
            })
            .await?;
            let path = url::query_param(&query, "path").unwrap_or_default();
            let asset = url::query_param(&query, "asset").unwrap_or_default();
            self.blocking(move || reader.asset(&path, &asset, owner))
                .await
        }
        .await;
        match outcome {
            // The verified fd is streamed directly: never reopen an attacker-swappable path.
            Ok((file, media, filename)) => {
                let kind = if media.starts_with("image/") {
                    "inline"
                } else {
                    "attachment"
                };
                http::file_response(
                    file,
                    media,
                    http::content_disposition(kind, &filename),
                    range.as_deref(),
                )
            }
            Err(Fault::Denied { message, retry }) => {
                let mut response = http::text(StatusCode::TOO_MANY_REQUESTS, message);
                if let Ok(value) = HeaderValue::from_str(&retry.to_string()) {
                    response.headers_mut().insert(header::RETRY_AFTER, value);
                }
                response
            }
            Err(fault) if fault.is_reader_not_found() => http::text(
                StatusCode::NOT_FOUND,
                "첨부파일이 없거나 읽을 권한이 없습니다.",
            ),
            Err(_) => http::text(
                StatusCode::SERVICE_UNAVAILABLE,
                "첨부파일을 잠시 사용할 수 없습니다.",
            ),
        }
    }

    // ----- session and auth ------------------------------------------------

    fn session(&self, request: &Request) -> Response {
        match self.owner(request.headers()) {
            Ok(owner) => http::json(
                StatusCode::OK,
                &json!({"login_available": self.auth.is_some(), "owner": owner}),
            ),
            Err(_) => http::text(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
        }
    }

    async fn login(&self, auth: &OwnerAuth, request: Request) -> Response {
        let had_flow = cookie(request.headers(), auth::FLOW_COOKIE).is_some();
        let next = url::query_param(request.uri().query().unwrap_or("").as_bytes(), "next");
        drop(request);
        if let Err(fault) = self.traffic("auth-login").await {
            let mut response = match fault {
                Fault::Denied { message, retry } => {
                    http::error(StatusCode::TOO_MANY_REQUESTS, message, Some(retry))
                }
                _ => http::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "로그인 서비스를 연결하지 못했습니다. 잠시 후 다시 시도해 주세요.",
                    None,
                ),
            };
            if had_flow && !matches!(response.status(), StatusCode::TOO_MANY_REQUESTS) {
                append_cookie(&mut response, &auth::clear_flow_cookie());
            }
            return response;
        }
        let return_to = if next.as_deref() == Some("/graph") {
            "/graph"
        } else {
            "/"
        };
        match auth.authorization(return_to).await {
            Ok((location, flow)) => {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::FOUND;
                if let Ok(value) = HeaderValue::from_str(&location) {
                    response.headers_mut().insert(header::LOCATION, value);
                }
                append_cookie(&mut response, &auth::flow_cookie(&auth.encode_flow(&flow)));
                response
            }
            Err(_) => {
                let mut response = http::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "로그인 서비스를 연결하지 못했습니다. 잠시 후 다시 시도해 주세요.",
                    None,
                );
                if had_flow {
                    append_cookie(&mut response, &auth::clear_flow_cookie());
                }
                response
            }
        }
    }

    async fn callback(&self, auth: &OwnerAuth, request: Request) -> Response {
        let headers = request.headers().clone();
        let headers = &headers;
        let raw_flow = cookie(headers, auth::FLOW_COOKIE);
        let flow = raw_flow
            .as_deref()
            .and_then(|value| auth.decode_flow(value));
        let current = cookie(headers, auth::COOKIE);
        let query = request.uri().query().unwrap_or("").as_bytes().to_vec();
        drop(request);
        let state = url::query_param(&query, "state");
        let code = url::query_param(&query, "code");
        let error = url::query_param(&query, "error").is_some_and(|e| !e.is_empty());
        let outcome = auth
            .callback(flow, state.as_deref(), code.as_deref(), error)
            .await;
        let mut response = match outcome {
            CallbackOutcome::Owner { location, token } => {
                let _ = auth.revoke(current.as_deref());
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::SEE_OTHER;
                response
                    .headers_mut()
                    .insert(header::LOCATION, HeaderValue::from_static(location));
                append_cookie(&mut response, &auth::owner_cookie(&token));
                response
            }
            CallbackOutcome::Denied(Fault::Denied { message, retry }) => {
                http::error(StatusCode::TOO_MANY_REQUESTS, message, Some(retry))
            }
            CallbackOutcome::Denied(_) | CallbackOutcome::Failed => {
                let _ = auth.revoke(current.as_deref());
                let mut response = http::error(
                    StatusCode::FORBIDDEN,
                    "로그인을 완료하지 못했거나 본인 전용 계정이 아닙니다. 공개 검색은 이용할 수 있습니다.",
                    None,
                );
                append_cookie(&mut response, &auth::delete_owner_cookie());
                response
            }
        };
        // `finally: request.session.clear()`
        if raw_flow.is_some() {
            append_cookie(&mut response, &auth::clear_flow_cookie());
        }
        response
    }

    fn logout(&self, auth: &OwnerAuth, request: &Request) -> Response {
        let headers = request.headers();
        if header_str(headers, "origin") != Some(auth.origin.as_str()) {
            return http::error(
                StatusCode::FORBIDDEN,
                "이 페이지에서 로그아웃해 주세요.",
                None,
            );
        }
        if auth
            .revoke(cookie(headers, auth::COOKIE).as_deref())
            .is_err()
        {
            return http::text(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        }
        let mut response = http::json(StatusCode::OK, &json!({"owner": false}));
        if cookie(headers, auth::FLOW_COOKIE).is_some() {
            append_cookie(&mut response, &auth::clear_flow_cookie());
        }
        append_cookie(&mut response, &auth::delete_owner_cookie());
        response
    }

    // ----- routing ------------------------------------------------------

    /// Starlette `Router`: exact paths, GET routes also answer HEAD.
    pub async fn route(self: Arc<Self>, request: Request) -> Response {
        let path = request.uri().path().to_string();
        let method = request.method().clone();
        let get = method == Method::GET || method == Method::HEAD;
        let auth = self.auth.clone();
        let response = match path.as_str() {
            "/api/search" if method == Method::POST => self.search(request).await,
            "/api/search" => http::method_not_allowed("POST"),
            "/auth/logout" if auth.is_some() && method == Method::POST => {
                self.logout(auth.as_deref().unwrap(), &request)
            }
            "/auth/logout" if auth.is_some() => http::method_not_allowed("POST"),
            "/auth/login" | "/auth/callback" if auth.is_some() && !get => {
                http::method_not_allowed("GET, HEAD")
            }
            "/auth/login" if auth.is_some() => self.login(auth.as_deref().unwrap(), request).await,
            "/auth/callback" if auth.is_some() => {
                self.callback(auth.as_deref().unwrap(), request).await
            }
            _ if http::route_asset(&path).is_some() || is_api_get(&path) => {
                if !get {
                    http::method_not_allowed("GET, HEAD")
                } else if let Some(asset) = http::route_asset(&path) {
                    http::asset_response(asset)
                } else {
                    match path.as_str() {
                        "/api/graph" => self.graph_data(request).await,
                        "/api/folders" => self.folders(request).await,
                        "/api/session" => self.session(&request),
                        "/read-blog" => self.read_blog(request).await,
                        "/read" => self.read_document(request).await,
                        "/read-asset" => self.read_asset(request).await,
                        "/healthz" => http::json(StatusCode::OK, &json!({"status": "ok"})),
                        "/robots.txt" => http::text(StatusCode::OK, "User-agent: *\nDisallow: /\n"),
                        _ => http::not_found(),
                    }
                }
            }
            _ => http::not_found(),
        };
        if method == Method::HEAD {
            let (parts, _) = response.into_parts();
            return Response::from_parts(parts, Body::empty());
        }
        response
    }
}

fn is_api_get(path: &str) -> bool {
    matches!(
        path,
        "/api/graph"
            | "/api/folders"
            | "/api/session"
            | "/read"
            | "/read-blog"
            | "/read-asset"
            | "/healthz"
            | "/robots.txt"
    )
}

fn with_flag(value: Value, cached: bool) -> Value {
    let mut map = value.as_object().cloned().unwrap_or_default();
    map.insert("cached".into(), cached.into());
    Value::Object(map)
}

fn append_cookie(response: &mut Response, cookie: &str) {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

/// `public_result(result, folder)`: allowlist result fields; never echo
/// arbitrary upstream errors or metadata.
pub fn public_result(
    result: &Value,
    folder: &str,
    citation_prefix: &str,
    note_prefix: &str,
) -> Result<Map<String, Value>> {
    let rows = result
        .get("results")
        .and_then(Value::as_array)
        .ok_or(Fault::Value("Invalid search response"))?;
    let mut safe = Vec::new();
    for row in rows.iter().take(5) {
        let row = row.as_object().ok_or(Fault::Other("malformed row"))?;
        let link = row
            .get("source_url")
            .ok_or(Fault::Other("missing source_url"))?;
        let blog = row.get("source_kind").and_then(Value::as_str) == Some("wordpress");
        let Some(link) = link.as_str().filter(|u| {
            if blog {
                crate::blog::valid_url(u)
            } else {
                u.starts_with(citation_prefix)
            }
        }) else {
            return Err(Fault::Value("Invalid citation URL"));
        };
        if blog
            && (!folder.is_empty()
                || !row
                    .get("post_id")
                    .and_then(Value::as_i64)
                    .is_some_and(|id| (1..10_000_000_000).contains(&id))
                || !row
                    .get("source_revision")
                    .and_then(Value::as_str)
                    .is_some_and(crate::blog::valid_revision))
        {
            return Err(Fault::Value("Invalid blog citation"));
        }
        if !folder.is_empty() {
            let path = url::unquote(
                &url::urlsplit(link)
                    .ok_or(Fault::Value("Invalid IPv6 URL"))?
                    .path,
            );
            if !path.starts_with(&format!("{note_prefix}{folder}/")) {
                return Err(Fault::Value("Citation outside requested folder"));
            }
        }
        let title = pyjson::to_str(row.get("title").ok_or(Fault::Other("missing title"))?);
        let content = pyjson::to_str(row.get("content").ok_or(Fault::Other("missing content"))?);
        let mut item = Map::new();
        item.insert("title".into(), text::prefix(&title, 400).into());
        let display = if blog {
            crate::blog_excerpt::excerpt(&content)
        } else {
            content
        };
        item.insert("excerpt".into(), text::prefix(&display, 6000).into());
        item.insert("url".into(), link.into());
        if blog {
            for key in [
                "source_kind",
                "post_id",
                "source_revision",
                "published",
                "modified",
            ] {
                if let Some(value) = row.get(key) {
                    item.insert(key.into(), value.clone());
                }
            }
            let path: Vec<crate::blog::Category> = serde_json::from_value(
                row.get("category_path")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            )
            .map_err(|_| Fault::Value("Invalid blog category path"))?;
            if !crate::blog::valid_category_path(&path) {
                return Err(Fault::Value("Invalid blog category path"));
            }
            item.insert(
                "category_path".into(),
                serde_json::to_value(path).expect("public category path"),
            );
        }
        item.insert(
            "history".into(),
            (row.get("rag_priority").and_then(Value::as_str) == Some("fallback")).into(),
        );
        item.insert(
            "visual_review".into(),
            row.get("needs_visual_review")
                .is_some_and(pyjson::truthy)
                .into(),
        );
        item.insert(
            "truncated".into(),
            row.get("truncated").is_some_and(pyjson::truthy).into(),
        );
        safe.push(Value::Object(item));
    }
    let empty = Map::new();
    let info = match result.get("index") {
        None => &empty,
        Some(Value::Object(info)) => info,
        Some(_) => return Err(Fault::Other("malformed index")),
    };
    let int = |value: Option<&Value>| match value {
        None => Ok(0),
        Some(value) => pyjson::to_int(value).ok_or(Fault::Value("invalid int")),
    };
    let indexed = int(info.get("indexed_pages"))?;
    let scoped = int(info.get("scoped_pages").or(info.get("indexed_pages")))?;
    let mut out = Map::new();
    out.insert("results".into(), safe.into());
    out.insert("indexed_pages".into(), indexed.into());
    out.insert("scoped_pages".into(), scoped.into());
    out.insert("folder".into(), folder.into());
    if result.get("blog_unavailable").and_then(Value::as_bool) == Some(true) {
        out.insert("blog_unavailable".into(), true.into());
    }
    Ok(out)
}

/// `public_folders(result)`
pub fn public_folders(result: &Map<String, Value>) -> Result<Map<String, Value>> {
    let rows = match result.get("folders").and_then(Value::as_array) {
        Some(rows) if rows.len() <= 10000 => rows,
        _ => return Err(Fault::Value("Invalid folder catalog")),
    };
    let mut safe = Vec::new();
    let mut seen = HashSet::new();
    for row in rows {
        let row = row
            .as_object()
            .ok_or(Fault::Other("malformed folder row"))?;
        let path =
            scope::validate_folder_value(row.get("path").ok_or(Fault::Other("missing path"))?)?
                .to_string();
        let pages = row.get("pages").ok_or(Fault::Other("missing pages"))?;
        let valid_pages = pyjson::is_int(pages) && pages.as_i64().is_some_and(|p| p >= 1);
        if path.is_empty() || seen.contains(&path) || !valid_pages {
            return Err(Fault::Value("Invalid folder entry"));
        }
        seen.insert(path.clone());
        let (parent, _, name) = text::rpartition(&path, "/");
        safe.push(json!({"path": path, "parent": parent, "name": name, "pages": pages}));
    }
    if safe.iter().any(|row| {
        row["parent"]
            .as_str()
            .is_some_and(|p| !p.is_empty() && !seen.contains(p))
    }) {
        return Err(Fault::Value("Incomplete folder hierarchy"));
    }
    let indexed = result
        .get("indexed_pages")
        .ok_or(Fault::Other("missing indexed_pages"))
        .and_then(|v| pyjson::to_int(v).ok_or(Fault::Value("invalid int")))?;
    let mut out = Map::new();
    out.insert("folders".into(), safe.into());
    out.insert("indexed_pages".into(), indexed.into());
    if let Some(revision) = result.get("blog_index_revision").and_then(Value::as_str)
        && (crate::blog::valid_revision(revision) || matches!(revision, "empty" | "unavailable"))
    {
        out.insert("blog_index_revision".into(), revision.into());
    }
    if result.contains_key("blog_posts") || result.contains_key("blog_categories") {
        let validated = (|| -> Result<(i64, Vec<Value>)> {
            let posts = result
                .get("blog_posts")
                .and_then(Value::as_i64)
                .filter(|n| (0..=10000).contains(n))
                .ok_or(Fault::Value("Invalid blog count"))?;
            let rows = result
                .get("blog_categories")
                .and_then(Value::as_array)
                .filter(|r| r.len() <= 10000)
                .ok_or(Fault::Value("Invalid blog categories"))?;
            let mut categories = BTreeMap::new();
            for row in rows {
                let id = row["id"]
                    .as_i64()
                    .filter(|id| *id > 0)
                    .ok_or(Fault::Value("Invalid blog category"))?;
                let parent = match row.get("parent") {
                    Some(Value::Null) => None,
                    Some(v) => Some(
                        v.as_i64()
                            .filter(|p| *p > 0 && *p != id)
                            .ok_or(Fault::Value("Invalid category parent"))?,
                    ),
                    None => return Err(Fault::Value("Missing category parent")),
                };
                let name = row["name"]
                    .as_str()
                    .filter(|s| {
                        !s.trim().is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control)
                    })
                    .ok_or(Fault::Value("Invalid category name"))?;
                let count = row["posts"]
                    .as_i64()
                    .filter(|n| (1..=posts).contains(n))
                    .ok_or(Fault::Value("Invalid category count"))?;
                if categories
                    .insert(
                        id,
                        json!({"id":id,"parent":parent,"name":name,"posts":count}),
                    )
                    .is_some()
                {
                    return Err(Fault::Value("Duplicate category"));
                }
            }
            for (&id, row) in &categories {
                let mut seen = BTreeSet::from([id]);
                let mut next = row["parent"].as_i64();
                let mut count = row["posts"].as_i64().unwrap_or(0);
                while let Some(parent) = next {
                    let node = categories
                        .get(&parent)
                        .ok_or(Fault::Value("Orphan category"))?;
                    if !seen.insert(parent)
                        || seen.len() > 32
                        || node["posts"].as_i64().unwrap_or(0) < count
                    {
                        return Err(Fault::Value("Invalid category hierarchy"));
                    }
                    count = node["posts"].as_i64().unwrap_or(0);
                    next = node["parent"].as_i64();
                }
            }
            Ok((posts, categories.into_values().collect::<Vec<_>>()))
        })();
        match validated {
            Ok((posts, categories)) => {
                out.insert("blog_posts".into(), posts.into());
                out.insert("blog_categories".into(), categories.into());
            }
            Err(_) => {
                // Blog metadata cannot make the independent note folder catalog fail.
                out.insert("blog_posts".into(), 0.into());
                out.insert("blog_categories".into(), json!([]));
                out.insert("blog_unavailable".into(), true.into());
            }
        }
    }
    if result.get("blog_unavailable").and_then(Value::as_bool) == Some(true) {
        out.insert("blog_unavailable".into(), true.into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "https://notes.cola16.app/OneNote/markdown/";
    const NOTE: &str = "/OneNote/markdown/";

    #[test]
    fn unsafe_source_rejected() {
        let result = json!({"results": [{"title": "x", "content": "x", "source_url": "javascript:alert(1)"}]});
        assert!(matches!(
            public_result(&result, "", PREFIX, NOTE),
            Err(Fault::Value(_))
        ));
    }

    #[test]
    fn scoped_response_rejects_sibling_prefix() {
        let result = json!({"results": [{"title": "t", "content": "c", "source_url": format!("{PREFIX}example2/a")}]});
        assert!(public_result(&result, "example", PREFIX, NOTE).is_err());
        let result = json!({"results": [{"title": "t", "content": "c", "source_url": format!("{PREFIX}example/a")}]});
        assert!(public_result(&result, "example", PREFIX, NOTE).is_ok());
    }

    #[test]
    fn public_result_allowlists_fields() {
        let result = json!({"index": {"indexed_pages": 2178}, "results": [{"title": "<script>test</script>",
            "content": "synthetic excerpt", "source_url": format!("{PREFIX}example"), "rag_priority": "normal",
            "page_key": "secret"}]});
        let safe = public_result(&result, "", PREFIX, NOTE).unwrap();
        assert_eq!(
            Value::Object(safe),
            json!({"results": [{"title": "<script>test</script>", "excerpt": "synthetic excerpt",
                   "url": format!("{PREFIX}example"), "history": false, "visual_review": false, "truncated": false}],
                   "indexed_pages": 2178, "scoped_pages": 2178, "folder": ""})
        );
    }

    #[test]
    fn catalog_rejects_orphan_duplicate_and_invalid_count() {
        for folders in [
            json!([{"path": "a/b", "pages": 1}]),
            json!([{"path": "a", "pages": true}]),
            json!([{"path": "a", "pages": 1}, {"path": "a", "pages": 1}]),
        ] {
            let input = json!({"folders": folders, "indexed_pages": 1});
            assert!(
                matches!(
                    public_folders(input.as_object().unwrap()),
                    Err(Fault::Value(_))
                ),
                "{input}"
            );
        }
    }
    #[test]
    fn category_catalog_rejects_cycles_orphans_duplicates_and_bad_counts() {
        let root = json!({"id":1,"parent":null,"name":"Root","posts":2});
        let leaf = json!({"id":2,"parent":1,"name":"Leaf","posts":1});
        let valid = json!({"folders":[],"indexed_pages":0,"blog_posts":2,"blog_categories":[root.clone(),leaf.clone()]});
        assert!(public_folders(valid.as_object().unwrap()).is_ok());
        for categories in [
            json!([root.clone(), root.clone()]),
            json!([leaf.clone()]),
            json!([{"id":1,"parent":2,"name":"A","posts":1},{"id":2,"parent":1,"name":"B","posts":1}]),
            json!([{"id":1,"parent":null,"name":"A","posts":3}]),
            json!([{"id":true,"parent":null,"name":"A","posts":1}]),
        ] {
            let data =
                json!({"folders":[],"indexed_pages":0,"blog_posts":2,"blog_categories":categories});
            let safe = public_folders(data.as_object().unwrap()).unwrap();
            assert_eq!(safe["blog_unavailable"], true, "{data}");
            assert_eq!(safe["folders"], json!([]));
        }
    }
    #[test]
    fn only_blog_display_excerpt_is_projected_and_evidence_stays_raw() {
        let mut row = json!({"title":"Synthetic","source_kind":"wordpress","post_id":1,"source_revision":"a".repeat(64),"source_url":"https://cola16.app/a/","content":"<p>one</p><p>two</p>\n\n`<literal>`"});
        let evidence = row["content"].clone();
        let result = json!({"index":{"indexed_pages":1},"results":[row.clone()]});
        let display = public_result(&result, "", PREFIX, NOTE).unwrap();
        assert!(
            display["results"][0]["excerpt"]
                .as_str()
                .unwrap()
                .contains("one\ntwo")
        );
        assert!(
            display["results"][0]["excerpt"]
                .as_str()
                .unwrap()
                .contains("<literal>")
        );
        assert_eq!(result["results"][0]["content"], evidence);
        row["source_kind"] = "note".into();
        row["source_url"] = format!("{PREFIX}synthetic").into();
        let result = json!({"results":[row]});
        assert_eq!(
            public_result(&result, "", PREFIX, NOTE).unwrap()["results"][0]["excerpt"],
            evidence
        );
    }
}
