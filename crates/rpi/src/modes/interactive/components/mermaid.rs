//! Port of
//! `packages/coding-agent/src/modes/interactive/components/mermaid.ts`
//! @ pi 4181f66 (66534fbdc).
//!
//! Replaces Mermaid fenced code blocks with Unicode terminal diagrams
//! (rendered by `rpi_tui::mermaid`, the grok-build port). The extension
//! chain runs after this transformer, matching upstream's
//! `[mermaidMarkdownTransformer, ...extensions]` order.
//!
//! Intentional differences:
//! - Upstream lexes with Marked and splices `token.raw` back together; rpi
//!   parses with comrak (the same parser as the markdown component, see
//!   coding-standards D-018) and splices whole source lines: each mermaid
//!   code block node's `sourcepos` line range is replaced bottom-up. Output
//!   for non-mermaid content is byte-identical; for a mermaid block the
//!   fences and content lines are consumed exactly like upstream's `raw`
//!   splice.
//! - `art.width` / `art.warnings` do not exist on `rpi_tui::mermaid`: the
//!   width is recomputed from `plain_lines` via `visible_width`, and the
//!   upstream non-streaming warnings fallback (mermaid.ts:77-82) has no
//!   counterpart because the ported renderer never reports warnings — it
//!   falls back to a boxed source for anything it cannot draw.
//! - The renderer always needs `MermaidStyles`; with no theme the styles
//!   are neutral (default terminal colors) and `plain_lines` are emitted,
//!   mirroring upstream's `options.theme ? styled : plain` choice.
//! - Upstream theme styling runs per span (`styleSpan`); here the styling
//!   is applied inside `rpi_tui::mermaid::render` via `MermaidStyles`
//!   (mermaid.rs `styled_lines` are already ANSI).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use comrak::nodes::NodeValue;
use comrak::{Arena, Options, parse_document};
use rpi_ext_host::types::{MarkdownTransformContext, MarkdownTransformerFn};
use rpi_tui::mermaid::{Color, MermaidArt, MermaidStyles, Style, render};
use rpi_tui::utils::visible_width;

use crate::core::settings_manager::MermaidRenderingMode;
use crate::core::themes::Theme;

/// Candidate scan (R3.1.1): the transformer only runs the comrak parse when
/// the source can contain a Mermaid fence. False positives fall back to the
/// normal parse; false negatives are not allowed.
///
/// A line starts a fence after up to three spaces of indentation plus any
/// blockquote/list container prefixes (`> `, `- `, `1. `, ...). Four-space
/// indented code is not a fence. The first info word must be `mermaid`
/// (ASCII case-insensitive).
fn contains_mermaid_candidate(source: &str) -> bool {
    let precise = source
        .split_inclusive('\n')
        .any(|line| fence_info_word(line).is_some_and(|word| word.eq_ignore_ascii_case("mermaid")));
    // Fallback (conservative): a real Mermaid fence's info string always
    // contains the word. This catches container/nested-list indentation the
    // line scanner cannot model without a full CommonMark block parser. False
    // positives only run the normal parse and return the source unchanged.
    precise || contains_ascii_case_insensitive(source, "mermaid")
}

/// Allocation-free ASCII case-insensitive substring scan.
fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

/// The first info word of a fence that starts at `line`, if any.
fn fence_info_word(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut index = 0usize;
    loop {
        // Up to three spaces at this container level (four spaces is an
        // indented code block, never a fence).
        let mut spaces = 0usize;
        while spaces < 3 && bytes.get(index) == Some(&b' ') {
            spaces += 1;
            index += 1;
        }
        match bytes.get(index) {
            Some(b'>') => {
                index += 1;
                if matches!(bytes.get(index), Some(b' ' | b'\t')) {
                    index += 1;
                }
            }
            Some(b'-' | b'*' | b'+') if matches!(bytes.get(index + 1), Some(b' ' | b'\t')) => {
                index += 2;
            }
            Some(b'0'..=b'9') => {
                let digits_start = index;
                while index - digits_start < 9 && bytes.get(index).is_some_and(u8::is_ascii_digit) {
                    index += 1;
                }
                if matches!(bytes.get(index), Some(b'.' | b')'))
                    && matches!(bytes.get(index + 1), Some(b' ' | b'\t'))
                {
                    index += 2;
                } else {
                    return None;
                }
            }
            _ => break,
        }
    }

    let fence_char = *bytes.get(index)?;
    if fence_char != b'`' && fence_char != b'~' {
        return None;
    }
    let mut fence_len = 0usize;
    while bytes.get(index + fence_len) == Some(&fence_char) {
        fence_len += 1;
    }
    if fence_len < 3 {
        return None;
    }
    index += fence_len;
    // Optional spaces/tabs, then the first info word.
    while matches!(bytes.get(index), Some(b' ' | b'\t')) {
        index += 1;
    }
    let word_start = index;
    while bytes
        .get(index)
        .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        index += 1;
    }
    (word_start < index).then(|| &line[word_start..index])
}

