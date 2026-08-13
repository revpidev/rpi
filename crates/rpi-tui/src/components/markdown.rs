//! Port of `packages/tui/src/components/markdown.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences (see the deviation record maintained by the main
//! session):
//! - The Markdown parser is `comrak` 0.54 (CommonMark + GFM extensions)
//!   instead of upstream's `marked` 18.0.5. The AST mapping is documented in
//!   the module; `token.raw` is reconstructed by slicing the source text with
//!   comrak `sourcepos` ranges (byte columns), and marked's `space` tokens
//!   (blank lines between blocks) are synthesized from the source gaps, since
//!   CommonMark ASTs drop them.
//! - Theme callbacks are `Box<dyn Fn(&str) -> String + Send + Sync>` fields
//!   (upstream plain functions); `Markdown::new` takes the theme as an
//!   `Arc<MarkdownTheme>` so callers can share one theme across instances.
//! - The render cache and the default-style-prefix cache use interior
//!   mutability (`RefCell`) because `Component::render` takes `&self`.
//! - Link fallback comparison uses the rendered plain text of the link
//!   children instead of marked's raw source text of the link body (upstream
//!   `token.text`); the two only differ for link bodies whose raw source and
//!   rendered text disagree in exotic cases (`[a\\*b](a*b)` shows a `(url)`
//!   suffix here but not upstream, and vice versa for `[**x**](**x**)`).
//! - Strikethrough is parsed by comrak's GFM extension. Verified identical to
//!   marked's `StrictStrikethroughTokenizer` for all tested cases
//!   (`~~ foo~~`, `~~foo ~~`, `~~foo~~~`, `~~~foo~~~`, `a~~b~~c`, ...),
//!   except: (1) marked strips backslash-escaped tildes in opener position
//!   (`~~\~~x~~` strikes upstream, stays plain text here), and (2) marked
//!   does not decode HTML entities inside strikethrough (`~~&amp;~~` renders
//!   `&amp;` upstream, `&` here).
//! - Empty-marked whitespace: JS `String.trim()` uses the ECMA-262 whitespace
//!   set; Rust `str::trim` uses Unicode White_Space. The sets differ only for
//!   exotic whitespace characters (U+FEFF etc.), where the render decisions
//!   may diverge.
//! - `preserveBackslashEscapes` re-emits backslashes by rescanning each text
//!   node's source slice (comrak resolves escapes into plain text nodes);
//!   equivalent to marked's `escape` tokens, including merged text runs.
//! - The table fallback (`token.raw`) re-appends the trailing newline when
//!   the source had one, so `wrapTextWithAnsi` produces the same trailing
//!   empty line as upstream.
//! - The width-aware `transform` option (markdown.ts:285 @ 4181f66) runs on
//!   every render and the transformed text is part of the render cache key;
//!   upstream caches by raw source text + width and skips the transform on
//!   cache hits. A stateful transform closure (e.g. the mermaid
//!   streaming/final switch) therefore cannot serve stale lines here.
//! - LaTeX integration (markdown.ts:26-144 @ 4181f66): marked tokenizer
//!   extensions (`tokenizeInlineLatex`, `tokenizeBlockLatex`,
//!   `looksLikePendingDollarMath`, `isEscaped`, `findClosingDelimiter`) are
//!   ported as free functions below (same snake_case names). comrak has no
//!   tokenizer extension mechanism, so the integration point differs:
//!   display-math blocks are detected on `Paragraph` nodes by scanning their
//!   raw source with `tokenize_block_latex` at every line start and splitting
//!   the paragraph into plain/display segments (render-time, equivalent to
//!   marked cutting its paragraph before the next `startBlock` match,
//!   markdown.ts:129 + marked's `startBlock` paragraph truncation); inline
//!   math is scanned on each `Text` node's raw source slice, equivalent to
//!   marked's inline extension being called at every lexer position.
//!   Deviations:
//!   - A pending inline token only consumes the rest of its text node plus
//!     the raw slices of the remaining siblings (upstream consumes the rest
//!     of the inline source); identical for the common case where the pending
//!     span ends the paragraph.
//!   - A pending display token is scoped to its paragraph (upstream consumes
//!     the rest of the document). Both self-heal when the delimiter closes.
//!   - Inline math cannot cross comrak inline-node boundaries (e.g. `*`
//!     emphasis inside `$...$`); upstream's latex tokenizer wins over
//!     emphasis in those cases. Plain segments of mixed text nodes are
//!     re-emitted from the raw source with CommonMark backslash-escape
//!     resolution (HTML entity decoding is not re-applied there).
//!   - Display math spanning a blank line is not merged (comrak splits the
//!     paragraph); upstream's lazy block regex can span blank lines.
//!   - `render_latex` is part of the render cache key (upstream has no such
//!     cache key); flipping the option without `invalidate()` re-renders
//!     instead of serving stale lines.

use std::cell::RefCell;
use std::sync::Arc;

use comrak::nodes::{Node, NodeValue, Sourcepos};
use comrak::Arena;
use comrak::{parse_document, Options};

use crate::latex::render_latex;
use crate::terminal_image::{get_capabilities, hyperlink, is_image_line};
use crate::tui::Component;
use crate::utils::{
    apply_background_to_line, extract_ansi_code, visible_width, wrap_text_with_ansi,
};

/// Theme callback (upstream `(text: string) => string`).
pub type ThemeTextFn = Box<dyn Fn(&str) -> String + Send + Sync>;

/// Highlight callback (upstream `(code: string, lang?: string) => string[]`).
pub type HighlightFn = Box<dyn Fn(&str, Option<&str>) -> Vec<String> + Send + Sync>;

/// Default text styling for markdown content (upstream `DefaultTextStyle`,
/// markdown.ts:59-72). Applied to all text unless overridden by markdown
/// formatting. Background color is applied at the padding stage, not here.
#[derive(Default)]
pub struct DefaultTextStyle {
    /// Foreground color function.
    pub color: Option<ThemeTextFn>,
    /// Background color function (applied to full-width lines).
    pub bg_color: Option<ThemeTextFn>,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
}

/// Theme functions for markdown elements (upstream `MarkdownTheme`,
/// markdown.ts:74-96). Each function takes text and returns styled text with
/// ANSI codes.
pub struct MarkdownTheme {
    pub heading: ThemeTextFn,
    pub link: ThemeTextFn,
    pub link_url: ThemeTextFn,
    pub code: ThemeTextFn,
    pub code_block: ThemeTextFn,
    pub code_block_border: ThemeTextFn,
    pub quote: ThemeTextFn,
    pub quote_border: ThemeTextFn,
    pub hr: ThemeTextFn,
    pub list_bullet: ThemeTextFn,
    pub bold: ThemeTextFn,
    pub italic: ThemeTextFn,
    pub strikethrough: ThemeTextFn,
    pub underline: ThemeTextFn,
    pub highlight_code: Option<HighlightFn>,
    /// Prefix applied to each rendered code block line (default: "  ").
    pub code_block_indent: Option<String>,
}

impl MarkdownTheme {
    /// Identity theme — every callback returns its input unchanged (used by
    /// tests and snapshots).
    pub fn identity() -> Self {
        let identity = |text: &str| text.to_string();
        Self {
            heading: Box::new(identity),
            link: Box::new(identity),
            link_url: Box::new(identity),
            code: Box::new(identity),
            code_block: Box::new(identity),
            code_block_border: Box::new(identity),
            quote: Box::new(identity),
            quote_border: Box::new(identity),
            hr: Box::new(identity),
            list_bullet: Box::new(identity),
            bold: Box::new(identity),
            italic: Box::new(identity),
            strikethrough: Box::new(identity),
            underline: Box::new(identity),
            highlight_code: None,
            code_block_indent: None,
        }
    }
}

/// Width-aware Markdown source transform (markdown.ts:102-103 @ 4181f66):
/// receives the raw Markdown source and the available content width
/// (`width - paddingX * 2`), returns the transformed source. Applied before
/// parsing (markdown.ts:285). rpi-tui stays host-agnostic, so this is a
/// plain function type — the ext-host `MarkdownTransformerFn` (with its
/// `MarkdownTransformContext`) is adapted onto it by the rpi crate.
pub type MarkdownTransformFn = Arc<dyn Fn(&str, usize) -> String + Send + Sync>;

/// `MarkdownOptions` (markdown.ts:98-103). Not `Copy`/`PartialEq`/`Debug`:
/// the `transform` field carries a closure. `render_latex` defaults to
/// `true` (markdown.ts:98-99), so `Default` is implemented manually.
#[derive(Clone)]
pub struct MarkdownOptions {
    /// Preserve source list markers instead of normalizing them.
    pub preserve_ordered_list_markers: bool,
    /// Preserve source backslash escapes instead of normalizing escaped
    /// punctuation.
    pub preserve_backslash_escapes: bool,
    /// Render LaTeX math spans (`$...$`, `\(...\)`, …). Consumed by the
    /// LaTeX renderer (T29); not yet wired into `render_token` — see the
    /// TODO marker there.
    pub render_latex: bool,
    /// Optional width-aware source transform, applied before parsing
    /// (markdown.ts:285). `None` = pass-through.
    pub transform: Option<MarkdownTransformFn>,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        MarkdownOptions {
            preserve_ordered_list_markers: false,
            preserve_backslash_escapes: false,
            // markdown.ts:98-99: latex rendering is on by default.
            render_latex: true,
            transform: None,
        }
    }
}

/// `InlineStyleContext` (markdown.ts:105-108).
struct InlineStyleContext<'a> {
    apply_text: &'a dyn Fn(&str) -> String,
    style_prefix: &'a str,
}

/// Synthetic "space" block — the comrak equivalent of marked's `space`
/// tokens (blank lines between blocks), reconstructed from source gaps.
enum Block<'a> {
    Space,
    Node(Node<'a>),
}

impl Block<'_> {
    /// The `nextTokenType` of this block (markdown.ts:182): only `"space"`
    /// and `"list"` are distinguished by the renderer.
    fn token_type(&self) -> TokenType {
        match self {
            Block::Space => TokenType::Space,
            Block::Node(node) => {
                if matches!(
                    node.data.borrow().value,
                    NodeValue::List(_) | NodeValue::Item(_) | NodeValue::TaskItem(_)
                ) {
                    TokenType::List
                } else {
                    TokenType::Other
                }
            }
        }
    }
}

/// `nextTokenType` comparisons (markdown.ts): only `space` and `list` matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenType {
    Space,
    List,
    Other,
}

/// Sentinel used to extract the ANSI prefix of a style function
/// (markdown.ts:288).
const STYLE_SENTINEL: char = '\0';

/// Render cache entry (upstream `cachedText` / `cachedWidth` / `cachedLines`,
/// markdown.ts:120-122). The transformed source and the `render_latex` option
/// are part of the cache key so a stateful transform closure or a flipped
/// option cannot serve stale lines (deviation from upstream, which keys on
/// the raw source text only — T29).
struct MarkdownCache {
    text: String,
    transformed: String,
    width: usize,
    render_latex: bool,
    lines: Vec<String>,
}

/// Markdown component (upstream `Markdown`, markdown.ts:110).
pub struct Markdown {
    text: String,
    padding_x: usize, // Left/right padding
    padding_y: usize, // Top/bottom padding
    default_text_style: Option<DefaultTextStyle>,
    theme: Arc<MarkdownTheme>,
    options: MarkdownOptions,
    default_style_prefix: RefCell<Option<String>>,
    cache: RefCell<Option<MarkdownCache>>,
}

/// Parse options: GFM extensions matching marked's defaults (tables,
/// strikethrough, task lists, autolinks).
fn parser_options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.tasklist = true;
    options.extension.autolink = true;
    options
}

/// Byte offset of the (line, column) position in `source`. comrak columns are
/// 1-based UTF-8 byte offsets (comrak `LineColumn` docs).
fn byte_offset_of(source: &str, line: usize, column: usize) -> usize {
    let mut offset = 0usize;
    for (index, part) in source.split_inclusive('\n').enumerate() {
        if index + 1 == line {
            // Column 0 marks the position right after the line's newline.
            return offset.saturating_add(column.saturating_sub(1));
        }
        offset += part.len();
    }
    // Past the last line (trailing newline): clamp to the end.
    offset
}

/// The source text covered by `sp`, end inclusive.
fn node_slice(source: &str, sp: Sourcepos) -> &str {
    let start = byte_offset_of(source, sp.start.line, sp.start.column);
    let end = byte_offset_of(source, sp.end.line, sp.end.column);
    source.get(start..=end).unwrap_or("")
}

/// `token.raw` reconstruction (markdown.ts): the source slice, plus the
/// trailing newline when the source line actually has one (marked's `raw`
/// includes it for tables; comrak's sourcepos excludes it).
fn node_raw(source: &str, sp: Sourcepos) -> &str {
    let slice = node_slice(source, sp);
    let end = byte_offset_of(source, sp.end.line, sp.end.column);
    if source.as_bytes().get(end + 1) == Some(&b'\n') {
        let start = byte_offset_of(source, sp.start.line, sp.start.column);
        return source.get(start..=end + 1).unwrap_or(slice);
    }
    slice
}

/// ECMA-262 `\s` (WhiteSpace + LineTerminator + U+FEFF); used for marked
/// regex character-class parity.
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// JS `\S` (negated `\s`).
fn is_js_non_whitespace(c: char) -> bool {
    !is_js_whitespace(c)
}

/// ECMA-262 `String.prototype.trimEnd` (upstream `.trim()`/`.trimEnd()` on
/// latex raw/text uses the `\s` set above, not Rust's Unicode White_Space).
fn trim_js_end(s: &str) -> &str {
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        if !is_js_whitespace(c) {
            break;
        }
        start = i;
    }
    &s[..start]
}

/// ECMA-262 `String.prototype.trimStart`.
fn trim_js_start(s: &str) -> &str {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if !is_js_whitespace(c) {
            break;
        }
        end = i + c.len_utf8();
    }
    &s[end..]
}

/// ECMA-262 `String.prototype.trim`.
fn trim_js(s: &str) -> &str {
    trim_js_end(trim_js_start(s))
}

/// `isEscaped` (markdown.ts:32-38): an odd number of backslashes before the
/// position.
fn is_escaped(source: &str, index: usize) -> bool {
    let mut backslashes = 0;
    let mut position = index;
    while position > 0 && source.as_bytes()[position - 1] == b'\\' {
        backslashes += 1;
        position -= 1;
    }
    backslashes % 2 == 1
}

/// `findClosingDelimiter` (markdown.ts:40-46): first closing delimiter that
/// is not escaped.
fn find_closing_delimiter(source: &str, closing: &str, start: usize) -> Option<usize> {
    let mut index = source[start..].find(closing).map(|i| i + start);
    while let Some(found) = index {
        if !is_escaped(source, found) {
            return Some(found);
        }
        index = source[found + closing.len()..]
            .find(closing)
            .map(|i| i + found + closing.len());
    }
    None
}

