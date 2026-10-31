//! Environment-only configuration.
//!
//! Every variable the Python web/MCP read keeps its name and meaning. Values
//! that the Python code hard-coded become variables whose defaults are
//! exactly the original constants, so an unchanged Deployment environment
//! behaves identically. See README "환경 변수".

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Snapshot of the process environment (injectable for tests).
#[derive(Clone, Default)]
pub struct Env(pub BTreeMap<String, String>);

impl Env {
    pub fn process() -> Self {
        Self(
            std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
        )
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn required(&self, name: &str) -> Result<String, String> {
        self.get(name)
            .map(str::to_string)
            .ok_or_else(|| format!("{name} is required"))
    }

    pub fn or(&self, name: &str, default: &str) -> String {
        self.get(name).unwrap_or(default).to_string()
    }
}

pub const DEFAULT_MCP_URL: &str = "http://notes-rag-mcp.myapps.svc:8000/mcp";
pub const DEFAULT_PUBLIC_ORIGIN: &str = "https://rag.cola16.app";
pub const DEFAULT_OIDC_ISSUER: &str = "https://auth.cola16.app/application/o/notes-rag/";
pub const DEFAULT_SILVERBULLET_URL: &str = "https://notes.cola16.app";
pub const DEFAULT_NOTE_PATH_PREFIX: &str = "/OneNote/markdown/";

/// Python `int(value)` for a positive configuration integer.
fn python_int(name: &str, value: &str) -> Result<i64, String> {
    crate::py::json::to_int(&serde_json::Value::String(value.to_string()))
        .ok_or_else(|| format!("{name} must be an integer"))
}

fn listen(env: &Env, default: &str) -> Result<SocketAddr, String> {
    env.or("RAG_LISTEN_ADDR", default)
        .parse()
        .map_err(|_| "RAG_LISTEN_ADDR must be host:port".to_string())
}

fn grace(env: &Env) -> Result<Duration, String> {
    let seconds = python_int(
        "RAG_SHUTDOWN_GRACE_SECONDS",
        &env.or("RAG_SHUTDOWN_GRACE_SECONDS", "20"),
    )?;
    if !(0..=3600).contains(&seconds) {
        return Err("RAG_SHUTDOWN_GRACE_SECONDS must be 0..3600".into());
    }
    Ok(Duration::from_secs(seconds as u64))
}

fn note_prefix(env: &Env) -> Result<String, String> {
    let prefix = env.or("RAG_NOTE_PATH_PREFIX", DEFAULT_NOTE_PATH_PREFIX);
    if !prefix.starts_with('/')
        || !prefix.ends_with('/')
        || prefix.len() < 3
        || prefix.contains("//")
    {
        return Err("RAG_NOTE_PATH_PREFIX must look like /Folder/sub/".into());
    }
    Ok(prefix)
}

pub struct McpConfig {
    pub listen: SocketAddr,
    pub shutdown_grace: Duration,
    pub token: String,
    pub owner_token: Option<String>,
    pub allowed_hosts: Vec<String>,
    pub allowlist: PathBuf,
    pub access_policy: PathBuf,
    pub corpus_root: PathBuf,
    pub database_url: String,
    pub litellm_base_url: String,
    pub litellm_api_key: String,
    pub allow_internal_http: bool,
    pub silverbullet_url: String,
    pub tokenizer_path: PathBuf,
    pub vision_state: Option<PathBuf>,
    pub alert_dir: Option<PathBuf>,
    pub note_prefix: String,
}

impl McpConfig {
    pub fn from_env(env: &Env) -> Result<Self, String> {
        Ok(Self {
            listen: listen(env, "0.0.0.0:8000")?,
            shutdown_grace: grace(env)?,
            token: env.required("RAG_MCP_TOKEN")?,
            owner_token: env.get("RAG_OWNER_MCP_TOKEN").map(str::to_string),
            // Python: os.environ.get('RAG_MCP_HOSTS', '127.0.0.1:*,localhost:*').split(',')
            allowed_hosts: env
                .or("RAG_MCP_HOSTS", "127.0.0.1:*,localhost:*")
                .split(',')
                .map(str::to_string)
                .collect(),
            allowlist: env.required("RAG_ALLOWLIST")?.into(),
            access_policy: env.required("RAG_ACCESS_POLICY")?.into(),
            corpus_root: env.required("RAG_CORPUS_ROOT")?.into(),
            database_url: env.or("RAG_DATABASE_URL", ""),
            litellm_base_url: env.required("RAG_LITELLM_BASE_URL")?,
            litellm_api_key: env.required("RAG_LITELLM_API_KEY")?,
            allow_internal_http: env.get("RAG_ALLOW_INTERNAL_HTTP") == Some("true"),
            silverbullet_url: env.required("RAG_SILVERBULLET_URL")?,
            tokenizer_path: env
                .or(
                    "RAG_TOKENIZER_PATH",
                    crate::tokenizer::DEFAULT_TOKENIZER_PATH,
                )
                .into(),
            vision_state: env
                .get("RAG_VISION_STATE")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            alert_dir: env
                .get("RAG_ALERT_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            note_prefix: note_prefix(env)?,
        })
    }
}

pub struct WebConfig {
    pub listen: SocketAddr,
    pub shutdown_grace: Duration,
    pub state_db: PathBuf,
    pub daily_limit: i64,
    pub hourly_limit: i64,
    pub mcp_url: String,
    pub mcp_token: String,
    pub owner_mcp_token: String,
    pub public_origin: String,
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    pub oidc_client_secret: String,
    pub session_key: String,
    pub owner_subject: String,
    pub corpus_root: PathBuf,
    pub selection: PathBuf,
    pub policy: PathBuf,
    pub public_access_revision: String,
    pub silverbullet_url: String,
    pub note_prefix: String,
    pub vision_state: Option<PathBuf>,
}

impl WebConfig {
    pub fn from_env(env: &Env) -> Result<Self, String> {
        let public_origin = env.or("RAG_PUBLIC_ORIGIN", DEFAULT_PUBLIC_ORIGIN);
        if !public_origin.starts_with("https://") || public_origin.ends_with('/') {
            return Err(
                "RAG_PUBLIC_ORIGIN must be an https origin without a trailing slash".into(),
            );
        }
        let mcp_url = env.or("RAG_MCP_URL", DEFAULT_MCP_URL);
        if !mcp_url.ends_with("/mcp") {
            return Err("RAG_MCP_URL must end with /mcp".into());
        }
        Ok(Self {
            listen: listen(env, "0.0.0.0:8080")?,
            shutdown_grace: grace(env)?,
            state_db: env.or("SEARCH_STATE_DB", "/state/search.db").into(),
            daily_limit: python_int("SEARCH_DAILY_LIMIT", &env.or("SEARCH_DAILY_LIMIT", "300"))?,
            hourly_limit: python_int("SEARCH_HOURLY_LIMIT", &env.or("SEARCH_HOURLY_LIMIT", "60"))?,
            mcp_url,
            mcp_token: env.required("RAG_MCP_TOKEN")?,
            owner_mcp_token: env.required("RAG_OWNER_MCP_TOKEN")?,
            public_origin,
            oidc_issuer: env.or("RAG_OIDC_ISSUER", DEFAULT_OIDC_ISSUER),
            oidc_client_id: env.required("RAG_OIDC_CLIENT_ID")?,
            oidc_client_secret: env.required("RAG_OIDC_CLIENT_SECRET")?,
            session_key: env.required("RAG_SESSION_KEY")?,
            owner_subject: env.required("RAG_OWNER_SUBJECT")?,
            corpus_root: env.required("RAG_CORPUS_ROOT")?.into(),
            selection: env
                .or("RAG_WEB_SELECTION_PATH", "/selection/markdown.json")
                .into(),
            policy: env.or("RAG_WEB_POLICY_PATH", "/policy/access.json").into(),
            public_access_revision: env.required("RAG_PUBLIC_ACCESS_REVISION")?,
            silverbullet_url: env.or("RAG_SILVERBULLET_URL", DEFAULT_SILVERBULLET_URL),
            note_prefix: note_prefix(env)?,
            vision_state: env
                .get("RAG_VISION_STATE")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        })
    }

