//! libpq connection settings without libpq.
//!
//! Python called `psycopg.connect(os.environ.get('RAG_DATABASE_URL', ''))`, so
//! libpq filled every keyword missing from the (usually empty) conninfo from
//! its `PG*` environment variables. Production relies on that: it sets
//! `PGHOST`/`PGPORT`/`PGDATABASE`/`PGUSER`/`PGPASSWORD`/`PGSSLMODE`/
//! `PGSSLROOTCERT` and no URL. The same precedence is reproduced here:
//!
//! 1. `PG*` environment variables become libpq keywords;
//! 2. keywords present in the explicit conninfo (URL or `key=value`) replace
//!    them one by one, so a URL that only names a host keeps `PGSSLMODE`.
//!
//! Empty environment values count as unset. Every keyword that cannot be
//! honoured exactly (service files, password files, CRLs, `require_auth`,
//! `PGTZ`, ...) is rejected instead of being silently ignored. Error texts
//! name keywords only, never values.

use crate::config::Env;
use std::collections::BTreeMap;

/// Resolved libpq keywords. Deliberately not `Debug`: it holds the password.
#[derive(Clone, Default)]
pub struct Options(pub(crate) BTreeMap<String, String>);

impl Options {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Explicit conninfo keywords win over environment defaults.
    pub fn overridden_by(mut self, explicit: Options) -> Options {
        self.0.extend(explicit.0);
        self
    }
}

pub fn invalid(reason: &str) -> String {
    format!("database configuration invalid: {reason}")
}

/// libpq environment variables and their keywords (PostgreSQL 17 docs,
/// "Environment Variables"). Unsupported keywords are mapped too so that the
/// validation in `db.rs` rejects them uniformly.
const ENVIRONMENT: [(&str, &str); 31] = [
    ("PGHOST", "host"),
    ("PGHOSTADDR", "hostaddr"),
    ("PGPORT", "port"),
    ("PGDATABASE", "dbname"),
    ("PGUSER", "user"),
    ("PGPASSWORD", "password"),
    ("PGPASSFILE", "passfile"),
    ("PGSERVICE", "service"),
    ("PGSERVICEFILE", "servicefile"),
    ("PGOPTIONS", "options"),
    ("PGAPPNAME", "application_name"),
    ("PGCONNECT_TIMEOUT", "connect_timeout"),
    ("PGSSLMODE", "sslmode"),
    ("PGREQUIRESSL", "requiressl"),
    ("PGSSLNEGOTIATION", "sslnegotiation"),
    ("PGSSLCERT", "sslcert"),
    ("PGSSLKEY", "sslkey"),
    ("PGSSLCERTMODE", "sslcertmode"),
    ("PGSSLROOTCERT", "sslrootcert"),
    ("PGSSLCRL", "sslcrl"),
    ("PGSSLCRLDIR", "sslcrldir"),
    ("PGSSLSNI", "sslsni"),
    ("PGREQUIREPEER", "requirepeer"),
    ("PGREQUIREAUTH", "require_auth"),
    ("PGSSLMINPROTOCOLVERSION", "ssl_min_protocol_version"),
    ("PGSSLMAXPROTOCOLVERSION", "ssl_max_protocol_version"),
    ("PGGSSENCMODE", "gssencmode"),
    ("PGKRBSRVNAME", "krbsrvname"),
    ("PGTARGETSESSIONATTRS", "target_session_attrs"),
    ("PGLOADBALANCEHOSTS", "load_balance_hosts"),
    ("PGCHANNELBINDING", "channel_binding"),
];

/// Session defaults libpq sends at startup that tokio-postgres does not.
/// `PGTZ` changes the reported `updated_at` offset, so it must not be ignored.
const SESSION_ENVIRONMENT: [&str; 1] = ["PGTZ"];

/// `PG*` defaults from an injected environment snapshot.
pub fn environment(env: &Env) -> Result<Options, String> {
    for name in SESSION_ENVIRONMENT {
        if env.get(name).is_some_and(|v| !v.is_empty()) {
            return Err(invalid(&format!("{name} is not supported; use PGOPTIONS")));
        }
    }
    Ok(Options(
        ENVIRONMENT
            .iter()
            .filter_map(|(name, key)| {
                let value = env.get(name).filter(|v| !v.is_empty())?;
                Some((key.to_string(), value.to_string()))
            })
            .collect(),
    ))
}

/// `PG*` defaults overridden by a non-empty `RAG_DATABASE_URL`.
pub fn resolve(env: &Env) -> Result<Options, String> {
    let defaults = environment(env)?;
    match env.get("RAG_DATABASE_URL").filter(|v| !v.trim().is_empty()) {
        Some(conninfo) => Ok(defaults.overridden_by(parse(conninfo)?)),
        None => Ok(defaults),
    }
}

