//! Port of `embedding_client.py` + the parts of `network_policy.py` used by it.
//!
//! Same contract as the Python query path: one attempt, 30 s connect/read
//! timeouts, no proxies, no redirects, a 4 MiB response cap, strict model /
//! count / index / dimension / float32 validation, and the shared server
//! cooldown (`Retry-After`, `X-RateLimit-*`) that suppresses calls while the
//! provider asked us to wait.
//!
//! Intentional difference: Python stores a 900 s retry deadline the first
//! time a long-lived client sends a request and never resets it, so in the
//! long-running MCP process every query embedding after 15 minutes fails with
//! `retry_deadline` (see README "알려진 원본 차이"). Here the 900 s budget
//! starts per `embed()` call, which is the documented intent for one run.

use crate::compat::ProducerManifest;
use crate::py::{float, json as pyjson, sha256_hex, url};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;

/// Every failure is reported generically by the search service; the variants
/// only drive cooldown bookkeeping and tests.
#[derive(Debug, Clone, PartialEq)]
pub enum EmbeddingError {
    Invalid(&'static str),
    Network {
        category: &'static str,
        deferred_until: Option<f64>,
    },
}

pub type Counter = dyn Fn(&str) -> Result<usize, String> + Send + Sync;

pub struct EmbeddingClient {
    url: String,
    api_key: String,
    model_alias: String,
    accepted_models: [String; 2],
    dimensions: usize,
    max_input_tokens: usize,
    http: reqwest::Client,
    cooldown: Cooldown,
}

impl EmbeddingClient {
    pub fn new(
        base_url: &str,
        api_key: &str,
        allow_http: bool,
        timeout: Duration,
        manifest: &ProducerManifest,
        alert_dir: Option<PathBuf>,
    ) -> Result<Self, String> {
        let parsed = url::urlsplit(base_url)
            .ok_or("invalid LiteLLM base URL; HTTP requires explicit opt-in")?;
        let allowed = parsed.scheme == "https" || (allow_http && parsed.scheme == "http");
        let userinfo = parsed.netloc.contains('@');
        if !allowed
            || hostname(&parsed.netloc).is_none()
            || userinfo
            || !parsed.query.is_empty()
            || !parsed.fragment.is_empty()
        {
            return Err("invalid LiteLLM base URL; HTTP requires explicit opt-in".into());
        }
        if api_key.is_empty() || api_key.contains('\n') || api_key.contains('\r') {
            return Err("missing or invalid LiteLLM API key".into());
        }
        if timeout.is_zero() || timeout > Duration::from_secs(120) {
            return Err("invalid request timeout".into());
        }
        let endpoint = format!("{}/embeddings", base_url.trim_end_matches('/'));
        crate::install_crypto_provider();
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .build()
            .map_err(|_| "cannot build the embedding HTTP client".to_string())?;
        let cooldown = Cooldown::new(&format!("{endpoint}\0{api_key}"), alert_dir);
        Ok(Self {
            url: endpoint,
            api_key: api_key.to_string(),
            model_alias: manifest.model_alias.clone(),
            accepted_models: [manifest.model.clone(), manifest.model_alias.clone()],
            dimensions: manifest.dimensions,
            max_input_tokens: manifest.max_input_tokens,
            http,
            cooldown,
        })
    }