    /// `https://notes.cola16.app/OneNote/markdown/` citation prefix.
    pub fn citation_prefix(&self) -> String {
        format!(
            "{}{}",
            self.silverbullet_url.trim_end_matches('/'),
            self.note_prefix
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        Env(pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect())
    }

    #[test]
    fn web_defaults_equal_original_constants() {
        let config = WebConfig::from_env(&env(&[
            ("RAG_MCP_TOKEN", "t"),
            ("RAG_OWNER_MCP_TOKEN", "o"),
            ("RAG_OIDC_CLIENT_ID", "c"),
            ("RAG_OIDC_CLIENT_SECRET", "s"),
            ("RAG_SESSION_KEY", "k"),
            ("RAG_OWNER_SUBJECT", "u"),
            ("RAG_CORPUS_ROOT", "/corpus"),
            ("RAG_PUBLIC_ACCESS_REVISION", "r"),
        ]))
        .ok()
        .unwrap();
        assert_eq!(config.listen.to_string(), "0.0.0.0:8080");
        assert_eq!((config.daily_limit, config.hourly_limit), (300, 60));
        assert_eq!(config.state_db, PathBuf::from("/state/search.db"));
        assert_eq!(config.mcp_url, DEFAULT_MCP_URL);
        assert_eq!(
            config.citation_prefix(),
            "https://notes.cola16.app/OneNote/markdown/"
        );
        assert_eq!(config.selection, PathBuf::from("/selection/markdown.json"));
        assert_eq!(config.policy, PathBuf::from("/policy/access.json"));
    }

    #[test]
    fn mcp_hosts_split_like_python() {
        let base = [
            ("RAG_MCP_TOKEN", "t"),
            ("RAG_ALLOWLIST", "/s/markdown.json"),
            ("RAG_ACCESS_POLICY", "/p.json"),
            ("RAG_CORPUS_ROOT", "/c"),
            ("RAG_LITELLM_BASE_URL", "http://x"),
            ("RAG_LITELLM_API_KEY", "k"),
            ("RAG_SILVERBULLET_URL", "https://n"),
        ];
        let config = McpConfig::from_env(&env(&base)).ok().unwrap();
        assert_eq!(config.allowed_hosts, ["127.0.0.1:*", "localhost:*"]);
        assert_eq!(config.listen.to_string(), "0.0.0.0:8000");
        let mut custom = base.to_vec();
        custom.push(("RAG_MCP_HOSTS", ""));
        assert_eq!(
            McpConfig::from_env(&env(&custom))
                .ok()
                .unwrap()
                .allowed_hosts,
            [""]
        );
    }
}