/// Parse an explicit conninfo: `postgres[ql]://` URI or `key=value` string.
pub fn parse(conninfo: &str) -> Result<Options, String> {
    let uri = ["postgresql://", "postgres://"]
        .iter()
        .find_map(|scheme| conninfo.strip_prefix(scheme));
    match uri {
        Some(rest) => parse_uri(rest),
        None => parse_keywords(conninfo),
    }
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|d| d as u8)
}

/// Strict percent-decoding (libpq rejects malformed escapes and NUL).
fn decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let byte = bytes
                .get(i + 1)
                .and_then(|&h| hex(h))
                .zip(bytes.get(i + 2).and_then(|&l| hex(l)))
                .map(|(h, l)| (h << 4) | l)
                .filter(|&b| b != 0)
                .ok_or_else(|| invalid("malformed percent-encoding in URL"))?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| invalid("URL is not valid UTF-8"))
}

/// `[userinfo@][host[:port][,...]][/dbname][?key=value&...]` (libpq URI).
fn parse_uri(rest: &str) -> Result<Options, String> {
    let mut options = BTreeMap::new();
    let (main, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, dbname) = main.split_once('/').unwrap_or((main, ""));
    let hostspec = match authority.rsplit_once('@') {
        Some((userinfo, hostspec)) => {
            let (user, password) = match userinfo.split_once(':') {
                Some((user, password)) => (user, Some(password)),
                None => (userinfo, None),
            };
            if !user.is_empty() {
                options.insert("user".to_string(), decode(user)?);
            }
            if let Some(password) = password {
                options.insert("password".to_string(), decode(password)?);
            }
            hostspec
        }
        None => authority,
    };
    if !hostspec.is_empty() {
        let mut hosts = Vec::new();
        let mut ports = Vec::new();
        for spec in hostspec.split(',') {
            let (host, port) = match spec.strip_prefix('[') {
                Some(bracketed) => {
                    let (host, after) = bracketed
                        .split_once(']')
                        .ok_or_else(|| invalid("malformed IPv6 host in URL"))?;
                    let port = match after {
                        "" => "",
                        _ => after
                            .strip_prefix(':')
                            .ok_or_else(|| invalid("malformed IPv6 host in URL"))?,
                    };
                    (host, port)
                }
                None => spec.split_once(':').unwrap_or((spec, "")),
            };
            hosts.push(decode(host)?);
            ports.push(decode(port)?);
        }
        if hosts.iter().any(|h| !h.is_empty()) {
            options.insert("host".to_string(), hosts.join(","));
        }
        if ports.iter().any(|p| !p.is_empty()) {
            options.insert("port".to_string(), ports.join(","));
        }
    }
    if !dbname.is_empty() {
        options.insert("dbname".to_string(), decode(dbname)?);
    }
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| invalid("malformed URL query parameter"))?;
        let (key, value) = (decode(key)?, decode(value)?);
        // libpq's JDBC-compatible alias.
        if key == "ssl" {
            if value != "true" {
                return Err(invalid("URL parameter ssl must be true"));
            }
            options.insert("sslmode".to_string(), "require".to_string());
        } else {
            options.insert(key, value);
        }
    }
    Ok(Options(options))
}

