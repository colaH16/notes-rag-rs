//! PostgreSQL connections with libpq-compatible TLS settings.
//!
//! Python used psycopg (libpq). `tokio-postgres` is used here because, like
//! libpq, it does not override session parameters such as `TimeZone`
//! (`updated_at` is reported in the server's default zone, exactly as
//! psycopg did). libpq's TLS options are interpreted here:
//!
//! | sslmode            | TLS        | certificate check                         |
//! |--------------------|------------|-------------------------------------------|
//! | disable            | never      | –                                         |
//! | prefer (default)   | if offered | chain only if `sslrootcert` is given      |
//! | require            | always     | chain only if `sslrootcert` is given      |
//! | verify-ca          | always     | chain against `sslrootcert`               |
//! | verify-full        | always     | chain against `sslrootcert` + host name   |
//!
//! `sslrootcert=system` uses the OS trust store with PostgreSQL 16 semantics:
//! it implies `verify-full` when `sslmode` is unset and rejects every weaker
//! explicit mode. Settings come from `PG*` variables overridden by an
//! optional explicit conninfo (see `conninfo`). `allow`, unknown keywords and
//! `~/.postgresql/root.crt` discovery are not supported and fail closed
//! rather than silently weakening TLS.

use super::conninfo::{self, Options, invalid};
use crate::config::Env;
use crate::fault::{Fault, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::config::{ChannelBinding, LoadBalanceHosts, SslMode, TargetSessionAttrs};

/// libpq keywords that are honoured exactly; anything else is rejected.
const SUPPORTED: [&str; 25] = [
    "host",
    "hostaddr",
    "port",
    "dbname",
    "user",
    "password",
    "options",
    "application_name",
    "connect_timeout",
    "sslmode",
    "sslrootcert",
    "sslcert",
    "sslkey",
    "sslnegotiation",
    "sslsni",
    "gssencmode",
    "ssl_min_protocol_version",
    "target_session_attrs",
    "channel_binding",
    "load_balance_hosts",
    "keepalives",
    "keepalives_idle",
    "keepalives_interval",
    "keepalives_retries",
    "tcp_user_timeout",
];

/// Keywords accepted only with values whose behaviour matches libpq here.
const FIXED: [(&str, &[&str]); 4] = [
    ("sslnegotiation", &["postgres"]),
    // rustls always sends SNI for host names.
    ("sslsni", &["1"]),
    // No GSSAPI support: `prefer` falls back exactly like a libpq without it.
    ("gssencmode", &["disable", "prefer"]),
    ("ssl_min_protocol_version", &["TLSv1.2", "TLSv1.3"]),
];

#[derive(Clone)]
pub struct Database {
    config: tokio_postgres::Config,
    tls: Option<tokio_postgres_rustls::MakeRustlsConnect>,
}

impl Database {
    /// Production: libpq `PG*` variables overridden by a non-empty
    /// `RAG_DATABASE_URL`, read from an injected environment snapshot.
    pub fn from_env(env: &Env) -> std::result::Result<Self, String> {
        Self::from_options(&conninfo::resolve(env)?)
    }

    /// An explicit conninfo (URL or `key=value`) without environment defaults.
    pub fn from_dsn(dsn: &str) -> std::result::Result<Self, String> {
        Self::from_options(&conninfo::parse(dsn)?)
    }

    pub fn from_options(options: &Options) -> std::result::Result<Self, String> {
        let mut config = connection(options)?;
        let tls = match tls_policy(options)? {
            None => {
                config.ssl_mode(SslMode::Disable);
                None
            }
            Some((mode, check)) => {
                config.ssl_mode(mode);
                let verification = match check {
                    Check::Encrypt => Verification::None,
                    Check::Chain(root) => Verification::Chain(roots(root)?),
                    Check::Full(root) => Verification::Full(roots(root)?),
                };
                let tls13_only = options.get("ssl_min_protocol_version") == Some("TLSv1.3");
                let client = client_config(
                    verification,
                    options.get("sslcert"),
                    options.get("sslkey"),
                    tls13_only,
                )?;
                Some(tokio_postgres_rustls::MakeRustlsConnect::new(client))
            }
        };
        Ok(Self { config, tls })
    }

    /// A fresh connection per snapshot, like `psycopg.connect` in Python.
    pub async fn connect(&self) -> Result<tokio_postgres::Client> {
        let failure = |_| Fault::Other("database connection failed");
        let client = match &self.tls {
            Some(tls) => {
                let (client, connection) =
                    self.config.connect(tls.clone()).await.map_err(failure)?;
                tokio::spawn(connection);
                client
            }
            None => {
                let (client, connection) = self
                    .config
                    .connect(tokio_postgres::NoTls)
                    .await
                    .map_err(failure)?;
                tokio::spawn(connection);
                client
            }
        };
        Ok(client)
    }
}

/// Keyword names reach error messages only when they look like keywords.
fn shown(key: &str) -> &str {
    let keyword = key.len() <= 32 && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    if keyword { key } else { "(unrecognised)" }
}

fn seconds(options: &Options, key: &str) -> std::result::Result<Option<Duration>, String> {
    let Some(value) = options.get(key) else {
        return Ok(None);
    };
    let value: i64 = value
        .trim()
        .parse()
        .map_err(|_| invalid(&format!("{key} must be an integer")))?;
    // libpq treats zero/negative timeouts as "not set".
    Ok((value > 0).then(|| Duration::from_secs(value as u64)))
}

/// Everything except TLS, built with setters so the password is never
/// re-serialised into a string that could end up in an error.
fn connection(options: &Options) -> std::result::Result<tokio_postgres::Config, String> {
    if let Some(key) = options.keys().find(|key| !SUPPORTED.contains(key)) {
        return Err(invalid(&format!("unsupported option {}", shown(key))));
    }
    for (key, allowed) in FIXED {
        if options
            .get(key)
            .is_some_and(|value| !allowed.contains(&value))
        {
            return Err(invalid(&format!("unsupported {key} value")));
        }
    }
    let list = |key: &str| options.get(key).map(|v| v.split(',').collect::<Vec<_>>());
    let hosts = list("host").unwrap_or_default();
    let addresses = list("hostaddr").unwrap_or_default();
    let ports = list("port").unwrap_or_default();
    if hosts.iter().chain(&addresses).any(|h| h.is_empty()) {
        return Err(invalid("empty host entry"));
    }
    // libpq would fall back to a local Unix socket; never guess here.
    if hosts.is_empty() && addresses.is_empty() {
        return Err(invalid("host is required (PGHOST or URL host)"));
    }
    let targets = hosts.len().max(addresses.len());
    if !hosts.is_empty() && !addresses.is_empty() && hosts.len() != addresses.len()
        || ports.len() > 1 && ports.len() != targets
    {
        return Err(invalid("host, hostaddr and port lists do not match"));
    }
    let mut config = tokio_postgres::Config::new();
    for host in hosts {
        config.host(host);
    }
    for address in addresses {
        config.hostaddr(
            address
                .parse::<IpAddr>()
                .map_err(|_| invalid("hostaddr must be an IP address"))?,
        );
    }
    for port in ports {
        config.port(match port {
            "" => 5432,
            _ => port
                .parse::<u16>()
                .ok()
                .filter(|&p| p != 0)
                .ok_or_else(|| invalid("port must be 1..65535"))?,
        });
    }
    // libpq would use the OS account name; uid 4261 has no passwd entry.
    config.user(
        options
            .get("user")
            .ok_or_else(|| invalid("user is required (PGUSER or URL user)"))?,
    );
    if let Some(dbname) = options.get("dbname") {
        config.dbname(dbname);
    }
    if let Some(password) = options.get("password") {
        config.password(password);
    }
    if let Some(value) = options.get("options") {
        config.options(value);
    }
    if let Some(value) = options.get("keepalives") {
        let value: i64 = value
            .trim()
            .parse()
            .map_err(|_| invalid("keepalives must be an integer"))?;
        config.keepalives(value != 0);
    }
    if let Some(idle) = seconds(options, "keepalives_idle")? {
        config.keepalives_idle(idle);
    }
    if let Some(interval) = seconds(options, "keepalives_interval")? {
        config.keepalives_interval(interval);
    }
    if let Some(value) = options.get("keepalives_retries") {
        config.keepalives_retries(
            value
                .trim()
                .parse()
                .map_err(|_| invalid("keepalives_retries must be an integer"))?,
        );
    }
    if let Some(timeout) = seconds(options, "tcp_user_timeout")? {
        config.tcp_user_timeout(timeout);
    }
    // Validated like libpq, then overridden below like psycopg's kwargs.
    seconds(options, "connect_timeout")?;
    if let Some(value) = options.get("target_session_attrs") {
        config.target_session_attrs(match value {
            "any" => TargetSessionAttrs::Any,
            "read-write" => TargetSessionAttrs::ReadWrite,
            "read-only" => TargetSessionAttrs::ReadOnly,
            _ => return Err(invalid("unsupported target_session_attrs value")),
        });
    }
    if let Some(value) = options.get("channel_binding") {
        config.channel_binding(match value {
            "disable" => ChannelBinding::Disable,
            "prefer" => ChannelBinding::Prefer,
            "require" => ChannelBinding::Require,
            _ => return Err(invalid("unsupported channel_binding value")),
        });
    }
    if let Some(value) = options.get("load_balance_hosts") {
        config.load_balance_hosts(match value {
            "disable" => LoadBalanceHosts::Disable,
            "random" => LoadBalanceHosts::Random,
            _ => return Err(invalid("unsupported load_balance_hosts value")),
        });
    }
    // psycopg.connect(..., connect_timeout=5, application_name='notes-rag-mcp')
    config.connect_timeout(Duration::from_secs(5));
    config.application_name("notes-rag-mcp");
    Ok(config)
}

/// How the server certificate is checked once TLS is negotiated.
#[derive(Debug, PartialEq)]
enum Check<'a> {
    /// `prefer`/`require` without a root: encryption only (libpq).
    Encrypt,
    /// Trusted chain, any host name.
    Chain(&'a str),
    /// Trusted chain and host name.
    Full(&'a str),
}

/// libpq `sslmode`/`sslrootcert` semantics; `None` means no TLS.
fn tls_policy(options: &Options) -> std::result::Result<Option<(SslMode, Check<'_>)>, String> {
    let root = options.get("sslrootcert");
    let system = root == Some("system");
    // PostgreSQL 16: sslrootcert=system defaults to, and requires, verify-full.
    let mode = options
        .get("sslmode")
        .unwrap_or(if system { "verify-full" } else { "prefer" });
    if system && mode != "verify-full" {
        return Err(invalid("sslrootcert=system requires sslmode=verify-full"));
    }
    Ok(match (mode, root) {
        ("disable", _) => None,
        ("prefer", None) => Some((SslMode::Prefer, Check::Encrypt)),
        ("require", None) => Some((SslMode::Require, Check::Encrypt)),
        ("prefer", Some(root)) => Some((SslMode::Prefer, Check::Chain(root))),
        ("require" | "verify-ca", Some(root)) => Some((SslMode::Require, Check::Chain(root))),
        ("verify-full", Some(root)) => Some((SslMode::Require, Check::Full(root))),
        ("verify-ca" | "verify-full", None) => {
            return Err(invalid(
                "sslmode verify-ca/verify-full requires sslrootcert",
            ));
        }
        _ => return Err(invalid("unsupported sslmode")),
    })
}

enum Verification {
    None,
    Chain(RootCertStore),
    Full(RootCertStore),
}

fn roots(path: &str) -> std::result::Result<RootCertStore, String> {
    let mut store = RootCertStore::empty();
    if path == "system" {
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = store.add(cert);
        }
    } else {
        for cert in CertificateDer::pem_file_iter(path).map_err(|_| "cannot read sslrootcert")? {
            store
                .add(cert.map_err(|_| "invalid sslrootcert")?)
                .map_err(|_| "invalid sslrootcert")?;
        }
    }
    if store.is_empty() {
        return Err("sslrootcert contains no certificates".into());
    }
    Ok(store)
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn client_config(
    verification: Verification,
    cert: Option<&str>,
    key: Option<&str>,
    tls13_only: bool,
) -> std::result::Result<ClientConfig, String> {
    static TLS13_ONLY: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];
    let versions = if tls13_only {
        TLS13_ONLY
    } else {
        rustls::DEFAULT_VERSIONS
    };
    let builder = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(versions)
        .map_err(|_| "TLS configuration failed")?;
    let builder = match verification {
        Verification::Full(store) => builder.with_root_certificates(store),
        Verification::Chain(store) => {
            let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(
                Arc::new(store),
                provider(),
            )
            .build()
            .map_err(|_| "TLS configuration failed")?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(ChainOnly(inner)))
        }
        Verification::None => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Unverified(provider()))),
    };
    match (cert, key) {
        (None, None) => Ok(builder.with_no_client_auth()),
        (Some(cert), Some(key)) => {
            let chain = CertificateDer::pem_file_iter(cert)
                .map_err(|_| "cannot read sslcert")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| "invalid sslcert")?;
            let key = PrivateKeyDer::from_pem_file(key).map_err(|_| "cannot read sslkey")?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|_| "invalid client certificate".into())
        }
        _ => Err("sslcert and sslkey must be configured together".into()),
    }
}

