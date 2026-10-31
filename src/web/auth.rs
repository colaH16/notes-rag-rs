//! Port of `web_auth.OwnerAuth`: OIDC authorization code + PKCE (S256),
//! state, nonce, ID-token signature/issuer/audience/expiry verification,
//! and opaque, revocable, server-side owner sessions.
//!
//! Only the configured immutable issuer/subject pair is owner. Email,
//! username, groups, proxy headers and client flags are never trusted. OAuth
//! tokens are transient and never stored in cookies or SQLite.
//!
//! Compatibility: owner sessions are random tokens whose SHA-256 digests live
//! in `owner_sessions` (unchanged schema), so sessions issued by Python stay
//! valid after cutover and vice versa. The 5-minute *login-flow* cookie
//! (`__Host-notes-oidc`) was a Starlette/itsdangerous signed session; this
//! port uses its own HMAC-SHA256 format instead of imitating itsdangerous, so
//! a login that is mid-flight at the moment of cutover must simply be retried.

use super::state::State;
use crate::fault::{Fault, Result};
use crate::py::sha256_hex;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use openidconnect::core::{
    CoreAuthenticationFlow, CoreClient, CoreJwsSigningAlgorithm, CoreProviderMetadata,
};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use rand::RngCore;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

pub const COOKIE: &str = "__Host-notes-owner";
pub const FLOW_COOKIE: &str = "__Host-notes-oidc";
const FLOW_SECONDS: i64 = 300;
const SESSION_SECONDS: i64 = 3600;
const ASYMMETRIC: [CoreJwsSigningAlgorithm; 9] = [
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha384,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha512,
    CoreJwsSigningAlgorithm::RsaSsaPssSha256,
    CoreJwsSigningAlgorithm::RsaSsaPssSha384,
    CoreJwsSigningAlgorithm::RsaSsaPssSha512,
    CoreJwsSigningAlgorithm::EcdsaP256Sha256,
    CoreJwsSigningAlgorithm::EcdsaP384Sha384,
    CoreJwsSigningAlgorithm::EdDsa,
];

/// `secrets.token_urlsafe(n)`
pub fn token_urlsafe(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

/// Temporary login state carried in the signed flow cookie. Like Python, it
/// holds no owner authority; it only binds the callback to this browser.
#[derive(Serialize, Deserialize)]
pub struct Flow {
    pub started: i64,
    pub flow: String,
    pub return_to: String,
    pub state: String,
    pub nonce: String,
    pub verifier: String,
}

#[derive(Debug)]
pub struct HttpError;
impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("identity provider request failed")
    }
}
impl std::error::Error for HttpError {}

/// Outbound OIDC HTTP: no proxies, no redirects, 15 s (Authlib `timeout=15`).
/// The request runs in its own task so the returned future is `Send + Sync`,
/// as the oauth2 `AsyncHttpClient` contract requires.
fn provider_call(
    client: reqwest::Client,
    request: openidconnect::HttpRequest,
) -> impl std::future::Future<Output = std::result::Result<openidconnect::HttpResponse, HttpError>>
+ Send
+ Sync {
    let task = tokio::spawn(provider_request(client, request));
    async move { task.await.map_err(|_| HttpError)? }
}

async fn provider_request(
    client: reqwest::Client,
    request: openidconnect::HttpRequest,
) -> std::result::Result<openidconnect::HttpResponse, HttpError> {
    let (parts, body) = request.into_parts();
    let mut outbound = client
        .request(parts.method, parts.uri.to_string())
        .body(body);
    for (name, value) in &parts.headers {
        outbound = outbound.header(name, value);
    }
    let response = outbound.send().await.map_err(|_| HttpError)?;
    let mut builder = http::Response::builder().status(response.status());
    for (name, value) in response.headers() {
        builder = builder.header(name, value);
    }
    let body = response.bytes().await.map_err(|_| HttpError)?;
    if body.len() > 1024 * 1024 {
        return Err(HttpError);
    }
    builder.body(body.to_vec()).map_err(|_| HttpError)
}

pub struct OwnerAuth {
    pub state: Arc<State>,
    client_id: String,
    client_secret: String,
    flow_key: Vec<u8>,
    pub subject: std::sync::RwLock<String>,
    pub origin: String,
    pub issuer: String,
    http: reqwest::Client,
    metadata: tokio::sync::Mutex<Option<CoreProviderMetadata>>,
}

