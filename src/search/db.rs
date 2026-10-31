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
//! `sslrootcert=system` uses the OS trust store (PostgreSQL 16 semantics).
//! `allow`, unknown keys and `~/.postgresql/root.crt` discovery are not
//! supported and fail closed rather than silently weakening TLS.

use crate::fault::{Fault, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::config::SslMode;

const TLS_KEYS: [&str; 4] = ["sslmode", "sslrootcert", "sslcert", "sslkey"];

#[derive(Clone)]
pub struct Database {
    config: tokio_postgres::Config,
    tls: Option<tokio_postgres_rustls::MakeRustlsConnect>,
}

impl Database {
    /// Parse `RAG_DATABASE_URL` (URL or `key=value` DSN).
    pub fn from_dsn(dsn: &str) -> std::result::Result<Self, String> {
        let (rest, tls) = split_tls(dsn)?;
        let mut config = tokio_postgres::Config::from_str(&rest)
            .map_err(|_| "RAG_DATABASE_URL cannot be parsed".to_string())?;
        // psycopg.connect(..., connect_timeout=5, application_name='notes-rag-mcp')
        config.connect_timeout(Duration::from_secs(5));
        config.application_name("notes-rag-mcp");
        let mode = tls.get("sslmode").map(String::as_str).unwrap_or("prefer");
        let root = tls.get("sslrootcert").map(String::as_str);
        let verification = match (mode, root) {
            ("disable", _) => None,
            ("prefer" | "require", None) => Some(Verification::None),
            ("prefer" | "require", Some(root)) | ("verify-ca", Some(root)) => {
                Some(Verification::Chain(roots(root)?))
            }
            ("verify-full", Some(root)) => Some(Verification::Full(roots(root)?)),
            ("verify-ca" | "verify-full", None) => {
                return Err("sslmode verify-* requires sslrootcert".into());
            }
            _ => return Err("unsupported sslmode".into()),
        };
        config.ssl_mode(match mode {
            "disable" => SslMode::Disable,
            "prefer" => SslMode::Prefer,
            _ => SslMode::Require,
        });
        let tls = verification
            .map(|verification| client_config(verification, tls.get("sslcert"), tls.get("sslkey")))
            .transpose()?
            .map(tokio_postgres_rustls::MakeRustlsConnect::new);
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

/// Remove libpq TLS options from the DSN (tokio-postgres rejects them) and
/// return them separately. Any other unknown option is rejected later by
/// `tokio_postgres::Config::from_str`.
fn split_tls(
    dsn: &str,
) -> std::result::Result<(String, std::collections::BTreeMap<String, String>), String> {
    let mut tls = std::collections::BTreeMap::new();
    if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        let Some((base, query)) = dsn.split_once('?') else {
            return Ok((dsn.to_string(), tls));
        };
        let mut kept = Vec::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if TLS_KEYS.contains(&key) {
                tls.insert(key.to_string(), crate::py::url::unquote(value));
            } else {
                kept.push(pair);
            }
        }
        let rest = if kept.is_empty() {
            base.to_string()
        } else {
            format!("{base}?{}", kept.join("&"))
        };
        return Ok((rest, tls));
    }
    if dsn.contains('\'') || dsn.contains('\\') {
        return Err("quoted key=value DSNs are not supported; use a postgresql:// URL".into());
    }
    let mut kept = Vec::new();
    for item in dsn.split_whitespace() {
        let (key, value) = item.split_once('=').ok_or("malformed key=value DSN")?;
        if TLS_KEYS.contains(&key) {
            tls.insert(key.to_string(), value.to_string());
        } else {
            kept.push(item);
        }
    }
    if kept.is_empty() {
        return Err("RAG_DATABASE_URL is empty".into());
    }
    Ok((kept.join(" "), tls))
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
    cert: Option<&String>,
    key: Option<&String>,
) -> std::result::Result<ClientConfig, String> {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
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

    #[test]
    fn tls_options_are_split_from_url_and_kv() {
        let (rest, tls) =
            split_tls("postgresql://u@db.example:5432/rag?sslmode=verify-full&sslrootcert=%2Fca.crt&connect_timeout=3").unwrap();
        assert_eq!(rest, "postgresql://u@db.example:5432/rag?connect_timeout=3");
        assert_eq!(tls["sslmode"], "verify-full");
        assert_eq!(tls["sslrootcert"], "/ca.crt");
        let (rest, tls) = split_tls("host=db dbname=rag sslmode=disable").unwrap();
        assert_eq!(
            (rest.as_str(), tls["sslmode"].as_str()),
            ("host=db dbname=rag", "disable")
        );
        assert!(split_tls("host='db x'").is_err());
        assert!(split_tls("").is_err());
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
        assert!(Database::from_dsn("postgresql://u@127.0.0.1/rag?sslmode=disable").is_ok());
    }
}