    /// `EmbeddingClient.embed(texts)` with Python's batching rules.
    pub async fn embed(
        &self,
        texts: &[String],
        count: &Counter,
    ) -> Result<Vec<Vec<f64>>, EmbeddingError> {
        let mut sizes = Vec::with_capacity(texts.len());
        for text in texts {
            if crate::py::text::strip(text).is_empty() {
                return Err(EmbeddingError::Invalid("empty embedding input"));
            }
            let size = count(text).map_err(|_| EmbeddingError::Invalid("tokenizer failure"))?;
            if size == 0 || size > self.max_input_tokens {
                return Err(EmbeddingError::Invalid(
                    "embedding input exceeds model limit; nothing was truncated",
                ));
            }
            sizes.push(size);
        }
        let deadline = Instant::now() + Duration::from_secs(900);
        let mut result = Vec::with_capacity(texts.len());
        let mut batch: Vec<&String> = Vec::new();
        let mut tokens = 0;
        for (text, size) in texts.iter().zip(sizes) {
            if !batch.is_empty() && (batch.len() >= 16 || tokens + size > self.max_input_tokens) {
                result.extend(self.request(&batch, deadline).await?);
                batch.clear();
                tokens = 0;
            }
            batch.push(text);
            tokens += size;
        }
        if !batch.is_empty() {
            result.extend(self.request(&batch, deadline).await?);
        }
        Ok(result)
    }

    async fn request(
        &self,
        texts: &[&String],
        deadline: Instant,
    ) -> Result<Vec<Vec<f64>>, EmbeddingError> {
        // network_policy.retry_call(attempts=1): cooldown, deadline, one try.
        self.cooldown.check()?;
        if Instant::now() >= deadline {
            return Err(EmbeddingError::Network {
                category: "retry_deadline",
                deferred_until: None,
            });
        }
        let body = json!({
            "model": self.model_alias,
            "input": texts,
            "dimensions": self.dimensions,
            "encoding_format": "float",
            "provider": {"zdr": true, "data_collection": "deny"},
        });
        let outcome = self
            .http
            .post(&self.url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(pyjson::dumps_ascii(&body))
            .send()
            .await;
        let response = match outcome {
            Ok(response) => response,
            Err(error) => {
                let category = if error.is_timeout() {
                    "delivery_unconfirmed"
                } else {
                    "connection_error"
                };
                return Err(EmbeddingError::Network {
                    category,
                    deferred_until: None,
                });
            }
        };
        let status = response.status();
        if !status.is_success() {
            let fault = classify_status(status.as_u16(), response.headers(), unix_now());
            self.cooldown.remember(&fault);
            return Err(fault);
        }
        let raw = read_limited(response, RESPONSE_LIMIT + 1).await?;
        if raw.len() > RESPONSE_LIMIT {
            return Err(EmbeddingError::Invalid(
                "embedding response exceeds safety limit",
            ));
        }
        let payload: Value = serde_json::from_slice(&raw)
            .map_err(|_| EmbeddingError::Invalid("invalid embedding JSON"))?;
        self.parse(&payload, texts.len())
    }

    /// `EmbeddingClient._parse(payload, expected)`
    fn parse(&self, payload: &Value, expected: usize) -> Result<Vec<Vec<f64>>, EmbeddingError> {
        let model = payload.get("model").and_then(Value::as_str);
        if !payload.is_object()
            || !model.is_some_and(|m| self.accepted_models.iter().any(|a| a == m))
        {
            return Err(EmbeddingError::Invalid(
                "unexpected embedding model; refusing mixed vector generations",
            ));
        }
        let data = match payload.get("data").and_then(Value::as_array) {
            Some(data) if data.len() == expected => data,
            _ => return Err(EmbeddingError::Invalid("embedding response count mismatch")),
        };
        let mut rows: Vec<Option<Vec<f64>>> = vec![None; expected];
        for item in data {
            let item = item
                .as_object()
                .ok_or(EmbeddingError::Invalid("invalid embedding row"))?;
            let index = item
                .get("index")
                .filter(|v| pyjson::is_int(v))
                .and_then(Value::as_u64)
                .map(|i| i as usize)
                .filter(|i| *i < expected && rows[*i].is_none())
                .ok_or(EmbeddingError::Invalid(
                    "invalid or duplicate embedding response index",
                ))?;
            rows[index] = Some(validate_vector(
                item.get("embedding").unwrap_or(&Value::Null),
                self.dimensions,
            )?);
        }
        Ok(rows
            .into_iter()
            .map(|row| row.unwrap_or_default())
            .collect())
    }
}

/// `embedding_client.validate_vector(vector)`
pub fn validate_vector(vector: &Value, dimensions: usize) -> Result<Vec<f64>, EmbeddingError> {
    let items = match vector.as_array() {
        Some(items) if items.len() == dimensions => items,
        _ => return Err(EmbeddingError::Invalid("embedding dimensions mismatch")),
    };
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        let value = item.as_f64().filter(|v| v.is_finite() && v.abs() <= 3.4e38);
        match (item, value) {
            (Value::Number(_), Some(value)) => values.push(value),
            _ => {
                return Err(EmbeddingError::Invalid(
                    "embedding contains invalid float32 values",
                ));
            }
        }
    }
    if values.iter().all(|v| *v == 0.0) {
        return Err(EmbeddingError::Invalid(
            "zero embedding cannot be searched with cosine distance",
        ));
    }
    Ok(values)
}

/// `'[' + ','.join(str(x) for x in vector) + ']'` (pgvector text input).
pub fn vector_literal(vector: &[f64]) -> String {
    let parts: Vec<String> = vector.iter().map(|v| float::repr(*v)).collect();
    format!("[{}]", parts.join(","))
}

/// `urlsplit(...).hostname` presence.
fn hostname(netloc: &str) -> Option<String> {
    let host = crate::py::text::rpartition(netloc, "@").2;
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        crate::py::text::partition(host, ":").0
    };
    (!host.is_empty()).then(|| host.to_lowercase())
}