/// `looksLikePendingDollarMath` (markdown.ts:48-50):
/// `/\\[A-Za-z]+|[_^=+*/<>()[\]|±≤≥≠≈∈→⇒∞∫∑√-]/`.
fn looks_like_pending_dollar_math(source: &str) -> bool {
    if source.contains('\\') {
        let bytes = source.as_bytes();
        for i in 0..bytes.len() - 1 {
            if bytes[i] == b'\\' && bytes[i + 1].is_ascii_alphabetic() {
                return true;
            }
        }
    }
    source.chars().any(|c| {
        matches!(
            c,
            '_' | '^'
                | '='
                | '+'
                | '*'
                | '/'
                | '<'
                | '>'
                | '('
                | ')'
                | '['
                | ']'
                | '|'
                | '±'
                | '≤'
                | '≥'
                | '≠'
                | '≈'
                | '∈'
                | '→'
                | '⇒'
                | '∞'
                | '∫'
                | '∑'
                | '√'
                | '-'
        )
    })
}

/// `/^[A-Z_][A-Z0-9_]*(?:[^A-Za-z0-9_\s])?$/` (markdown.ts:77) — the currency
/// half of the `$` guard.
fn is_currency_content(content: &str) -> bool {
    let mut chars = content.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_uppercase() || first == '_') {
        return false;
    }
    for c in chars.by_ref() {
        if c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' {
            continue;
        }
        // The single optional trailing character must be the last one and
        // must not be a word character or ECMA whitespace.
        return chars.next().is_none() && !is_js_whitespace(c);
    }
    true
}

/// An inline LaTeX token (upstream `LatexToken` with `type: "latex"`,
/// markdown.ts:26-30).
struct LatexInlineToken<'a> {
    raw: &'a str,
    text: &'a str,
    pending: bool,
}

/// `tokenizeInlineLatex` (markdown.ts:52-99).
fn tokenize_inline_latex(source: &str) -> Option<LatexInlineToken<'_>> {
    let (opening, closing) = if source.starts_with("$$") {
        ("$$", "$$")
    } else if source.starts_with("\\(") {
        ("\\(", "\\)")
    } else if source.starts_with("\\[") {
        ("\\[", "\\]")
    } else if source.starts_with('$') && !source[1..].chars().next().is_some_and(is_js_whitespace) {
        ("$", "$")
    } else {
        return None;
    };

    if let Some(closing_index) = find_closing_delimiter(source, closing, opening.len()) {
        if opening == "$" {
            let content = &source[opening.len()..closing_index];
            let after = &source[closing_index + closing.len()..];
            let content_ends_whitespace = content.chars().next_back().is_some_and(is_js_whitespace);
            let after_starts_digit = after.chars().next().is_some_and(|c| c.is_ascii_digit());
            let currency = is_currency_content(content)
                && after
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
            if content_ends_whitespace || after_starts_digit || currency || content.contains('`') {
                return None;
            }
        }
        let text = &source[opening.len()..closing_index];
        if text.is_empty() || text.contains('\n') {
            return None;
        }
        return Some(LatexInlineToken {
            raw: &source[..closing_index + closing.len()],
            text,
            pending: false,
        });
    }

    let pending_source = &source[opening.len()..];
    if opening.starts_with('\\') || looks_like_pending_dollar_math(pending_source) {
        return Some(LatexInlineToken {
            raw: source,
            text: pending_source,
            pending: true,
        });
    }
    None
}

/// Skip the ` {0,3}` prefix of the block latex patterns.
fn skip_up_to_3_spaces(source: &str) -> &str {
    let mut rest = source;
    for _ in 0..3 {
        rest = rest.strip_prefix(' ').unwrap_or(rest);
    }
    rest
}

/// Skip the `[ \t]*` runs of the block latex patterns.
fn skip_horizontal(source: &str) -> &str {
    source.trim_start_matches([' ', '\t'])
}

/// The shared shape of `dollarMatch` and `bracketMatch` (markdown.ts:102-110):
/// `^ {0,3}(opening)[ \t]*(?:\n)?([\s\S]*?)(closing)[ \t]*(?:\n|$)`. Returns
/// the content (un-trimmed) and the total match length; the closing must be
/// followed by horizontal whitespace and then a newline or end of input
/// (lazy — the first such closing wins).
fn match_block_delimited<'s>(
    source: &'s str,
    opening: &str,
    closing: &str,
) -> Option<(&'s str, usize)> {
    let after_opening = skip_up_to_3_spaces(source).strip_prefix(opening)?;
    let after_horizontal = skip_horizontal(after_opening);
    let content_start = after_horizontal
        .strip_prefix('\n')
        .unwrap_or(after_horizontal);

    let mut search = content_start;
    let content_start_offset = content_start.as_ptr() as usize - source.as_ptr() as usize;
    while let Some(relative) = search.find(closing) {
        let tail = &search[relative + closing.len()..];
        let tail_after_horizontal = skip_horizontal(tail);
        let trailing_ws_len = tail.len() - tail_after_horizontal.len();
        if tail_after_horizontal.is_empty() || tail_after_horizontal.starts_with('\n') {
            let trailing_newline = usize::from(tail_after_horizontal.starts_with('\n'));
            let raw_len = content_start_offset
                + relative
                + closing.len()
                + trailing_ws_len
                + trailing_newline;
            return Some((&content_start[..relative], raw_len));
        }
        search = &search[relative + closing.len()..];
    }
    None
}

/// The shared `^ {0,3}(opening)[ \t]*(?:\n)?` prefix of the pending block
/// patterns (markdown.ts:112-119).
fn strip_block_opening<'s>(source: &'s str, opening: &str) -> Option<&'s str> {
    let after_opening = skip_up_to_3_spaces(source).strip_prefix(opening)?;
    let after_horizontal = skip_horizontal(after_opening);
    Some(
        after_horizontal
            .strip_prefix('\n')
            .unwrap_or(after_horizontal),
    )
}

/// A display LaTeX token (upstream `LatexToken` with `type: "latexBlock"`).
struct LatexBlockToken<'a> {
    raw: &'a str,
    text: &'a str,
    pending: bool,
}

/// `tokenizeBlockLatex` (markdown.ts:101-121).
fn tokenize_block_latex(source: &str) -> Option<LatexBlockToken<'_>> {
    // dollarMatch: `^ {0,3}\$\$[ \t]*(?:\n)?([\s\S]*?)\$\$[ \t]*(?:\n|$)`.
    // A falsy (empty) content group makes upstream fall through.
    if let Some((content, raw_len)) = match_block_delimited(source, "$$", "$$") {
        if !content.is_empty() {
            return Some(LatexBlockToken {
                raw: &source[..raw_len],
                text: trim_js(content),
                pending: false,
            });
        }
    }
    // bracketMatch: `^ {0,3}\\\[[ \t]*(?:\n)?([\s\S]*?)\\\][ \t]*(?:\n|$)`.
    if let Some((content, raw_len)) = match_block_delimited(source, "\\[", "\\]") {
        if !content.is_empty() {
            return Some(LatexBlockToken {
                raw: &source[..raw_len],
                text: trim_js(content),
                pending: false,
            });
        }
    }
    // pendingBracket: `^ {0,3}\\\[[ \t]*(?:\n)?([\s\S]*)$` — always pending.
    if let Some(rest) = strip_block_opening(source, "\\[") {
        return Some(LatexBlockToken {
            raw: source,
            text: rest,
            pending: true,
        });
    }
    // pendingDollar: `^ {0,3}\$\$[ \t]*(?:\n)?([\s\S]*)$` — pending only for
    // non-empty math-looking content.
    if let Some(rest) = strip_block_opening(source, "$$") {
        if !rest.is_empty() && looks_like_pending_dollar_math(rest) {
            return Some(LatexBlockToken {
                raw: source,
                text: rest,
                pending: true,
            });
        }
    }
    None
}

/// CommonMark backslash-escape resolution for the plain segments of mixed
/// text nodes (comrak resolves escapes in its literal; raw slices keep them).
fn resolve_backslash_escapes(text: &str) -> String {
    if !text.contains('\\') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek().copied() {
                Some(next) if next.is_ascii_punctuation() => {
                    out.push(next);
                    chars.next();
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// One segment of a paragraph split for display LaTeX (see
/// `split_latex_blocks`).
enum LatexParagraphSegment<'a> {
    Plain {
        start: usize,
        end: usize,
    },
    Latex {
        start: usize,
        end: usize,
        token: LatexBlockToken<'a>,
    },
}

/// Split a paragraph's raw source into plain and display-LaTeX segments,
/// trying `tokenize_block_latex` at every line start — the equivalent of
/// marked's block tokenizer running at block boundaries and its `startBlock`
/// truncating the paragraph before the next block-LaTeX opening.
fn split_latex_blocks(raw: &str) -> Vec<LatexParagraphSegment<'_>> {
    let mut segments = Vec::new();
    let mut position = 0;
    let mut plain_start = 0;
    while position < raw.len() {
        match tokenize_block_latex(&raw[position..]) {
            Some(token) => {
                if position > plain_start {
                    segments.push(LatexParagraphSegment::Plain {
                        start: plain_start,
                        end: position,
                    });
                }
                let raw_len = token.raw.len();
                segments.push(LatexParagraphSegment::Latex {
                    start: position,
                    end: position + raw_len,
                    token,
                });
                position += raw_len;
                plain_start = position;
            }
            None => match raw[position..].find('\n') {
                Some(newline) => position += newline + 1,
                None => break,
            },
        }
    }
    if plain_start < raw.len() {
        segments.push(LatexParagraphSegment::Plain {
            start: plain_start,
            end: raw.len(),
        });
    }
    segments
}

/// Whether the source gap between two blocks contains a blank line. Blank
/// lines are quoted with `>` markers stripped (`> a\n>\n> b` has a blank
/// line between the two paragraphs). The boundary segments (before the first
/// newline / after the last) belong to the neighboring block lines.
fn contains_blank_line(gap: &str) -> bool {
    let mut segments = gap.split('\n');
    let _leading_boundary = segments.next();
    // Skip the trailing boundary segment as well; only the middle segments
    // are actual lines of the gap.
    let mut segments = segments.peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            break;
        }
        if is_blank_quoted_line(segment) {
            return true;
        }
    }
    false
}

/// A line that is blank after stripping blockquote markers (`^ {0,3}>` per
/// nesting level) and ECMA-262 whitespace.
fn is_blank_quoted_line(line: &str) -> bool {
    let mut rest = line;
    loop {
        let bytes = rest.as_bytes();
        let mut i = 0;
        while i < 3 && bytes.get(i) == Some(&b' ') {
            i += 1;
        }
        if bytes.get(i) == Some(&b'>') {
            rest = &rest[i + 1..];
            continue;
        }
        break;
    }
    rest.chars().all(is_js_whitespace)
}

/// Build the block list for a container's children, synthesizing `space`
/// blocks where the source has blank lines (marked emits `space` tokens for
/// blank lines between block tokens; comrak drops them). Leading/trailing
/// blank lines become leading/trailing space blocks.
fn synthesize_blocks<'a>(
    children: Vec<Node<'a>>,
    source: &str,
    container_start: usize,
    container_end: usize,
) -> Vec<Block<'a>> {
    let mut blocks: Vec<Block<'a>> = Vec::with_capacity(children.len() * 2 + 1);

    if let Some(first) = children.first() {
        let start = node_start_offset(source, first);
        if contains_blank_line(&source[container_start..start]) {
            blocks.push(Block::Space);
        }
    }

    for (index, child) in children.iter().enumerate() {
        if index > 0 {
            let prev_end = node_end_offset(source, children[index - 1]);
            let next_start = node_start_offset(source, child);
            if contains_blank_line(&source[prev_end + 1..next_start]) {
                blocks.push(Block::Space);
            }
        }
        blocks.push(Block::Node(child));
    }

    if let Some(last) = children.last() {
        let last_end = node_end_offset(source, last);
        if last_end < container_end && contains_blank_line(&source[last_end + 1..container_end]) {
            blocks.push(Block::Space);
        }
    }

    blocks
}

fn node_start_offset(source: &str, node: Node<'_>) -> usize {
    let sp = node.data.borrow().sourcepos;
    byte_offset_of(source, sp.start.line, sp.start.column)
}

fn node_end_offset(source: &str, node: Node<'_>) -> usize {
    let sp = node.data.borrow().sourcepos;
    byte_offset_of(source, sp.end.line, sp.end.column)
}

/// `trimPartialClosingFences` (markdown.ts:25-48): recursively find the last
/// code token (through list items / blockquotes) and trim a streamed partial
/// closing fence from its text so code blocks do not shrink/flicker when the
/// final fence character arrives.
fn trim_partial_closing_fences<'a>(node: Node<'a>, source: &str) {
    fn walk<'a>(node: Node<'a>, source: &str) {
        let is_container = matches!(
            node.data.borrow().value,
            NodeValue::List(_)
                | NodeValue::Item(_)
                | NodeValue::TaskItem(_)
                | NodeValue::BlockQuote
        );
        if is_container {
            if let Some(last_child) = node.children().last() {
                walk(last_child, source);
            }
            return;
        }
        if !matches!(node.data.borrow().value, NodeValue::CodeBlock(_)) {
            return;
        }

        let sp = node.data.borrow().sourcepos;
        let raw = node_slice(source, sp);

        // /^(`{3,}|~{3,})/ (markdown.ts:41)
        let Some(marker_char) = fence_marker(raw) else {
            return;
        };
        let marker_len = raw.bytes().take_while(|&b| b == marker_char as u8).count();

        let last_line = raw.rsplit('\n').next().unwrap_or(raw);
        let last_line_len = last_line.chars().count();
        if last_line.is_empty()
            || last_line_len >= marker_len
            || !last_line.chars().all(|c| c == marker_char)
        {
            return;
        }

        // token.text = token.text.slice(0, -lastLine.length).replace(/\n$/, "")
        let mut data = node.data.borrow_mut();
        if let NodeValue::CodeBlock(code_block) = &mut data.value {
            let keep = code_block.literal.len().saturating_sub(last_line.len());
            let mut text = code_block.literal[..keep].to_string();
            if text.ends_with('\n') {
                text.pop();
            }
            code_block.literal = text;
        }
    }
    if let Some(last_child) = node.children().last() {
        walk(last_child, source);
    }
}

/// `^(`{3,}|~{3,})` fence marker check for `trimPartialClosingFences`.
fn fence_marker(raw: &str) -> Option<char> {
    let mut chars = raw.chars();
    let first = chars.next()?;
    if (first == '`' || first == '~') && raw.chars().take(3).all(|c| c == first) {
        Some(first)
    } else {
        None
    }
}

/// marked's `lang` normalization: `lang.trim().replace(/\\([\p{P}\p{S}])/gu,
/// "$1")` (marked 18 `fences` tokenizer). comrak's `info` is the raw info
/// string; unescape punctuation so the border line matches upstream.
fn normalize_info(info: &str) -> String {
    static ESCAPED_PUNCT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\\([\p{P}\p{S}])").expect("valid punctuation escape regex")
    });
    ESCAPED_PUNCT.replace_all(info.trim(), "$1").into_owned()
}

/// `codeRemoveIndent` (marked): remove `{1,4} spaces` or `{0,3} spaces +
/// tab` from the start of each line — used for indented code blocks.
fn remove_code_indent(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (content, newline) = match line.strip_suffix('\n') {
            Some(content) => (content, Some('\n')),
            None => (line, None),
        };
        let bytes = content.as_bytes();
        let mut spaces = 0;
        while spaces < bytes.len() && bytes[spaces] == b' ' {
            spaces += 1;
        }
        if (1..=4).contains(&spaces) {
            out.push_str(&content[spaces..]);
        } else if spaces <= 3 && bytes.get(spaces) == Some(&b'\t') {
            out.push_str(&content[spaces + 1..]);
        } else {
            out.push_str(content);
        }
        if let Some(nl) = newline {
            out.push(nl);
        }
    }
    out
}

