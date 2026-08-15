//! Content extraction (FR-P0-7; design §3.2/§3.3).
//!
//! Two layers, mirroring upstream `packages/core/src/extract.ts` @ b0111612:
//! 1. the extraction-engine trait with `dom_smoothie` (Readability port) as
//!    its first implementation — the declared [VARIANT] surface validated by
//!    metrics, not byte parity;
//! 2. the DOM fallback chain (`extractDomMarkdownFallback` /
//!    `extractDomTextFallback`, extract.ts:407-603) — a self-contained port
//!    over `scraper` that IS byte-parity fixture material.
//!
//! Site-specific extractors (X oEmbed, deleted-tweet detection, YouTube
//! transcripts) are P2 [DEFER] (requirements §2.3) and intentionally absent.

use std::sync::LazyLock;

use dom_smoothie::{Config, Readability, TextMode};
use regex::Regex;
use scraper::{ElementRef, Html, Node, Selector};

use crate::format::estimate_word_count;
use crate::types::{ExtractedContent, IncludeReplies};

/// Upstream defuddle call options (extract.ts:1557-1564):
/// `markdown: format !== "html"`, `removeImages`, `includeReplies`.
/// `include_replies` is carried for the P2 site-extractor surface (design
/// §6 open question); dom_smoothie does not consume it, same as upstream
/// defuddle without site extractors.
#[derive(Debug, Clone)]
pub struct ExtractOptions {
    pub markdown: bool,
    pub remove_images: bool,
    #[allow(dead_code)]
    pub include_replies: IncludeReplies,
}

/// Extraction-engine abstraction (design §3.2): keeps the engine swappable —
/// `dom_smoothie` is the first implementation, not a hard-wired choice.
pub trait Extractor: Send + Sync {
    /// `dependencies.defuddle(document, url, options)` equivalent. Never
    /// panics: engine failures surface as `content: None` + `word_count: 0`,
    /// exactly how upstream degrades a throwing defuddle call (extract.ts:
    /// 1569-1574).
    fn extract(&self, html: &str, url: &str, options: &ExtractOptions) -> ExtractedContent;
}

/// dom_smoothie engine ([VARIANT]): Readability-style extraction with a
/// native markdown serializer (`TextMode::Markdown`). Output content differs
/// from Defuddle's by design; metadata fields are validated to ≥90% parity
/// and the title to 100% (design §5.2).
pub struct DomSmoothieExtractor;

static RE_MD_IMAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[[^\]]*\]\([^)]+\)").expect("valid regex"));
static RE_HTML_IMG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<img\b[^>]*>").expect("valid regex"));

impl Extractor for DomSmoothieExtractor {
    fn extract(&self, html: &str, url: &str, options: &ExtractOptions) -> ExtractedContent {
        let config = Config {
            text_mode: if options.markdown {
                TextMode::Markdown
            } else {
                TextMode::Raw
            },
            ..Config::default()
        };
        let Ok(mut readability) = Readability::new(html.to_string(), Some(url), Some(config))
        else {
            return ExtractedContent {
                content: None,
                word_count: 0,
                ..ExtractedContent::default()
            };
        };
        let Ok(article) = readability.parse() else {
            return ExtractedContent {
                content: None,
                word_count: 0,
                ..ExtractedContent::default()
            };
        };

        let content = if options.markdown {
            article.text_content.to_string()
        } else {
            article.content.to_string()
        };
        // dom_smoothie has no removeImages switch; strip image references at
        // the serialized surface (markdown image syntax / <img> tags) — part
        // of the declared extraction-layer divergence.
        let content = if options.remove_images {
            if options.markdown {
                RE_MD_IMAGE.replace_all(&content, "").into_owned()
            } else {
                RE_HTML_IMG.replace_all(&content, "").into_owned()
            }
        } else {
            content
        };

        ExtractedContent {
            content: Some(content.clone()),
            word_count: estimate_word_count(&content),
            title: non_empty(article.title),
            author: article
                .byline
                .or_else(|| meta_fallback(html, &["author"], "name")),
            published: article
                .published_time
                .or_else(|| meta_fallback(html, &["article:published_time"], "property")),
            site: article
                .site_name
                .or_else(|| meta_fallback(html, &["og:site_name"], "property")),
            language: article.lang,
            extractor_type: Some("dom_smoothie".to_string()),
        }
    }
}

