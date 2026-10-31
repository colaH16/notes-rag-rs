//! Port of the original `tests/test_web_auth.py`: real signed OIDC against
//! a local mock provider (discovery, JWKS, token endpoint with PKCE check),
//! owner/public isolation, logout, replay and revocation races.

mod common;

use axum::extract::State as AxumState;
use axum::routing::{get as route_get, post as route_post};
use axum::{Form, Json, Router};
use common::*;
use http::StatusCode;
use notes_rag_rs::web::app::WebApp;
use notes_rag_rs::web::auth::{COOKIE, FLOW_COOKIE, OwnerAuth};
use notes_rag_rs::web::router;
use openidconnect::core::{
    CoreGenderClaim, CoreJsonWebKeyType, CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm, CoreRsaPrivateSigningKey,
};
use openidconnect::{
    AccessToken, AdditionalClaims, Audience, IdToken, IdTokenClaims, IssuerUrl, JsonWebKeyId,
    Nonce, PkceCodeChallenge, PkceCodeVerifier, PrivateSigningKey, StandardClaims,
    SubjectIdentifier,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const ORIGIN: &str = "https://rag.cola16.app";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Extra {
    #[serde(skip_serializing_if = "Option::is_none")]
    nonce_supported: Option<bool>,
}
impl AdditionalClaims for Extra {}

type TestIdToken =
    IdToken<Extra, CoreGenderClaim, CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm>;

#[derive(Default, Clone)]
struct Changes {
    sub: Option<String>,
    iss: Option<String>,
    aud: Option<String>,
    nonce: Option<String>,
    expired: bool,
    nonce_supported: Option<bool>,
    tamper: bool,
}

struct Provider {
    issuer: String,
    key: CoreRsaPrivateSigningKey,
    challenge: Mutex<String>,
    nonce: Mutex<String>,
    changes: Mutex<Changes>,
    token_requests: Mutex<usize>,
}

fn rsa_key() -> CoreRsaPrivateSigningKey {
    let output = std::process::Command::new("openssl")
        .args(["genrsa", "-traditional", "2048"])
        .output()
        .unwrap();
    assert!(output.status.success());
    CoreRsaPrivateSigningKey::from_pem(
        std::str::from_utf8(&output.stdout).unwrap(),
        Some(JsonWebKeyId::new("synthetic-key".into())),
    )
    .unwrap()
}

async fn discovery(AxumState(p): AxumState<Arc<Provider>>) -> Json<Value> {
    Json(json!({
        "issuer": p.issuer,
        "authorization_endpoint": format!("{}authorize", p.issuer),
        "token_endpoint": format!("{}token", p.issuer),
        "jwks_uri": format!("{}jwks", p.issuer),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
    }))
}

async fn jwks(AxumState(p): AxumState<Arc<Provider>>) -> Json<Value> {
    let _ = CoreJsonWebKeyType::RSA;
    Json(json!({"keys": [p.key.as_verification_key()]}))
}

async fn token(
    AxumState(p): AxumState<Arc<Provider>>,
    Form(form): Form<BTreeMap<String, String>>,
) -> Json<Value> {
    *p.token_requests.lock().unwrap() += 1;
    // PKCE: the verifier must hash to the challenge sent at /auth/login.
    let verifier = PkceCodeVerifier::new(form["code_verifier"].clone());
    assert_eq!(
        PkceCodeChallenge::from_code_verifier_sha256(&verifier).as_str(),
        p.challenge.lock().unwrap().as_str()
    );
    assert_eq!(form["grant_type"], "authorization_code");
    let changes = p.changes.lock().unwrap().clone();
    let issuer = changes.iss.clone().unwrap_or(p.issuer.clone());
    let now = chrono::Utc::now();
    let expiry = if changes.expired {
        chrono::DateTime::from_timestamp(1, 0).unwrap()
    } else {
        now + chrono::Duration::seconds(300)
    };
    let claims = IdTokenClaims::new(
        IssuerUrl::new(issuer).unwrap(),
        vec![Audience::new(
            changes.aud.clone().unwrap_or("synthetic-client".into()),
        )],
        expiry,
        now,
        StandardClaims::new(SubjectIdentifier::new(
            changes.sub.clone().unwrap_or("owner-uuid".into()),
        )),
        Extra {
            nonce_supported: changes.nonce_supported,
        },
    )
    .set_nonce(Some(Nonce::new(
        changes
            .nonce
            .clone()
            .unwrap_or(p.nonce.lock().unwrap().clone()),
    )));
    let access = AccessToken::new("transient-token".into());
    let mut encoded = TestIdToken::new(
        claims,
        &p.key,
        CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
        Some(&access),
        None,
    )
    .unwrap()
    .to_string();
    if changes.tamper {
        let parts: Vec<&str> = encoded.split('.').collect();
        let first = if parts[2].starts_with('A') { 'B' } else { 'A' };
        encoded = format!("{}.{}.{}{}", parts[0], parts[1], first, &parts[2][1..]);
    }
    Json(
        json!({"access_token": access.secret(), "token_type": "Bearer", "id_token": encoded, "expires_in": 300}),
    )
}

struct Fixture {
    _dir: tempfile::TempDir,
    state: Arc<notes_rag_rs::web::state::State>,
    auth: Arc<OwnerAuth>,
    provider: Arc<Provider>,
    public: Arc<MockUpstream>,
    owner: Arc<MockUpstream>,
    router: axum::Router,
    corpus: Option<Corpus>,
}

async fn fixture(with_reader: bool) -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!(
        "http://{}/application/o/notes-rag/",
        listener.local_addr().unwrap()
    );
    let provider = Arc::new(Provider {
        issuer: issuer.clone(),
        key: rsa_key(),
        challenge: Mutex::default(),
        nonce: Mutex::default(),
        changes: Mutex::default(),
        token_requests: Mutex::default(),
    });
    let app = Router::new()
        .route(
            "/application/o/notes-rag/.well-known/openid-configuration",
            route_get(discovery),
        )
        .route("/application/o/notes-rag/jwks", route_get(jwks))
        .route("/application/o/notes-rag/token", route_post(token))
        .with_state(provider.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let (_clock, source) = TestClock::new(0);
    let real_clock: notes_rag_rs::web::state::Clock = notes_rag_rs::web::state::system_clock();
    let _ = source;
    let state = state(dir.path(), 100, 100, real_clock);
    let auth = Arc::new(
        OwnerAuth::new(
            state.clone(),
            "synthetic-client",
            &"c".repeat(48),
            &"s".repeat(48),
            "owner-uuid",
            ORIGIN,
            &issuer,
        )
        .unwrap(),
    );
    let revision = "a".repeat(64);
    let owner_revision = notes_rag_rs::policy::owner_revision(&revision);
    let public = Arc::new(MockUpstream::default());
    let owner = Arc::new(MockUpstream::default());
    for (gateway, rev, title) in [
        (&public, &revision, "public"),
        (&owner, &owner_revision, "private"),
    ] {
        *gateway.folders_result.lock().unwrap() = Some(
            json!({"access_revision": rev, "indexed_pages": 1, "folders": [{"path": title, "pages": 1}]}),
        );
        *gateway.search_result.lock().unwrap() = Some(
            json!({"index": {"access_revision": rev, "indexed_pages": 1},
            "results": [{"title": title, "content": title, "source_url": format!("{PREFIX}{title}")}]}),
        );
    }
    let corpus = with_reader.then(Corpus::new);
    let (revision, reader) = match &corpus {
        Some(corpus) => (corpus.revision.clone(), Some(corpus.reader())),
        None => (revision, None),
    };
    if corpus.is_some() {
        let owner_revision = notes_rag_rs::policy::owner_revision(&revision);
        for (gateway, rev) in [(&public, &revision), (&owner, &owner_revision)] {
            gateway.folders_result.lock().unwrap().as_mut().unwrap()["access_revision"] =
                json!(rev);
            gateway.search_result.lock().unwrap().as_mut().unwrap()["index"]["access_revision"] =
                json!(rev);
        }
    }
    let web = WebApp::new(
        state.clone(),
        public.clone(),
        Some(revision),
        Some(auth.clone()),
        Some(owner.clone()),
        reader,
        PREFIX.into(),
        NOTE_PREFIX.into(),
    )
    .unwrap();
    Fixture {
        _dir: dir,
        state,
        auth,
        provider,
        public,
        owner,
        router: router(Arc::new(web)),
        corpus,
    }
}

/// A tiny cookie jar: the browser side of the flow.
#[derive(Default)]
struct Jar(BTreeMap<String, String>);

impl Jar {
    fn absorb(&mut self, response: &Response) {
        for cookie in response.cookies() {
            let pair = cookie.split(';').next().unwrap();
            let (name, value) = pair.split_once('=').unwrap();
            let cleared =
                cookie.contains("Max-Age=0") || cookie.contains("expires=Thu, 01 Jan 1970");
            if cleared {
                self.0.remove(name);
            } else {
                self.0.insert(name.to_string(), value.to_string());
            }
        }
    }

    fn header(&self) -> String {
        self.0
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

impl Fixture {
    async fn request(
        &self,
        jar: &mut Jar,
        method: http::Method,
        uri: &str,
        extra: &[(&str, &str)],
        body: Option<Value>,
    ) -> Response {
        let cookie = jar.header();
        let mut headers: Vec<(&str, &str)> = extra.to_vec();
        if !cookie.is_empty() {
            headers.push(("cookie", &cookie));
        }
        let response = match body {
            Some(body) => post_json(&self.router, uri, &body, &headers).await,
            None => {
                send(
                    &self.router,
                    method,
                    uri,
                    &headers,
                    axum::body::Body::empty(),
                )
                .await
            }
        };
        jar.absorb(&response);
        response
    }

    async fn login(&self, jar: &mut Jar, next: Option<&str>) -> Response {
        let uri = match next {
            Some(next) => format!(
                "/auth/login?next={}",
                notes_rag_rs::py::url::quote_plus(next)
            ),
            None => "/auth/login".into(),
        };
        let start = self.request(jar, http::Method::GET, &uri, &[], None).await;
        assert_eq!(start.status, StatusCode::FOUND, "{}", start.text());
        let location = start.header("location").unwrap().to_string();
        let query = location.split_once('?').unwrap().1.as_bytes().to_vec();
        let param = |name| notes_rag_rs::py::url::query_param(&query, name).unwrap();
        *self.provider.nonce.lock().unwrap() = param("nonce");
        *self.provider.challenge.lock().unwrap() = param("code_challenge");
        assert_eq!(param("code_challenge_method"), "S256");
        assert_eq!(param("redirect_uri"), format!("{ORIGIN}/auth/callback"));
        assert!(
            param("scope").split(' ').any(|s| s == "openid") && param("scope").contains("profile")
        );
        let callback = format!(
            "/auth/callback?code=synthetic-code&state={}",
            notes_rag_rs::py::url::quote_plus(&param("state"))
        );
        self.request(jar, http::Method::GET, &callback, &[], None)
            .await
    }

    async fn search(&self, jar: &mut Jar) -> Response {
        self.request(
            jar,
            http::Method::POST,
            "/api/search",
            &[("origin", ORIGIN)],
            Some(json!({"query": "same query"})),
        )
        .await
    }
}

#[tokio::test]
async fn graph_return_path_is_allowlisted() {
    let f = fixture(false).await;
    let mut jar = Jar::default();
    assert_eq!(
        f.login(&mut jar, Some("/graph")).await.header("location"),
        Some("/graph")
    );
    for path in [
        "https://evil.invalid",
        "//evil.invalid",
        "/read?path=private",
        "/graph/evil",
    ] {
        assert_eq!(
            f.login(&mut Jar::default(), Some(path))
                .await
                .header("location"),
            Some("/")
        );
    }
}

#[tokio::test]
async fn real_signed_oidc_owner_cache_isolation_and_logout() {
    let f = fixture(false).await;
    let mut jar = Jar::default();
    assert_eq!(
        f.search(&mut jar).await.json()["results"][0]["title"],
        "public"
    );
    assert_eq!(
        f.request(&mut jar, http::Method::GET, "/api/folders", &[], None)
            .await
            .json()["folders"][0]["path"],
        "public"
    );
    let response = f.login(&mut jar, None).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let cookie = response
        .cookies()
        .into_iter()
        .find(|c| c.starts_with(COOKIE))
        .unwrap();
    for flag in [
        "HttpOnly",
        "Secure",
        "SameSite=lax",
        "Max-Age=3600",
        "Path=/",
    ] {
        assert!(cookie.contains(flag), "{cookie}");
    }
    assert!(!cookie.contains("transient-token"));
    assert!(
        !jar.0.contains_key(FLOW_COOKIE),
        "flow cookie must be cleared after the callback"
    );
    assert_eq!(
        f.request(&mut jar, http::Method::GET, "/api/session", &[], None)
            .await
            .json()["owner"],
        true
    );
    assert_eq!(
        f.search(&mut jar).await.json()["results"][0]["title"],
        "private"
    );
    assert_eq!(
        f.request(&mut jar, http::Method::GET, "/api/folders", &[], None)
            .await
            .json()["folders"][0]["path"],
        "private"
    );
    let stolen = jar.0[COOKIE].clone();
    let evil = f
        .request(
            &mut jar,
            http::Method::POST,
            "/auth/logout",
            &[("origin", "https://evil.invalid")],
            None,
        )
        .await;
    assert_eq!(evil.status, StatusCode::FORBIDDEN);
    let logout = f
        .request(
            &mut jar,
            http::Method::POST,
            "/auth/logout",
            &[("origin", ORIGIN)],
            None,
        )
        .await;
    assert_eq!(logout.status, StatusCode::OK);
    assert_eq!(logout.json(), json!({"owner": false}));
    jar.0.insert(COOKIE.into(), stolen);
    assert_eq!(
        f.request(&mut jar, http::Method::GET, "/api/session", &[], None)
            .await
            .json()["owner"],
        false
    );
    assert_eq!(
        f.search(&mut jar).await.json()["results"][0]["title"],
        "public"
    );
    assert_eq!(f.owner.searches(), 1);
}

#[tokio::test]
async fn wrong_subject_issuer_audience_nonce_expiry_signature_denied() {
    let f = fixture(false).await;
    let cases = [
        Changes {
            sub: Some("another-user".into()),
            ..Default::default()
        },
        Changes {
            iss: Some("https://evil.invalid/".into()),
            ..Default::default()
        },
        Changes {
            aud: Some("another-client".into()),
            ..Default::default()
        },
        Changes {
            nonce: Some("wrong".into()),
            ..Default::default()
        },
        Changes {
            expired: true,
            ..Default::default()
        },
        Changes {
            nonce_supported: Some(false),
            ..Default::default()
        },
        Changes {
            tamper: true,
            ..Default::default()
        },
    ];
    for change in cases {
        *f.provider.changes.lock().unwrap() = change;
        let mut jar = Jar::default();
        let response = f.login(&mut jar, None).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert!(
            response.json()["error"]
                .as_str()
                .unwrap()
                .contains("본인 전용 계정이 아닙니다")
        );
        assert_eq!(
            f.request(&mut jar, http::Method::GET, "/api/session", &[], None)
                .await
                .json()["owner"],
            false
        );
    }
    assert_eq!(f.owner.searches(), 0);
}

#[tokio::test]
async fn missing_state_cookie_forged_headers_and_role_inputs_denied() {
    let f = fixture(false).await;
    let mut jar = Jar::default();
    assert_eq!(
        f.request(
            &mut jar,
            http::Method::GET,
            "/auth/callback?code=x&state=x",
            &[],
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    jar.0.insert(COOKIE.into(), "fake-token-".repeat(6));
    let forged = f
        .request(
            &mut jar,
            http::Method::GET,
            "/api/session",
            &[("x-user", "akadmin"), ("x-forwarded-user", "akadmin")],
            None,
        )
        .await;
    assert_eq!(forged.json()["owner"], false);
    let role = f
        .request(
            &mut jar,
            http::Method::POST,
            "/api/search",
            &[],
            Some(json!({"query": "x", "owner": true})),
        )
        .await;
    assert_eq!(role.status, StatusCode::BAD_REQUEST);
    let private = f
        .request(
            &mut jar,
            http::Method::POST,
            "/api/search",
            &[],
            Some(json!({"query": "x", "folder": "private"})),
        )
        .await;
    assert_eq!(private.status, StatusCode::BAD_REQUEST);
    assert_eq!(f.owner.searches(), 0);
}

#[tokio::test]
async fn expiry_and_owner_policy_change_revoke_session() {
    let f = fixture(false).await;
    let mut jar = Jar::default();
    assert_eq!(f.login(&mut jar, None).await.status, StatusCode::SEE_OTHER);
    f.state
        .connect()
        .unwrap()
        .execute("UPDATE owner_sessions SET expires=1", [])
        .unwrap();
    assert_eq!(
        f.request(&mut jar, http::Method::GET, "/api/session", &[], None)
            .await
            .json()["owner"],
        false
    );
    assert_eq!(f.login(&mut jar, None).await.status, StatusCode::SEE_OTHER);
    *f.auth.subject.write().unwrap() = "new-owner".into();
    assert_eq!(
        f.request(&mut jar, http::Method::GET, "/api/session", &[], None)
            .await
            .json()["owner"],
        false
    );
}

#[tokio::test]
async fn owner_post_requires_same_origin() {
    let f = fixture(false).await;
    let mut jar = Jar::default();
    assert_eq!(f.login(&mut jar, None).await.status, StatusCode::SEE_OTHER);
    let response = f
        .request(
            &mut jar,
            http::Method::POST,
            "/api/search",
            &[],
            Some(json!({"query": "x"})),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(f.owner.searches(), 0);
}

#[tokio::test]
async fn public_traffic_exhaustion_cannot_block_login_or_owner_search() {
    let f = fixture(false).await;
    for _ in 0..60 {
        f.state.traffic("public").unwrap();
    }
    let mut jar = Jar::default();
    assert_eq!(
        f.search(&mut jar).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(f.login(&mut jar, None).await.status, StatusCode::SEE_OTHER);
    assert_eq!(f.search(&mut jar).await.status, StatusCode::OK);
}

#[tokio::test]
async fn flow_cookie_replay_cannot_repeat_exchange_or_consume_callback_budget() {
    let f = fixture(false).await;
    let mut jar = Jar::default();
    let start = f
        .request(&mut jar, http::Method::GET, "/auth/login", &[], None)
        .await;
    let location = start.header("location").unwrap().to_string();
    let query = location.split_once('?').unwrap().1.as_bytes().to_vec();
    let param = |name| notes_rag_rs::py::url::query_param(&query, name).unwrap();
    *f.provider.nonce.lock().unwrap() = param("nonce");
    *f.provider.challenge.lock().unwrap() = param("code_challenge");
    let flow = jar.0[FLOW_COOKIE].clone();
    let callback = format!(
        "/auth/callback?code=synthetic-code&state={}",
        notes_rag_rs::py::url::quote_plus(&param("state"))
    );
    assert_eq!(
        f.request(&mut jar, http::Method::GET, &callback, &[], None)
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    let mut replay = Jar::default();
    replay.0.insert(FLOW_COOKIE.into(), flow);
    assert_eq!(
        f.request(&mut replay, http::Method::GET, &callback, &[], None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(*f.provider.token_requests.lock().unwrap(), 1);
    let db = f.state.connect().unwrap();
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM traffic_v2 WHERE bucket='auth-callback'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn login_limit_reports_429_not_service_failure() {
    let f = fixture(false).await;
    for _ in 0..30 {
        f.state.traffic("auth-login").unwrap();
    }
    let result = f
        .request(
            &mut Jar::default(),
            http::Method::GET,
            "/auth/login",
            &[],
            None,
        )
        .await;
    assert_eq!(result.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(result.header("retry-after").is_some());
}

#[tokio::test]
async fn tampered_flow_cookie_is_rejected_without_token_exchange() {
    let f = fixture(false).await;
    let mut jar = Jar::default();
    f.request(&mut jar, http::Method::GET, "/auth/login", &[], None)
        .await;
    let value = jar.0[FLOW_COOKIE].clone();
    let (payload, signature) = value.split_once('.').unwrap();
    let mut forged = payload.as_bytes().to_vec();
    forged[0] = if forged[0] == b'A' { b'B' } else { b'A' };
    jar.0.insert(
        FLOW_COOKIE.into(),
        format!("{}.{signature}", String::from_utf8(forged).unwrap()),
    );
    let response = f
        .request(
            &mut jar,
            http::Method::GET,
            "/auth/callback?code=x&state=x",
            &[],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(*f.provider.token_requests.lock().unwrap(), 0);
}

#[tokio::test]
async fn revoked_session_during_upstream_search_cannot_receive_private_result() {
    let f = fixture(false).await;
    let mut jar = Jar::default();
    assert_eq!(f.login(&mut jar, None).await.status, StatusCode::SEE_OTHER);
    let state = f.state.clone();
    *f.owner.during_search.lock().unwrap() = Some(Box::new(move || {
        state
            .connect()
            .unwrap()
            .execute("DELETE FROM owner_sessions", [])
            .unwrap();
    }));
    let response = f.search(&mut jar).await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert!(!response.text().contains("private"));
}

#[tokio::test]
async fn reader_uses_signed_owner_session_and_revokes_on_logout() {
    let f = fixture(true).await;
    let _corpus = f.corpus.as_ref().unwrap();
    let mut jar = Jar::default();
    let page = "/read?path=private%2Fnote.md";
    let asset = "/read-asset?path=private%2Fnote.md&asset=private%2Fnote.assets%2Fimage.png";
    assert_eq!(
        f.request(&mut jar, http::Method::GET, page, &[], None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request(&mut jar, http::Method::GET, asset, &[], None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(f.login(&mut jar, None).await.status, StatusCode::SEE_OTHER);
    assert_eq!(
        f.request(&mut jar, http::Method::GET, page, &[], None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        f.request(&mut jar, http::Method::GET, asset, &[], None)
            .await
            .body
            .as_ref(),
        b"synthetic-image"
    );
    assert_eq!(
        f.request(
            &mut jar,
            http::Method::POST,
            "/auth/logout",
            &[("origin", ORIGIN)],
            None
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        f.request(&mut jar, http::Method::GET, page, &[], None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request(&mut jar, http::Method::GET, asset, &[], None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let _ = (&f.public,);
}