/// `preserveBackslashEscapes`: re-emit `\` before ASCII punctuation in a text
/// node's source slice (comrak resolves escapes into the literal).
fn re_escape_backslashes(text: &str) -> String {
    if !text.contains('\\') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek().copied() {
                Some(next) if next.is_ascii_punctuation() => {
                    out.push('\\');
                    out.push(next);
                    chars.next();
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Strip ANSI/OSC/APC escape sequences (used for the link fallback
/// comparison, which must use unstyled text).
fn strip_ansi(text: &str) -> String {
    if !text.contains('\x1b') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some(ansi) = extract_ansi_code(text, i) {
            i += ansi.length;
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// marked's `StrictStrikethroughTokenizer` regex
/// (`/^(~~)(?=[^\s~])((?:\\.|[^\\])*?(?:\\.|[^\s~\\]))\1(?=[^~]|$)/`,
/// markdown.ts:6): a strike requires `~~` openers/closers with non-whitespace
/// non-tilde bounds and content that ends on a non-whitespace/non-tilde/non-
/// backslash character (or an escaped pair). comrak's GFM strikethrough also
/// matches single tildes (`~x~`), so strikes that fail this check are
/// re-rendered as plain text.
fn is_marked_strikethrough(raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix("~~") else {
        return false;
    };
    let chars: Vec<char> = rest.chars().collect();
    let n = chars.len();
    if n == 0 || chars[0] == '~' || is_js_whitespace(chars[0]) {
        return false;
    }
    for end in 1..=n {
        if end + 2 > n {
            break;
        }
        if chars[end] != '~' || chars[end + 1] != '~' {
            continue;
        }
        // (?=[^~]|$)
        if end + 2 < n && chars[end + 2] == '~' {
            continue;
        }
        if strike_content_parses(&chars[..end]) {
            return true;
        }
    }
    false
}

/// `((?:\\.|[^\\])*?(?:\\.|[^\s~\\]))`: the content is a run of
/// escape pairs / non-backslash characters ending on an escape pair or a
/// non-whitespace non-tilde non-backslash character.
fn strike_content_parses(content: &[char]) -> bool {
    let n = content.len();
    if n == 0 {
        return false;
    }
    for split in 0..n {
        // Final unit: escape pair (backslash + any) or single [^\s~\\].
        let final_ok = if split == n - 1 {
            let c = content[split];
            c != '\\' && !is_js_whitespace(c) && c != '~'
        } else if content[split] == '\\' && split == n - 2 {
            true
        } else {
            continue;
        };
        if final_ok && strike_units_parse(&content[..split]) {
            return true;
        }
    }
    false
}

/// `(?:\\.|[^\\])*`: escape pairs and single non-backslash characters.
fn strike_units_parse(units: &[char]) -> bool {
    let mut i = 0;
    while i < units.len() {
        if units[i] == '\\' {
            if i + 1 >= units.len() {
                return false;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    true
}

impl Markdown {
    pub fn new(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        theme: Arc<MarkdownTheme>,
        default_text_style: Option<DefaultTextStyle>,
        options: Option<MarkdownOptions>,
    ) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
            default_text_style,
            theme,
            options: options.unwrap_or_default(),
            default_style_prefix: RefCell::new(None),
            cache: RefCell::new(None),
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.invalidate();
    }

    /// `applyDefaultStyle` (markdown.ts:250-277): base styling for all text.
    /// Background color is NOT applied here — it is applied at the padding
    /// stage to extend to the full line width.
    fn apply_default_style(&self, text: &str) -> String {
        let Some(default_style) = &self.default_text_style else {
            return text.to_string();
        };

        let mut styled = text.to_string();
        if let Some(color) = &default_style.color {
            styled = color(&styled);
        }
        if default_style.bold {
            styled = (self.theme.bold)(&styled);
        }
        if default_style.italic {
            styled = (self.theme.italic)(&styled);
        }
        if default_style.strikethrough {
            styled = (self.theme.strikethrough)(&styled);
        }
        if default_style.underline {
            styled = (self.theme.underline)(&styled);
        }
        styled
    }

    /// `getDefaultStylePrefix` (markdown.ts:279-311), cached.
    fn get_default_style_prefix(&self) -> String {
        if let Some(prefix) = self.default_style_prefix.borrow().as_ref() {
            return prefix.clone();
        }
        let prefix = if self.default_text_style.is_none() {
            String::new()
        } else {
            self.compute_default_style_prefix()
        };
        *self.default_style_prefix.borrow_mut() = Some(prefix.clone());
        prefix
    }

    fn compute_default_style_prefix(&self) -> String {
        let mut styled = STYLE_SENTINEL.to_string();
        if let Some(default_style) = &self.default_text_style {
            if let Some(color) = &default_style.color {
                styled = color(&styled);
            }
            if default_style.bold {
                styled = (self.theme.bold)(&styled);
            }
            if default_style.italic {
                styled = (self.theme.italic)(&styled);
            }
            if default_style.strikethrough {
                styled = (self.theme.strikethrough)(&styled);
            }
            if default_style.underline {
                styled = (self.theme.underline)(&styled);
            }
        }
        match styled.find(STYLE_SENTINEL) {
            Some(index) => styled[..index].to_string(),
            None => String::new(),
        }
    }

    /// `getStylePrefix` (markdown.ts:313-318): apply a style function to a
    /// sentinel and take everything before it.
    fn get_style_prefix(&self, style_fn: &dyn Fn(&str) -> String) -> String {
        let styled = style_fn(&STYLE_SENTINEL.to_string());
        match styled.find(STYLE_SENTINEL) {
            Some(index) => styled[..index].to_string(),
            None => String::new(),
        }
    }

    /// `renderInlineTokens` (markdown.ts:492-589).
    fn render_inline_tokens(
        &self,
        node: Node<'_>,
        source: &str,
        style_context: &InlineStyleContext<'_>,
    ) -> String {
        self.render_inline_tokens_in_range(node, source, style_context, None)
    }

    /// `renderInlineTokens` restricted to an absolute source byte range —
    /// used for the plain segments of a paragraph split around display LaTeX.
    /// A soft break at `end - 1` (the newline the next segment starts after)
    /// is excluded so the segment does not end with a stray line break.
    fn render_inline_tokens_in_range(
        &self,
        node: Node<'_>,
        source: &str,
        style_context: &InlineStyleContext<'_>,
        range: Option<(usize, usize)>,
    ) -> String {
        let mut result = String::new();
        let apply_text_with_newlines = |text: &str| -> String {
            let segments: Vec<&str> = text.split('\n').collect();
            segments
                .iter()
                .map(|segment| (style_context.apply_text)(segment))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let children: Vec<Node<'_>> = match range {
            Some((start, end)) => node
                .children()
                .filter(|child| {
                    let child_start = node_start_offset(source, child);
                    if child_start < start || child_start >= end {
                        return false;
                    }
                    if child_start == end - 1
                        && matches!(child.data.borrow().value, NodeValue::SoftBreak)
                    {
                        return false;
                    }
                    true
                })
                .collect(),
            None => node.children().collect(),
        };

        let mut index = 0;
        while index < children.len() {
            let token = children[index];
            let value = &token.data.borrow().value;
            match value {
                NodeValue::Text(text) => {
                    // Escape tokens (upstream `escape`) are resolved into
                    // text nodes; re-emit the backslash when preserving.
                    let raw = node_slice(source, token.data.borrow().sourcepos);
                    if self.options.render_latex {
                        if let Some(pending_tail) = self.render_inline_text_with_latex(
                            raw,
                            &apply_text_with_newlines,
                            &mut result,
                        ) {
                            // Pending token (markdown.ts:84-89): the raw
                            // source from the opening to the end of the
                            // inline source, i.e. the rest of this text node
                            // plus the raw slices of the remaining siblings.
                            let mut pending_raw = pending_tail.to_string();
                            for sibling in &children[index + 1..] {
                                pending_raw
                                    .push_str(node_slice(source, sibling.data.borrow().sourcepos));
                            }
                            result.push_str(&apply_text_with_newlines(&pending_raw));
                            break;
                        }
                    } else {
                        let rendered = if self.options.preserve_backslash_escapes {
                            re_escape_backslashes(raw)
                        } else {
                            text.to_string()
                        };
                        result.push_str(&apply_text_with_newlines(&rendered));
                    }
                }
                NodeValue::Strong => {
                    let bold_content = self.render_inline_tokens(token, source, style_context);
                    result.push_str(&(self.theme.bold)(&bold_content));
                    result.push_str(style_context.style_prefix);
                }
                NodeValue::Emph => {
                    let italic_content = self.render_inline_tokens(token, source, style_context);
                    result.push_str(&(self.theme.italic)(&italic_content));
                    result.push_str(style_context.style_prefix);
                }
                NodeValue::Code(code) => {
                    result.push_str(&(self.theme.code)(&code.literal));
                    result.push_str(style_context.style_prefix);
                }
                NodeValue::Link(link) => {
                    let link_text = self.render_inline_tokens(token, source, style_context);
                    let styled_link = (self.theme.link)(&(self.theme.underline)(&link_text));
                    if get_capabilities().hyperlinks {
                        // OSC 8: clickable hyperlink; the URL is not printed
                        // inline, so the link text always shows.
                        result.push_str(&hyperlink(&styled_link, &link.url));
                        result.push_str(style_context.style_prefix);
                    } else {
                        // Fallback: print the URL in parentheses when the
                        // text differs from the href. mailto: links compare
                        // with the prefix stripped (autolinked emails).
                        let href_for_comparison =
                            link.url.strip_prefix("mailto:").unwrap_or(&link.url);
                        let plain_text = strip_ansi(&link_text);
                        if plain_text == link.url || plain_text == href_for_comparison {
                            result.push_str(&styled_link);
                        } else {
                            result.push_str(&styled_link);
                            result.push_str(&(self.theme.link_url)(&format!(" ({})", link.url)));
                        }
                        result.push_str(style_context.style_prefix);
                    }
                }
                NodeValue::Strikethrough => {
                    let raw = node_slice(source, token.data.borrow().sourcepos);
                    if is_marked_strikethrough(raw) {
                        let del_content = self.render_inline_tokens(token, source, style_context);
                        result.push_str(&(self.theme.strikethrough)(&del_content));
                        result.push_str(style_context.style_prefix);
                    } else {
                        // marked leaves unmatched tilde runs as plain text
                        // while still processing inner formatting; re-emit
                        // the raw tildes around the rendered children.
                        let children: Vec<Node<'_>> = token.children().collect();
                        let (prefix, suffix) = match (children.first(), children.last()) {
                            (Some(first), Some(last)) => {
                                let raw_start = node_start_offset(source, token);
                                let first_start = node_start_offset(source, first);
                                let last_end = node_end_offset(source, last);
                                let p = first_start.saturating_sub(raw_start);
                                let s = last_end.saturating_sub(raw_start) + 1;
                                if p <= raw.len() && s <= raw.len() && p <= s {
                                    (raw[..p].to_string(), raw[s..].to_string())
                                } else {
                                    (String::new(), String::new())
                                }
                            }
                            _ => (String::new(), String::new()),
                        };
                        if children.is_empty() {
                            result.push_str(&apply_text_with_newlines(raw));
                        } else {
                            if !prefix.is_empty() {
                                result.push_str(&apply_text_with_newlines(&prefix));
                            }
                            result.push_str(&self.render_inline_tokens(
                                token,
                                source,
                                style_context,
                            ));
                            if !suffix.is_empty() {
                                result.push_str(&apply_text_with_newlines(&suffix));
                            }
                        }
                    }
                }
                NodeValue::LineBreak | NodeValue::SoftBreak => result.push('\n'),
                NodeValue::HtmlInline(html) => {
                    result.push_str(&apply_text_with_newlines(html));
                }
                // Images and any other inline nodes render as nothing,
                // matching the upstream default case (no `text` property).
                _ => {}
            }
            index += 1;
        }

        while !style_context.style_prefix.is_empty() && result.ends_with(style_context.style_prefix)
        {
            result.truncate(result.len() - style_context.style_prefix.len());
        }
        result
    }

    /// Scan a text node's raw source for inline LaTeX — the equivalent of
    /// marked's inline `latex` tokenizer extension being called at every
    /// lexer position (markdown.ts:52-99, 645-652). Emits plain segments
    /// (with backslash-escape resolution, like comrak's resolved literal)
    /// and rendered math (`renderLatex(text) ?? raw`). Returns the pending
    /// tail when the node ends with an unclosed math opening (upstream
    /// pending token raw, markdown.ts:84-89).
    fn render_inline_text_with_latex<'r>(
        &self,
        raw: &'r str,
        apply: &dyn Fn(&str) -> String,
        result: &mut String,
    ) -> Option<&'r str> {
        let emit_plain = |segment: &str, result: &mut String| {
            if segment.is_empty() {
                return;
            }
            let text = if self.options.preserve_backslash_escapes {
                re_escape_backslashes(segment)
            } else {
                resolve_backslash_escapes(segment)
            };
            result.push_str(&apply(&text));
        };

        let mut position = 0;
        let mut plain_start = 0;
        while position < raw.len() {
            match tokenize_inline_latex(&raw[position..]) {
                Some(token) => {
                    emit_plain(&raw[plain_start..position], result);
                    if token.pending {
                        return Some(&raw[position..]);
                    }
                    let rendered =
                        render_latex(token.text, false).unwrap_or_else(|| token.raw.to_string());
                    result.push_str(&apply(&rendered));
                    position += token.raw.len();
                    plain_start = position;
                }
                None => {
                    // marked's lexer order: the latex extension fails at the
                    // backslash, then the escape tokenizer consumes `\X`
                    // (backslash + ASCII punctuation) as one unit — the
                    // escaped character is never a tokenizer position.
                    let rest = &raw[position..];
                    let advance = if rest.starts_with('\\') {
                        match rest.chars().nth(1) {
                            Some(next) if next.is_ascii_punctuation() => 2,
                            _ => 1,
                        }
                    } else {
                        rest.chars().next().map_or(1, char::len_utf8)
                    };
                    position += advance;
                }
            }
        }
        emit_plain(&raw[plain_start..], result);
        None
    }

    /// `getOrderedListMarker` (markdown.ts:591-594).
    fn get_ordered_list_marker(raw: &str) -> Option<String> {
        let bytes = raw.as_bytes();
        let mut i = 0;
        while i < 3 && bytes.get(i) == Some(&b' ') {
            i += 1;
        }
        let mut digits = 0;
        while digits < 9 && bytes.get(i + digits).is_some_and(u8::is_ascii_digit) {
            digits += 1;
        }
        if digits == 0 {
            return None;
        }
        let delim = bytes.get(i + digits);
        if !matches!(delim, Some(b'.') | Some(b')')) {
            return None;
        }
        let after = bytes.get(i + digits + 1);
        if !matches!(after, Some(b' ') | Some(b'\t')) {
            return None;
        }
        Some(format!("{} ", &raw[i..i + digits + 1]))
    }

    /// `getUnorderedListMarker` (markdown.ts:596-599).
    fn get_unordered_list_marker(raw: &str) -> Option<String> {
        let bytes = raw.as_bytes();
        let mut i = 0;
        while i < 3 && bytes.get(i) == Some(&b' ') {
            i += 1;
        }
        let bullet = bytes.get(i)?;
        if !matches!(bullet, b'-' | b'+' | b'*') {
            return None;
        }
        let next = bytes.get(i + 1);
        let ends_line = match next {
            Some(b' ') | Some(b'\t') => true,
            None => true,
            Some(b'\r') => bytes.get(i + 2) == Some(&b'\n'),
            Some(b'\n') => true,
            _ => false,
        };
        if ends_line {
            Some(format!("{} ", *bullet as char))
        } else {
            None
        }
    }

    /// Strip the list marker (` {0,3}([-+*]|\d{1,9}[.)])[ \t]+`) from an
    /// item's raw text (marked's `list` tokenizer task check).
    fn strip_list_marker(raw: &str) -> &str {
        let bytes = raw.as_bytes();
        let mut i = 0;
        while i < 3 && bytes.get(i) == Some(&b' ') {
            i += 1;
        }
        let after_marker = match bytes.get(i) {
            Some(b'-') | Some(b'+') | Some(b'*') => i + 1,
            Some(b'0'..=b'9') => {
                let mut digits = i;
                while digits < i + 9 && bytes.get(digits).is_some_and(u8::is_ascii_digit) {
                    digits += 1;
                }
                if matches!(bytes.get(digits), Some(b'.') | Some(b')')) {
                    digits + 1
                } else {
                    return raw;
                }
            }
            _ => return raw,
        };
        let mut j = after_marker;
        while matches!(bytes.get(j), Some(b' ') | Some(b'\t')) {
            j += 1;
        }
        &raw[j..]
    }

    /// marked's `listIsTask` (`/^\[[ xX]\] +\S/`) applied to the item text.
    fn is_task_item(raw: &str) -> bool {
        let rest = Self::strip_list_marker(raw);
        let bytes = rest.as_bytes();
        if bytes.len() < 4
            || bytes[0] != b'['
            || !matches!(bytes[1], b' ' | b'x' | b'X')
            || bytes[2] != b']'
            || bytes[3] != b' '
        {
            return false;
        }
        rest[4..].chars().next().is_some_and(is_js_non_whitespace)
    }

    /// Render a paragraph whose raw source contains display-LaTeX blocks
    /// (`$$...$$` / `\[...\]` at line starts, markdown.ts:101-144 + 505-517).
    /// Splits the paragraph into plain and display segments — the equivalent
    /// of marked's block tokenizer + `startBlock` paragraph truncation — and
    /// renders them with the upstream block spacing rules: each non-last
    /// segment gets a blank line after it; the last one follows the spacing
    /// rule of its own block type (paragraph skips `list`/`space`,
    /// latexBlock only skips `space`, markdown.ts:495-498 + 514-516).
    /// Returns `None` when the paragraph contains no display LaTeX.
    fn render_paragraph_with_latex(
        &self,
        token: Node<'_>,
        source: &str,
        next_token_type: Option<TokenType>,
        style_context: &InlineStyleContext<'_>,
    ) -> Option<Vec<String>> {
        let raw = node_raw(source, token.data.borrow().sourcepos);
        let segments = split_latex_blocks(raw);
        if !segments
            .iter()
            .any(|segment| matches!(segment, LatexParagraphSegment::Latex { .. }))
        {
            return None;
        }

        let paragraph_start = node_start_offset(source, token);
        let segment_count = segments.len();
        let mut lines: Vec<String> = Vec::new();
        for (index, segment) in segments.iter().enumerate() {
            let is_latex = match segment {
                LatexParagraphSegment::Plain { start, end } => {
                    let text = self.render_inline_tokens_in_range(
                        token,
                        source,
                        style_context,
                        Some((paragraph_start + start, paragraph_start + end)),
                    );
                    lines.push(text);
                    false
                }
                LatexParagraphSegment::Latex {
                    start,
                    end,
                    token: latex,
                } => {
                    // markdown.ts:507-510: pending or unsupported math falls
                    // back to the raw source (delimiters included).
                    let rendered = if latex.pending {
                        trim_js(&raw[*start..*end]).to_string()
                    } else {
                        render_latex(latex.text, true)
                            .unwrap_or_else(|| trim_js(&raw[*start..*end]).to_string())
                    };
                    for line in rendered.split('\n') {
                        lines.push(self.apply_default_style(line));
                    }
                    true
                }
            };

            if index + 1 < segment_count {
                lines.push(String::new());
            } else {
                let spaced = match next_token_type {
                    None => false,
                    Some(TokenType::Space) => false,
                    Some(TokenType::List) => is_latex,
                    Some(TokenType::Other) => true,
                };
                if spaced {
                    lines.push(String::new());
                }
            }
        }
        Some(lines)
    }

    /// `renderToken` (markdown.ts:327-490).
    fn render_token(
        &self,
        block: &Block<'_>,
        source: &str,
        width: usize,
        next_token_type: Option<TokenType>,
        style_context: &InlineStyleContext<'_>,
    ) -> Vec<String> {
        let Block::Node(token) = block else {
            // "space": blank lines in markdown
            return vec![String::new()];
        };

        let mut lines: Vec<String> = Vec::new();

        // Upstream checks `nextTokenType && nextTokenType !== ...`: no
        // spacing when there is no next token at all.
        let spacing_after = |token_type: Option<TokenType>| -> bool {
            matches!(token_type, Some(t) if t != TokenType::Space)
        };

        let value = &token.data.borrow().value;
        match value {
            NodeValue::Heading(heading) => {
                let heading_level = heading.level;
                let heading_prefix = format!("{} ", "#".repeat(heading_level as usize));

                // Heading-specific style context so inline tokens restore
                // heading styling after their own ANSI resets.
                let theme = &self.theme;
                let heading_style_fn: Box<dyn Fn(&str) -> String> = if heading_level == 1 {
                    Box::new(move |text: &str| {
                        (theme.heading)(&(theme.bold)(&(theme.underline)(text)))
                    })
                } else {
                    Box::new(move |text: &str| (theme.heading)(&(theme.bold)(text)))
                };
                let heading_style_prefix = self.get_style_prefix(&heading_style_fn);
                let heading_style_context = InlineStyleContext {
                    apply_text: &heading_style_fn,
                    style_prefix: &heading_style_prefix,
                };

                let heading_text = self.render_inline_tokens(token, source, &heading_style_context);
                let styled_heading = if heading_level >= 3 {
                    heading_style_fn(&heading_prefix) + &heading_text
                } else {
                    heading_text
                };
                lines.push(styled_heading);
                if spacing_after(next_token_type) {
                    lines.push(String::new());
                }
            }
            NodeValue::Paragraph => {
                if self.options.render_latex {
                    if let Some(latex_lines) = self.render_paragraph_with_latex(
                        token,
                        source,
                        next_token_type,
                        style_context,
                    ) {
                        return latex_lines;
                    }
                }
                let paragraph_text = self.render_inline_tokens(token, source, style_context);
                lines.push(paragraph_text);
                // Don't add spacing if next token is space or list, or if
                // there is no next token at all.
                if matches!(next_token_type, Some(t) if t != TokenType::Space && t != TokenType::List)
                {
                    lines.push(String::new());
                }
            }
            NodeValue::CodeBlock(code_block) => {
                let indent = self
                    .theme
                    .code_block_indent
                    .clone()
                    .unwrap_or_else(|| "  ".to_string());
                let lang = if code_block.fenced {
                    normalize_info(&code_block.info)
                } else {
                    String::new()
                };
                lines.push((self.theme.code_block_border)(&format!("```{lang}")));
                let code_text = if code_block.fenced {
                    code_block
                        .literal
                        .strip_suffix('\n')
                        .unwrap_or(&code_block.literal)
                        .to_string()
                } else {
                    remove_code_indent(node_slice(source, token.data.borrow().sourcepos))
                };
                if let Some(highlight_code) = &self.theme.highlight_code {
                    let highlighted_lines = highlight_code(
                        &code_text,
                        if lang.is_empty() { None } else { Some(&lang) },
                    );
                    for highlighted_line in highlighted_lines {
                        lines.push(format!("{indent}{highlighted_line}"));
                    }
                } else {
                    for code_line in code_text.split('\n') {
                        lines.push(format!("{indent}{}", (self.theme.code_block)(code_line)));
                    }
                }
                lines.push((self.theme.code_block_border)("```"));
                if spacing_after(next_token_type) {
                    lines.push(String::new());
                }
            }
            NodeValue::List(_) => {
                lines.extend(self.render_list(token, source, 0, width, style_context));
            }
            NodeValue::Table(_) => {
                lines.extend(self.render_table(
                    token,
                    source,
                    width,
                    next_token_type,
                    style_context,
                ));
            }
            NodeValue::BlockQuote => {
                let theme = &self.theme;
                let quote_style: Box<dyn Fn(&str) -> String> =
                    Box::new(move |text: &str| (theme.quote)(&(theme.italic)(text)));
                let quote_style_prefix = self.get_style_prefix(&quote_style);
                let apply_quote_style = |line: &str| -> String {
                    if quote_style_prefix.is_empty() {
                        return quote_style(line);
                    }
                    let reapplied =
                        line.replace("\x1b[0m", &format!("\x1b[0m{quote_style_prefix}"));
                    quote_style(&reapplied)
                };

                // Subtract the border "│ " (2 chars).
                let quote_content_width = width.saturating_sub(2).max(1);

                // Blockquotes contain block-level tokens; render children
                // with renderToken(). Default message style does not apply
                // inside blockquotes.
                let quote_style_context = InlineStyleContext {
                    apply_text: &|text: &str| text.to_string(),
                    style_prefix: &quote_style_prefix,
                };
                let source_start = node_start_offset(source, token);
                let source_end = node_end_offset(source, token);
                let quote_blocks =
                    synthesize_blocks(token.children().collect(), source, source_start, source_end);
                let mut rendered_quote_lines: Vec<String> = Vec::new();
                for (index, quote_block) in quote_blocks.iter().enumerate() {
                    let next_quote_type = quote_blocks
                        .get(index + 1)
                        .map(Block::token_type)
                        .unwrap_or(TokenType::Other);
                    rendered_quote_lines.extend(self.render_token(
                        quote_block,
                        source,
                        quote_content_width,
                        Some(next_quote_type),
                        &quote_style_context,
                    ));
                }

                // Avoid an extra empty quote line before the outer spacing.
                while rendered_quote_lines
                    .last()
                    .is_some_and(|line| line.is_empty())
                {
                    rendered_quote_lines.pop();
                }

                for quote_line in rendered_quote_lines {
                    let styled_line = apply_quote_style(&quote_line);
                    for wrapped_line in wrap_text_with_ansi(&styled_line, quote_content_width) {
                        lines.push((self.theme.quote_border)("│ ") + &wrapped_line);
                    }
                }
                if spacing_after(next_token_type) {
                    lines.push(String::new());
                }
            }
            NodeValue::ThematicBreak => {
                lines.push((self.theme.hr)(&"─".repeat(width.min(80))));
                if spacing_after(next_token_type) {
                    lines.push(String::new());
                }
            }
            NodeValue::HtmlBlock(html) => {
                // Render HTML as plain text (escaped for terminal).
                lines.push(self.apply_default_style(html.literal.trim()));
            }
            _ => {
                // Any other block types render as nothing (upstream default
                // case only renders tokens with a string `text`).
            }
        }

        lines
    }

    /// `renderList` (markdown.ts:604-654).
    fn render_list<'a>(
        &self,
        token: Node<'a>,
        source: &str,
        depth: usize,
        width: usize,
        style_context: &InlineStyleContext<'_>,
    ) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let indent = "    ".repeat(depth);
        let list = match &token.data.borrow().value {
            NodeValue::List(list) => *list,
            _ => return lines,
        };
        let start_number = list.start;
        let ordered = list.list_type == comrak::nodes::ListType::Ordered;
        let items: Vec<Node<'_>> = token.children().collect();

        for (index, item) in items.iter().enumerate() {
            let is_last_item = index == items.len() - 1;
            let item_raw = node_slice(source, item.data.borrow().sourcepos);

            let bullet = if ordered {
                if self.options.preserve_ordered_list_markers {
                    Self::get_ordered_list_marker(item_raw)
                        .unwrap_or_else(|| format!("{}. ", start_number + index))
                } else {
                    format!("{}. ", start_number + index)
                }
            } else if self.options.preserve_ordered_list_markers {
                Self::get_unordered_list_marker(item_raw).unwrap_or_else(|| "- ".to_string())
            } else {
                "- ".to_string()
            };

            // Task markers: comrak's `tasklist` extension emits a TaskItem
            // node; marked only treats the item as a task when its text
            // matches `/^\[[ xX]\] +\S/` (a checkbox with content after it).
            let item_value = &item.data.borrow().value;
            let is_task_node = matches!(item_value, NodeValue::TaskItem(_));
            let is_real_task = is_task_node && Self::is_task_item(item_raw);
            let task_marker = if is_real_task {
                let checked = matches!(
                    item_value,
                    NodeValue::TaskItem(task) if task.symbol.is_some()
                );
                if checked { "[x] " } else { "[ ] " }.to_string()
            } else {
                String::new()
            };

            let marker = bullet + &task_marker;
            let first_prefix = format!("{indent}{}", (self.theme.list_bullet)(&marker));
            let continuation_prefix = format!("{indent}{}", " ".repeat(visible_width(&marker)));
            let item_width = width.saturating_sub(visible_width(&first_prefix)).max(1);
            let mut rendered_any_line = false;

            // A non-task TaskItem keeps its checkbox as plain text content
            // (marked: `- [ ]` alone renders the checkbox as item text).
            let checkbox_text = if is_task_node && !is_real_task {
                let rest = Self::strip_list_marker(item_raw);
                if rest.starts_with('[') {
                    &rest[..rest.len().min(3)]
                } else {
                    ""
                }
            } else {
                ""
            };
            if !checkbox_text.is_empty() {
                lines.push(format!("{first_prefix}{checkbox_text}"));
                rendered_any_line = true;
            }

            let item_start = node_start_offset(source, item);
            let item_end = node_end_offset(source, item);
            let item_blocks =
                synthesize_blocks(item.children().collect(), source, item_start, item_end);
            for item_block in &item_blocks {
                match item_block {
                    Block::Space => {
                        let line_prefix = if rendered_any_line {
                            &continuation_prefix
                        } else {
                            &first_prefix
                        };
                        lines.push(line_prefix.to_string());
                        rendered_any_line = true;
                    }
                    Block::Node(nested) => {
                        if matches!(nested.data.borrow().value, NodeValue::List(_)) {
                            lines.extend(self.render_list(
                                nested,
                                source,
                                depth + 1,
                                width,
                                style_context,
                            ));
                            rendered_any_line = true;
                            continue;
                        }

                        let item_lines =
                            self.render_token(item_block, source, item_width, None, style_context);
                        for item_line in item_lines {
                            for wrapped_line in wrap_text_with_ansi(&item_line, item_width) {
                                let line_prefix = if rendered_any_line {
                                    &continuation_prefix
                                } else {
                                    &first_prefix
                                };
                                lines.push(format!("{line_prefix}{wrapped_line}"));
                                rendered_any_line = true;
                            }
                        }
                    }
                }
            }

            if !rendered_any_line {
                lines.push(first_prefix);
            }

            if !list.tight && !is_last_item {
                lines.push(String::new());
            }
        }

        lines
    }

    /// `getLongestWordWidth` (markdown.ts:659-669).
    fn get_longest_word_width(text: &str, max_width: Option<usize>) -> usize {
        let mut longest = 0;
        for word in text.split_whitespace() {
            if !word.is_empty() {
                longest = longest.max(visible_width(word));
            }
        }
        match max_width {
            Some(max_width) => longest.min(max_width),
            None => longest,
        }
    }

    /// `renderTable` (markdown.ts:685-857).
    fn render_table<'a>(
        &self,
        token: Node<'a>,
        source: &str,
        available_width: usize,
        next_token_type: Option<TokenType>,
        style_context: &InlineStyleContext<'_>,
    ) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let rows: Vec<Node<'_>> = token.children().collect();
        let header_row = rows.first().copied();
        let Some(header_row) = header_row else {
            return lines;
        };
        let header_cells: Vec<Node<'_>> = header_row.children().collect();
        let num_cols = header_cells.len();
        if num_cols == 0 {
            return lines;
        }
        let data_rows: Vec<Node<'_>> = rows
            .iter()
            .skip(1)
            .copied()
            .filter(|row| !matches!(row.data.borrow().value, NodeValue::TableRow(true)))
            .collect();

        // Border overhead: "│ " + (n-1) * " │ " + " │" = 3n + 1.
        let border_overhead = 3 * num_cols + 1;
        let available_for_cells = available_width as i64 - border_overhead as i64;
        if available_for_cells < num_cols as i64 {
            // Too narrow to render a stable table: fall back to raw markdown.
            let raw = node_raw(source, token.data.borrow().sourcepos);
            let mut fallback_lines = wrap_text_with_ansi(raw, available_width);
            if matches!(next_token_type, Some(t) if t != TokenType::Space) {
                fallback_lines.push(String::new());
            }
            return fallback_lines;
        }

        let max_unbroken_word_width = 30;

        // Natural column widths (what each column needs without constraints).
        let mut natural_widths: Vec<i64> = vec![0; num_cols];
        let mut min_word_widths: Vec<i64> = vec![1; num_cols];
        for (i, cell) in header_cells.iter().enumerate() {
            let header_text = self.render_inline_tokens(cell, source, style_context);
            natural_widths[i] = visible_width(&header_text) as i64;
            min_word_widths[i] =
                Self::get_longest_word_width(&header_text, Some(max_unbroken_word_width)).max(1)
                    as i64;
        }
        for row in &data_rows {
            for (i, cell) in row.children().enumerate() {
                if i >= num_cols {
                    break;
                }
                let cell_text = self.render_inline_tokens(cell, source, style_context);
                natural_widths[i] = natural_widths[i].max(visible_width(&cell_text) as i64);
                min_word_widths[i] = min_word_widths[i].max(Self::get_longest_word_width(
                    &cell_text,
                    Some(max_unbroken_word_width),
                ) as i64);
            }
        }

        let mut min_column_widths = min_word_widths.clone();
        let mut min_cells_width: i64 = min_column_widths.iter().sum();

        if min_cells_width > available_for_cells {
            min_column_widths = vec![1; num_cols];
            let remaining = available_for_cells - num_cols as i64;

            if remaining > 0 {
                let total_weight: i64 = min_word_widths.iter().map(|w| (*w - 1).max(0)).sum();
                let growth: Vec<i64> = min_word_widths
                    .iter()
                    .map(|width| {
                        let weight = (*width - 1).max(0);
                        if total_weight > 0 {
                            (weight * remaining) / total_weight
                        } else {
                            0
                        }
                    })
                    .collect();

                for (column, growth) in min_column_widths.iter_mut().zip(&growth) {
                    *column += growth;
                }

                let allocated: i64 = growth.iter().sum();
                let mut leftover = remaining - allocated;
                for column in min_column_widths.iter_mut() {
                    if leftover <= 0 {
                        break;
                    }
                    *column += 1;
                    leftover -= 1;
                }
            }

            min_cells_width = min_column_widths.iter().sum();
        }

        // Column widths that fit within the available width.
        let total_natural_width: i64 = natural_widths.iter().sum::<i64>() + border_overhead as i64;
        let mut column_widths: Vec<i64>;

        if total_natural_width <= available_width as i64 {
            // Everything fits naturally.
            column_widths = natural_widths
                .iter()
                .enumerate()
                .map(|(index, width)| (*width).max(min_column_widths[index]))
                .collect();
        } else {
            // Shrink columns to fit.
            let total_grow_potential: i64 = natural_widths
                .iter()
                .enumerate()
                .map(|(index, width)| (*width - min_column_widths[index]).max(0))
                .sum();
            let extra_width = (available_for_cells - min_cells_width).max(0);
            column_widths = min_column_widths
                .iter()
                .enumerate()
                .map(|(index, min_width)| {
                    let natural_width = natural_widths[index];
                    let min_width_delta = (natural_width - min_width).max(0);
                    let mut grow = 0;
                    if total_grow_potential > 0 {
                        grow = (min_width_delta * extra_width) / total_grow_potential;
                    }
                    min_width + grow
                })
                .collect();

            // Adjust for rounding errors — distribute remaining space.
            let allocated: i64 = column_widths.iter().sum();
            let mut remaining = available_for_cells - allocated;
            while remaining > 0 {
                let mut grew = false;
                for i in 0..num_cols {
                    if remaining <= 0 {
                        break;
                    }
                    if column_widths[i] < natural_widths[i] {
                        column_widths[i] += 1;
                        remaining -= 1;
                        grew = true;
                    }
                }
                if !grew {
                    break;
                }
            }
        }

        // Top border.
        let top_border_cells: Vec<String> = column_widths
            .iter()
            .map(|w| "─".repeat(*w as usize))
            .collect();
        lines.push(format!("┌─{}─┐", top_border_cells.join("─┬─")));

        // Header with wrapping.
        let header_cell_lines: Vec<Vec<String>> = header_cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let text = self.render_inline_tokens(cell, source, style_context);
                Self::wrap_cell_text(&text, column_widths[i] as usize)
            })
            .collect();
        let header_line_count = header_cell_lines.iter().map(Vec::len).max().unwrap_or(0);

        for line_index in 0..header_line_count {
            let row_parts: Vec<String> = header_cell_lines
                .iter()
                .enumerate()
                .map(|(col_index, cell_lines)| {
                    let text = cell_lines.get(line_index).cloned().unwrap_or_default();
                    let padded = format!(
                        "{text}{}",
                        " ".repeat(
                            (column_widths[col_index] as usize)
                                .saturating_sub(visible_width(&text))
                        )
                    );
                    (self.theme.bold)(&padded)
                })
                .collect();
            lines.push(format!("│ {} │", row_parts.join(" │ ")));
        }

        // Separator.
        let separator_cells: Vec<String> = column_widths
            .iter()
            .map(|w| "─".repeat(*w as usize))
            .collect();
        lines.push(format!("├─{}─┤", separator_cells.join("─┼─")));

        // Rows with wrapping.
        for (row_index, row) in data_rows.iter().enumerate() {
            let row_cell_lines: Vec<Vec<String>> = row
                .children()
                .enumerate()
                .filter(|(i, _)| *i < num_cols)
                .map(|(i, cell)| {
                    let text = self.render_inline_tokens(cell, source, style_context);
                    Self::wrap_cell_text(&text, column_widths[i] as usize)
                })
                .collect();
            let row_line_count = row_cell_lines.iter().map(Vec::len).max().unwrap_or(0);

            for line_index in 0..row_line_count {
                let row_parts: Vec<String> = row_cell_lines
                    .iter()
                    .enumerate()
                    .map(|(col_index, cell_lines)| {
                        let text = cell_lines.get(line_index).cloned().unwrap_or_default();
                        format!(
                            "{text}{}",
                            " ".repeat(
                                (column_widths[col_index] as usize)
                                    .saturating_sub(visible_width(&text))
                            )
                        )
                    })
                    .collect();
                lines.push(format!("│ {} │", row_parts.join(" │ ")));
            }

            if row_index < data_rows.len() - 1 {
                lines.push(format!("├─{}─┤", separator_cells.join("─┼─")));
            }
        }

        // Bottom border.
        let bottom_border_cells: Vec<String> = column_widths
            .iter()
            .map(|w| "─".repeat(*w as usize))
            .collect();
        lines.push(format!("└─{}─┘", bottom_border_cells.join("─┴─")));

        if matches!(next_token_type, Some(t) if t != TokenType::Space) {
            lines.push(String::new());
        }
        lines
    }

    /// `wrapCellText` (markdown.ts:677-679).
    fn wrap_cell_text(text: &str, max_width: usize) -> Vec<String> {
        wrap_text_with_ansi(text, max_width.max(1))
    }
}