/// Adapter-layer metadata fallback (design §3.2): dom_smoothie misses some of
/// the standard meta surfaces defuddle reads; when the engine returns nothing,
/// read `meta[name=author]`, `meta[property=og:site_name]` etc. directly.
/// This tunes the declared [VARIANT] toward defuddle's metadata behavior —
/// metric-calibrated in extract-metrics.mjs.
fn meta_fallback(html: &str, values: &[&str], key_attribute: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("meta").expect("valid selector");
    for meta in document.select(&selector) {
        let matched = meta
            .value()
            .attr(key_attribute)
            .is_some_and(|key| values.contains(&key));
        if matched {
            if let Some(content) = meta.value().attr("content") {
                let content = content.trim();
                if !content.is_empty() {
                    return Some(content.to_string());
                }
            }
        }
    }
    None
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

// ===== DOM fallback chain (extract.ts:407-603) — byte-parity surface =====

/// `extractDomTextFallback` (extract.ts:407-418): body textContent (falling
/// back to documentElement) through the upstream whitespace pipeline.
pub fn extract_dom_text_fallback(document: &Html) -> String {
    let root = find_body(document)
        .or_else(|| Some(document.root_element()))
        .map(|element| element.text().collect::<String>())
        .unwrap_or_default();
    let text = text_replace_crlf(&root);
    let text = squeeze_blank_lines(&text, 3);
    let lines = text
        .split('\n')
        .map(|line| line.trim())
        .collect::<Vec<_>>()
        .join("\n");
    static RE_SPACES: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("[ \t]{2,}").expect("valid regex"));
    RE_SPACES
        .replace_all(&lines, " ")
        .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .to_string()
}

fn text_replace_crlf(value: &str) -> String {
    value.replace("\r\n", "\n")
}

/// JS `value.replace(/\n{n,}/g, "\n\n")`.
fn squeeze_blank_lines(value: &str, min_newlines: usize) -> String {
    let mut out = String::with_capacity(value.len());
    let mut run = 0usize;
    let flush = |out: &mut String, run: usize| {
        if run == 0 {
            // nothing
        } else if run < min_newlines {
            for _ in 0..run {
                out.push('\n');
            }
        } else {
            out.push_str("\n\n");
        }
    };
    for ch in value.chars() {
        if ch == '\n' {
            run += 1;
            continue;
        }
        flush(&mut out, run);
        run = 0;
        out.push(ch);
    }
    flush(&mut out, run);
    out
}

/// `extractDomMarkdownFallback` (extract.ts:592-603).
pub fn extract_dom_markdown_fallback(document: &Html) -> String {
    let Some(root) = find_body(document).or_else(|| root_element_ref(document)) else {
        return String::new();
    };
    let mut out = String::new();
    for child in child_nodes(&root) {
        out.push_str(&render_block_markdown(child, 0));
    }
    let text = text_replace_crlf(&out);
    static RE_TRAILING: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("[ \t]+\n").expect("valid regex"));
    let text = RE_TRAILING.replace_all(&text, "\n").into_owned();
    let text = squeeze_blank_lines(&text, 3);
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .to_string()
}

fn find_body(document: &Html) -> Option<ElementRef<'_>> {
    document.tree.nodes().find_map(|node| match node.value() {
        Node::Element(element) if element.name() == "body" => ElementRef::wrap(node),
        _ => None,
    })
}

fn root_element_ref(document: &Html) -> Option<ElementRef<'_>> {
    let root = document.root_element();
    // root_element is <html>; upstream falls back to documentElement which is
    // the same node.
    Some(root)
}

/// A child-node view matching the upstream `childNodes` walk (text nodes and
/// elements; comments etc. render as nothing). `ElementRef` derefs to the
/// ego-tree `NodeRef`, whose `children()` iterates ALL child nodes.
enum ChildNode<'a> {
    Text(&'a str),
    Element(ElementRef<'a>),
}

fn child_nodes<'a>(element: &ElementRef<'a>) -> Vec<ChildNode<'a>> {
    element
        .children()
        .filter_map(|child| match child.value() {
            Node::Text(text) => Some(ChildNode::Text(text)),
            Node::Element(_) => ElementRef::wrap(child).map(ChildNode::Element),
            _ => None,
        })
        .collect()
}

/// `normalizeInlineWhitespace` (extract.ts:424-426): `\s+` → " " + trim.
/// JS `\s` includes U+FEFF which Rust's `\s` does not — the regex spellings
/// stay Rust-native; drift samples live in fixtures.
fn normalize_inline_whitespace(value: &str) -> String {
    static RE_WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("valid regex"));
    RE_WS
        .replace_all(value, " ")
        .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .to_string()
}

/// `escapeMarkdownText` (extract.ts:420-422).
fn escape_markdown_text(value: &str) -> String {
    static RE_ESC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"([\\`*_{}\[\]()+#.!|>-])"#).expect("valid regex"));
    RE_ESC.replace_all(value, "\\${1}").into_owned()
}

