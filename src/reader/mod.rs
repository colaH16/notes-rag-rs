//! Port of `document_reader.py` (+ `vision_overlay.py`): read-only viewer with
//! configured notebook IDs and the current-folder ACL.
//!
//! No static directory mount, directory listing, writes, remote image
//! fetching, script execution, or authority derived from a browser-supplied
//! notebook name. Every request re-reads the selection and policy files and
//! fails closed when their revision differs from the one the process started
//! with.

pub mod anchor;
pub mod markdown;

use crate::fault::{Fault, Result};
use crate::native;
use crate::policy::{load_allowlist, load_public_notebooks};
use crate::py::{html_escape, path as pypath, sha256_hex, text, url};
use anchor::{OpenedFile, open_file, read_limited};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

pub const MAX_NOTE: u64 = 8 * 1024 * 1024;
pub const MAX_ASSET: u64 = 512 * 1024 * 1024;
const MAX_OVERLAY: u64 = 16 * 1024 * 1024;
const MAX_OVERLAY_ASSET: u64 = 64 * 1024 * 1024;
const PAGE_TEMPLATE: &str = include_str!("page.html");

#[derive(Debug, Clone)]
pub struct ReaderConfig {
    pub root: PathBuf,
    pub selection: PathBuf,
    pub policy: PathBuf,
    pub revision: String,
    /// `NOTE_PREFIX` (default `/OneNote/markdown/`).
    pub note_prefix: String,
    /// SilverBullet origin for converted links and the edit link.
    pub silverbullet_origin: String,
    /// `RAG_VISION_STATE` (published OCR overlays), if configured.
    pub vision_state: Option<PathBuf>,
    /// `RAG_NATIVE_NOTES=1` (current-folder ACL), read once at startup.
    pub native_notes: bool,
}

pub struct Reader {
    pub config: ReaderConfig,
    pub links: LinkTargets,
}

/// The owned data `target()` needs, so the sanitizer callback can be `'static`.
#[derive(Debug, Clone)]
pub struct LinkTargets {
    pub silverbullet_netloc: String,
    pub note_prefix: String,
}

impl LinkTargets {
    /// `target(path, value)`: resolve converted SilverBullet/relative links
    /// without accepting other roots.
    pub fn target(&self, path: &str, value: &str) -> Option<String> {
        let parsed = url::urlsplit(value)?;
        if (!parsed.scheme.is_empty() || !parsed.netloc.is_empty())
            && (parsed.scheme != "https" || parsed.netloc != self.silverbullet_netloc)
        {
            return None;
        }
        let target = url::unquote(&parsed.path);
        let relative = if let Some(rest) = target.strip_prefix(self.note_prefix.as_str()) {
            rest.to_string()
        } else if target.starts_with('/') || !parsed.netloc.is_empty() {
            return None;
        } else {
            pypath::normpath(&pypath::join(&pypath::dirname(path), &target))
        };
        canonical_path(&relative).ok().map(str::to_string)
    }
}

/// `canonical_path(value)`: query values are URL-decoded exactly once.
pub fn canonical_path(value: &str) -> Result<&str> {
    if value.is_empty() || value.starts_with('/') {
        return Err(Fault::NotReadable);
    }
    crate::scope::validate_folder(value).map_err(|_| Fault::NotReadable)?;
    if value.split('/').any(|part| part.starts_with('.')) {
        return Err(Fault::NotReadable);
    }
    Ok(value)
}

/// `frontmatter(text)` → (metadata, body).
pub fn frontmatter(source: &str) -> Result<(Map<String, Value>, String)> {
    let lines = text::splitlines_keepends(source);
    if lines.is_empty() || text::strip_chars(lines[0], "\u{feff}\r\n ") != "---" {
        return Err(Fault::NotReadable);
    }
    let mut metadata = Map::new();
    for (index, line) in lines.iter().enumerate().skip(1).take(100) {
        if text::strip_chars(line, "\r\n") == "---" {
            return Ok((metadata, lines[index + 1..].concat()));
        }
        let (key, separator, value) = text::partition(text::rstrip_chars(line, "\r\n"), ": ");
        if separator.is_empty() || !text::is_identifier(key) || metadata.contains_key(key) {
            return Err(Fault::NotReadable);
        }
        let parsed = if key == "tags" {
            Value::String(value.to_string())
        } else {
            serde_json::from_str(value).map_err(|_| Fault::NotReadable)?
        };
        metadata.insert(key.to_string(), parsed);
    }
    Err(Fault::NotReadable)
}

fn is_note_path(path: &str) -> bool {
    path.ends_with(".md") && path.split('/').next() != Some("_meta")
}