/// Bounded cache for the built-in transform (R3.1.2): at most
/// [`MERMAID_CACHE_MAX_ENTRIES`] entries and [`MERMAID_CACHE_MAX_BYTES`]
/// retained source/result bytes. Streaming replaces the current generation;
/// older deltas are evicted from the front.
const MERMAID_CACHE_MAX_ENTRIES: usize = 8;
const MERMAID_CACHE_MAX_BYTES: usize = 512 * 1024;

#[derive(PartialEq, Eq)]
struct MermaidTransformKey {
    source: String,
    width: usize,
    message_type: String,
    is_streaming: bool,
    mode: MermaidRenderingMode,
    theme: u64,
    capability_generation: u64,
}

/// Borrowed lookup form of [`MermaidTransformKey`] (avoids cloning the
/// source on a cache hit).
struct MermaidTransformKeyRef<'a> {
    source: &'a str,
    width: usize,
    message_type: &'a str,
    is_streaming: bool,
    mode: MermaidRenderingMode,
    theme: u64,
    capability_generation: u64,
}

struct MermaidTransformCache {
    entries: VecDeque<(MermaidTransformKey, Arc<str>)>,
    bytes: usize,
}

impl MermaidTransformCache {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
        }
    }

    fn get(&mut self, key: &MermaidTransformKeyRef<'_>) -> Option<Arc<str>> {
        let position = self.entries.iter().position(|(cached, _)| {
            cached.source == key.source
                && cached.width == key.width
                && cached.message_type == key.message_type
                && cached.is_streaming == key.is_streaming
                && cached.mode == key.mode
                && cached.theme == key.theme
                && cached.capability_generation == key.capability_generation
        })?;
        let entry = self.entries.remove(position)?;
        let value = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(value)
    }

    fn insert(&mut self, key: MermaidTransformKey, value: Arc<str>) {
        let retained = key.source.len() + value.len();
        // A single entry larger than the whole budget cannot satisfy it;
        // leave it uncached (the next render re-parses it).
        if retained > MERMAID_CACHE_MAX_BYTES {
            return;
        }
        self.bytes += retained;
        self.entries.push_back((key, value));
        while self.entries.len() > MERMAID_CACHE_MAX_ENTRIES
            || (self.bytes > MERMAID_CACHE_MAX_BYTES && self.entries.len() > 1)
        {
            if let Some((evicted_key, evicted_value)) = self.entries.pop_front() {
                self.bytes = self
                    .bytes
                    .saturating_sub(evicted_key.source.len() + evicted_value.len());
            }
        }
    }
}

/// Fingerprint of the theme-derived styles captured by the transformer.
/// The cache is per transformer instance, but the fingerprint keeps the key
/// self-describing if the closure is ever reused across theme instances.
fn styles_fingerprint(styles: &MermaidStyles) -> u64 {
    use std::hash::{Hash, Hasher};
    fn hash_color(color: Option<Color>, hasher: &mut impl Hasher) {
        match color {
            None => 0u8.hash(hasher),
            Some(Color::Rgb(r, g, b)) => {
                1u8.hash(hasher);
                r.hash(hasher);
                g.hash(hasher);
                b.hash(hasher);
            }
            Some(Color::Ansi256(index)) => {
                2u8.hash(hasher);
                index.hash(hasher);
            }
        }
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for style in [
        &styles.border,
        &styles.node_text,
        &styles.edge,
        &styles.edge_label,
        &styles.title,
    ] {
        hash_color(style.fg, &mut hasher);
        hash_color(style.bg, &mut hasher);
        style.bold.hash(&mut hasher);
        style.italic.hash(&mut hasher);
    }
    hasher.finish()
}

/// comrak options matching the markdown component's parser view
/// (markdown.rs `parser_options`): extensions the source may rely on parse
/// the same way they do at render time.
fn parser_options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.tasklist = true;
    options.extension.autolink = true;
    options
}