/// `renderInlineMarkdown` (extract.ts:428-480).
fn render_inline_markdown(node: &ChildNode<'_>) -> String {
    match node {
        ChildNode::Text(text) => normalize_inline_whitespace(text),
        ChildNode::Element(element) => {
            let tag = element.value().name();
            if matches!(tag, "script" | "style" | "meta" | "link") {
                return String::new();
            }
            if tag == "br" {
                return "  \n".to_string();
            }
            if tag == "code" {
                let content = normalize_inline_whitespace(&element.text().collect::<String>());
                return if content.is_empty() {
                    String::new()
                } else {
                    format!("`{content}`")
                };
            }
            if tag == "img" {
                let alt = element.value().attr("alt").unwrap_or_default();
                let src = element.value().attr("src").unwrap_or_default();
                return if src.is_empty() {
                    String::new()
                } else {
                    format!("![{}]({src})", escape_markdown_text(alt))
                };
            }

            let child_content = child_nodes(element)
                .iter()
                .map(render_inline_markdown)
                .collect::<Vec<_>>()
                .join(" ");
            let child_content = normalize_inline_whitespace(&child_content);

            if tag == "a" {
                let href = element.value().attr("href").unwrap_or_default();
                if href.is_empty() {
                    return child_content;
                }
                let label = if child_content.is_empty() {
                    href.to_string()
                } else {
                    child_content.clone()
                };
                return format!("[{label}]({href})");
            }
            if tag == "strong" || tag == "b" {
                return if child_content.is_empty() {
                    String::new()
                } else {
                    format!("**{child_content}**")
                };
            }
            if tag == "em" || tag == "i" {
                return if child_content.is_empty() {
                    String::new()
                } else {
                    format!("*{child_content}*")
                };
            }
            child_content
        }
    }
}

