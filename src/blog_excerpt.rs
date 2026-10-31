//! Display-only Markdown/HTML text projection. Indexed evidence is never changed.
use html5ever::buffer_queue::BufferQueue;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{TagKind, Token, TokenSink, TokenSinkResult, Tokenizer};
use std::cell::RefCell;

#[derive(Default)]
struct TextSink {
    text: RefCell<String>,
    links: RefCell<Vec<(Option<String>, usize)>>,
}
impl TokenSink for TextSink {
    type Handle = ();
    fn process_token(&self, token: Token, _: u64) -> TokenSinkResult<()> {
        let mut out = self.text.borrow_mut();
        match token {
            Token::CharacterTokens(text) => out.push_str(&text),
            Token::NullCharacterToken => out.push('\u{fffd}'),
            Token::TagToken(tag) => {
                let name = tag.name.as_ref();
                let start = tag.kind == TagKind::StartTag;
                match name {
                    "br" | "p" | "div" | "section" | "article" | "blockquote" | "pre" | "h1"
                    | "h2" | "h3" | "h4" | "h5" | "h6" | "tr" => {
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                    "li" => {
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                        if start {
                            out.push_str("• ");
                        }
                    }
                    "td" | "th" if !start => out.push('\t'),
                    "a" if start => self.links.borrow_mut().push((
                        tag.attrs
                            .iter()
                            .find(|a| a.name.local.as_ref() == "href")
                            .map(|a| a.value.to_string()),
                        out.len(),
                    )),
                    "a" => {
                        if let Some((Some(href), start)) = self.links.borrow_mut().pop()
                            && !href.starts_with('#')
                            && out.get(start..) != Some(href.as_str())
                        {
                            out.push_str(" (");
                            out.push_str(&href);
                            out.push(')');
                        }
                    }
                    "img" if start => {
                        if let Some(alt) = tag.attrs.iter().find(|a| a.name.local.as_ref() == "alt")
                        {
                            out.push_str(&alt.value);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

pub fn excerpt(markdown: &str) -> String {
    let mut md = markdown_it::MarkdownIt::new();
    markdown_it::plugins::cmark::add(&mut md);
    markdown_it::plugins::html::add(&mut md);
    markdown_it::plugins::extra::tables::add(&mut md);
    markdown_it::plugins::extra::strikethrough::add(&mut md);
    let rendered = md.parse(markdown).render();
    // The HTML parser only tokenizes the sanitized string; it has no networking
    // or browser/JavaScript execution. Entities decode once, so code's escaped
    // '<literal>' and literal '&amp;' survive unchanged as text.
    let sanitized = ammonia::clean(&rendered);
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(sanitized));
    let parser = Tokenizer::new(TextSink::default(), Default::default());
    let _ = parser.feed(&input);
    parser.end();
    parser.sink.text.into_inner().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separates_layout_and_preserves_code_entities_links_and_tables() {
        let source = "<p>one</p><p>two<br>three &amp; four</p><ul><li>first</li><li>second</li></ul>\n\n<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>\n\n```html\n<literal> &amp;\n```\n\n`<inline>` and [docs](https://example.invalid/docs)\n\n<script>forbidden()</script>\n";
        let output = excerpt(source);
        assert!(output.contains("one\ntwo\nthree & four"), "{output:?}");
        assert!(output.contains("• first\n• second"));
        assert!(output.contains("A\tB\t\n1\t2"));
        assert!(output.contains("<literal> &amp;"));
        assert!(output.contains("<inline>"));
        assert!(output.contains("docs (https://example.invalid/docs)"));
        assert!(!output.contains("<p>") && !output.contains("forbidden()"));
        assert!(
            source.contains("<p>one</p>"),
            "the evidence stays untouched"
        );
    }
    #[test]
    fn atomic_code_block_and_truncated_open_fence_keep_literals() {
        let source = format!(
            "Page: Synthetic\n\n```html\n{}\n```\n",
            "<literal> &amp;\n".repeat(700)
        );
        // The producer keeps fenced blocks atomic. The display's 6000-character
        // bound can omit the closing fence, which Markdown still reads as code.
        let truncated = source.chars().take(6000).collect::<String>();
        let output = excerpt(&truncated);
        assert!(output.contains("<literal> &amp;"));
        assert!(!output.contains("<code>"));
        let links = excerpt("<https://example.invalid/> [jump](#anchor)");
        assert_eq!(links.matches("https://example.invalid/").count(), 1);
        assert!(!links.contains("(#anchor)"));
    }
}