/// `verify-ca` (or require/prefer with a root): trusted chain, any host name.
#[derive(Debug)]
struct ChainOnly(Arc<rustls::client::WebPkiServerVerifier>);

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// libpq `require`/`prefer` without a root certificate: encryption only.
#[derive(Debug)]
struct Unverified(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for Unverified {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
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

    /// The production contract: no URL, libpq variables only.
    const PRODUCTION: [(&str, &str); 7] = [
        ("PGHOST", "pg.internal"),
        ("PGPORT", "6432"),
        ("PGDATABASE", "rag"),
        ("PGUSER", "reader"),
        ("PGPASSWORD", "pw-secret-123"),
        ("PGSSLMODE", "verify-full"),
        ("PGSSLROOTCERT", "/ca/ca.crt"),
    ];

    fn with(extra: &[(&str, &str)]) -> Options {
        let mut pairs = PRODUCTION.to_vec();
        pairs.extend_from_slice(extra);
        conninfo::resolve(&env(&pairs)).ok().unwrap()
    }

    #[test]
    fn pg_environment_alone_builds_a_verify_full_connection() {
        let options = with(&[]);
        let config = connection(&options).unwrap();
        assert_eq!(
            config.get_hosts(),
            [tokio_postgres::config::Host::Tcp("pg.internal".into())]
        );
        assert_eq!(config.get_ports(), [6432]);
        assert_eq!(config.get_dbname(), Some("rag"));
        assert_eq!(config.get_user(), Some("reader"));
        assert_eq!(config.get_password(), Some(&b"pw-secret-123"[..]));
        assert_eq!(config.get_application_name(), Some("notes-rag-mcp"));
        assert_eq!(config.get_connect_timeout(), Some(&Duration::from_secs(5)));
        assert_eq!(
            tls_policy(&options).unwrap(),
            Some((SslMode::Require, Check::Full("/ca/ca.crt")))
        );
    }

    #[test]
    fn url_fields_override_environment_without_downgrading_tls() {
        // URL without sslmode: PGSSLMODE/PGSSLROOTCERT still apply (B2).
        let options = with(&[("RAG_DATABASE_URL", "postgresql://debug.internal/other")]);
        let config = connection(&options).unwrap();
        assert_eq!(
            config.get_hosts(),
            [tokio_postgres::config::Host::Tcp("debug.internal".into())]
        );
        assert_eq!(
            (config.get_dbname(), config.get_ports()),
            (Some("other"), &[6432][..])
        );
        assert_eq!(
            tls_policy(&options).unwrap(),
            Some((SslMode::Require, Check::Full("/ca/ca.crt")))
        );
        // An explicit URL sslmode wins over the environment.
        let options = with(&[("RAG_DATABASE_URL", "postgresql://h/db?sslmode=disable")]);
        assert_eq!(tls_policy(&options).unwrap(), None);
        let options = with(&[("RAG_DATABASE_URL", "host=h sslmode=verify-ca")]);
        assert_eq!(
            tls_policy(&options).unwrap(),
            Some((SslMode::Require, Check::Chain("/ca/ca.crt")))
        );
        // Without any TLS setting the libpq default `prefer` applies.
        let bare = conninfo::parse("host=h user=u").ok().unwrap();
        assert_eq!(
            tls_policy(&bare).unwrap(),
            Some((SslMode::Prefer, Check::Encrypt))
        );
    }

    #[test]
    fn system_roots_require_verify_full() {
        let system = |mode: Option<&str>| {
            let mut dsn = "host=h user=u sslrootcert=system".to_string();
            if let Some(mode) = mode {
                dsn.push_str(&format!(" sslmode={mode}"));
            }
            tls_policy(&conninfo::parse(&dsn).ok().unwrap()).map(|p| p.map(|(m, _)| m))
        };
        // Unset sslmode is inferred as verify-full (PostgreSQL 16).
        let options = conninfo::parse("host=h user=u sslrootcert=system")
            .ok()
            .unwrap();
        assert_eq!(
            tls_policy(&options).unwrap(),
            Some((SslMode::Require, Check::Full("system")))
        );
        assert_eq!(system(Some("verify-full")), Ok(Some(SslMode::Require)));
        for weaker in ["disable", "prefer", "require", "verify-ca"] {
            let error = system(Some(weaker)).unwrap_err();
            assert!(
                error.contains("requires sslmode=verify-full"),
                "{weaker}: {error}"
            );
        }
        // The environment's explicit weaker mode is rejected too.
        let error = tls_policy(&with(&[
            ("PGSSLMODE", "require"),
            ("PGSSLROOTCERT", "system"),
        ]))
        .unwrap_err();
        assert!(error.contains("verify-full"));
    }

    #[test]
    fn unsupported_security_settings_are_rejected() {
        for extra in [
            ("PGSERVICE", "svc"),
            ("PGSERVICEFILE", "/s"),
            ("PGPASSFILE", "/p"),
            ("PGSSLCRL", "/crl"),
            ("PGSSLCRLDIR", "/crl.d"),
            ("PGREQUIRESSL", "1"),
            ("PGREQUIREAUTH", "scram-sha-256"),
            ("PGSSLCERTMODE", "require"),
            ("PGSSLMAXPROTOCOLVERSION", "TLSv1.2"),
            ("PGSSLMINPROTOCOLVERSION", "TLSv1.1"),
            ("PGSSLNEGOTIATION", "direct"),
            ("PGSSLSNI", "0"),
            ("PGGSSENCMODE", "require"),
            ("PGTARGETSESSIONATTRS", "primary"),
            ("PGCHANNELBINDING", "maybe"),
        ] {
            assert!(
                connection(&with(&[extra])).is_err(),
                "{extra:?} must be rejected"
            );
        }
        assert!(conninfo::resolve(&env(&[("PGTZ", "UTC")])).is_err());
        for accepted in [
            ("PGSSLNEGOTIATION", "postgres"),
            ("PGSSLSNI", "1"),
            ("PGGSSENCMODE", "prefer"),
            ("PGSSLMINPROTOCOLVERSION", "TLSv1.3"),
            ("PGTARGETSESSIONATTRS", "read-write"),
            ("PGCHANNELBINDING", "require"),
            ("PGCONNECT_TIMEOUT", "30"),
        ] {
            assert!(
                connection(&with(&[accepted])).is_ok(),
                "{accepted:?} must be accepted"
            );
        }
        // The same rule applies to URL query parameters.
        let options = with(&[("RAG_DATABASE_URL", "postgresql://h/db?sslcrl=/x")]);
        assert!(connection(&options).is_err());
    }

    #[test]
    fn missing_host_or_user_fails_closed_and_errors_never_show_passwords() {
        assert!(Database::from_env(&Env::default()).is_err());
        let no_user = conninfo::parse("host=h password=pw-secret-123")
            .ok()
            .unwrap();
        let error = connection(&no_user).err().unwrap();
        assert!(error.contains("user is required") && !error.contains("pw-secret"));
        let no_host = conninfo::parse("user=u").ok().unwrap();
        assert!(
            connection(&no_host)
                .err()
                .unwrap()
                .contains("host is required")
        );
        for broken in [
            ("PGPORT", "nope"),
            ("PGHOSTADDR", "pw-secret-123"),
            ("PGSSLMODE", "pw-secret-123"),
            ("PGSSLROOTCERT", "/nonexistent/pw-secret-123"),
        ] {
            let error = Database::from_options(&with(&[broken])).err().unwrap();
            assert!(
                error.starts_with("database configuration invalid")
                    || error.contains("sslrootcert")
            );
            assert!(!error.contains("pw-secret"), "{broken:?}: {error}");
        }
        let error = Database::from_options(&with(&[(
            "RAG_DATABASE_URL",
            "postgresql://h/db?pw-secret-123=1",
        )]))
        .err()
        .unwrap();
        assert!(!error.contains("pw-secret"), "{error}");
        let debug = format!("{:?}", connection(&with(&[])).unwrap());
        assert!(!debug.contains("pw-secret"), "{debug}");
    }

    #[test]
    fn unsafe_or_unknown_settings_fail_closed() {
        assert!(Database::from_dsn("postgresql://u@db/rag?sslmode=verify-full").is_err());
        assert!(Database::from_dsn("postgresql://u@db/rag?sslmode=allow").is_err());
        assert!(Database::from_dsn("postgresql://u@db/rag?sslcrl=/x").is_err());
        assert!(
            Database::from_dsn("postgresql://u@db/rag?sslmode=verify-ca&sslrootcert=/nonexistent")
                .is_err()
        );
        assert!(Database::from_dsn("").is_err());
        assert!(Database::from_dsn("postgresql://u@127.0.0.1/rag?sslmode=disable").is_ok());
        // Quoted key=value DSNs (previously rejected) now parse like libpq.
        assert!(
            Database::from_dsn("host=127.0.0.1 user='u v' password='p \\'w' sslmode=disable")
                .is_ok()
        );
    }
}