/// `renderBlockMarkdown` (extract.ts:482-590).
fn render_block_markdown(node: ChildNode<'_>, depth: usize) -> String {
    match node {
        ChildNode::Text(text) => {
            let normalized = normalize_inline_whitespace(text);
            if normalized.is_empty() {
                String::new()
            } else {
                format!("{normalized}\n\n")
            }
        }
        ChildNode::Element(element) => {
            let tag = element.value().name().to_string();
            if matches!(tag.as_str(), "script" | "style" | "meta" | "link") {
                return String::new();
            }
            if let Some(level) = heading_level(&tag) {
                let content = inline_children(&element, depth);
                return if content.is_empty() {
                    String::new()
                } else {
                    format!("{} {content}\n\n", "#".repeat(level))
                };
            }
            if tag == "p" {
                let content = inline_children(&element, depth);
                return if content.is_empty() {
                    String::new()
                } else {
                    format!("{content}\n\n")
                };
            }
            if tag == "pre" {
                // extract.ts:518-520: pre content is textContent TRIMMED ONLY
                // (no whitespace squeeze — internal newlines must survive).
                let collected = element.text().collect::<String>();
                let content =
                    collected.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
                return if content.is_empty() {
                    String::new()
                } else {
                    format!("```\n{content}\n```\n\n")
                };
            }
            if tag == "blockquote" {
                let content = child_nodes(&element)
                    .into_iter()
                    .map(|child| render_block_markdown(child, depth))
                    .collect::<String>();
                let trimmed = content
                    .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
                    .to_string();
                if trimmed.is_empty() {
                    return String::new();
                }
                let quoted = trimmed
                    .split('\n')
                    .map(|line| {
                        if line.is_empty() {
                            ">".to_string()
                        } else {
                            format!("> {line}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return format!("{quoted}\n\n");
            }
            if tag == "ul" || tag == "ol" {
                return render_list(&element, &tag, depth);
            }
            if tag == "hr" {
                return "---\n\n".to_string();
            }

            let block_content = child_nodes(&element)
                .into_iter()
                .map(|child| render_block_markdown(child, depth))
                .collect::<String>();
            if !block_content
                .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
                .is_empty()
            {
                return block_content;
            }
            let inline_content = inline_children(&element, depth);
            if inline_content.is_empty() {
                String::new()
            } else {
                format!("{inline_content}\n\n")
            }
        }
    }
}

/// `element.childNodes.map(renderInlineMarkdown).join(" ")` + normalize
/// (extract.ts:498-505 / 509-515 pattern).
fn inline_children(element: &ElementRef<'_>, _depth: usize) -> String {
    let joined = child_nodes(element)
        .iter()
        .map(render_inline_markdown)
        .collect::<Vec<_>>()
        .join(" ");
    normalize_inline_whitespace(&joined)
}

/// List rendering (extract.ts:535-569): `li` children only, nested lists
/// recurse as blocks with depth+1, non-first lines indent one level deeper.
fn render_list(element: &ElementRef<'_>, tag: &str, depth: usize) -> String {
    let items = element
        .child_elements()
        .filter(|li| li.value().name() == "li")
        .enumerate()
        .filter_map(|(index, li)| {
            let prefix = if tag == "ol" {
                format!("{}. ", index + 1)
            } else {
                "- ".to_string()
            };
            let pieces = child_nodes(&li)
                .into_iter()
                .map(|grandchild| match &grandchild {
                    ChildNode::Element(el)
                        if el.value().name() == "ul" || el.value().name() == "ol" =>
                    {
                        format!("\n{}", render_block_markdown(grandchild, depth + 1))
                    }
                    _ => render_inline_markdown(&grandchild),
                })
                .collect::<Vec<_>>()
                .join(" ");
            static RE_WS_NL: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r"\s+\n").expect("valid regex"));
            static RE_NL_WS: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r"\n\s+").expect("valid regex"));
            static RE_WS: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r"\s+").expect("valid regex"));
            let squeezed = RE_WS_NL.replace_all(&pieces, "\n");
            let squeezed = RE_NL_WS.replace_all(&squeezed, "\n");
            let content = RE_WS.replace_all(&squeezed, " ");
            let content = content
                .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
                .to_string();
            if content.is_empty() {
                return None;
            }
            let indented = content
                .split('\n')
                .enumerate()
                .map(|(line_index, line)| {
                    if line_index == 0 {
                        format!("{}{}{}", "  ".repeat(depth), prefix, line)
                    } else {
                        format!("{}{}", "  ".repeat(depth + 1), line)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(indented)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if items.is_empty() {
        String::new()
    } else {
        format!("{items}\n\n")
    }
}

fn heading_level(tag: &str) -> Option<usize> {
    let mut chars = tag.chars();
    if chars.next() != Some('h') {
        return None;
    }
    let digits: String = chars.collect();
    if digits.len() != 1 {
        return None;
    }
    let level = digits.parse::<usize>().ok()?;
    (1..=6).contains(&level).then_some(level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dom_text_fallback_matches_upstream_pipeline() {
        let html = "<html><body>Hello   world\n\n\n\nEnd</body></html>";
        let document = Html::parse_document(html);
        // \n{3,} → \n\n runs first; the [ \t]{2,} → " " squeeze runs after the
        // per-line trim, collapsing the three spaces to one.
        assert_eq!(extract_dom_text_fallback(&document), "Hello world\n\nEnd");
    }

    #[test]
    fn dom_markdown_fallback_renders_structure() {
        let html = "<html><body><h1>Title</h1><p>Some <b>bold</b> text</p><ul><li>one</li><li>two</li></ul></body></html>";
        let document = Html::parse_document(html);
        let markdown = extract_dom_markdown_fallback(&document);
        assert_eq!(markdown, "# Title\n\nSome **bold** text\n\n- one\n- two");
    }

    #[test]
    fn dom_markdown_fallback_links_and_code() {
        let html = "<html><body><p>See <a href=\"/docs\">docs</a> and <code>let x</code>.</p><pre>line1\nline2</pre></body></html>";
        let document = Html::parse_document(html);
        let markdown = extract_dom_markdown_fallback(&document);
        // Upstream joins child-node renders with " " (extract.ts:459-463), so
        // the trailing "." lands after a join space: "`let x` ." — preserved.
        // pre keeps internal newlines (textContent, trim only).
        assert_eq!(
            markdown,
            "See [docs](/docs) and `let x` .\n\n```\nline1\nline2\n```"
        );
    }

    #[test]
    fn dom_smoothie_extracts_article_markdown() {
        let html = "<html><head><title>Test Page</title></head><body><article><h1>Big Title</h1><p>This article body has plenty of words to pass readability scoring thresholds and be considered the main readable content of the page for sure.</p></article></body></html>";
        let extracted = DomSmoothieExtractor.extract(
            html,
            "https://example.com/post",
            &ExtractOptions {
                markdown: true,
                remove_images: false,
                include_replies: IncludeReplies::Extractors,
            },
        );
        let content = extracted.content.expect("content");
        assert!(content.contains("plenty of words"), "got: {content}");
        assert!(extracted.word_count > 10);
    }

    #[test]
    fn engine_failure_degrades_to_empty_extraction() {
        // A document Readability cannot make sense of still returns the
        // empty-extraction shape (never panics), matching the upstream
        // defuddle-throw degradation path.
        let extracted = DomSmoothieExtractor.extract(
            "",
            "https://example.com/empty",
            &ExtractOptions {
                markdown: true,
                remove_images: false,
                include_replies: IncludeReplies::Extractors,
            },
        );
        assert_eq!(extracted.word_count, 0);
    }
}
