//! `DocumentReader.render_body`: CommonMark (+ raw HTML, tables,
//! strikethrough, SilverBullet `![[wiki images]]`, unique heading ids) →
//! ammonia sanitization with a per-attribute link/asset rewriter.
//!
//! Python used markdown-it-py 4.2.0 and nh3 0.3.7. nh3 is a thin binding to
//! the Rust `ammonia` crate (4.1.4 in that wheel), so the sanitizer here is
//! the same library with the same `Builder::default()` + overrides, and its
//! html5ever re-serialization normalises renderer formatting differences
//! (e.g. `<br />` vs `<br>`). The Markdown parser is the Rust port of the same
//! markdown-it design; remaining differences are measured by the Python oracle
//! tests rather than assumed away.

use super::{LinkTargets, Reader};
use crate::py::{path as pypath, text, url};
use markdown_it::parser::block::builtin::BlockParserRule;
use markdown_it::parser::core::CoreRule;
use markdown_it::parser::extset::MarkdownItExt;
use markdown_it::parser::inline::builtin::InlineParserRule;
use markdown_it::parser::inline::{InlineRoot, InlineRule, InlineState};
use markdown_it::plugins::cmark::block::heading::ATXHeading;
use markdown_it::plugins::cmark::block::lheading::SetextHeader;
use markdown_it::plugins::cmark::inline::image::Image;
use markdown_it::{MarkdownIt, Node};
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub const INLINE_IMAGES: [(&str, &str); 6] = [
    (".png", "image/png"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".gif", "image/gif"),
    (".webp", "image/webp"),
    (".avif", "image/avif"),
];

pub fn inline_image_type(extension: &str) -> Option<&'static str> {
    INLINE_IMAGES
        .iter()
        .find(|(ext, _)| *ext == extension)
        .map(|(_, media)| *media)
}

/// Per-render data for the wiki-image rule (Python closes over `path`).
#[derive(Debug)]
struct WikiContext {
    notebook_prefix: String,
    note_prefix: String,
}
impl MarkdownItExt for WikiContext {}

/// `wiki_image` inline rule, registered before the standard image rule.
struct WikiImage;
impl InlineRule for WikiImage {
    const MARKER: char = '!';

    fn run(state: &mut InlineState) -> Option<(Node, usize)> {
        let start = state.pos;
        if !state.src[start..].starts_with("![[") {
            return None;
        }
        // Python searches the whole inline source, not just up to posMax.
        let end = start + 3 + state.src[start + 3..].find("]]")?;
        if end + 2 > state.pos_max {
            // markdown-it-py would move past posMax inside a nested label; the
            // Rust parser must not, so such pathological input stays literal.
            return None;
        }
        let raw = &state.src[start + 3..end];
        let value = text::strip(raw.split('|').next().unwrap_or(""));
        if value.contains('\n') {
            return None;
        }
        let context = state.md.ext.get::<WikiContext>()?;
        let bare_prefix = context.note_prefix.trim_start_matches('/');
        let value = if value.starts_with(bare_prefix) {
            format!("/{value}")
        } else if value.starts_with(&context.notebook_prefix) {
            format!("{}{}", context.note_prefix, value)
        } else {
            value.to_string()
        };
        Some((
            Node::new(Image {
                url: value,
                title: None,
            }),
            end + 2 - start,
        ))
    }
}

/// Heading ids from the *raw* inline source, like Python's
/// `tokens[index + 1].content`, assigned in document order.
struct HeadingIds;
impl CoreRule for HeadingIds {
    fn run(root: &mut Node, _: &MarkdownIt) {
        let mut used: HashSet<String> = HashSet::new();
        root.walk_mut(|node, _| {
            if !(node.is::<ATXHeading>() || node.is::<SetextHeader>()) {
                return;
            }
            let content = node
                .children
                .first()
                .and_then(|child| child.cast::<InlineRoot>())
                .map(|inline| inline.content.clone())
                .unwrap_or_default();
            let unique = unique_slug(&slug(&content), &mut used);
            node.attrs.push(("id", unique));
        });
    }
}

/// `re.sub(r'[^\w\s-]', '', content).strip().lower()` then `\s+` → `-`.
pub fn slug(content: &str) -> String {
    let kept: String = content
        .chars()
        .filter(|&c| text::is_word(c) || text::is_space(c) || c == '-')
        .collect();
    let lowered = text::strip(&kept).to_lowercase();
    let slug = lowered
        .split(text::is_space)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "heading".into()
    } else {
        slug
    }
}

fn unique_slug(slug: &str, used: &mut HashSet<String>) -> String {
    let mut unique = slug.to_string();
    let mut number = 1;
    while used.contains(&unique) {
        unique = format!("{slug}-{number}");
        number += 1;
    }
    used.insert(unique.clone());
    unique
}

