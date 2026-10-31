//! `urllib.parse` behaviour the original relies on for citations, reader links
//! and query parameters. Generic URL crates normalise differently (WHATWG), so
//! the exact CPython algorithms are reproduced instead.

const ALWAYS_SAFE: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_.-~";

/// `urllib.parse.quote(s, safe=safe)`
pub fn quote(s: &str, safe: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() && (ALWAYS_SAFE.contains(c) || safe.contains(c)) {
            out.push(c);
        } else {
            let mut buffer = [0u8; 4];
            for byte in c.encode_utf8(&mut buffer).bytes() {
                out.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    out
}

/// `urllib.parse.quote_plus(s, safe='')`
pub fn quote_plus(s: &str) -> String {
    if s.contains(' ') {
        quote(s, " ").replace(' ', "+")
    } else {
        quote(s, "")
    }
}

/// `urllib.parse.urlencode(pairs)` for string values.
pub fn urlencode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|d| d as u8)
}

/// `urllib.parse.unquote(s)` (UTF-8, errors="replace"). Like CPython, each run
/// of ASCII characters is percent-decoded independently and non-ASCII
/// characters pass through untouched.
pub fn unquote(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut run = Vec::new();
    let flush = |run: &mut Vec<u8>, out: &mut String| {
        if !run.is_empty() {
            out.push_str(&String::from_utf8_lossy(&percent_decode(run)));
            run.clear();
        }
    };
    for c in s.chars() {
        if c.is_ascii() {
            run.push(c as u8);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%'
            && let (Some(high), Some(low)) = (
                input.get(index + 1).copied().and_then(hex),
                input.get(index + 2).copied().and_then(hex),
            )
        {
            out.push(high * 16 + low);
            index += 3;
        } else {
            out.push(input[index]);
            index += 1;
        }
    }
    out
}

/// Starlette `QueryParams(scope["query_string"].decode("latin-1"))` followed by
/// `.get(name, "")`: `parse_qsl(keep_blank_values=True)`, last value wins.
pub fn query_param(raw_query: &[u8], name: &str) -> Option<String> {
    let text: String = raw_query.iter().map(|&b| b as char).collect();
    let mut found = None;
    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if unquote(&key.replace('+', " ")) == name {
            found = Some(unquote(&value.replace('+', " ")));
        }
    }
    found
}

/// Result of `urllib.parse.urlsplit`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SplitUrl {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
    pub fragment: String,
}

/// `urllib.parse.urlsplit(url)`. Returns `None` where CPython raises
/// `ValueError` (unbalanced IPv6 brackets).
pub fn urlsplit(url: &str) -> Option<SplitUrl> {
    let url = url.trim_start_matches(|c: char| c <= ' ');
    let mut url: String = url
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut result = SplitUrl::default();
    if let Some(colon) = url.find(':')
        && colon > 0
        && url.as_bytes()[0].is_ascii_alphabetic()
        && url[..colon]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        result.scheme = url[..colon].to_ascii_lowercase();
        url = url[colon + 1..].to_string();
    }
    if url.starts_with("//") {
        let rest = &url[2..];
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        result.netloc = rest[..end].to_string();
        url = rest[end..].to_string();
        if result.netloc.contains('[') != result.netloc.contains(']') {
            return None;
        }
    }
    if let Some((before, fragment)) = url.split_once('#') {
        result.fragment = fragment.to_string();
        url = before.to_string();
    }
    if let Some((before, query)) = url.split_once('?') {
        result.query = query.to_string();
        url = before.to_string();
    }
    result.path = url;
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_like_python() {
        assert_eq!(
            quote("a (b)/한글 #%", "/"),
            "a%20%28b%29/%ED%95%9C%EA%B8%80%20%23%25"
        );
        assert_eq!(
            urlencode(&[("path", "public/next note.md")]),
            "path=public%2Fnext+note.md"
        );
    }

    #[test]
    fn unquote_like_python() {
        assert_eq!(unquote("%ED%95%9C%20x"), "한 x");
        assert_eq!(unquote("%zz%4"), "%zz%4");
        assert_eq!(unquote("%ED%95"), "\u{fffd}");
        assert_eq!(unquote("한%41"), "한A");
    }

    #[test]
    fn query_like_starlette() {
        assert_eq!(
            query_param(b"path=a%2Fb.md&path=c+d", "path").as_deref(),
            Some("c d")
        );
        assert_eq!(query_param(b"x", "x").as_deref(), Some(""));
        assert_eq!(query_param(b"y=1", "x"), None);
        // Raw non-ASCII bytes become latin-1 mojibake, exactly like Starlette.
        assert_eq!(
            query_param("p=한".as_bytes(), "p").as_deref(),
            Some("í\u{95}\u{9c}")
        );
    }

    #[test]
    fn urlsplit_like_python() {
        let u = urlsplit("HTTPS://notes.example/OneNote/a?x=1#frag").unwrap();
        assert_eq!(
            (u.scheme.as_str(), u.netloc.as_str(), u.path.as_str()),
            ("https", "notes.example", "/OneNote/a")
        );
        assert_eq!((u.query.as_str(), u.fragment.as_str()), ("x=1", "frag"));
        assert_eq!(
            urlsplit("note.assets/x.png").unwrap().path,
            "note.assets/x.png"
        );
        assert_eq!(
            urlsplit("javascript:alert(1)").unwrap().scheme,
            "javascript"
        );
        assert_eq!(urlsplit("1a:b").unwrap().scheme, "");
        assert!(urlsplit("http://[bad/x").is_none());
    }
}