async fn read_limited(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, EmbeddingError> {
    let mut raw = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                raw.extend_from_slice(&chunk);
                if raw.len() >= limit {
                    raw.truncate(limit);
                    return Ok(raw);
                }
            }
            Ok(None) => return Ok(raw),
            Err(error) => {
                let category = if error.is_timeout() {
                    "delivery_unconfirmed"
                } else {
                    "connection_error"
                };
                return Err(EmbeddingError::Network {
                    category,
                    deferred_until: None,
                });
            }
        }
    }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `network_policy.classify(HTTPError)` for the paid embedding request.
fn classify_status(status: u16, headers: &reqwest::header::HeaderMap, now: f64) -> EmbeddingError {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let retry_after = header("Retry-After");
    let exhausted = header("X-RateLimit-Remaining").as_deref() == Some("0");
    let cooldown = status == 429
        || (status == 403 && (exhausted || retry_after.is_some()))
        || (matches!(status, 408 | 500 | 502 | 503 | 504) && retry_after.is_some());
    if cooldown {
        return match wait_until(
            retry_after.as_deref(),
            exhausted,
            header("X-RateLimit-Reset").as_deref(),
            now,
        ) {
            Some(until) => EmbeddingError::Network {
                category: "server_cooldown",
                deferred_until: Some(until),
            },
            None => EmbeddingError::Network {
                category: "invalid_server_wait",
                deferred_until: None,
            },
        };
    }
    let category = if matches!(status, 408 | 500 | 502 | 503 | 504) {
        "server_error"
    } else {
        "request_rejected"
    };
    EmbeddingError::Network {
        category,
        deferred_until: None,
    }
}

/// `network_policy.wait_until(headers, now)`; `None` where Python raises the
/// permanent `invalid_server_wait` fault.
fn wait_until(
    retry_after: Option<&str>,
    exhausted: bool,
    reset: Option<&str>,
    now: f64,
) -> Option<f64> {
    let mut delay = retry_after.and_then(|value| {
        python_float(value).or_else(|| parse_http_date(value).map(|at| at - now))
    });
    if exhausted && let Some(reset) = reset.and_then(python_float) {
        delay = Some(delay.unwrap_or(0.0).max(reset - now + 1.0));
    }
    match delay {
        Some(delay) if delay.is_finite() => {
            if delay > 366.0 * 86400.0 {
                None
            } else {
                Some(now + delay.ceil().max(1.0))
            }
        }
        _ => Some(now + 60.0),
    }
}