impl Component for Markdown {
    fn render(&self, width: usize) -> Vec<String> {
        // Available width for content (subtract horizontal padding).
        let content_width = width.saturating_sub(self.padding_x * 2).max(1);

        // Apply the width-aware transform (markdown.ts:285). The transform
        // runs on every render (not only on cache misses like upstream):
        // the transformed text is part of the cache key, so a stateful
        // transform (e.g. the mermaid streaming/final switch) cannot serve
        // stale lines.
        let transformed_text = self
            .options
            .transform
            .as_ref()
            .map(|transform| transform(&self.text, content_width))
            .unwrap_or_else(|| self.text.clone());

        // Check cache.
        if let Some(cache) = self.cache.borrow().as_ref() {
            if cache.text == self.text
                && cache.transformed == transformed_text
                && cache.width == width
                && cache.render_latex == self.options.render_latex
            {
                return cache.lines.clone();
            }
        }

        // Don't render anything if there's no actual text (upstream checks
        // the transformed text, markdown.ts:288).
        if transformed_text.trim().is_empty() {
            let result = vec![String::new()];
            *self.cache.borrow_mut() = Some(MarkdownCache {
                text: self.text.clone(),
                transformed: transformed_text,
                width,
                render_latex: self.options.render_latex,
                lines: result.clone(),
            });
            return result;
        }

        // Replace tabs with 3 spaces for consistent rendering.
        let normalized_text = transformed_text.replace('\t', "   ");

        // Parse markdown (comrak replaces marked's lexer).
        let arena = Arena::new();
        let root = parse_document(&arena, &normalized_text, &parser_options());
        trim_partial_closing_fences(root, &normalized_text);

        // Convert tokens to styled terminal output.
        let mut rendered_lines: Vec<String> = Vec::new();
        let blocks = synthesize_blocks(
            root.children().collect(),
            &normalized_text,
            0,
            normalized_text.len(),
        );
        let default_style_prefix = self.get_default_style_prefix();
        let default_context = InlineStyleContext {
            apply_text: &|text: &str| self.apply_default_style(text),
            style_prefix: &default_style_prefix,
        };

        for (index, block) in blocks.iter().enumerate() {
            let next_type = blocks.get(index + 1).map(Block::token_type);
            let token_lines = self.render_token(
                block,
                &normalized_text,
                content_width,
                next_type,
                &default_context,
            );
            rendered_lines.extend(token_lines);
        }

        // Wrap lines (NO padding, NO background yet).
        let mut wrapped_lines: Vec<String> = Vec::new();
        for line in rendered_lines {
            if is_image_line(&line) {
                wrapped_lines.push(line);
            } else {
                wrapped_lines.extend(wrap_text_with_ansi(&line, content_width));
            }
        }

        // Add margins and background to each wrapped line.
        let left_margin = " ".repeat(self.padding_x);
        let right_margin = " ".repeat(self.padding_x);
        let bg_fn = self
            .default_text_style
            .as_ref()
            .and_then(|s| s.bg_color.as_ref());
        let mut content_lines: Vec<String> = Vec::new();

        for line in wrapped_lines {
            if is_image_line(&line) {
                content_lines.push(line);
                continue;
            }

            let line_with_margins = format!("{left_margin}{line}{right_margin}");
            if let Some(bg_fn) = bg_fn {
                content_lines.push(apply_background_to_line(&line_with_margins, width, |t| {
                    bg_fn(t)
                }));
            } else {
                let visible_len = visible_width(&line_with_margins);
                let padding_needed = width.saturating_sub(visible_len);
                content_lines.push(format!("{line_with_margins}{}", " ".repeat(padding_needed)));
            }
        }

        // Top/bottom padding (empty lines).
        let empty_line = " ".repeat(width);
        let mut empty_lines: Vec<String> = Vec::new();
        for _ in 0..self.padding_y {
            let line = if let Some(bg_fn) = bg_fn {
                apply_background_to_line(&empty_line, width, |t| bg_fn(t))
            } else {
                empty_line.clone()
            };
            empty_lines.push(line);
        }

        // Combine top padding, content, and bottom padding.
        let mut result = empty_lines.clone();
        result.extend(content_lines);
        result.extend(empty_lines);

        let result = if result.is_empty() {
            vec![String::new()]
        } else {
            result
        };

        // Update cache.
        *self.cache.borrow_mut() = Some(MarkdownCache {
            text: self.text.clone(),
            transformed: transformed_text,
            width,
            render_latex: self.options.render_latex,
            lines: result.clone(),
        });

        result
    }

