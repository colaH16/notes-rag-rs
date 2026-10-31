//! Python `str` semantics that the original code relies on.
//!
//! Python's string methods are Unicode-aware and count *code points*, while
//! Rust `str` APIs mostly work in UTF-8 bytes. Every length limit and slice in
//! the original (`len(query) <= 300`, `content[:6000]`, ...) is therefore
//! implemented here with `chars()`, never with byte indices.

/// `ch.isspace()` in CPython: Unicode White_Space-ish set used by `str.split()`
/// and `str.strip()` without arguments.
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{0b}'
            | '\u{0c}'
            | '\r'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{1f}'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// `s.strip()`
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `s.strip(chars)` with an explicit character set.
pub fn strip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_matches(|c| chars.contains(c))
}

/// `s.rstrip(chars)`
pub fn rstrip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_end_matches(|c| chars.contains(c))
}

/// `' '.join(s.split())`: collapse every Python-whitespace run to one space.
pub fn normalize_whitespace(s: &str) -> String {
    s.split(is_space)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `len(s)` (code points).
pub fn len(s: &str) -> usize {
    s.chars().count()
}

/// `s[:n]` (code points).
pub fn prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((index, _)) => &s[..index],
        None => s,
    }
}

/// Line boundaries recognised by `str.splitlines()`.
fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\u{0b}'
            | '\u{0c}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// `s.splitlines(keepends=True)`; `"\r\n"` is one boundary.
pub fn splitlines_keepends(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if is_line_break(c) {
            let mut end = index + c.len_utf8();
            if c == '\r'
                && let Some(&(_, '\n')) = chars.peek()
            {
                chars.next();
                end += 1;
            }
            lines.push(&s[start..end]);
            start = end;
        }
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

/// `str.partition(sep)`
pub fn partition<'a>(s: &'a str, sep: &str) -> (&'a str, &'a str, &'a str) {
    match s.find(sep) {
        Some(index) => (
            &s[..index],
            &s[index..index + sep.len()],
            &s[index + sep.len()..],
        ),
        None => (s, "", ""),
    }
}

/// `str.rpartition(sep)`
pub fn rpartition<'a>(s: &'a str, sep: &str) -> (&'a str, &'a str, &'a str) {
    match s.rfind(sep) {
        Some(index) => (
            &s[..index],
            &s[index..index + sep.len()],
            &s[index + sep.len()..],
        ),
        None => ("", "", s),
    }
}

/// `str.isidentifier()` (XID_Start/`_` followed by XID_Continue).
pub fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) if first == '_' || unicode_ident::is_xid_start(first) => {
            chars.all(unicode_ident::is_xid_continue)
        }
        _ => false,
    }
}

/// `str.casefold()` (full Unicode default case folding).
pub fn casefold(s: &str) -> String {
    caseless::default_case_fold_str(s)
}

/// Python's `re` `\w` for `str` patterns: alphanumeric (letters, all numeric
/// categories) or underscore.
pub fn is_word(c: char) -> bool {
    use unicode_general_category::{GeneralCategory as G, get_general_category};
    c == '_'
        || matches!(
            get_general_category(c),
            G::UppercaseLetter
                | G::LowercaseLetter
                | G::TitlecaseLetter
                | G::ModifierLetter
                | G::OtherLetter
                | G::DecimalNumber
                | G::LetterNumber
                | G::OtherNumber
        )
}

/// `str.isprintable()` (used for Python `repr` of strings).
fn is_printable(c: char) -> bool {
    use unicode_general_category::{GeneralCategory as G, get_general_category};
    if c == ' ' {
        return true;
    }
    !matches!(
        get_general_category(c),
        G::Control
            | G::Format
            | G::Surrogate
            | G::PrivateUse
            | G::Unassigned
            | G::LineSeparator
            | G::ParagraphSeparator
            | G::SpaceSeparator
    )
}

/// `repr(s)` for a Python `str`.
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if is_printable(c) => out.push(c),
            c if (c as u32) < 0x100 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_matches_python_split() {
        assert_eq!(normalize_whitespace("  a\u{a0}\u{3000}b\t\nc  "), "a b c");
        assert_eq!(normalize_whitespace("\u{200b}x"), "\u{200b}x");
        assert_eq!(strip("\u{85} 한글 \u{2028}"), "한글");
    }

    #[test]
    fn code_point_slicing() {
        assert_eq!(prefix("한글abc", 2), "한글");
        assert_eq!(prefix("ab", 10), "ab");
        assert_eq!(len("한글"), 2);
    }

    #[test]
    fn splitlines_like_python() {
        assert_eq!(
            splitlines_keepends("a\r\nb\rc\u{2028}d\n\ne"),
            ["a\r\n", "b\r", "c\u{2028}", "d\n", "\n", "e"]
        );
        assert_eq!(splitlines_keepends(""), Vec::<&str>::new());
        assert_eq!(splitlines_keepends("x\n"), ["x\n"]);
    }

    #[test]
    fn repr_quotes_like_python() {
        assert_eq!(repr_str("a'b"), "\"a'b\"");
        assert_eq!(repr_str("a'\"b"), "'a\\'\"b'");
        assert_eq!(repr_str("\u{7}한"), "'\\x07한'");
    }

    #[test]
    fn identifiers() {
        assert!(is_identifier("onenote_id"));
        assert!(is_identifier("_x1"));
        assert!(!is_identifier("1x"));
        assert!(!is_identifier("a-b"));
        assert!(!is_identifier(""));
    }
}