fn python_float(value: &str) -> Option<f64> {
    crate::py::text::strip(value)
        .replace('_', "")
        .parse::<f64>()
        .ok()
}

/// RFC 7231 IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) → Unix seconds.
fn parse_http_date(value: &str) -> Option<f64> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let [_, day, month, year, time, zone] = parts.as_slice() else {
        return None;
    };
    if !matches!(*zone, "GMT" | "UTC" | "+0000") {
        return None;
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| m == month)? as u32
        + 1;
    let hms: Vec<u32> = time
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let [h, m, s] = hms.as_slice() else {
        return None;
    };
    let days = crate::timefmt::days_from_civil(year.parse().ok()?, month, day.parse().ok()?);
    Some((days * 86400 + i64::from(*h) * 3600 + i64::from(*m) * 60 + i64::from(*s)) as f64)
}

/// `network_policy.Cooldown`: in-memory, or shared through `RAG_ALERT_DIR`
/// with the same lock/file format the Python jobs use.
struct Cooldown {
    key: String,
    root: Option<PathBuf>,
    local_until: Mutex<f64>,
}

impl Cooldown {
    fn new(scope: &str, root: Option<PathBuf>) -> Self {
        Self {
            key: sha256_hex(scope),
            root,
            local_until: Mutex::new(0.0),
        }
    }

    fn check(&self) -> Result<(), EmbeddingError> {
        let until = match &self.root {
            None => *self.local_until.lock().unwrap_or_else(|e| e.into_inner()),
            Some(root) => outbox::read_not_before(root, &self.key)
                .map_err(|_| EmbeddingError::Invalid("cooldown state unavailable"))?,
        };
        if until > unix_now() {
            return Err(EmbeddingError::Network {
                category: "server_cooldown",
                deferred_until: Some(until),
            });
        }
        Ok(())
    }

    fn remember(&self, fault: &EmbeddingError) {
        let EmbeddingError::Network {
            deferred_until: Some(until),
            ..
        } = fault
        else {
            return;
        };
        let local = {
            let mut local = self.local_until.lock().unwrap_or_else(|e| e.into_inner());
            *local = local.max(*until);
            *local
        };
        if let Some(root) = &self.root {
            // Python lets a failed state write propagate; the request already
            // failed, so the caller sees the same generic error either way.
            let _ = outbox::store_not_before(root, &self.key, local);
        }
    }
}