    fn invalidate(&mut self) {
        *self.cache.borrow_mut() = None;
        *self.default_style_prefix.borrow_mut() = None;
    }
}

#[cfg(test)]
mod tests {
    //! Ports of `test/markdown.test.ts` @ pi 0.82.1 (2efa728). The upstream
    //! suite's three TUI-integration cases (xterm headless cell-style checks)
    //! are ported as output-level assertions where possible; the rest are
    //! asserted against the component's own render output.

    use super::*;
    use crate::terminal_image::{reset_capabilities_cache, set_capabilities, TerminalCapabilities};

    fn theme() -> Arc<MarkdownTheme> {
        // Mirrors test-themes.ts `defaultMarkdownTheme` (chalk level 3).
        fn style(code: &'static str, close: &'static str) -> ThemeTextFn {
            Box::new(move |text: &str| format!("\x1b[{code}m{text}\x1b[{close}m"))
        }
        Arc::new(MarkdownTheme {
            heading: style("36", "0"),
            link: style("34", "0"),
            link_url: style("2", "22"),
            code: style("33", "0"),
            code_block: style("32", "0"),
            code_block_border: style("2", "22"),
            quote: style("3", "23"),
            quote_border: style("2", "22"),
            hr: style("2", "22"),
            list_bullet: style("36", "0"),
            bold: style("1", "22"),
            italic: style("3", "23"),
            strikethrough: style("9", "29"),
            underline: style("4", "24"),
            highlight_code: None,
            code_block_indent: None,
        })
    }

