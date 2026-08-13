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

use std::sync::Arc;

use comrak::nodes::NodeValue;
use comrak::{parse_document, Arena, Options};
use rpi_ext_host::types::{MarkdownTransformContext, MarkdownTransformerFn};
use rpi_tui::mermaid::{render, Color, MermaidArt, MermaidStyles, Style};
use rpi_tui::utils::visible_width;

use crate::core::settings_manager::MermaidRenderingMode;
use crate::core::themes::Theme;

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
            bold: false,
            italic: false,
        },
        node_text: Style {
            fg: theme_fg(theme, "text"),
            bold: false,
            italic: false,
        },
        edge: Style {
            fg: theme_fg(theme, "accent"),
            bold: false,
            italic: false,
        },
        edge_label: Style {
            fg: theme_fg(theme, "muted"),
            bold: false,
            italic: false,
        },
        title: Style {
            fg: theme_fg(theme, "accent"),
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

/// `createMermaidMarkdownTransformer` (mermaid.ts:60-89).
pub fn create_mermaid_markdown_transformer(
    get_mode: impl Fn() -> MermaidRenderingMode + Send + Sync + 'static,
    theme: Option<Arc<Theme>>,
) -> MarkdownTransformerFn {
    let styles = theme
        .as_deref()
        .map(mermaid_styles)
        .unwrap_or_else(neutral_styles);
    Arc::new(move |markdown: String, context: MarkdownTransformContext| {
        let mode = get_mode();
        if mode == MermaidRenderingMode::Off
            || context.message_type == "assistant-thinking"
            || (context.is_streaming && mode != MermaidRenderingMode::Streaming)
        {
            return markdown;
        }

        let arena = Arena::new();
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
            return markdown;
        }

        // Splice bottom-up so earlier spans' offsets stay valid.
        let mut out = markdown;
        for (start_line, end_line, replacement) in replacements.into_iter().rev() {
            let (start, end) = line_span(&out, start_line, end_line);
            out.replace_range(start..end, &replacement);
        }
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
}