fn parser(path: &str, note_prefix: &str) -> MarkdownIt {
    let mut md = MarkdownIt::new();
    markdown_it::plugins::cmark::add(&mut md);
    markdown_it::plugins::html::add(&mut md);
    markdown_it::plugins::extra::tables::add(&mut md);
    markdown_it::plugins::extra::strikethrough::add(&mut md);
    // Python: `ruler.before('image', 'wiki_image', ...)`. Inline rules are
    // dispatched by marker character, and the image rule is the only other
    // `!` rule, so running first is equivalent.
    md.inline.add_rule::<WikiImage>().before_all();
    md.add_rule::<HeadingIds>()
        .after::<BlockParserRule>()
        .before::<InlineParserRule>();
    md.ext.insert(WikiContext {
        notebook_prefix: format!("{}/", path.split('/').next().unwrap_or("")),
        note_prefix: note_prefix.to_string(),
    });
    md
}

/// Markdown → (sanitized HTML, referenced same-notebook asset paths).
pub fn render_body(reader: &Reader, path: &str, body: &str) -> (String, BTreeSet<String>) {
    let rendered = parser(path, &reader.config.note_prefix)
        .parse(body)
        .render();
    let assets: Arc<Mutex<BTreeSet<String>>> = Arc::default();
    let safe = {
        let (links, page, collected) = (reader.links.clone(), path.to_string(), assets.clone());
        let filter = move |_tag: &str, name: &str, value: &str| -> Option<String> {
            attribute(&links, &page, name, value, &collected)
        };
        let tags: HashSet<&str> = [
            "p",
            "br",
            "hr",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "blockquote",
            "pre",
            "code",
            "strong",
            "em",
            "s",
            "del",
            "ul",
            "ol",
            "li",
            "a",
            "img",
            "table",
            "thead",
            "tbody",
            "tfoot",
            "tr",
            "th",
            "td",
            "caption",
            "details",
            "summary",
            "div",
            "span",
            "sub",
            "sup",
            "kbd",
        ]
        .into();
        let mut attributes: HashMap<&str, HashSet<&str>> = HashMap::from([
            ("a", HashSet::from(["href", "title"])),
            ("img", HashSet::from(["src", "alt", "title"])),
            ("th", HashSet::from(["colspan", "rowspan"])),
            ("td", HashSet::from(["colspan", "rowspan"])),
            ("code", HashSet::from(["class"])),
            ("ol", HashSet::from(["start"])),
        ]);
        for heading in ["h1", "h2", "h3", "h4", "h5", "h6"] {
            attributes.insert(heading, HashSet::from(["id"]));
        }
        ammonia::Builder::default()
            .tags(tags)
            .clean_content_tags(HashSet::from([
                "script", "style", "iframe", "object", "embed", "svg", "math",
            ]))
            .tag_attributes(attributes)
            .attribute_filter(move |tag, name, value| filter(tag, name, value).map(Cow::Owned))
            .url_schemes(HashSet::from(["https", "http", "mailto"]))
            .strip_comments(true)
            .link_rel(Some("noopener noreferrer"))
            .clean(&rendered)
            .to_string()
    };
    let assets = assets.lock().unwrap_or_else(|e| e.into_inner()).clone();
    (safe, assets)
}

/// The nh3 `attribute_filter` callback from `render_body`.
fn attribute(
    links: &LinkTargets,
    path: &str,
    name: &str,
    value: &str,
    assets: &Mutex<BTreeSet<String>>,
) -> Option<String> {
    if name != "href" && name != "src" {
        return Some(value.to_string());
    }
    if name == "href" && value.starts_with('#') {
        return Some(value.to_string());
    }
    // Stricter than Python: where `urlsplit` raises (bad IPv6 brackets), nh3
    // kept the original value; here the attribute is dropped.
    let parsed = url::urlsplit(value)?;
    if let Some(target) = links.target(path, value) {
        let segments: Vec<&str> = target.split('/').collect();
        let asset = segments[..segments.len() - 1]
            .iter()
            .any(|part| part.ends_with(".assets"))
            || inline_image_type(&pypath::suffix(&target).to_lowercase()).is_some();
        if asset {
            // A public page cannot grant access to another notebook's attachments.
            if target.split('/').next() != path.split('/').next() {
                return None;
            }
            assets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(target.clone());
            return Some(format!(
                "/read-asset?{}",
                url::urlencode(&[("path", path), ("asset", &target)])
            ));
        }
        if name == "src" {
            return None;
        }
        let target = if target.ends_with(".md") {
            target
        } else {
            format!("{target}.md")
        };
        let fragment = if parsed.fragment.is_empty() {
            String::new()
        } else {
            format!("#{}", url::quote(&url::unquote(&parsed.fragment), "/"))
        };
        return Some(format!(
            "/read?{}{}",
            url::urlencode(&[("path", &target)]),
            fragment
        ));
    }
    // Never embed remote images or fetch external URLs.
    if name == "href" && matches!(parsed.scheme.as_str(), "https" | "http" | "mailto") {
        return Some(value.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_like_python() {
        assert_eq!(slug("제목"), "제목");
        assert_eq!(slug("  Hello, World! **bold** "), "hello-world-bold");
        assert_eq!(slug("a_b - c"), "a_b---c");
        assert_eq!(slug("!!!"), "heading");
        let mut used = HashSet::new();
        assert_eq!(unique_slug("제목", &mut used), "제목");
        assert_eq!(unique_slug("제목", &mut used), "제목-1");
        assert_eq!(unique_slug("제목", &mut used), "제목-2");
    }
}