/// `isMermaid` (mermaid.ts:14-16): fenced code block whose info string's
/// first word is `mermaid` (case-insensitive).
fn is_mermaid(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("mermaid"))
}

/// `codeSpan` (mermaid.ts:18-36): encode each diagram row as inline code so
/// Markdown preserves its spacing and box-drawing characters. Blank rows
/// use a non-breaking space (an empty code span has no visible height);
/// the backtick fence is one longer than the longest backtick run in the
/// content, and a leading/trailing backtick is separated from the fence by
/// a space (CommonMark strips that padding when rendering).
fn code_span(line: &str) -> String {
    let content = if line.is_empty() { "\u{00a0}" } else { line };
    let mut longest_backtick_run = 0usize;
    let mut run = 0usize;
    for c in content.chars() {
        if c == '`' {
            run += 1;
            longest_backtick_run = longest_backtick_run.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(longest_backtick_run + 1);
    let padding = if content.starts_with('`') || content.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{padding}{content}{padding}{fence}")
}

/// Theme colour for a key, parsed back from the theme's pre-computed ANSI
/// prefix (themes.rs `fg_ansi`): truecolor `38;2;r;g;b`, 256-colour
/// `38;5;n`, or none for an unknown/empty colour (`\x1b[39m` is the
/// default-colour reset, not a colour).
fn theme_fg(theme: &Theme, key: &str) -> Option<Color> {
    let ansi = theme.get_fg_ansi(key);
    if let Some(rest) = ansi.strip_prefix("\x1b[38;2;") {
        let body = rest.strip_suffix('m')?;
        let mut parts = body.split(';');
        let r = parts.next()?.parse().ok()?;
        let g = parts.next()?.parse().ok()?;
        let b = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        return Some(Color::Rgb(r, g, b));
    }
    if let Some(rest) = ansi.strip_prefix("\x1b[38;5;") {
        return rest.strip_suffix('m')?.parse().ok().map(Color::Ansi256);
    }
    None
}

/// The five diagram styles mapped from theme keys (`styleSpan`,
/// mermaid.ts:38-53): border → `borderMuted`, text → `text`,
/// edge → `accent`, edgeLabel → `muted`, title → `accent` + bold.
fn mermaid_styles(theme: &Theme) -> MermaidStyles {
    MermaidStyles {
        border: Style {
            fg: theme_fg(theme, "borderMuted"),
            bg: None,
            bold: false,
            italic: false,
        },
        node_text: Style {
            fg: theme_fg(theme, "text"),
            bg: None,
            bold: false,
            italic: false,
        },
        edge: Style {
            fg: theme_fg(theme, "accent"),
            bg: None,
            bold: false,
            italic: false,
        },
        edge_label: Style {
            fg: theme_fg(theme, "muted"),
            bg: None,
            bold: false,
            italic: false,
        },
        title: Style {
            fg: theme_fg(theme, "accent"),
            bg: None,
            bold: true,
            italic: false,
        },
    }
}

/// Neutral styles (no theme): default terminal colours
/// (mermaid.ts:83 `art.plain`).
fn neutral_styles() -> MermaidStyles {
    MermaidStyles {
        border: Style::default(),
        node_text: Style::default(),
        edge: Style::default(),
        edge_label: Style::default(),
        title: Style::default(),
    }
}

/// Diagram width in display columns, the counterpart of grok-mermaid's
/// `art.width` (mermaid.ts:76). The rpi-tui `MermaidArt` carries no width,
/// so it is recomputed from the plain lines.
fn art_width(art: &MermaidArt) -> usize {
    art.plain_lines
        .iter()
        .map(|line| visible_width(line))
        .max()
        .unwrap_or(0)
}

/// The source text covered by the given 1-based, inclusive line range
/// (`byte_offset_of` / `node_slice` from markdown.rs; duplicated because
/// those helpers are private to rpi-tui).
fn line_span(source: &str, start_line: usize, end_line: usize) -> (usize, usize) {
    let mut offset = 0usize;
    let mut start = None;
    let mut end = source.len();
    for (index, part) in source.split_inclusive('\n').enumerate() {
        let line = index + 1;
        if line == start_line {
            start = Some(offset);
        }
        offset += part.len();
        if line == end_line {
            // `part` is the block's last line; consuming it includes its
            // trailing newline, like Marked's `token.raw`.
            end = offset;
            break;
        }
    }
    (start.unwrap_or(end), end)
}

#[cfg(test)]
thread_local! {
    /// Counts comrak parses performed by the built-in Mermaid transformer
    /// (R3.1.1 counting invariant: no candidate must mean zero parses).
    static MERMAID_PARSE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `createMermaidMarkdownTransformer` (mermaid.ts:60-89).
pub fn create_mermaid_markdown_transformer(
    get_mode: impl Fn() -> MermaidRenderingMode + Send + Sync + 'static,
    theme: Option<Arc<Theme>>,
) -> MarkdownTransformerFn {
    let styles = theme
        .as_deref()
        .map(mermaid_styles)
        .unwrap_or_else(neutral_styles);
    let theme_fingerprint = styles_fingerprint(&styles);
    let cache = Mutex::new(MermaidTransformCache::new());
    Arc::new(move |markdown: String, context: MarkdownTransformContext| {
        let mode = get_mode();
        if mode == MermaidRenderingMode::Off
            || !matches!(
                context.message_type.as_str(),
                "user" | "assistant" | "summary"
            )
            || (context.is_streaming && mode != MermaidRenderingMode::Streaming)
        {
            return markdown;
        }
        // Fast reject (R3.1.1): no candidate fence means no comrak parse.
        if !contains_mermaid_candidate(&markdown) {
            return markdown;
        }

        // Bounded transform cache (R3.1.2). The capability generation covers
        // clickable-label hyperlink changes; the theme fingerprint covers the
        // styles captured above; the mode/streaming/message-type/width are
        // part of the key. Third-party transformers are not cached here.
        let capability_generation = rpi_tui::terminal_image::capabilities_generation();
        if let Some(hit) = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&MermaidTransformKeyRef {
                source: &markdown,
                width: context.available_width,
                message_type: &context.message_type,
                is_streaming: context.is_streaming,
                mode,
                theme: theme_fingerprint,
                capability_generation,
            })
        {
            return hit.to_string();
        }

        let arena = Arena::new();
        #[cfg(test)]
        MERMAID_PARSE_COUNT.with(|count| count.set(count.get() + 1));
        let root = parse_document(&arena, &markdown, &parser_options());

        // (start_line, end_line, replacement) for each mermaid block, in
        // document order. Blocks that render nothing or exceed the
        // available width keep their raw source (mermaid.ts:75-76).
        let mut replacements: Vec<(usize, usize, String)> = Vec::new();
        for node in root.descendants() {
            let data = node.data.borrow();
            if !matches!(data.value, NodeValue::CodeBlock(_)) {
                continue;
            }
            let sp = data.sourcepos;
            if sp.start.line == 0 || sp.end.line < sp.start.line {
                continue;
            }
            let NodeValue::CodeBlock(code_block) = &data.value else {
                continue;
            };
            if !is_mermaid(&code_block.info) {
                continue;
            }
            let Some(art) = render(&code_block.literal, &styles, None) else {
                continue;
            };
            if art_width(&art) > context.available_width {
                continue;
            }
            let lines: Vec<String> = if theme.is_some() {
                art.styled_lines
            } else {
                art.plain_lines
            };
            // Markdown hard breaks keep every diagram row on its own line
            // (mermaid.ts:84-85).
            let mut replacement = lines
                .iter()
                .map(|line| code_span(line))
                .collect::<Vec<_>>()
                .join("  \n");
            replacement.push('\n');
            replacements.push((sp.start.line, sp.end.line, replacement));
        }

        if replacements.is_empty() {
            cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    MermaidTransformKey {
                        source: markdown.clone(),
                        width: context.available_width,
                        message_type: context.message_type,
                        is_streaming: context.is_streaming,
                        mode,
                        theme: theme_fingerprint,
                        capability_generation,
                    },
                    Arc::from(markdown.as_str()),
                );
            return markdown;
        }

        // Splice bottom-up so earlier spans' offsets stay valid.
        let mut out = markdown.clone();
        for (start_line, end_line, replacement) in replacements.into_iter().rev() {
            let (start, end) = line_span(&out, start_line, end_line);
            out.replace_range(start..end, &replacement);
        }
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                MermaidTransformKey {
                    source: markdown,
                    width: context.available_width,
                    message_type: context.message_type,
                    is_streaming: context.is_streaming,
                    mode,
                    theme: theme_fingerprint,
                    capability_generation,
                },
                Arc::from(out.as_str()),
            );
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;

    const FLOWCHART: &str = "flowchart TD\n  A[Start] --> B{Is it ready?}\n  B -->|Yes| C[Ship it]\n  B -->|No| D[Fix bugs]\n  D --> B";

    fn markdown_with_mermaid() -> String {
        format!("intro paragraph\n\n```mermaid\n{FLOWCHART}\n```\n\nafter paragraph\n")
    }

    /// Bind the streaming context and apply one transform call.
    fn apply(
        transformer: &MarkdownTransformerFn,
        markdown: String,
        is_streaming: bool,
        message_type: &str,
        available_width: usize,
    ) -> String {
        transformer(
            markdown,
            MarkdownTransformContext {
                message_type: message_type.to_string(),
                is_streaming,
                available_width,
                ..Default::default()
            },
        )
    }

    #[test]
    fn mode_off_returns_markdown_unchanged() {
        // mermaid.ts:63-69: mode "off" short-circuits before any parsing.
        let markdown = markdown_with_mermaid();
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Off, None);
        assert_eq!(
            apply(&f, markdown.clone(), false, "assistant", 100),
            markdown
        );
    }

    #[test]
    fn assistant_thinking_returns_markdown_unchanged() {
        // mermaid.ts:63-69: thinking blocks are never transformed.
        let markdown = markdown_with_mermaid();
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Streaming, None);
        assert_eq!(
            apply(&f, markdown.clone(), false, "assistant-thinking", 100),
            markdown
        );
    }

    #[test]
    fn final_mode_skips_while_streaming() {
        // mermaid.ts:63-69: `isStreaming && mode !== "streaming"`.
        let markdown = markdown_with_mermaid();
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Final, None);
        assert_eq!(
            apply(&f, markdown.clone(), true, "assistant", 100),
            markdown
        );
    }

    #[test]
    fn streaming_mode_transforms_while_streaming() {
        // mode "streaming" renders as the source streams.
        let markdown = markdown_with_mermaid();
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Streaming, None);
        let out = apply(&f, markdown.clone(), true, "assistant", 100);
        assert_ne!(out, markdown);
        assert!(!out.contains("```mermaid"));
        // The diagram rows arrive as inline code spans joined by hard
        // breaks (mermaid.ts:85).
        assert!(out.contains('┌'));
        assert!(out.contains("  \n"));
    }

    #[test]
    fn final_mode_transforms_when_not_streaming() {
        let markdown = markdown_with_mermaid();
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Final, None);
        let out = apply(&f, markdown.clone(), false, "assistant", 100);
        assert_ne!(out, markdown);
        assert!(!out.contains("```mermaid"));
        // Surrounding markdown survives byte-identically.
        assert!(out.starts_with("intro paragraph\n\n"));
        assert!(out.ends_with("\n\nafter paragraph\n"));
    }

    #[test]
    fn width_overflow_keeps_raw_block() {
        // mermaid.ts:76: `art.width > availableWidth` keeps the raw fenced
        // source.
        let markdown = markdown_with_mermaid();
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Streaming, None);
        assert_eq!(apply(&f, markdown.clone(), false, "assistant", 4), markdown);
    }

    #[test]
    fn non_mermaid_content_is_untouched() {
        // Non-mermaid code blocks and prose pass through unchanged
        // (mermaid.ts:73-74).
        let markdown = "# Title\n\n```rust\nfn main() {}\n```\n\ntext\n".to_string();
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Streaming, None);
        assert_eq!(
            apply(&f, markdown.clone(), false, "assistant", 100),
            markdown
        );
    }

    #[test]
    fn info_string_first_word_matches_case_insensitively() {
        // mermaid.ts:15: `lang.trim().split(/\s+/, 1)[0].toLowerCase()`.
        let markdown = "```MERMAID extra info\ngraph TD\n  A --> B\n```\n".to_string();
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Streaming, None);
        let out = apply(&f, markdown.clone(), false, "assistant", 100);
        assert_ne!(out, markdown);
        assert!(!out.contains("```"));
    }

    #[test]
    fn code_span_pads_backticks_and_blank_lines() {
        // mermaid.ts:20-36.
        assert_eq!(code_span("plain"), "`plain`");
        assert_eq!(code_span(""), "`\u{00a0}`");
        assert_eq!(code_span("`edge`"), "`` `edge` ``");
        assert_eq!(code_span("``two``"), "``` ``two`` ```");
    }

    #[test]
    fn themed_transform_emits_ansi_styled_lines() {
        // mermaid.ts:83: with a theme the styled spans are used; the
        // rpi-tui renderer pre-serializes them to ANSI.
        let theme = Arc::new(load_theme("dark", None).expect("builtin dark theme"));
        let markdown = markdown_with_mermaid();
        let f = create_mermaid_markdown_transformer(
            move || MermaidRenderingMode::Streaming,
            Some(theme),
        );
        let out = apply(&f, markdown.clone(), false, "assistant", 100);
        assert_ne!(out, markdown);
        assert!(out.contains("\x1b[38;2;"), "expected truecolor ANSI");
    }

    #[test]
    fn styles_follow_theme_keys() {
        // styleSpan (mermaid.ts:38-53): border/borderMuted, text/text,
        // edge/accent, edgeLabel/muted, title/accent+bold.
        let theme = load_theme("dark", None).expect("builtin dark theme");
        let styles = mermaid_styles(&theme);
        assert_eq!(styles.border.fg, theme_fg(&theme, "borderMuted"));
        assert_eq!(styles.node_text.fg, theme_fg(&theme, "text"));
        assert_eq!(styles.edge.fg, theme_fg(&theme, "accent"));
        assert_eq!(styles.edge_label.fg, theme_fg(&theme, "muted"));
        assert_eq!(styles.title.fg, theme_fg(&theme, "accent"));
        assert!(styles.title.bold);
        assert!(!styles.node_text.bold);
    }

    #[test]
    fn multiple_mermaid_blocks_all_replace() {
        let markdown = "```mermaid\ngraph TD\n  A --> B\n```\n\nmid\n\n```mermaid\nstateDiagram-v2\n  [*] --> S\n```\n".to_string();
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Streaming, None);
        let out = apply(&f, markdown, false, "assistant", 100);
        assert!(!out.contains("```mermaid"));
        assert!(out.contains("mid"));
        assert!(out.contains('┌') && out.contains('╭'));
    }

    // ------------------------------------------------------------------
    // V19-04 FR-A: candidate scan, zero-parse fast reject and bounded cache.
    // ------------------------------------------------------------------

    #[test]
    fn candidate_scan_covers_fence_forms() {
        // R3.1.1 matrix: backtick/tilde, 3+/4+ fence characters, optional
        // spaces before the first info word, 0-3 spaces indentation, and
        // container prefixes (`> `, list markers).
        for source in [
            "```mermaid\ngraph TD\n```",
            "````mermaid\ngraph TD\n````",
            "~~~mermaid\ngraph TD\n~~~",
            "~~~~mermaid\ngraph TD\n~~~~",
            "```   mermaid extra\ngraph TD\n```",
            "```MERMAID\ngraph TD\n```",
            "   ```mermaid\ngraph TD\n```",
            "> ```mermaid\n> graph TD\n> ```",
            "> > ```mermaid\ngraph TD\n```",
            "- ```mermaid\ngraph TD\n```",
            "1. ```mermaid\ngraph TD\n```",
            "  - ~~~ mermaid\ngraph TD\n~~~",
            "paragraph\n\n```mermaid\ngraph TD\n```\n",
            ">\t```mermaid\ngraph TD\n```",
            "-\t~~~mermaid\ngraph TD\n~~~",
            "- a\n  - b\n\n    ```mermaid\ngraph TD\n```",
        ] {
            assert!(
                contains_mermaid_candidate(source),
                "must detect a Mermaid candidate in {source:?}"
            );
        }
        // Four-space indentation is an indented code block, never a fence for
        // the precise scanner. The conservative substring fallback may still
        // classify it as a candidate (it then parses and returns the source
        // unchanged), so the precise scanner is asserted directly.
        for line in [
            "    ```mermaid",
            "```rust",
            "```",
            "```mermaidx",
            "```mermaidish",
        ] {
            assert!(
                !fence_info_word(line).is_some_and(|word| word.eq_ignore_ascii_case("mermaid")),
                "precise scanner must not treat {line:?} as a Mermaid fence"
            );
        }
        // No word occurrence: the fallback does not fire either.
        for source in [
            "```rust\nfn main() {}\n```",
            "```\nplain code\n```",
            "plain prose without the word",
        ] {
            assert!(
                !contains_mermaid_candidate(source),
                "must not treat {source:?} as a Mermaid candidate"
            );
        }
        // Conservative fallback: a word occurrence without a valid fence is
        // still a candidate (false positives only run the normal parse).
        assert!(contains_mermaid_candidate(
            "    ```mermaid\ngraph TD\n    ```"
        ));
    }

    #[test]
    fn prose_mentioning_mermaid_is_a_false_positive_but_stays_unchanged() {
        MERMAID_PARSE_COUNT.with(|count| count.set(0));
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Streaming, None);
        let prose = "plain prose mentioning mermaid without a fence\n".to_string();
        assert_eq!(apply(&f, prose.clone(), false, "assistant", 100), prose);
        // The fallback candidate scan runs the normal parse; it must not
        // rewrite prose.
        assert_eq!(MERMAID_PARSE_COUNT.with(|count| count.get()), 1);
    }

    #[test]
    fn no_candidate_never_parses() {
        MERMAID_PARSE_COUNT.with(|count| count.set(0));
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Streaming, None);
        let prose = "# Title\n\nordinary **markdown** without diagrams\n".to_string();
        assert_eq!(apply(&f, prose.clone(), true, "assistant", 100), prose);
        assert_eq!(MERMAID_PARSE_COUNT.with(|count| count.get()), 0);
    }

    #[test]
    fn candidate_parses_once_and_cache_hits_do_not_parse() {
        MERMAID_PARSE_COUNT.with(|count| count.set(0));
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Streaming, None);
        let markdown = markdown_with_mermaid();
        let first = apply(&f, markdown.clone(), false, "assistant", 100);
        assert_eq!(MERMAID_PARSE_COUNT.with(|count| count.get()), 1);
        // Same key: cached, no parse.
        assert_eq!(apply(&f, markdown.clone(), false, "assistant", 100), first);
        assert_eq!(MERMAID_PARSE_COUNT.with(|count| count.get()), 1);
        // A different width is a different key.
        let _ = apply(&f, markdown.clone(), false, "assistant", 90);
        assert_eq!(MERMAID_PARSE_COUNT.with(|count| count.get()), 2);
        // Streaming state and mode are part of the key.
        let _ = apply(&f, markdown.clone(), true, "assistant", 90);
        assert_eq!(MERMAID_PARSE_COUNT.with(|count| count.get()), 3);
    }

    #[test]
    fn cache_is_bounded_and_evicts_old_generations() {
        let mut cache = MermaidTransformCache::new();
        for index in 0..(MERMAID_CACHE_MAX_ENTRIES * 3) {
            let source = format!("```mermaid\ngraph TD\n  A{index} --> B\n```");
            cache.insert(
                MermaidTransformKey {
                    source: source.clone(),
                    width: 100,
                    message_type: "assistant".to_string(),
                    is_streaming: true,
                    mode: MermaidRenderingMode::Streaming,
                    theme: 0,
                    capability_generation: 0,
                },
                Arc::from(source.as_str()),
            );
        }
        assert!(cache.entries.len() <= MERMAID_CACHE_MAX_ENTRIES);
        assert!(cache.bytes <= MERMAID_CACHE_MAX_BYTES);

        // A single source larger than the whole budget is not retained.
        let large = format!("```mermaid\n{}\n```", "x".repeat(MERMAID_CACHE_MAX_BYTES));
        let before = cache.entries.len();
        cache.insert(
            MermaidTransformKey {
                source: large.clone(),
                width: 100,
                message_type: "assistant".to_string(),
                is_streaming: true,
                mode: MermaidRenderingMode::Streaming,
                theme: 0,
                capability_generation: 0,
            },
            Arc::from(large.as_str()),
        );
        assert_eq!(cache.entries.len(), before);
        assert!(cache.bytes <= MERMAID_CACHE_MAX_BYTES);
    }

    #[test]
    fn cache_key_includes_capability_generation() {
        // R3.1.2: clickable-label output depends on the hyperlink capability,
        // which is represented by the capabilities generation. The renderer's
        // own test (`click_links_only_labels_when_hyperlinks_are_enabled` in
        // rpi-tui) covers the output switch; here the cache must not serve a
        // stale generation.
        let mut cache = MermaidTransformCache::new();
        let source = "```mermaid\nflowchart LR\n  A --> B\n```";
        cache.insert(
            MermaidTransformKey {
                source: source.to_string(),
                width: 100,
                message_type: "assistant".to_string(),
                is_streaming: false,
                mode: MermaidRenderingMode::Streaming,
                theme: 7,
                capability_generation: 1,
            },
            Arc::from("cached"),
        );
        let key = |capability_generation| MermaidTransformKeyRef {
            source,
            width: 100,
            message_type: "assistant",
            is_streaming: false,
            mode: MermaidRenderingMode::Streaming,
            theme: 7,
            capability_generation,
        };
        assert!(cache.get(&key(1)).is_some());
        assert!(cache.get(&key(2)).is_none());
    }

    #[test]
    fn same_transformer_instance_transforms_after_streaming_ends() {
        // The streaming flag is read per call, so one transformer instance
        // must switch from "keep raw" to "render" when the stream ends.
        let markdown = markdown_with_mermaid();
        let f = create_mermaid_markdown_transformer(move || MermaidRenderingMode::Final, None);
        assert_eq!(
            apply(&f, markdown.clone(), true, "assistant", 100),
            markdown
        );
        let rendered = apply(&f, markdown.clone(), false, "assistant", 100);
        assert_ne!(rendered, markdown);
        assert!(!rendered.contains("```mermaid"));
    }

    #[test]
    fn theme_instances_do_not_share_the_transform_cache() {
        let dark = Arc::new(load_theme("dark", None).expect("builtin dark theme"));
        let light = Arc::new(load_theme("light", None).expect("builtin light theme"));
        let markdown = markdown_with_mermaid();
        let dark_transformer = create_mermaid_markdown_transformer(
            move || MermaidRenderingMode::Streaming,
            Some(dark),
        );
        let light_transformer = create_mermaid_markdown_transformer(
            move || MermaidRenderingMode::Streaming,
            Some(light),
        );
        let dark_out = apply(&dark_transformer, markdown.clone(), false, "assistant", 100);
        let light_out = apply(
            &light_transformer,
            markdown.clone(),
            false,
            "assistant",
            100,
        );
        assert_ne!(dark_out, light_out, "styled output must follow the theme");
        // The same instance still hits its own cache.
        assert_eq!(
            apply(&dark_transformer, markdown, false, "assistant", 100),
            dark_out
        );
    }
}