    fn strip_ansi(line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn md(text: &str) -> Markdown {
        Markdown::new(text, 0, 0, theme(), None, None)
    }

    fn plain(markdown: &Markdown, width: usize) -> Vec<String> {
        markdown
            .render(width)
            .into_iter()
            .map(|line| strip_ansi(&line).trim_end().to_string())
            .collect()
    }

    fn caps(hyperlinks: bool) -> TerminalCapabilities {
        TerminalCapabilities {
            images: None,
            true_color: false,
            hyperlinks,
        }
    }

    /// Serializes tests that mutate the global capabilities cache (cargo
    /// runs tests in one binary with parallel threads).
    static TEST_CAPS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // ── Lists ──────────────────────────────────────────────────────────────

    #[test]
    fn renders_simple_nested_list() {
        let markdown = md("- Item 1\n  - Nested 1.1\n  - Nested 1.2\n- Item 2");
        let lines = plain(&markdown, 80);
        assert!(lines.iter().any(|l| l.contains("- Item 1")));
        assert!(lines.iter().any(|l| l.contains("    - Nested 1.1")));
        assert!(lines.iter().any(|l| l.contains("    - Nested 1.2")));
        assert!(lines.iter().any(|l| l.contains("- Item 2")));
    }

    #[test]
    fn renders_deeply_nested_list() {
        let markdown = md("- Level 1\n  - Level 2\n    - Level 3\n      - Level 4");
        let lines = plain(&markdown, 80);
        assert!(lines.iter().any(|l| l.contains("- Level 1")));
        assert!(lines.iter().any(|l| l.contains("    - Level 2")));
        assert!(lines.iter().any(|l| l.contains("        - Level 3")));
        assert!(lines.iter().any(|l| l.contains("            - Level 4")));
    }

    #[test]
    fn renders_ordered_nested_list() {
        let markdown = md("1. First\n   1. Nested first\n   2. Nested second\n2. Second");
        let lines = plain(&markdown, 80);
        assert!(lines.iter().any(|l| l.contains("1. First")));
        assert!(lines.iter().any(|l| l.contains("    1. Nested first")));
        assert!(lines.iter().any(|l| l.contains("    2. Nested second")));
        assert!(lines.iter().any(|l| l.contains("2. Second")));
    }

    #[test]
    fn normalizes_ordered_list_markers_by_default() {
        let markdown = md("1. alpha\n1. beta\n1. gamma");
        assert_eq!(plain(&markdown, 80), ["1. alpha", "2. beta", "3. gamma"]);
    }

    #[test]
    fn preserves_source_list_markers_when_configured() {
        let markdown = Markdown::new(
            "  4. forth\n  3. third\n\n10) ten\n7) seven\n\n+ plus\n* star\n- minus\n+",
            0,
            0,
            theme(),
            None,
            Some(MarkdownOptions {
                preserve_ordered_list_markers: true,
                ..Default::default()
            }),
        );
        assert_eq!(
            plain(&markdown, 80),
            [
                "4. forth", "3. third", "", "10) ten", "7) seven", "", "+ plus", "* star",
                "- minus", "+",
            ]
        );
    }

    #[test]
    fn renders_mixed_ordered_and_unordered_nested_lists() {
        let markdown = md("1. Ordered item\n   - Unordered nested\n   - Another nested\n2. Second ordered\n   - More nested");
        let lines = plain(&markdown, 80);
        assert!(lines.iter().any(|l| l.contains("1. Ordered item")));
        assert!(lines.iter().any(|l| l.contains("    - Unordered nested")));
        assert!(lines.iter().any(|l| l.contains("2. Second ordered")));
    }

    #[test]
    fn renders_blank_lines_between_loose_list_items() {
        let markdown = md(
            "1. Lorem ipsum dolor sit amet.\n\n   Ut enim ad minim veniam.\n\n2. Duis aute irure dolor.\n\n   Excepteur sint occaecat cupidatat.\n\n3. Beep boop",
        );
        assert_eq!(
            plain(&markdown, 80),
            [
                "1. Lorem ipsum dolor sit amet.",
                "",
                "   Ut enim ad minim veniam.",
                "",
                "2. Duis aute irure dolor.",
                "",
                "   Excepteur sint occaecat cupidatat.",
                "",
                "3. Beep boop",
            ]
        );
    }

    #[test]
    fn renders_task_list_markers() {
        let markdown = md("- [ ] beep\n- [x] boop");
        assert_eq!(plain(&markdown, 80), ["- [ ] beep", "- [x] boop"]);
    }

    #[test]
    fn maintains_numbering_when_code_blocks_are_not_indented() {
        // When code blocks aren't indented, each item is a separate list.
        // The list `start` preserves the original numbering.
        let markdown = md(
            "1. First item\n\n```typescript\n// code block\n```\n\n2. Second item\n\n```typescript\n// another code block\n```\n\n3. Third item",
        );
        let lines: Vec<String> = plain(&markdown, 80);
        let numbered_lines: Vec<&String> = lines
            .iter()
            .filter(|l| l.starts_with(|c: char| c.is_ascii_digit()))
            .collect();
        assert_eq!(numbered_lines.len(), 3);
        assert!(numbered_lines[0].starts_with("1."));
        assert!(numbered_lines[1].starts_with("2."));
        assert!(numbered_lines[2].starts_with("3."));
    }

    #[test]
    fn indents_wrapped_unordered_list_lines() {
        let markdown = md("- alpha beta gamma delta epsilon");
        assert_eq!(
            plain(&markdown, 20),
            ["- alpha beta gamma", "  delta epsilon"]
        );
    }

    #[test]
    fn indents_wrapped_ordered_list_lines() {
        let markdown = md("1. alpha beta gamma delta epsilon");
        assert_eq!(
            plain(&markdown, 20),
            ["1. alpha beta gamma", "   delta epsilon"]
        );
    }

    #[test]
    fn indents_wrapped_ordered_list_lines_with_multi_digit_markers() {
        let markdown = md("10. alpha beta gamma delta epsilon");
        assert_eq!(
            plain(&markdown, 21),
            ["10. alpha beta gamma", "    delta epsilon"]
        );
    }

    #[test]
    fn indents_wrapped_nested_list_lines() {
        let markdown = md("- parent\n  - alpha beta gamma delta epsilon");
        assert_eq!(
            plain(&markdown, 24),
            ["- parent", "    - alpha beta gamma", "      delta epsilon"]
        );
    }

    #[test]
    fn indents_wrapped_nested_list_lines_under_ordered_parents() {
        let markdown = md("1. parent\n   - alpha beta gamma delta epsilon");
        assert_eq!(
            plain(&markdown, 24),
            ["1. parent", "    - alpha beta gamma", "      delta epsilon"]
        );
    }

    #[test]
    fn renders_and_wraps_blockquotes_inside_list_items() {
        let markdown = md("- > alpha beta gamma delta epsilon zeta");
        assert_eq!(
            plain(&markdown, 24),
            ["- │ alpha beta gamma", "  │ delta epsilon zeta"]
        );
    }

    #[test]
    fn renders_and_wraps_code_blocks_inside_list_items() {
        let markdown = md("- ```ts\n  alpha beta gamma delta epsilon zeta\n  ```");
        assert_eq!(
            plain(&markdown, 24),
            [
                "- ```ts",
                "    alpha beta gamma",
                "  delta epsilon zeta",
                "  ```"
            ]
        );
    }

    // ── Tables ─────────────────────────────────────────────────────────────

    fn table_markdown() -> &'static str {
        "| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |"
    }

    #[test]
    fn renders_simple_table() {
        let markdown = md(table_markdown());
        let lines = plain(&markdown, 80);
        assert!(lines.iter().any(|l| l.contains("Name")));
        assert!(lines.iter().any(|l| l.contains("Age")));
        assert!(lines.iter().any(|l| l.contains("Alice")));
        assert!(lines.iter().any(|l| l.contains("Bob")));
        assert!(lines.iter().any(|l| l.contains("│")));
        assert!(lines.iter().any(|l| l.contains("─")));
    }

    #[test]
    fn renders_row_dividers_between_data_rows() {
        let markdown = md(table_markdown());
        let lines = plain(&markdown, 80);
        let divider_lines = lines.iter().filter(|l| l.contains('┼')).count();
        assert_eq!(divider_lines, 2, "Expected header + row divider");
    }

    #[test]
    fn keeps_column_width_at_least_the_longest_word() {
        let longest_word = "superlongword";
        let markdown = md(&format!(
            "| Column One | Column Two |\n| --- | --- |\n| {longest_word} short | otherword |\n| small | tiny |"
        ));
        let lines = plain(&markdown, 32);
        let data_line = lines
            .iter()
            .find(|l| l.contains(longest_word))
            .expect("Expected data row containing longest word");
        let segments: Vec<&str> = data_line.split('│').skip(1).collect();
        let first_segment = segments[0];
        let first_column_width = first_segment.trim_end().len();
        assert!(first_column_width >= longest_word.len());
    }

    #[test]
    fn renders_table_with_alignment() {
        let markdown = md("| Left | Center | Right |\n| :--- | :---: | ---: |\n| A | B | C |\n| Long text | Middle | End |");
        let lines = plain(&markdown, 80);
        assert!(lines.iter().any(|l| l.contains("Left")));
        assert!(lines.iter().any(|l| l.contains("Center")));
        assert!(lines.iter().any(|l| l.contains("Right")));
        assert!(lines.iter().any(|l| l.contains("Long text")));
    }

    #[test]
    fn handles_tables_with_varying_column_widths() {
        let markdown = md("| Short | Very long column header |\n| --- | --- |\n| A | This is a much longer cell content |\n| B | Short |");
        let lines = plain(&markdown, 80);
        assert!(lines.iter().any(|l| l.contains("Very long column header")));
        assert!(lines
            .iter()
            .any(|l| l.contains("This is a much longer cell content")));
    }

    #[test]
    fn wraps_table_cells_when_table_exceeds_available_width() {
        let markdown = md("| Command | Description | Example |\n| --- | --- | --- |\n| npm install | Install all dependencies | npm install |\n| npm run build | Build the project | npm run build |");
        let width = 50;
        let lines = plain(&markdown, width);
        for line in &lines {
            assert!(
                line.chars().count() <= width,
                "Line exceeds width {width}: {line:?}"
            );
        }
        let all_text = lines.join(" ");
        assert!(all_text.contains("Command"));
        assert!(all_text.contains("Description"));
        assert!(all_text.contains("npm install"));
        assert!(all_text.contains("Install"));
    }

    #[test]
    fn wraps_long_cell_content_to_multiple_lines() {
        let markdown =
            md("| Header |\n| --- |\n| This is a very long cell content that should wrap |");
        let lines = plain(&markdown, 25);
        let data_rows: Vec<&String> = lines
            .iter()
            .filter(|l| l.starts_with('│') && !l.contains('─'))
            .collect();
        assert!(
            data_rows.len() > 2,
            "Expected wrapped rows, got {}",
            data_rows.len()
        );
        let all_text = lines.join(" ");
        assert!(all_text.contains("very long"));
        assert!(all_text.contains("cell content"));
        assert!(all_text.contains("should wrap"));
    }