/// The subset of `scripts/notification_outbox.py` used for cooldown files.
mod outbox {
    use super::*;
    use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};
    use std::io::Write;

    fn prepare(root: &Path) -> std::io::Result<()> {
        if std::fs::symlink_metadata(root).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(std::io::Error::other("Symlink operational directory"));
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        match std::fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join("incidents"))
        {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
        Ok(())
    }

    fn locked<T>(root: &Path, work: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
        prepare(root)?;
        let fd = open(
            root.join("outbox.lock"),
            OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        flock(&fd, FlockOperation::LockExclusive)?;
        work()
    }

    fn read(path: &Path) -> std::io::Result<serde_json::Map<String, Value>> {
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(std::io::Error::other("Symlink operational state"));
        }
        match std::fs::read(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            Err(error) => Err(error),
            Ok(raw) => match serde_json::from_slice(&raw) {
                Ok(Value::Object(map)) => Ok(map),
                _ => Err(std::io::Error::other("Invalid operational state")),
            },
        }
    }

    fn state_path(root: &Path, key: &str) -> PathBuf {
        root.join(format!("cooldown-{key}.json"))
    }

    pub fn read_not_before(root: &Path, key: &str) -> std::io::Result<f64> {
        locked(root, || {
            Ok(read(&state_path(root, key))?
                .get("not_before")
                .and_then(Value::as_f64)
                .unwrap_or(0.0))
        })
    }

    pub fn store_not_before(root: &Path, key: &str, local: f64) -> std::io::Result<()> {
        let path = state_path(root, key);
        locked(root, || {
            let old = read(&path)?
                .get("not_before")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let mut file = tempfile_in(root)?;
            let body = format!("{{\"not_before\": {}}}", float::repr(old.max(local)));
            file.1.write_all(body.as_bytes())?;
            file.1.sync_all()?;
            std::fs::rename(&file.0, &path)?;
            std::fs::File::open(root)?.sync_all()
        })
    }

    /// `tempfile.mkstemp(prefix='.pending-', dir=parent)` with mode 0600.
    fn tempfile_in(root: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
        use std::os::unix::fs::OpenOptionsExt;
        for _ in 0..16 {
            let name = format!(".pending-{}", hex::encode(rand::random::<[u8; 6]>()));
            let path = root.join(name);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::other("cannot create temporary state file"))
    }

    use std::os::unix::fs::DirBuilderExt;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vector_validation_matches_python() {
        let mut ok = vec![json!(0.0); 4];
        ok[0] = json!(1);
        assert_eq!(
            validate_vector(&Value::Array(ok.clone()), 4).unwrap(),
            [1.0, 0.0, 0.0, 0.0]
        );
        assert!(validate_vector(&Value::Array(ok.clone()), 3).is_err());
        let mut bad = ok.clone();
        bad[1] = json!(true);
        assert!(validate_vector(&Value::Array(bad), 4).is_err());
        let mut bad = ok.clone();
        bad[1] = json!(3.5e38);
        assert!(validate_vector(&Value::Array(bad), 4).is_err());
        assert!(validate_vector(&json!([0.0, -0.0, 0, 0]), 4).is_err());
        assert_eq!(vector_literal(&[1.0, 1e-5, -0.25]), "[1.0,1e-05,-0.25]");
    }

    #[test]
    fn cooldown_headers_like_python() {
        let now = 1_000_000.0;
        assert_eq!(wait_until(Some("2.2"), false, None, now), Some(now + 3.0));
        assert_eq!(wait_until(None, false, None, now), Some(now + 60.0));
        assert_eq!(wait_until(Some("0"), false, None, now), Some(now + 1.0));
        assert_eq!(wait_until(Some("nan"), false, None, now), Some(now + 60.0));
        assert_eq!(wait_until(Some("99999999"), false, None, now), None);
        assert_eq!(
            wait_until(None, true, Some("1000010"), now),
            Some(now + 11.0)
        );
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784111777.0)
        );
    }

    #[test]
    fn local_cooldown_blocks_until_expiry() {
        let cooldown = Cooldown::new("scope", None);
        cooldown.remember(&EmbeddingError::Network {
            category: "server_cooldown",
            deferred_until: Some(unix_now() + 30.0),
        });
        assert!(cooldown.check().is_err());
        let cooldown = Cooldown::new("scope", None);
        cooldown.remember(&EmbeddingError::Network {
            category: "server_error",
            deferred_until: None,
        });
        assert!(cooldown.check().is_ok());
    }

    #[test]
    fn shared_cooldown_file_format() {
        let dir = tempfile::tempdir().unwrap();
        let cooldown = Cooldown::new("scope", Some(dir.path().to_path_buf()));
        let until = unix_now() + 30.0;
        cooldown.remember(&EmbeddingError::Network {
            category: "server_cooldown",
            deferred_until: Some(until),
        });
        let stored: Value = serde_json::from_slice(
            &std::fs::read(
                dir.path()
                    .join(format!("cooldown-{}.json", sha256_hex("scope"))),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(stored["not_before"].as_f64(), Some(until));
        assert!(
            Cooldown::new("scope", Some(dir.path().to_path_buf()))
                .check()
                .is_err()
        );
        assert!(dir.path().join("incidents").is_dir());
    }
}