pub enum CallbackOutcome {
    /// 303 to the return path with a fresh owner session token.
    Owner {
        location: &'static str,
        token: String,
    },
    Denied(Fault),
    Failed,
}

impl OwnerAuth {
    pub fn new(
        state: Arc<State>,
        client_id: &str,
        client_secret: &str,
        session_key: &str,
        owner_subject: &str,
        origin: &str,
        issuer: &str,
    ) -> std::result::Result<Self, String> {
        if client_id.is_empty()
            || client_secret.len().min(session_key.len()) < 40
            || owner_subject.is_empty()
        {
            return Err("Complete strong owner authentication configuration required".into());
        }
        let db = state
            .connect()
            .map_err(|_| "state database unavailable".to_string())?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS owner_sessions (digest TEXT PRIMARY KEY, subject TEXT NOT NULL, policy TEXT NOT NULL, expires INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS owner_flows (digest TEXT PRIMARY KEY, expires INTEGER NOT NULL);",
        )
        .map_err(|_| "state database unavailable".to_string())?;
        // Domain-separated key: the raw session key is never used directly.
        let mut mac = Hmac::<Sha256>::new_from_slice(session_key.as_bytes())
            .map_err(|_| "invalid session key")?;
        mac.update(b"notes-rag-rs/oidc-flow-cookie/v1");
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| "cannot build the OIDC HTTP client".to_string())?;
        Ok(Self {
            state,
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            flow_key: mac.finalize().into_bytes().to_vec(),
            subject: std::sync::RwLock::new(owner_subject.to_string()),
            origin: origin.to_string(),
            issuer: issuer.to_string(),
            http,
            metadata: tokio::sync::Mutex::new(None),
        })
    }

    pub fn callback_url(&self) -> String {
        format!("{}/auth/callback", self.origin)
    }

    fn subject(&self) -> String {
        self.subject
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `sha256(ISSUER + '\n' + subject)`: changing either revokes sessions.
    fn policy(&self) -> String {
        sha256_hex(format!("{}\n{}", self.issuer, self.subject()))
    }

    fn now(&self) -> i64 {
        (self.state.clock)().floor() as i64
    }

    /// `owner(request)`: an unexpired session for the pinned issuer/subject.
    pub fn owner(&self, token: Option<&str>) -> Result<bool> {
        let token = token.unwrap_or("");
        if !(40..=128).contains(&token.chars().count()) {
            return Ok(false);
        }
        let db = self.state.connect()?;
        let row: Option<(String, String, i64)> = db
            .query_row(
                "SELECT subject, policy, expires FROM owner_sessions WHERE digest=?",
                params![sha256_hex(token)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map(Some)
            .or_else(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    Ok(None)
                } else {
                    Err(e)
                }
            })
            .map_err(|_| Fault::Other("state database failure"))?;
        let now = (self.state.clock)();
        Ok(row.is_some_and(|(subject, policy, expires)| {
            subject == self.subject() && policy == self.policy() && (expires as f64) > now
        }))
    }

    /// `revoke(request)`: delete this session and every expired one.
    pub fn revoke(&self, token: Option<&str>) -> Result<()> {
        let db = self.state.connect()?;
        db.execute(
            "DELETE FROM owner_sessions WHERE digest=? OR expires<=?",
            params![sha256_hex(token.unwrap_or("")), self.now()],
        )
        .map_err(|_| Fault::Other("state database failure"))?;
        Ok(())
    }

    /// `issue()`: a new opaque one-hour session token.
    pub fn issue(&self) -> Result<String> {
        let token = token_urlsafe(48);
        let now = self.now();
        let db = self.state.connect()?;
        let fail = |_| Fault::Other("state database failure");
        db.execute_batch("BEGIN").map_err(fail)?;
        db.execute("DELETE FROM owner_sessions WHERE expires<=?", params![now])
            .map_err(fail)?;
        db.execute(
            "INSERT INTO owner_sessions VALUES(?,?,?,?)",
            params![
                sha256_hex(&token),
                self.subject(),
                self.policy(),
                now + SESSION_SECONDS
            ],
        )
        .map_err(fail)?;
        db.execute_batch("COMMIT").map_err(fail)?;
        Ok(token)
    }

    // ----- flow cookie ---------------------------------------------------

    fn sign(&self, payload: &str) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.flow_key).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    pub fn encode_flow(&self, flow: &Flow) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(flow).unwrap_or_default());
        let signature = self.sign(&payload);
        format!("{payload}.{signature}")
    }

    /// Verify the signature in constant time, then decode.
    pub fn decode_flow(&self, value: &str) -> Option<Flow> {
        let (payload, signature) = value.split_once('.')?;
        let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.flow_key).ok()?;
        mac.update(payload.as_bytes());
        mac.verify_slice(&signature).ok()?;
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
    }

    // ----- provider ------------------------------------------------------

    async fn provider(
        &self,
        refresh: bool,
    ) -> std::result::Result<CoreProviderMetadata, HttpError> {
        let mut cached = self.metadata.lock().await;
        if !refresh && let Some(metadata) = cached.as_ref() {
            return Ok(metadata.clone());
        }
        let issuer = IssuerUrl::new(self.issuer.clone()).map_err(|_| HttpError)?;
        let http = self.http.clone();
        let call = move |request| provider_call(http.clone(), request);
        let metadata = CoreProviderMetadata::discover_async(issuer, &call)
            .await
            .map_err(|_| HttpError)?;
        *cached = Some(metadata.clone());
        Ok(metadata)
    }

    /// `login(request)` after the traffic check: authorization URL + flow.
    pub async fn authorization(&self, return_to: &'static str) -> Result<(String, Flow)> {
        let started = self.now();
        let flow_secret = token_urlsafe(32);
        let fail = |_| Fault::Other("state database failure");
        let db = self.state.connect()?;
        db.execute_batch("BEGIN").map_err(fail)?;
        db.execute("DELETE FROM owner_flows WHERE expires<=?", params![started])
            .map_err(fail)?;
        db.execute(
            "INSERT INTO owner_flows VALUES(?,?)",
            params![sha256_hex(&flow_secret), started + FLOW_SECONDS],
        )
        .map_err(fail)?;
        db.execute_batch("COMMIT").map_err(fail)?;
        let metadata = self
            .provider(false)
            .await
            .map_err(|_| Fault::Other("identity provider unavailable"))?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(self.client_id.clone()),
            Some(ClientSecret::new(self.client_secret.clone())),
        )
        .set_redirect_uri(
            RedirectUrl::new(self.callback_url())
                .map_err(|_| Fault::Other("invalid callback URL"))?,
        );
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (state, nonce) = (token_urlsafe(24), token_urlsafe(24));
        let (state_value, nonce_value) = (state.clone(), nonce.clone());
        let (url, _, _) = client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                move || CsrfToken::new(state_value),
                move || Nonce::new(nonce_value),
            )
            .add_scope(Scope::new("profile".into()))
            .set_pkce_challenge(challenge)
            .url();
        let flow = Flow {
            started,
            flow: flow_secret,
            return_to: return_to.to_string(),
            state,
            nonce,
            verifier: verifier.secret().clone(),
        };
        Ok((url.to_string(), flow))
    }

    /// `callback(request)` up to, but excluding, the HTTP response.
    pub async fn callback(
        &self,
        flow: Option<Flow>,
        query_state: Option<&str>,
        code: Option<&str>,
        error: bool,
    ) -> CallbackOutcome {
        let Some(flow) = flow else {
            return CallbackOutcome::Failed;
        };
        let elapsed = self.now() - flow.started;
        if !(0..=FLOW_SECONDS).contains(&elapsed) || !(40..=128).contains(&flow.flow.len()) {
            return CallbackOutcome::Failed;
        }
        let now = self.now();
        let consumed = self.state.immediate(|db| {
            db.execute(
                "DELETE FROM owner_flows WHERE digest=? AND expires>?",
                params![sha256_hex(&flow.flow), now],
            )
            .map_err(|_| Fault::Other("state database failure"))
        });
        if consumed != Ok(1) {
            return CallbackOutcome::Failed;
        }
        if let Err(fault) = self.state.traffic("auth-callback") {
            return match fault {
                Fault::Denied { .. } => CallbackOutcome::Denied(fault),
                _ => CallbackOutcome::Failed,
            };
        }
        let state_matches = query_state.is_some_and(|s| {
            use subtle::ConstantTimeEq;
            s.len() == flow.state.len() && bool::from(s.as_bytes().ct_eq(flow.state.as_bytes()))
        });
        let (Some(code), false, true) = (code, error, state_matches) else {
            return CallbackOutcome::Failed;
        };
        match self.verify(code, &flow).await {
            Some(true) => match self.issue() {
                Ok(token) => CallbackOutcome::Owner {
                    location: if flow.return_to == "/graph" {
                        "/graph"
                    } else {
                        "/"
                    },
                    token,
                },
                Err(_) => CallbackOutcome::Failed,
            },
            _ => CallbackOutcome::Failed,
        }
    }

    /// Exchange the code (PKCE) and verify the ID token; `Some(true)` only for
    /// the designated owner.
    async fn verify(&self, code: &str, flow: &Flow) -> Option<bool> {
        let http = self.http.clone();
        let call = move |request| provider_call(http.clone(), request);
        let metadata = self.provider(false).await.ok()?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(self.client_id.clone()),
            Some(ClientSecret::new(self.client_secret.clone())),
        )
        .set_redirect_uri(RedirectUrl::new(self.callback_url()).ok()?);
        let token = client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .ok()?
            .set_pkce_verifier(PkceCodeVerifier::new(flow.verifier.clone()))
            .request_async(&call)
            .await
            .ok()?;
        let id_token = token.id_token()?;
        let nonce = Nonce::new(flow.nonce.clone());
        // A key rotation shows up as a verification failure: refresh the
        // provider metadata (JWKS) once, like Authlib's kid-miss refetch.
        for refresh in [false, true] {
            let metadata = self.provider(refresh).await.ok()?;
            let mut algorithms: Vec<CoreJwsSigningAlgorithm> = metadata
                .id_token_signing_alg_values_supported()
                .iter()
                .filter(|a| ASYMMETRIC.contains(a))
                .cloned()
                .collect();
            if algorithms.is_empty() {
                algorithms.push(CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256);
            }
            // Public-key verifier only: the client secret is never an HMAC key.
            let verifier_client = CoreClient::from_provider_metadata(
                metadata,
                ClientId::new(self.client_id.clone()),
                None,
            );
            // Authlib leeway=30: accept tokens up to 30 s past `exp`.
            let verifier = verifier_client
                .id_token_verifier()
                .set_allowed_algs(algorithms)
                .set_time_fn(|| chrono::Utc::now() - chrono::Duration::seconds(30));
            let Ok(claims) = id_token.claims(&verifier, &nonce) else {
                continue;
            };
            if let Some(expected) = claims.access_token_hash() {
                let actual = id_token
                    .signing_alg()
                    .ok()
                    .zip(id_token.signing_key(&verifier).ok())
                    .and_then(|(alg, key)| {
                        AccessTokenHash::from_token(token.access_token(), alg, key).ok()
                    });
                if actual.as_ref() != Some(expected) {
                    return Some(false);
                }
            }
            let nonce_supported_false = jwt_payload(&id_token.to_string()).is_some_and(|claims| {
                claims.get("nonce_supported") == Some(&serde_json::Value::Bool(false))
            });
            let issuer_ok = claims.issuer().as_str() == self.issuer;
            return Some(
                issuer_ok && claims.subject().as_str() == self.subject() && !nonce_supported_false,
            );
        }
        Some(false)
    }
}

/// Payload of an already signature-verified compact JWT.
fn jwt_payload(token: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
}

/// Starlette `Response.set_cookie` output for the owner session cookie.
pub fn owner_cookie(token: &str) -> String {
    format!("{COOKIE}={token}; HttpOnly; Max-Age={SESSION_SECONDS}; Path=/; SameSite=lax; Secure")
}

/// Starlette `Response.delete_cookie(COOKIE, secure=True, httponly=True, samesite='lax')`.
pub fn delete_owner_cookie() -> String {
    format!(
        "{COOKIE}=\"\"; expires=Thu, 01 Jan 1970 00:00:00 GMT; HttpOnly; Max-Age=0; Path=/; SameSite=lax; Secure"
    )
}

/// Starlette `SessionMiddleware` cookie attributes (max_age=300, https_only).
pub fn flow_cookie(value: &str) -> String {
    format!("{FLOW_COOKIE}={value}; path=/; Max-Age={FLOW_SECONDS}; httponly; samesite=lax; secure")
}

pub fn clear_flow_cookie() -> String {
    format!(
        "{FLOW_COOKIE}=null; path=/; expires=Thu, 01 Jan 1970 00:00:00 GMT; httponly; samesite=lax; secure"
    )
}
