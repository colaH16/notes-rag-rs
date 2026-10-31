//! `posixpath` / `PurePosixPath` operations on corpus-relative strings.
//! These never touch the filesystem; file access goes through `reader::anchor`.

/// `posixpath.normpath(path)`
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part != ".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
            parts.push(part);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let joined = format!("{}{}", "/".repeat(initial), parts.join("/"));
    if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// `posixpath.join(a, b)` for two components.
pub fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `posixpath.dirname(path)`
pub fn dirname(path: &str) -> String {
    let head = match path.rfind('/') {
        Some(index) => &path[..=index],
        None => "",
    };
    if !head.is_empty() && head.chars().any(|c| c != '/') {
        head.trim_end_matches('/').to_string()
    } else {
        head.to_string()
    }
}

/// `PurePosixPath(path).parts` for a relative path (empty and `.` segments dropped).
pub fn parts(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect()
}

/// `PurePosixPath(path).name`
pub fn name(path: &str) -> &str {
    parts(path).last().copied().unwrap_or("")
}

/// `PurePosixPath(path).suffix` (CPython 3.12/3.13 rule).
pub fn suffix(path: &str) -> &str {
    let name = name(path);
    match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => &name[index..],
        _ => "",
    }
}

/// `PurePosixPath(path).stem`
pub fn stem(path: &str) -> &str {
    let name = name(path);
    let suffix = suffix(path);
    &name[..name.len() - suffix.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posixpath_like_python() {
        assert_eq!(
            normpath(&join(&dirname("public/note.md"), "../private/x.png")),
            "private/x.png"
        );
        assert_eq!(normpath("../a"), "../a");
        assert_eq!(normpath("a/../.."), "..");
        assert_eq!(normpath(""), ".");
        assert_eq!(normpath("//a//b/"), "//a/b");
        assert_eq!(dirname("note.md"), "");
        assert_eq!(dirname("/x"), "/");
        assert_eq!(join("", "note.assets/a.png"), "note.assets/a.png");
    }

    #[test]
    fn pure_path_parts() {
        assert_eq!(suffix("a/b.tar.gz"), ".gz");
        assert_eq!(suffix("a/.hidden"), "");
        assert_eq!(suffix("a/b."), "");
        assert_eq!(stem("a/제목.md"), "제목");
        assert_eq!(name("a/b/"), "b");
    }
}