    #[test]
    fn wraps_long_unbroken_tokens_inside_table_cells() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let url = "https://example.com/this/is/a/very/long/url/that/should/wrap";
        let markdown = Markdown::new(
            format!("| Value |\n| --- |\n| prefix {url} |"),
            0,
            0,
            theme(),
            None,
            None,
        );
        set_capabilities(caps(false));
        let width = 30;
        let lines = plain(&markdown, width);
        reset_capabilities_cache();
        for line in &lines {
            assert!(
                line.chars().count() <= width,
                "Line exceeds width {width}: {line:?}"
            );
        }
        let table_lines: Vec<&String> = lines.iter().filter(|l| l.starts_with('│')).collect();
        assert!(!table_lines.is_empty());
        for line in &table_lines {
            let border_count = line.split('│').count() - 1;
            assert_eq!(
                border_count, 2,
                "Expected 2 borders, got {border_count}: {line:?}"
            );
        }
        let extracted: String = lines.join("").replace(['│', '├', '┤', '─', ' '], "");
        assert!(extracted.contains("prefix"));
        assert!(extracted.contains(url));
    }

    #[test]
    fn wraps_styled_inline_code_inside_table_cells_without_breaking_borders() {
        let markdown = md("| Code |\n| --- |\n| `averyveryveryverylongidentifier` |");
        let width = 20;
        let raw_lines = markdown.render(width);
        let joined_output = raw_lines.join("\n");
        assert!(
            joined_output.contains("\x1b[33m"),
            "Inline code should be styled (yellow)"
        );
        let lines = plain(&markdown, width);
        for line in &lines {
            assert!(
                line.chars().count() <= width,
                "Line exceeds width {width}: {line:?}"
            );
        }
        let table_lines: Vec<&String> = lines.iter().filter(|l| l.starts_with('│')).collect();
        for line in &table_lines {
            let border_count = line.split('│').count() - 1;
            assert_eq!(
                border_count, 2,
                "Expected 2 borders, got {border_count}: {line:?}"
            );
        }
    }

    #[test]
    fn handles_extremely_narrow_width_gracefully() {
        let markdown = md("| A | B | C |\n| --- | --- | --- |\n| 1 | 2 | 3 |");
        let lines = plain(&markdown, 15);
        assert!(!lines.is_empty());
        for line in &lines {
            assert!(
                line.chars().count() <= 15,
                "Line exceeds width 15: {line:?}"
            );
        }
    }

    #[test]
    fn renders_table_correctly_when_it_fits_naturally() {
        let markdown = md("| A | B |\n| --- | --- |\n| 1 | 2 |");
        let lines = plain(&markdown, 80);
        let header_line = lines.iter().find(|l| l.contains("A") && l.contains("B"));
        assert!(header_line.is_some());
        assert!(header_line.unwrap().contains('│'));
        assert!(lines.iter().any(|l| l.contains('├') && l.contains('┼')));
        assert!(lines.iter().any(|l| l.contains("1") && l.contains("2")));
    }

    #[test]
    fn respects_padding_x_when_calculating_table_width() {
        let markdown = Markdown::new(
            "| Column One | Column Two |\n| --- | --- |\n| Data 1 | Data 2 |",
            2,
            0,
            theme(),
            None,
            None,
        );
        let width = 40;
        let lines = plain(&markdown, width);
        for line in &lines {
            assert!(
                line.chars().count() <= width,
                "Line exceeds width {width}: {line:?}"
            );
        }
        let table_row = lines.iter().find(|l| l.contains('│'));
        assert!(table_row.is_some());
        assert!(
            table_row.unwrap().starts_with("  "),
            "Table should have left padding"
        );
    }

    #[test]
    fn does_not_add_trailing_blank_line_when_table_is_last_block() {
        let markdown = md("| Name |\n| --- |\n| Alice |");
        let lines = plain(&markdown, 80);
        assert_ne!(lines.last().map(String::as_str), Some(""));
    }

    // ── Combined features ──────────────────────────────────────────────────

    #[test]
    fn renders_lists_and_tables_together() {
        let markdown = md("# Test Document\n\n- Item 1\n  - Nested item\n- Item 2\n\n| Col1 | Col2 |\n| --- | --- |\n| A | B |");
        let lines = plain(&markdown, 80);
        assert!(lines.iter().any(|l| l.contains("Test Document")));
        assert!(lines.iter().any(|l| l.contains("- Item 1")));
        assert!(lines.iter().any(|l| l.contains("    - Nested item")));
        assert!(lines.iter().any(|l| l.contains("Col1")));
        assert!(lines.iter().any(|l| l.contains("│")));
    }

    // ── Backslash escapes ──────────────────────────────────────────────────

    #[test]
    fn normalizes_escaped_punctuation_by_default() {
        let markdown = md("\"\\\"");
        assert_eq!(plain(&markdown, 80), ["\"\""]);
    }

    #[test]
    fn preserves_source_backslash_escapes_when_configured() {
        let markdown = Markdown::new(
            "\"\\\"",
            0,
            0,
            theme(),
            None,
            Some(MarkdownOptions {
                preserve_backslash_escapes: true,
                ..Default::default()
            }),
        );
        assert_eq!(plain(&markdown, 80), ["\"\\\""]);
    }

    // ── Width-aware transform (markdown.ts:285, T29) ─────────────────────────

    #[test]
    fn transform_applies_before_parsing() {
        let markdown = Markdown::new(
            "original",
            0,
            0,
            theme(),
            None,
            Some(MarkdownOptions {
                transform: Some(Arc::new(|text, _| format!("**{text}**"))),
                ..Default::default()
            }),
        );
        // The transform output is parsed as markdown: the bold markers are
        // gone from the source and the bold ANSI styling is present.
        let rendered = markdown.render(80).join("\n");
        assert!(!rendered.contains("**original**"));
        assert!(rendered.contains("\x1b[1moriginal\x1b[22m"));
    }

    #[test]
    fn transform_receives_available_content_width() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_inner = Arc::clone(&seen);
        let markdown = Markdown::new(
            "hello",
            3,
            0,
            theme(),
            None,
            Some(MarkdownOptions {
                transform: Some(Arc::new(move |text, width| {
                    seen_inner
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(width);
                    text.to_owned()
                })),
                ..Default::default()
            }),
        );
        markdown.render(80);
        markdown.render(80);
        markdown.render(40);
        let widths = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // width - padding_x * 2 (markdown.ts:284), clamped to >= 1.
        assert_eq!(widths, vec![74, 74, 34]);
    }

    #[test]
    fn transform_result_changes_invalidate_cache() {
        // A stateful transform can return different output for the same
        // source + width; the transformed text is part of the cache key, so
        // the second render must not serve stale lines.
        let state = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state_inner = Arc::clone(&state);
        let markdown = Markdown::new(
            "hello",
            0,
            0,
            theme(),
            None,
            Some(MarkdownOptions {
                transform: Some(Arc::new(move |text, _| {
                    if state_inner.load(std::sync::atomic::Ordering::Relaxed) {
                        format!("**{text}**")
                    } else {
                        text.to_owned()
                    }
                })),
                ..Default::default()
            }),
        );
        let first = markdown.render(80).join("\n");
        assert!(first.contains("hello"));
        assert!(!first.contains("\x1b[1mhello\x1b[22m"));

        state.store(true, std::sync::atomic::Ordering::Relaxed);
        let second = markdown.render(80).join("\n");
        assert!(second.contains("\x1b[1mhello\x1b[22m"));
    }

    // ── Pre-styled text (thinking traces) ──────────────────────────────────

    fn gray_italic_default() -> DefaultTextStyle {
        DefaultTextStyle {
            color: Some(Box::new(|text: &str| format!("\x1b[90m{text}\x1b[0m"))),
            bg_color: None,
            bold: false,
            italic: true,
            strikethrough: false,
            underline: false,
        }
    }

    #[test]
    fn preserves_gray_italic_styling_after_inline_code() {
        let markdown = Markdown::new(
            "This is thinking with `inline code` and more text after",
            1,
            0,
            theme(),
            Some(gray_italic_default()),
            None,
        );
        let joined = markdown.render(80).join("\n");
        assert!(joined.contains("inline code"));
        assert!(joined.contains("\x1b[90m"), "Should have gray color code");
        assert!(joined.contains("\x1b[3m"), "Should have italic code");
        assert!(joined.contains("\x1b[33m"), "Should style inline code");
    }

    #[test]
    fn preserves_gray_italic_styling_after_bold_text() {
        let markdown = Markdown::new(
            "This is thinking with **bold text** and more after",
            1,
            0,
            theme(),
            Some(gray_italic_default()),
            None,
        );
        let joined = markdown.render(80).join("\n");
        assert!(joined.contains("bold text"));
        assert!(joined.contains("\x1b[90m"), "Should have gray color code");
        assert!(joined.contains("\x1b[3m"), "Should have italic code");
        assert!(joined.contains("\x1b[1m"), "Should have bold code");
    }

    #[test]
    fn does_not_leak_styles_into_following_lines() {
        // Upstream renders Markdown + an INPUT line through a TUI and asserts
        // the row below the markdown is not italic; here we check the
        // equivalent output-level property: every rendered markdown line
        // closes its styles (ends with a reset) so nothing bleeds into the
        // next TUI row.
        let markdown = Markdown::new(
            "This is thinking with `inline code`",
            1,
            0,
            theme(),
            Some(gray_italic_default()),
            None,
        );
        let lines = markdown.render(80);
        assert!(!lines.is_empty());
        for line in &lines {
            assert!(
                line.trim_end().ends_with("\x1b[0m"),
                "line must end with a style reset: {line:?}"
            );
        }
        // The code span itself is styled yellow and the default gray/italic
        // style is re-applied after it.
        let joined = lines.join("\n");
        assert!(joined.contains("\x1b[33m"));
        assert!(joined.contains("\x1b[90m"));
        assert!(joined.contains("\x1b[3m"));
    }

    // ── Spacing after code blocks ──────────────────────────────────────────

    #[test]
    fn one_blank_line_between_code_block_and_following_paragraph() {
        let markdown =
            md("hello world\n\n```js\nconst hello = \"world\";\n```\n\nagain, hello world");
        let lines = plain(&markdown, 80);
        let closing_backticks = lines
            .iter()
            .position(|l| l == "```")
            .expect("Should have closing backticks");
        let after = &lines[closing_backticks + 1..];
        let empty_count = after.iter().position(|l| !l.is_empty());
        assert_eq!(empty_count, Some(1));
    }

    #[test]
    fn normalizes_paragraph_and_code_block_spacing_to_one_blank_line() {
        let cases = [
            "hello this is text\n```\ncode block\n```\nmore text",
            "hello this is text\n\n```\ncode block\n```\n\nmore text",
        ];
        let expected = [
            "hello this is text",
            "",
            "```",
            "  code block",
            "```",
            "",
            "more text",
        ];
        for text in cases {
            let markdown = md(text);
            assert_eq!(
                plain(&markdown, 80),
                expected,
                "Unexpected spacing for {text:?}"
            );
        }
    }

    #[test]
    fn no_trailing_blank_line_when_code_block_is_last_block() {
        let cases = [
            "```js\nconst hello = 'world';\n```",
            "hello world\n\n```js\nconst hello = 'world';\n```",
        ];
        for text in cases {
            let markdown = md(text);
            let lines = plain(&markdown, 80);
            assert_ne!(lines.last().map(String::as_str), Some(""));
        }
    }

    // ── Spacing after dividers ─────────────────────────────────────────────

    #[test]
    fn one_blank_line_between_divider_and_following_paragraph() {
        let markdown = md("hello world\n\n---\n\nagain, hello world");
        let lines = plain(&markdown, 80);
        let divider = lines
            .iter()
            .position(|l| l.contains('─'))
            .expect("Should have divider");
        let after = &lines[divider + 1..];
        let empty_count = after.iter().position(|l| !l.is_empty());
        assert_eq!(empty_count, Some(1));
    }

    #[test]
    fn no_trailing_blank_line_when_divider_is_last_block() {
        let markdown = md("---");
        let lines = plain(&markdown, 80);
        assert_ne!(lines.last().map(String::as_str), Some(""));
    }

    // ── Spacing after headings ─────────────────────────────────────────────

    #[test]
    fn one_blank_line_between_heading_and_following_paragraph() {
        let markdown = md("# Hello\n\nThis is a paragraph");
        let lines = plain(&markdown, 80);
        let heading = lines
            .iter()
            .position(|l| l.contains("Hello"))
            .expect("Should have heading");
        let after = &lines[heading + 1..];
        let empty_count = after.iter().position(|l| !l.is_empty());
        assert_eq!(empty_count, Some(1));
    }

    #[test]
    fn no_trailing_blank_line_when_heading_is_last_block() {
        let markdown = md("# Hello");
        let lines = plain(&markdown, 80);
        assert_ne!(lines.last().map(String::as_str), Some(""));
    }

    // ── Spacing after blockquotes ──────────────────────────────────────────

    #[test]
    fn one_blank_line_between_blockquote_and_following_paragraph() {
        let markdown = md("hello world\n\n> This is a quote\n\nagain, hello world");
        let lines = plain(&markdown, 80);
        let quote = lines
            .iter()
            .position(|l| l.contains("This is a quote"))
            .expect("Should have blockquote");
        let after = &lines[quote + 1..];
        let empty_count = after.iter().position(|l| !l.is_empty());
        assert_eq!(empty_count, Some(1));
    }

    #[test]
    fn no_trailing_blank_line_when_blockquote_is_last_block() {
        let markdown = md("> This is a quote");
        let lines = plain(&markdown, 80);
        assert_ne!(lines.last().map(String::as_str), Some(""));
    }

    // ── Blockquotes with multiline content ─────────────────────────────────

    #[test]
    fn consistent_styling_in_lazy_continuation_blockquote() {
        let markdown = Markdown::new(
            ">Foo\nbar",
            0,
            0,
            theme(),
            Some(DefaultTextStyle {
                color: Some(Box::new(|text: &str| format!("\x1b[35m{text}\x1b[0m"))),
                ..Default::default()
            }),
            None,
        );
        let lines = markdown.render(80);
        let plain_lines: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        let quoted: Vec<&String> = plain_lines.iter().filter(|l| l.starts_with("│ ")).collect();
        assert_eq!(quoted.len(), 2);
        let foo_line = lines.iter().find(|l| l.contains("Foo")).expect("Foo line");
        let bar_line = lines.iter().find(|l| l.contains("bar")).expect("bar line");
        assert!(foo_line.contains("\x1b[3m"), "Foo line should have italic");
        assert!(bar_line.contains("\x1b[3m"), "bar line should have italic");
        assert!(
            !foo_line.contains("\x1b[35m"),
            "Foo line should NOT have magenta"
        );
        assert!(
            !bar_line.contains("\x1b[35m"),
            "bar line should NOT have magenta"
        );
    }

    #[test]
    fn consistent_styling_in_explicit_multiline_blockquote() {
        let markdown = Markdown::new(
            ">Foo\n>bar",
            0,
            0,
            theme(),
            Some(DefaultTextStyle {
                color: Some(Box::new(|text: &str| format!("\x1b[36m{text}\x1b[0m"))),
                ..Default::default()
            }),
            None,
        );
        let lines = markdown.render(80);
        let plain_lines: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        let quoted: Vec<&String> = plain_lines.iter().filter(|l| l.starts_with("│ ")).collect();
        assert_eq!(quoted.len(), 2);
        let foo_line = lines.iter().find(|l| l.contains("Foo")).expect("Foo line");
        let bar_line = lines.iter().find(|l| l.contains("bar")).expect("bar line");
        assert!(foo_line.contains("\x1b[3m"));
        assert!(bar_line.contains("\x1b[3m"));
        assert!(!foo_line.contains("\x1b[36m"));
        assert!(!bar_line.contains("\x1b[36m"));
    }

    #[test]
    fn renders_list_content_inside_blockquotes() {
        let markdown = md("> 1. bla bla\n> - nested bullet");
        let lines = plain(&markdown, 80);
        let quoted: Vec<String> = lines.into_iter().filter(|l| l.starts_with("│ ")).collect();
        assert!(quoted.iter().any(|l| l.contains("1. bla bla")));
        assert!(quoted.iter().any(|l| l.contains("- nested bullet")));
    }

    #[test]
    fn wraps_long_blockquote_lines_and_borders_each_wrapped_line() {
        let markdown = md("> This is a very long blockquote line that should wrap to multiple lines when rendered");
        let lines = plain(&markdown, 30);
        let content: Vec<&String> = lines.iter().filter(|l| !l.is_empty()).collect();
        assert!(content.len() > 1, "Expected multiple wrapped lines");
        for line in &content {
            assert!(
                line.starts_with("│ "),
                "Wrapped line should have quote border: {line:?}"
            );
        }
        let all_text: Vec<&str> = content.iter().map(|s| s.as_str()).collect();
        let all_text = all_text.join(" ");
        assert!(all_text.contains("very long"));
        assert!(all_text.contains("blockquote"));
        assert!(all_text.contains("multiple"));
    }

    #[test]
    fn properly_indents_wrapped_blockquote_lines_with_styling() {
        let markdown = Markdown::new(
            "> This is styled text that is long enough to wrap",
            0,
            0,
            theme(),
            Some(DefaultTextStyle {
                color: Some(Box::new(|text: &str| format!("\x1b[33m{text}\x1b[0m"))),
                italic: true,
                ..Default::default()
            }),
            None,
        );
        let lines = markdown.render(25);
        let plain_lines: Vec<String> = lines
            .iter()
            .map(|l| strip_ansi(l).trim_end().to_string())
            .collect();
        let content: Vec<&String> = plain_lines.iter().filter(|l| !l.is_empty()).collect();
        for line in &content {
            assert!(
                line.starts_with("│ "),
                "Line should have quote border: {line:?}"
            );
        }
        let all_output = lines.join("\n");
        assert!(all_output.contains("\x1b[3m"), "Should have italic");
        assert!(
            !all_output.contains("\x1b[33m"),
            "Should NOT have yellow color from default style"
        );
    }

    #[test]
    fn renders_inline_formatting_inside_blockquotes_and_reapplies_quote_styling() {
        let markdown = md("> Quote with **bold** and `code`");
        let lines = markdown.render(80);
        let plain_lines: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(plain_lines.iter().any(|l| l.starts_with("│ ")));
        let all_plain = plain_lines.join(" ");
        assert!(all_plain.contains("Quote with"));
        assert!(all_plain.contains("bold"));
        assert!(all_plain.contains("code"));
        let all_output = lines.join("\n");
        assert!(all_output.contains("\x1b[1m"), "Should have bold styling");
        assert!(
            all_output.contains("\x1b[33m"),
            "Should have code styling (yellow)"
        );
        assert!(
            all_output.contains("\x1b[3m"),
            "Should have italic from quote styling"
        );
    }

    // ── Headings with inline code ──────────────────────────────────────────

    #[test]
    fn preserves_heading_styling_after_inline_code() {
        let markdown = md("### Why `sourceInfo` should not be optional");
        let joined = markdown.render(80).join("\n");
        assert!(
            joined.contains("\x1b[33m"),
            "Should have yellow for inline code"
        );
        let after_code = joined
            .find("should not be optional")
            .expect("text after inline code");
        let preceding = &joined[after_code.saturating_sub(40)..after_code];
        assert!(
            preceding.contains("\x1b[1m"),
            "Should re-apply bold: {preceding:?}"
        );
        assert!(
            preceding.contains("\x1b[36m"),
            "Should re-apply cyan: {preceding:?}"
        );
    }

    #[test]
    fn preserves_heading_styling_after_inline_code_for_h1() {
        let markdown = md("# Title with `code` inside");
        let joined = markdown.render(80).join("\n");
        let after_code = joined.find("inside").expect("text after inline code");
        let preceding = &joined[after_code.saturating_sub(40)..after_code];
        assert!(
            preceding.contains("\x1b[1m"),
            "Should re-apply bold for h1: {preceding:?}"
        );
        assert!(
            preceding.contains("\x1b[36m"),
            "Should re-apply cyan for h1: {preceding:?}"
        );
        assert!(
            preceding.contains("\x1b[4m"),
            "Should re-apply underline for h1: {preceding:?}"
        );
    }

    #[test]
    fn does_not_leak_h1_underline_into_padding() {
        // Upstream checks xterm buffer cells in the padding region; the
        // equivalent output property: no underline sequence may appear after
        // the last reset that closes the heading content.
        let markdown = md("# Important distinction from `open()`");
        let rendered = markdown.render(80);
        let first = rendered.first().expect("Should render heading line");
        let stripped = strip_ansi(first);
        let content_width = stripped.trim_end().len();
        assert!(content_width > 0, "Should have visible heading content");
        let last_reset = first.rfind("\x1b[0m");
        if let Some(reset_end) = last_reset {
            assert!(
                !first[reset_end..].contains("\x1b[4m"),
                "Underline must not leak into padding: {first:?}"
            );
        }
    }

    #[test]
    fn preserves_heading_styling_after_bold_text() {
        let markdown = md("## Heading with **bold** and more");
        let joined = markdown.render(80).join("\n");
        let after_bold = joined.find("and more").expect("text after bold");
        let preceding = &joined[after_bold.saturating_sub(40)..after_bold];
        assert!(
            preceding.contains("\x1b[1m"),
            "Should re-apply bold for h2: {preceding:?}"
        );
        assert!(
            preceding.contains("\x1b[36m"),
            "Should re-apply cyan for h2: {preceding:?}"
        );
    }

    // ── Strikethrough syntax ───────────────────────────────────────────────

    #[test]
    fn renders_double_tilde_as_strikethrough() {
        let markdown = md("Use ~~strikethrough~~ here");
        let lines = markdown.render(80);
        let joined = lines.join("\n");
        let joined_plain = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("\x1b[9m"),
            "Should apply strikethrough styling"
        );
        assert!(joined_plain.contains("strikethrough"));
        assert!(!joined_plain.contains("~~strikethrough~~"));
    }

    #[test]
    fn keeps_single_tilde_as_plain_text() {
        let markdown = md("Use ~strikethrough~ literally");
        let lines = markdown.render(80);
        let joined = lines.join("\n");
        let joined_plain = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined_plain.contains("~strikethrough~"));
        assert!(
            !joined.contains("\x1b[9m"),
            "Single-tilde text should not use strikethrough"
        );
    }

    #[test]
    fn does_not_strike_whitespace_flanked_or_triple_tilde() {
        // marked's StrictStrikethroughTokenizer edge cases, verified equal in
        // comrak's GFM strikethrough.
        for text in [
            "~~ foo~~",
            "~~foo ~~",
            "~~foo~~~",
            "~~ foo ~~ bar",
            "~~  foo  ~~",
        ] {
            let markdown = md(text);
            let joined = markdown.render(80).join("\n");
            assert!(
                !joined.contains("\x1b[9m"),
                "Should not strike {text:?}: {joined:?}"
            );
        }
    }

    // ── Links ──────────────────────────────────────────────────────────────

    #[test]
    fn does_not_duplicate_url_for_autolinked_emails() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(caps(false));
        let markdown = md("Contact user@example.com for help");
        let lines = markdown.render(80);
        let joined = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("user@example.com"));
        assert!(
            !joined.contains("mailto:"),
            "Should not show mailto: prefix"
        );
        reset_capabilities_cache();
    }

    #[test]
    fn does_not_duplicate_url_for_bare_urls() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(caps(false));
        let markdown = md("Visit https://example.com for more");
        let lines = markdown.render(80);
        let joined = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined.matches("https://example.com").count(), 1);
        reset_capabilities_cache();
    }

    #[test]
    fn shows_url_in_parentheses_when_hyperlinks_not_supported() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(caps(false));
        let markdown = md("[click here](https://example.com)");
        let lines = markdown.render(80);
        let joined = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("click here"));
        assert!(joined.contains("(https://example.com)"));
        reset_capabilities_cache();
    }

    #[test]
    fn shows_mailto_url_in_parentheses_when_hyperlinks_not_supported() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(caps(false));
        let markdown = md("[Email me](mailto:test@example.com)");
        let lines = markdown.render(80);
        let joined = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("Email me"));
        assert!(joined.contains("(mailto:test@example.com)"));
        reset_capabilities_cache();
    }

    #[test]
    fn emits_osc8_hyperlink_sequence_when_terminal_supports_hyperlinks() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(caps(true));
        let markdown = md("[click here](https://example.com)");
        let lines = markdown.render(80);
        let joined = lines.join("");
        assert!(
            joined.contains("\x1b]8;;https://example.com\x1b\\"),
            "OSC 8 open"
        );
        assert!(joined.contains("\x1b]8;;\x1b\\"), "OSC 8 close");
        assert!(joined.contains("click here"));
        assert!(
            !joined.contains("(https://example.com)"),
            "URL should not appear inline"
        );
        reset_capabilities_cache();
    }

    #[test]
    fn uses_osc8_for_mailto_links_when_terminal_supports_hyperlinks() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(caps(true));
        let markdown = md("[Email me](mailto:test@example.com)");
        let joined = markdown.render(80).join("");
        assert!(joined.contains("\x1b]8;;mailto:test@example.com\x1b\\"));
        assert!(joined.contains("\x1b]8;;\x1b\\"));
        reset_capabilities_cache();
    }

    #[test]
    fn uses_osc8_for_bare_urls_when_terminal_supports_hyperlinks() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(caps(true));
        let markdown = md("Visit https://example.com for more");
        let joined = markdown.render(80).join("");
        assert!(joined.contains("\x1b]8;;https://example.com\x1b\\"));
        assert!(!joined.contains("(https://example.com)"));
        reset_capabilities_cache();
    }

    // ── HTML-like tags in text ─────────────────────────────────────────────

    #[test]
    fn renders_content_with_html_like_tags_as_text() {
        let markdown =
            md("This is text with <thinking>hidden content</thinking> that should be visible");
        let lines = markdown.render(80);
        let joined = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("hidden content") || joined.contains("<thinking>"),
            "Should render HTML-like tags or their content as text"
        );
    }

    #[test]
    fn renders_html_tags_in_code_blocks_correctly() {
        let markdown = md("```html\n<div>Some HTML</div>\n```");
        let lines = markdown.render(80);
        let joined = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("<div>") && joined.contains("</div>"));
    }

    // ── Streaming code fences ──────────────────────────────────────────────

    #[test]
    fn stabilizes_partial_closing_fence_rendering() {
        let cases: &[(&str, &[&str])] = &[
            (
                "```ts\nconst x = 1;\n``",
                &["```ts", "  const x = 1;", "```"],
            ),
            (
                "```md\nnot a closing fence:\n``\n```",
                &["```md", "  not a closing fence:", "  ``", "```"],
            ),
            ("```ts\n``", &["```ts", "", "```"]),
            ("````\n```", &["```", "", "```"]),
            ("~~~~~\n~~~~", &["```", "", "```"]),
            (
                "```md\nnot a closing fence:\n``\n```\n\nafter",
                &[
                    "```md",
                    "  not a closing fence:",
                    "  ``",
                    "```",
                    "",
                    "after",
                ],
            ),
        ];
        for (input, expected) in cases {
            let markdown = md(input);
            assert_eq!(
                plain(&markdown, 80),
                *expected,
                "Unexpected rendering for {input:?}"
            );
        }

        let partial = md("```ts\nconst x = 1;\n``");
        let complete = md("```ts\nconst x = 1;\n```");
        assert_eq!(partial.render(80).len(), complete.render(80).len());
    }

    #[test]
    fn empty_text_renders_single_empty_line() {
        let markdown = md("");
        assert_eq!(markdown.render(80), [""]);
        let whitespace = md("   \n  ");
        assert_eq!(whitespace.render(80), [""]);
    }

    // ── LaTeX math ─────────────────────────────────────────────────────────
    // Ports of the upstream `describe("LaTeX math")` cases (markdown.test.ts
    // @ 4181f66), expectations verified against the upstream renderer.

    #[test]
    fn renders_inline_dollar_and_parenthesis_delimiters() {
        let markdown = md(
            r"A map $\mathbb{C}^3 \to \mathbb{C}^3$, $xy$, $x-y$, $-x$, $\frac{1}{2}$, and \(s \to \infty\).",
        );
        assert_eq!(
            plain(&markdown, 80),
            ["A map ℂ³ → ℂ³, xy, x-y, -x, 1/2, and s → ∞."]
        );
    }

    #[test]
    fn renders_display_dollar_delimiters() {
        let markdown = md(r"Before

$$\{3x+2y,\; x \in \{0, \pm 1\}\}$$

after");
        assert_eq!(
            plain(&markdown, 80),
            ["Before", "", "{3x+2y, x ∈ {0, ± 1}}", "", "after"]
        );
    }

    #[test]
    fn renders_display_bracket_delimiters() {
        let markdown = md(r"Before

\[
E \approx \frac{0.1\ \text{lux}}{100\ \text{lm/W}}
\]

after");
        assert_eq!(
            plain(&markdown, 80),
            [
                "Before",
                "",
                "    0.1 lux",
                "E ≈ ────────",
                "    100 lm/W",
                "",
                "after"
            ]
        );
    }

    #[test]
    fn aligns_matrix_rows_with_the_opening_delimiter() {
        let markdown = md(r"Consider the matrix

\[
A=
\begin{pmatrix}
\pi & 0\\
0 & \frac{1}{\pi}
\end{pmatrix}.
\]");
        assert_eq!(
            plain(&markdown, 80),
            [
                "Consider the matrix",
                "",
                "A = ⎛ π │ 0   ⎞",
                "    ⎝ 0 │ 1/π ⎠.",
            ]
        );
    }

    #[test]
    fn renders_lower_limits_beneath_display_operators() {
        let markdown = md(r"\[
\lim_{x\to 0}\frac{\frac{\sin x}{x}-1}{\frac{e^x-1}{x}-1}=0
\]");
        assert_eq!(
            plain(&markdown, 80),
            [
                "     (sin x)/x-1",
                "lim  ─────────── = 0",
                "x→0  (eˣ-1)/x-1"
            ]
        );
    }

    #[test]
    fn renders_math_inside_lists_and_tables() {
        let markdown = md(r"- Formula: $F_1 = u^2$

| Value |
| --- |
| $\mathbb{C}^3$ |");
        assert_eq!(
            plain(&markdown, 80),
            [
                "- Formula: F₁ = u²",
                "",
                "┌───────┐",
                "│ Value │",
                "├───────┤",
                "│ ℂ³    │",
                "└───────┘",
            ]
        );
    }

    #[test]
    fn does_not_treat_currency_shell_variables_or_code_spans_as_math() {
        let source = "Costs $5 and $10 or $8k–$12k; use `$x$`, $HOME, and ${PATH}.";
        let markdown = md(source);
        assert_eq!(
            plain(&markdown, 80),
            ["Costs $5 and $10 or $8k–$12k; use $x$, $HOME, and ${PATH}."]
        );

        let shell_variables = "Paths: $HOME/$USER and $XDG_CONFIG_HOME/$APP_CONFIG";
        let shell_markdown = md(shell_variables);
        assert_eq!(plain(&shell_markdown, 80), [shell_variables]);
    }

    #[test]
    fn preserves_unsupported_and_incomplete_latex() {
        let cases = [
            (
                r"Unknown $x + \unknown{y}$ after",
                "Unknown $x + \\unknown{y}$ after",
            ),
            (r"Streaming $\mathbb{C}^3", "Streaming $\\mathbb{C}^3"),
        ];
        for (source, expected) in cases {
            let markdown = md(source);
            assert_eq!(plain(&markdown, 80), [expected], "source: {source:?}");
        }
    }

    #[test]
    fn preserves_incomplete_backslash_delimiters_while_streaming() {
        let inline = md(r"Map \(\mathbb{C}^3");
        assert_eq!(plain(&inline, 80), [r"Map \(\mathbb{C}^3"]);

        let display = md("\\[\nx^2");
        assert_eq!(plain(&display, 80), ["\\[", "x^2"]);
    }

    #[test]
    fn does_not_render_latex_inside_escaped_delimiters_or_code_fences() {
        let source = "Escaped \\$x-y\\$.\n\n```text\n$\\mathbb{C}^3$\n```";
        let markdown = md(source);
        assert_eq!(
            plain(&markdown, 80),
            ["Escaped $x-y$.", "", "```text", "  $\\mathbb{C}^3$", "```"]
        );
    }

    #[test]
    fn allows_latex_rendering_to_be_disabled() {
        let markdown = Markdown::new(
            r"Map $\mathbb{C}^3 \to \mathbb{C}^3$",
            0,
            0,
            theme(),
            None,
            Some(MarkdownOptions {
                render_latex: false,
                ..Default::default()
            }),
        );
        assert_eq!(
            plain(&markdown, 80),
            [r"Map $\mathbb{C}^3 \to \mathbb{C}^3$"]
        );
    }

    #[test]
    fn switches_from_raw_to_rendered_math_when_a_streamed_delimiter_closes() {
        let mut markdown = md(r"Map $\mathbb{C}^3");
        assert_eq!(plain(&markdown, 80), [r"Map $\mathbb{C}^3"]);
        markdown.set_text(r"Map $\mathbb{C}^3$");
        assert_eq!(plain(&markdown, 80), ["Map ℂ³"]);
    }

    #[test]
    fn splits_paragraphs_at_mid_paragraph_display_math() {
        let markdown = md("text\n$$x^2+1$$\nmore");
        assert_eq!(plain(&markdown, 80), ["text", "", "x²+1", "", "more"]);

        let markdown = md("$$x^2+1$$\nafter");
        assert_eq!(plain(&markdown, 80), ["x²+1", "", "after"]);
    }

    #[test]
    fn renders_math_inside_blockquotes_and_headings() {
        let markdown = md("> quote $x^2$");
        assert_eq!(plain(&markdown, 80), ["│ quote x²"]);

        let markdown = md("# Head $x^2$");
        assert_eq!(plain(&markdown, 80), ["Head x²"]);
    }

    #[test]
    fn latex_output_contains_no_pua_markers() {
        let markdown = md(r"$$
\begin{pmatrix}a & b \\ c & d\end{pmatrix}
$$");
        let output = markdown.render(80).join("\n");
        for marker in '\u{f0000}'..='\u{f0005}' {
            assert!(
                !output.contains(marker),
                "PUA marker leaked: {:#x}",
                marker as u32
            );
        }
    }

    #[test]
    fn render_latex_option_change_invalidates_cache() {
        let mut markdown = md(r"Map $\mathbb{C}^3$");
        assert_eq!(plain(&markdown, 80), ["Map ℂ³"]);
        // No invalidate(): the cache key must include the render_latex flag.
        markdown.options.render_latex = false;
        assert_eq!(plain(&markdown, 80), [r"Map $\mathbb{C}^3$"]);
    }
}