/// `key = value ...` with libpq quoting: values may be single-quoted, and a
/// backslash escapes the next character (quoted or not).
fn parse_keywords(conninfo: &str) -> Result<Options, String> {
    let mut options = BTreeMap::new();
    let mut chars = conninfo.chars().peekable();
    fn skip_space(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
    }
    loop {
        skip_space(&mut chars);
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && !c.is_whitespace()) {
            key.push(c);
        }
        skip_space(&mut chars);
        if key.is_empty() || chars.next() != Some('=') {
            return Err(invalid("malformed key=value conninfo"));
        }
        skip_space(&mut chars);
        let mut value = String::new();
        if chars.next_if_eq(&'\'').is_some() {
            loop {
                match chars.next() {
                    Some('\'') => break,
                    Some('\\') => value.push(
                        chars
                            .next()
                            .ok_or_else(|| invalid("unterminated quoted value"))?,
                    ),
                    Some(c) => value.push(c),
                    None => return Err(invalid("unterminated quoted value")),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                if c == '\\' {
                    value.push(chars.next().ok_or_else(|| invalid("dangling escape"))?);
                } else {
                    value.push(c);
                }
            }
        }
        options.insert(key, value);
    }
    Ok(Options(options))
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

    fn pairs(options: &Options) -> Vec<(&str, &str)> {
        options
            .0
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    #[test]
    fn uri_fields_are_percent_decoded_like_libpq() {
        let options = parse(
            "postgresql://us%40er:p%3Aw%2Fd@db1.example:5433,[::1],%2Frun%2Fpg/rag%20db?sslmode=verify-full&sslrootcert=%2Fca%2Froot.crt&ssl=true",
        )
        .unwrap();
        assert_eq!(
            pairs(&options),
            [
                ("dbname", "rag db"),
                ("host", "db1.example,::1,/run/pg"),
                ("password", "p:w/d"),
                ("port", "5433,,"),
                // `ssl=true` comes after `sslmode` and wins, as in libpq.
                ("sslmode", "require"),
                ("sslrootcert", "/ca/root.crt"),
                ("user", "us@er"),
            ]
        );
        assert!(pairs(&parse("postgresql://").unwrap()).is_empty());
        assert_eq!(
            pairs(&parse("postgres:///rag").unwrap()),
            [("dbname", "rag")]
        );
        assert!(parse("postgresql://db/rag?sslmode").is_err());
        assert!(parse("postgresql://db/%zz").is_err());
        assert!(parse("postgresql://u:%00@db/rag").is_err());
        assert!(parse("postgresql://[::1/rag").is_err());
    }

    #[test]
    fn keyword_values_support_libpq_quoting() {
        let options =
            parse(r"host = db.example  password='p w\'d\\' user=a\ b options='-c x=1'").unwrap();
        assert_eq!(
            pairs(&options),
            [
                ("host", "db.example"),
                ("options", "-c x=1"),
                ("password", r"p w'd\"),
                ("user", "a b"),
            ]
        );
        assert!(parse("host").is_err());
        assert!(parse("=x").is_err());
        assert!(parse("password='open").is_err());
        assert!(pairs(&parse("  ").unwrap()).is_empty());
    }

    #[test]
    fn environment_supplies_defaults_and_url_fields_win() {
        let production = [
            ("PGHOST", "pg.internal"),
            ("PGPORT", "5432"),
            ("PGDATABASE", "rag"),
            ("PGUSER", "reader"),
            ("PGPASSWORD", "env-secret"),
            ("PGSSLMODE", "verify-full"),
            ("PGSSLROOTCERT", "/ca/ca.crt"),
            ("PGAPPNAME", ""),
            ("UNRELATED", "x"),
        ];
        // No URL at all: environment only (the production contract).
        let only_env = resolve(&env(&production)).unwrap();
        assert_eq!(only_env.get("host"), Some("pg.internal"));
        assert_eq!(only_env.get("sslmode"), Some("verify-full"));
        assert_eq!(only_env.get("application_name"), None, "empty means unset");
        // An empty URL is the same as no URL.
        let mut with_empty = production.to_vec();
        with_empty.push(("RAG_DATABASE_URL", ""));
        assert_eq!(
            pairs(&resolve(&env(&with_empty)).unwrap()),
            pairs(&only_env)
        );
        // A URL naming only the host keeps every other environment default,
        // in particular sslmode/sslrootcert (no silent TLS downgrade).
        let mut with_url = production.to_vec();
        with_url.push(("RAG_DATABASE_URL", "postgresql://other.internal"));
        let merged = resolve(&env(&with_url)).unwrap();
        assert_eq!(merged.get("host"), Some("other.internal"));
        assert_eq!(merged.get("sslmode"), Some("verify-full"));
        assert_eq!(merged.get("sslrootcert"), Some("/ca/ca.crt"));
        assert_eq!(merged.get("password"), Some("env-secret"));
        // Explicit fields beat the environment field by field.
        with_url.pop();
        with_url.push((
            "RAG_DATABASE_URL",
            "postgresql://u:url-secret@h/db?sslmode=require",
        ));
        let merged = resolve(&env(&with_url)).unwrap();
        assert_eq!(
            [
                merged.get("user"),
                merged.get("password"),
                merged.get("dbname"),
                merged.get("sslmode"),
                merged.get("port")
            ],
            [
                Some("u"),
                Some("url-secret"),
                Some("db"),
                Some("require"),
                Some("5432")
            ]
        );
    }

    #[test]
    fn unsupported_environment_is_mapped_for_rejection_and_errors_hide_values() {
        let options = environment(&env(&[("PGSERVICE", "svc"), ("PGSSLCRL", "/crl")])).unwrap();
        assert_eq!(pairs(&options), [("service", "svc"), ("sslcrl", "/crl")]);
        let error = environment(&env(&[("PGTZ", "Asia/Seoul")])).err().unwrap();
        assert!(error.contains("PGTZ") && !error.contains("Seoul"));
        let error = resolve(&env(&[
            ("PGPASSWORD", "env-secret"),
            ("RAG_DATABASE_URL", "postgresql://u:url-secret@h/%zz"),
        ]))
        .err()
        .unwrap();
        assert!(!error.contains("secret"), "{error}");
    }
}