impl Reader {
    /// `DocumentReader(root, selection, policy, revision)`: a missing or
    /// mismatched policy fails startup closed.
    pub fn new(config: ReaderConfig) -> Result<Self> {
        let silverbullet_netloc = url::urlsplit(&config.silverbullet_origin)
            .map(|u| u.netloc)
            .filter(|n| !n.is_empty())
            .ok_or(Fault::Other("invalid SilverBullet origin"))?;
        let links = LinkTargets {
            silverbullet_netloc,
            note_prefix: config.note_prefix.clone(),
        };
        let reader = Self { config, links };
        reader.permissions()?;
        Ok(reader)
    }

    /// `permissions()` → (allowed notebook IDs, public notebook IDs).
    pub fn permissions(&self) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
        let allowed = load_allowlist(&self.config.selection)?;
        let (public, revision) = load_public_notebooks(&self.config.policy, &allowed.ids)?;
        if revision != self.config.revision {
            return Err(Fault::Other("Reader access policy revision mismatch"));
        }
        Ok((allowed.ids, public.into_iter().collect()))
    }

    /// `visible_paths(paths, owner)`: fresh metadata-only gate for cached
    /// graph/search paths. Never caches ACL decisions.
    pub fn visible_paths(&self, paths: &[String], owner: bool) -> Result<BTreeSet<String>> {
        let (allowed, public) = self.permissions()?;
        let mut visible = BTreeSet::new();
        let mut notebooks: HashMap<String, String> = HashMap::new();
        let unique: BTreeSet<&String> = paths.iter().collect();
        for path in unique {
            let outcome: Result<bool> = (|| {
                let path = canonical_path(path)?;
                if !is_note_path(path) {
                    return Ok(false);
                }
                if !self.config.native_notes {
                    self.read_note(path, owner)?;
                    return Ok(true);
                }
                let folder = path.split('/').next().unwrap_or("").to_string();
                let notebook = match notebooks.get(&folder) {
                    Some(notebook) => notebook.clone(),
                    None => {
                        let notebook = native::folder_notebook(path, &self.config.selection)?;
                        notebooks.insert(folder, notebook.clone());
                        notebook
                    }
                };
                if !allowed.contains(&notebook) || (!owner && !public.contains(&notebook)) {
                    return Ok(false);
                }
                open_file(&self.config.root, path, MAX_NOTE)?;
                Ok(true)
            })();
            match outcome {
                Ok(true) => {
                    visible.insert(path.clone());
                }
                Ok(false) => {}
                // except (NotReadable, ValueError, OSError): continue
                Err(fault) if fault.is_reader_not_found() => {}
                Err(fault) => return Err(fault),
            }
        }
        Ok(visible)
    }

    pub fn read_note(&self, path: &str, owner: bool) -> Result<(Map<String, Value>, String)> {
        let (metadata, body, _) = self.read_note_snapshot(path, owner)?;
        Ok((metadata, body))
    }

    /// `read_note_snapshot(path, owner)`: validated metadata/body and the
    /// exact bytes they came from.
    pub fn read_note_snapshot(
        &self,
        path: &str,
        owner: bool,
    ) -> Result<(Map<String, Value>, String, Vec<u8>)> {
        let path = canonical_path(path)?;
        if !is_note_path(path) {
            return Err(Fault::NotReadable);
        }
        let (allowed, public) = self.permissions()?;
        let raw = read_limited(open_file(&self.config.root, path, MAX_NOTE)?, MAX_NOTE + 1)?;
        if raw.len() as u64 > MAX_NOTE {
            return Err(Fault::NotReadable);
        }
        let normalized =
            match native::normalize(&raw, path, &self.config.selection, self.config.native_notes) {
                Ok(bytes) => bytes,
                Err(Fault::Value(_)) => return Err(Fault::NotReadable),
                Err(other) => return Err(other),
            };
        let decoded = String::from_utf8(normalized).map_err(|_| Fault::NotReadable)?;
        let (metadata, body) = frontmatter(&decoded)?;
        let notebook = metadata.get("onenote_notebook_id").and_then(Value::as_str);
        let source = metadata.get("source_system").and_then(Value::as_str);
        let readable = matches!(source, Some("onenote" | "silverbullet"))
            && metadata.get("content_type").and_then(Value::as_str) == Some("note")
            && metadata.get("generated") != Some(&Value::Bool(true))
            && notebook.is_some_and(|n| allowed.contains(n) && (owner || public.contains(n)));
        if !readable {
            return Err(Fault::NotReadable);
        }
        Ok((metadata, body, raw))
    }

    pub fn target(&self, path: &str, value: &str) -> Option<String> {
        self.links.target(path, value)
    }

    pub fn render_body(&self, path: &str, body: &str) -> (String, BTreeSet<String>) {
        markdown::render_body(self, path, body)
    }

    /// `document(path, owner)` → complete reader HTML page.
    pub fn document(&self, path: &str, owner: bool) -> Result<String> {
        let (metadata, body) = self.read_note(path, owner)?;
        let body = self.supplement(path, &metadata, body)?;
        let (safe, _) = self.render_body(path, &body);
        let title_value = metadata.get("title").filter(|v| crate::py::json::truthy(v));
        let title = html_escape(&match title_value {
            Some(value) => crate::py::json::to_str(value),
            None => pypath::stem(path).to_string(),
        });
        let stem_path = &path[..path.len() - 3];
        let edit = format!(
            "{}{}{}",
            self.config.silverbullet_origin.trim_end_matches('/'),
            self.config.note_prefix,
            url::quote(stem_path, "/")
        );
        let audience = if owner {
            "OWNER · 비공개 포함"
        } else {
            "PUBLIC · 읽기 전용"
        };
        Ok(fill(
            PAGE_TEMPLATE,
            &[
                ("{{TITLE}}", &title),
                ("{{AUDIENCE_ATTR}}", if owner { "owner" } else { "public" }),
                ("{{AUDIENCE}}", audience),
                ("{{EDIT}}", &html_escape(&edit)),
                ("{{PATH}}", &html_escape(stem_path)),
                ("{{SAFE}}", &safe),
            ],
        ))
    }

    /// `asset(path, asset, owner)` → verified open file, media type, filename.
    /// Only assets referenced by the authorised note itself are served.
    pub fn asset(
        &self,
        path: &str,
        asset: &str,
        owner: bool,
    ) -> Result<(OpenedFile, &'static str, String)> {
        let (_, body) = self.read_note(path, owner)?;
        let (_, references) = self.render_body(path, &body);
        let asset = canonical_path(asset)?;
        if !references.contains(asset) {
            return Err(Fault::NotReadable);
        }
        let file = open_file(&self.config.root, asset, MAX_ASSET)?;
        let extension = pypath::suffix(asset).to_lowercase();
        let media = markdown::inline_image_type(&extension).unwrap_or("application/octet-stream");
        Ok((file, media, pypath::name(asset).to_string()))
    }

    /// `vision_overlay.supplement(reader, page, meta, body)`: the published OCR
    /// supplement replaces the body only while it is still bound to the exact
    /// source bytes and every referenced image hash. User edits always win.
    fn supplement(&self, page: &str, meta: &Map<String, Value>, body: String) -> Result<String> {
        let Some(root) = &self.config.vision_state else {
            return Ok(body);
        };
        let target = root
            .join("published")
            .join(format!("{}.json", sha256_hex(page)));
        if std::fs::metadata(&target).is_err() {
            return Ok(body);
        }
        let link = std::fs::symlink_metadata(&target).map_err(|_| Fault::Os)?;
        let size = std::fs::metadata(&target).map_err(|_| Fault::Os)?.len();
        if link.file_type().is_symlink() || size > MAX_OVERLAY {
            return Err(Fault::Value("Invalid vision overlay"));
        }
        let raw_record = std::fs::read(&target).map_err(|_| Fault::Os)?;
        let record: Value = serde_json::from_slice(&raw_record)
            .map_err(|_| Fault::Value("invalid overlay JSON"))?;
        let record = record
            .as_object()
            .ok_or(Fault::Other("invalid overlay record"))?;
        let field = |name: &str| {
            record
                .get(name)
                .ok_or(Fault::Other("incomplete overlay record"))
        };
        let raw = read_limited(open_file(&self.config.root, page, MAX_NOTE)?, u64::MAX)?;
        // Same short-circuit order as Python (a missing key only matters once
        // the previous comparison matched).
        let notebook = meta
            .get("onenote_notebook_id")
            .cloned()
            .unwrap_or(Value::Null);
        if field("page")? != &Value::String(page.to_string())
            || field("notebook_id")? != &notebook
            || field("raw_sha256")? != &Value::String(sha256_hex(&raw))
        {
            return Ok(body);
        }
        let authored = field("authored_body")?
            .as_str()
            .ok_or(Fault::Other("invalid overlay record"))?;
        let (_, references) = self.render_body(page, authored);
        let assets = field("assets")?
            .as_object()
            .ok_or(Fault::Other("invalid overlay record"))?;
        for (asset, expected) in assets {
            if !references.contains(asset) {
                return Ok(body);
            }
            let digest = match open_file(&self.config.root, asset, MAX_OVERLAY_ASSET)
                .and_then(|file| read_limited(file, u64::MAX))
            {
                Ok(bytes) => sha256_hex(&bytes),
                Err(Fault::NotReadable | Fault::Os) => return Ok(body),
                Err(other) => return Err(other),
            };
            if &Value::String(digest) != expected {
                return Ok(body);
            }
        }
        field("body")?
            .as_str()
            .map(str::to_string)
            .ok_or(Fault::Other("invalid overlay record"))
    }
}

/// Single-pass placeholder substitution (inserted values are never rescanned).
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 4096);
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find("}}")
            .map(|e| start + e + 2)
            .unwrap_or(rest.len());
        let marker = &rest[start..end];
        match values.iter().find(|(name, _)| *name == marker) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(marker),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}
