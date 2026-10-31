//! Starlette response/cookie behaviour the original relied on, the security
//! header middleware, the embedded frontend assets and the fd-backed attachment
//! response.

use crate::reader::anchor::OpenedFile;
use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde_json::Value;

/// The original policy plus the shared cola16 header provider: its script and
/// its public `navigation.json` (fetched without credentials). Styles stay `'self'`;
/// the header adopts a constructed stylesheet, and its no-JS fallback is `/cola-header.css`.
pub const CSP: &str = "default-src 'none'; script-src 'self' https://apps.cola16.app; worker-src 'self'; style-src 'self'; connect-src 'self' https://apps.cola16.app; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

fn with_type(status: StatusCode, content_type: &'static str, body: impl Into<Body>) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

/// `JSONResponse(content, status_code)` (compact, UTF-8, no ASCII escaping).
pub fn json(status: StatusCode, value: &Value) -> Response {
    with_type(
        status,
        "application/json",
        serde_json::to_string(value).unwrap_or_else(|_| "null".into()),
    )
}

/// JSON `{"error": message}` with an optional `Retry-After`.
pub fn error(status: StatusCode, message: &str, retry: Option<u64>) -> Response {
    let mut response = json(status, &serde_json::json!({"error": message}));
    if let Some(retry) = retry
        && let Ok(value) = HeaderValue::from_str(&retry.to_string())
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

pub fn html(status: StatusCode, body: impl Into<Body>) -> Response {
    with_type(status, "text/html; charset=utf-8", body)
}

pub fn text(status: StatusCode, body: impl Into<Body>) -> Response {
    with_type(status, "text/plain; charset=utf-8", body)
}

/// Starlette's router answers for unknown paths and wrong methods.
pub fn not_found() -> Response {
    text(StatusCode::NOT_FOUND, "Not Found")
}

pub fn method_not_allowed(allow: &'static str) -> Response {
    let mut response = text(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static(allow));
    response
}

/// The `headers` middleware from `create_app`, applied to every response.
pub async fn security_headers(request: Request, next: Next) -> Response {
    let blog_preview = request.uri().path() == "/read-blog";
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CACHE_CONTROL, "no-store"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (
            header::HeaderName::from_static("x-robots-tag"),
            "noindex, nofollow",
        ),
        (header::CONTENT_SECURITY_POLICY, CSP),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    if blog_preview {
        headers.insert(header::CONTENT_SECURITY_POLICY,HeaderValue::from_static("default-src 'none'; script-src 'self' https://apps.cola16.app; style-src 'self'; connect-src 'self' https://apps.cola16.app; img-src 'self' https://cola16.app; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
    }
    response
}

/// Starlette `cookie_parser(request.headers['cookie'])[name]` (last wins).
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?;
    let text: String = raw.as_bytes().iter().map(|&b| b as char).collect();
    let mut found = None;
    for chunk in text.split(';') {
        let (key, value) = chunk.split_once('=').unwrap_or(("", chunk));
        let (key, value) = (key.trim(), value.trim());
        if (!key.is_empty() || !value.is_empty()) && key == name {
            found = Some(unquote_cookie(value));
        }
    }
    found
}

/// `http.cookies._unquote`
fn unquote_cookie(value: &str) -> String {
    if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
        return value.to_string();
    }
    let inner = &value[1..value.len() - 1];
    let mut out = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let octal: String = chars.clone().take(3).collect();
        if octal.len() == 3
            && octal.chars().all(|d| {
                ('0'..='3').contains(&octal.chars().next().unwrap_or('9')) && d.is_digit(8)
            })
        {
            if let Ok(code) = u32::from_str_radix(&octal, 8) {
                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
            for _ in 0..3 {
                chars.next();
            }
        } else if let Some(next) = chars.next() {
            out.push(next);
        }
    }
    out
}

pub fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// One embedded frontend file, pinned by `compat/static-assets.sha256` (checked in
/// tests). The page UI files listed there under `changed` intentionally diverge
/// from the pinned Python commit; the others are byte-identical to it.
pub struct Asset {
    pub name: &'static str,
    pub media_type: &'static str,
    pub bytes: &'static [u8],
}

macro_rules! asset {
    ($name:literal, $media:literal) => {
        Asset {
            name: $name,
            media_type: $media,
            bytes: include_bytes!(concat!("../../static/", $name)),
        }
    };
}

pub const ASSETS: [Asset; 13] = [
    asset!("public_search.html", "text/html; charset=utf-8"),
    asset!("public_search.js", "application/javascript"),
    asset!("public_search.css", "text/css; charset=utf-8"),
    asset!("graph.html", "text/html; charset=utf-8"),
    asset!("graph.js", "application/javascript"),
    asset!("graph.css", "text/css; charset=utf-8"),
    asset!("graph-layout.js", "application/javascript"),
    asset!("reader.css", "text/css; charset=utf-8"),
    asset!("reader.js", "application/javascript"),
    asset!("cola-theme.css", "text/css; charset=utf-8"),
    asset!("cola-theme-init.js", "application/javascript"),
    asset!("cola-theme.js", "application/javascript"),
    asset!("cola-header.css", "text/css; charset=utf-8"),
];

/// URL → asset, exactly the routes registered by `create_app`.
pub fn route_asset(path: &str) -> Option<&'static Asset> {
    let name = match path {
        "/" => "public_search.html",
        "/app.js" => "public_search.js",
        "/style.css" => "public_search.css",
        "/graph" => "graph.html",
        "/graph.js" => "graph.js",
        "/graph.css" => "graph.css",
        "/graph-layout.js" => "graph-layout.js",
        "/reader.css" => "reader.css",
        "/reader.js" => "reader.js",
        "/cola-theme.css" => "cola-theme.css",
        "/cola-theme-init.js" => "cola-theme-init.js",
        "/cola-theme.js" => "cola-theme.js",
        "/cola-header.css" => "cola-header.css",
        _ => return None,
    };
    ASSETS.iter().find(|asset| asset.name == name)
}

pub fn asset_response(asset: &'static Asset) -> Response {
    let mut response = with_type(StatusCode::OK, asset.media_type, Body::from(asset.bytes));
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response
}

/// Starlette `FileResponse(... filename=..., content_disposition_type=...)`
/// `Content-Disposition` formatting.
pub fn content_disposition(kind: &str, filename: &str) -> String {
    let quoted = crate::py::url::quote(filename, "/");
    if quoted != filename {
        format!("{kind}; filename*=utf-8''{quoted}")
    } else {
        format!("{kind}; filename=\"{filename}\"")
    }
}

/// Parse a single `bytes=a-b` range against `size` (multi-range: `None`,
/// i.e. the whole file is served).
fn single_range(value: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = (start.trim(), end.trim());
    let range = match (start.parse::<u64>().ok(), end.parse::<u64>().ok()) {
        (Some(start), Some(end)) if start <= end => (start, end.min(size.saturating_sub(1))),
        (Some(start), None) if end.is_empty() => (start, size.saturating_sub(1)),
        (None, Some(suffix)) if start.is_empty() && suffix > 0 => {
            (size.saturating_sub(suffix), size.saturating_sub(1))
        }
        _ => return Some(Err(())),
    };
    if size == 0 || range.0 >= size {
        Some(Err(()))
    } else {
        Some(Ok(range))
    }
}

/// Stream an already-verified descriptor (never re-opening the path).
pub fn file_response(
    file: OpenedFile,
    media_type: &'static str,
    disposition: String,
    range: Option<&str>,
) -> Response {
    use std::io::{Seek, SeekFrom};
    let size = file.size;
    let mut std_file = std::fs::File::from(file.fd);
    let mut status = StatusCode::OK;
    let mut length = size;
    let mut content_range = None;
    match range.and_then(|r| single_range(r, size)) {
        Some(Ok((start, end))) => {
            if std_file.seek(SeekFrom::Start(start)).is_ok() {
                status = StatusCode::PARTIAL_CONTENT;
                length = end - start + 1;
                content_range = Some(format!("bytes {start}-{end}/{size}"));
            }
        }
        Some(Err(())) => {
            let mut response = text(StatusCode::RANGE_NOT_SATISFIABLE, "Range Not Satisfiable");
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{size}")) {
                response.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            return response;
        }
        None => {}
    }
    use tokio::io::AsyncReadExt;
    let reader = tokio::fs::File::from_std(std_file).take(length);
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(reader));
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(media_type));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(value) = HeaderValue::from_str(&crate::timefmt::http_date(file.modified)) {
        headers.insert(header::LAST_MODIFIED, value);
    }
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    if let Some(value) = content_range.and_then(|v| HeaderValue::from_str(&v).ok()) {
        headers.insert(header::CONTENT_RANGE, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_like_starlette() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static(
                "a=1; __Host-notes-owner=abc; x; __Host-notes-owner=\"q\\\"r\"",
            ),
        );
        assert_eq!(
            cookie(&headers, "__Host-notes-owner").as_deref(),
            Some("q\"r")
        );
        assert_eq!(cookie(&headers, "a").as_deref(), Some("1"));
        assert_eq!(cookie(&headers, "missing"), None);
    }

    #[test]
    fn disposition_like_starlette() {
        assert_eq!(
            content_disposition("inline", "image.png"),
            "inline; filename=\"image.png\""
        );
        assert_eq!(
            content_disposition("attachment", "한 글.pdf"),
            "attachment; filename*=utf-8''%ED%95%9C%20%EA%B8%80.pdf"
        );
    }

    #[test]
    fn ranges() {
        assert_eq!(single_range("bytes=0-3", 10), Some(Ok((0, 3))));
        assert_eq!(single_range("bytes=5-", 10), Some(Ok((5, 9))));
        assert_eq!(single_range("bytes=-4", 10), Some(Ok((6, 9))));
        assert_eq!(single_range("bytes=10-", 10), Some(Err(())));
        assert_eq!(single_range("bytes=0-1,4-5", 10), None);
        assert_eq!(single_range("items=0-1", 10), None);
    }

    #[test]
    fn embedded_assets_match_the_manifest() {
        let manifest = include_str!("../../compat/static-assets.sha256");
        let entries: Vec<_> = manifest
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect();
        assert_eq!(entries.len(), ASSETS.len());
        for line in entries {
            let (digest, name) = line.split_once("  ").unwrap();
            let asset = ASSETS.iter().find(|a| a.name == name).unwrap();
            assert_eq!(crate::py::sha256_hex(asset.bytes), digest, "{name}");
        }
    }

    #[test]
    fn header_fallback_css_is_an_unedited_provider_copy() {
        // Written by scripts/vendor-header-css.py; the recorded hash covers everything after the note.
        let css = std::str::from_utf8(route_asset("/cola-header.css").unwrap().bytes).unwrap();
        let (note, body) = css.split_once(" */\n").unwrap();
        let recorded = note.rsplit_once("source-sha256: ").unwrap().1;
        assert_eq!(crate::py::sha256_hex(body.as_bytes()), recorded);
        // Scoped to the undefined element, so a stale copy never restyles the live header.
        let mut rules = String::new();
        for (index, part) in body.split("/*").enumerate() {
            rules.push_str(if index == 0 {
                part
            } else {
                part.split_once("*/").unwrap().1
            });
        }
        assert!(!rules.contains(":host") && !rules.contains("@import"));
        // The shared account-button style sets `display`; `hidden` #login/#logout must still
        // stay hidden until the session check (no logout on public pages).
        assert!(
            rules.contains(
                "cola-header:not(:defined)>[slot=account][hidden]{display:none!important}"
            )
        );
        for rule in rules.split('}').filter(|rule| rule.contains('{')) {
            let selectors = rule.rsplit('{').nth(1).unwrap();
            for selector in selectors.split(',') {
                assert!(
                    selector.trim().starts_with("cola-header:not(:defined)"),
                    "{selector}"
                );
            }
        }
    }

    #[test]
    fn csp_allows_only_the_header_provider_beyond_self() {
        let directives: Vec<_> = CSP.split("; ").collect();
        for directive in ["script-src", "connect-src"] {
            assert!(
                directives
                    .contains(&format!("{directive} 'self' https://apps.cola16.app").as_str())
            );
        }
        for kept in [
            "default-src 'none'",
            "worker-src 'self'",
            "style-src 'self'",
            "img-src 'self'",
            "base-uri 'none'",
            "frame-ancestors 'none'",
            "form-action 'self'",
        ] {
            assert!(directives.contains(&kept), "{kept}");
        }
        assert_eq!(directives.len(), 9);
        assert!(!CSP.contains("unsafe") && !CSP.contains('*') && !CSP.contains("https: "));
    }
}
