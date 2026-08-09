//! Write tool renderer — port of the `renderCall`/`renderResult` hooks in
//! `packages/coding-agent/src/core/tools/write.ts` (formatWriteCall :136-167,
//! formatWriteResult :169-184, highlight cache :47-126, hooks :232-266)
//! @ pi 0.82.1 (2efa728) (T17).
//!
//! Intentional differences:
//! - Upstream keeps the per-call highlight cache on the call component
//!   instance (`WriteCallRenderComponent.cache`, write.ts:55-61) and reuses
//!   the component via `context.lastComponent`; here the cache lives in the
//!   component's [`RendererStateSlot`] as [`WriteRenderState`] and the visual
//!   component is rebuilt from it on every update (same pattern as the
//!   bash/edit renderers, T17) — the rendered bytes are identical.
//! - `highlightSingleLine` (write.ts:65-68) calls the whole-text
//!   [`highlight_code`] with a newline-free line and takes its first
//!   element: `split("\n")` of a newline-free string yields exactly one
//!   element, so this is equivalent to upstream `highlightCode(line,
//!   lang)[0] ?? ""`.
//! - `renderResult` always returns a component — an empty [`Container`] on
//!   success — because `None` would fall through to the generic fallback
//!   and print `Successfully wrote N bytes` (upstream write/edit show no
//!   result line on success).

use std::sync::Mutex;

use rpi_tui::components::text::Text;
use rpi_tui::tui::{Component, Container};
use serde_json::Value;

use super::render_utils::{normalize_display_text, render_tool_path, replace_tabs, str_value};
use crate::core::highlight::{get_language_from_path, highlight_code};
use crate::core::themes::Theme;
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::components::tool_execution::{
    lock_recover, RenderShell, ResultRenderOptions, ToolDefinition, ToolRenderContext,
    ToolResultState,
};

/// `WRITE_PARTIAL_FULL_HIGHLIGHT_LINES` (write.ts:63): the streaming prefix
/// re-highlighted in full after every incremental update.
const WRITE_PARTIAL_FULL_HIGHLIGHT_LINES: usize = 50;

/// `maxLines` for the collapsed call preview (write.ts:157).
const WRITE_PREVIEW_LINES: usize = 10;

/// `WriteHighlightCache` (write.ts:47-53): per tool call, carried by the
/// [`RendererStateSlot`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteHighlightCache {
    /// `rawPath` — the `file_path ?? path` arg; `None` only when the arg is a
    /// non-string (invalid).
    raw_path: Option<String>,
    /// `lang` — the language resolved from `rawPath`; the cache only exists
    /// when a language is recognized (write.ts:81-83).
    lang: &'static str,
    /// `rawContent` — the raw (un-normalized) content the cache covers.
    raw_content: String,
    /// `normalizedLines` — tab-replaced, CR-stripped lines of `rawContent`.
    normalized_lines: Vec<String>,
    /// `highlightedLines` — one styled line per `normalizedLines` entry.
    highlighted_lines: Vec<String>,
}

/// `WriteRenderState` (write.ts:55-61 `WriteCallRenderComponent.cache`): the
/// highlight cache, carried by the [`RendererStateSlot`]. The call component
/// itself is rebuilt from this state on every update (T17).
#[derive(Default)]
pub struct WriteRenderState {
    cache: Mutex<Option<WriteHighlightCache>>,
}

/// `highlightSingleLine` (write.ts:65-68): `highlightCode(line, lang)[0] ?? ""`.
/// [`highlight_code`] is a whole-text API; a newline-free single line splits
/// into exactly one element, so taking the first is equivalent — see the
/// module doc.
fn highlight_single_line(line: &str, lang: &str, theme: &Theme) -> String {
    highlight_code(line, Some(lang), theme)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// `refreshWriteHighlightPrefix` (write.ts:70-79): re-highlight the first
/// [`WRITE_PARTIAL_FULL_HIGHLIGHT_LINES`] lines as one block so multi-line
/// constructs that streaming per-line highlighting misses are corrected.
fn refresh_write_highlight_prefix(cache: &mut WriteHighlightCache, theme: &Theme) {
    let prefix_count = WRITE_PARTIAL_FULL_HIGHLIGHT_LINES.min(cache.normalized_lines.len());
    if prefix_count == 0 {
        return;
    }
    let prefix_source = cache.normalized_lines[..prefix_count].join("\n");
    let prefix_highlighted = highlight_code(&prefix_source, Some(cache.lang), theme);
    for i in 0..prefix_count {
        // `prefixHighlighted[i] ?? highlightSingleLine(...)` (write.ts:76-78).
        cache.highlighted_lines[i] = prefix_highlighted.get(i).cloned().unwrap_or_else(|| {
            highlight_single_line(&cache.normalized_lines[i], cache.lang, theme)
        });
    }
}

/// `rebuildWriteHighlightCacheFull` (write.ts:81-93): highlight the whole
/// content from scratch; `None` when the path has no recognized language.
fn rebuild_write_highlight_cache_full(
    raw_path: Option<String>,
    file_content: &str,
    theme: &Theme,
) -> Option<WriteHighlightCache> {
    // `rawPath ? getLanguageFromPath(rawPath) : undefined; if (!lang) return
    // undefined` (write.ts:82-83) — an empty path resolves to no language.
    let lang = raw_path.as_deref().and_then(get_language_from_path)?;
    let display_content = normalize_display_text(file_content);
    let normalized = replace_tabs(&display_content);
    Some(WriteHighlightCache {
        raw_path,
        lang,
        raw_content: file_content.to_string(),
        normalized_lines: normalized.split('\n').map(str::to_string).collect(),
        highlighted_lines: highlight_code(&normalized, Some(lang), theme),
    })
}

/// `updateWriteHighlightCacheIncremental` (write.ts:95-126): keep the cache
/// in sync with a growing (streamed) content. Only the delta segment is
/// appended and highlighted; the first [`WRITE_PARTIAL_FULL_HIGHLIGHT_LINES`]
/// lines are then re-highlighted in full. A changed path/language or a
/// non-prefix content change falls back to a full rebuild.
fn update_write_highlight_cache_incremental(
    cache: Option<WriteHighlightCache>,
    raw_path: Option<String>,
    file_content: &str,
    theme: &Theme,
) -> Option<WriteHighlightCache> {
    let lang = raw_path.as_deref().and_then(get_language_from_path)?;
    let Some(mut cache) = cache else {
        return rebuild_write_highlight_cache_full(raw_path, file_content, theme);
    };
    if cache.lang != lang || cache.raw_path != raw_path {
        return rebuild_write_highlight_cache_full(raw_path, file_content, theme);
    }
    if !file_content.starts_with(&cache.raw_content) {
        return rebuild_write_highlight_cache_full(raw_path, file_content, theme);
    }
    if file_content.len() == cache.raw_content.len() {
        return Some(cache);
    }

    let delta_raw = &file_content[cache.raw_content.len()..];
    let delta_normalized = replace_tabs(&normalize_display_text(delta_raw));
    cache.raw_content = file_content.to_string();
    // write.ts:111-114: a fresh cache always has at least one (empty) line
    // from the split; the guard is kept for parity.
    if cache.normalized_lines.is_empty() {
        cache.normalized_lines.push(String::new());
        cache.highlighted_lines.push(String::new());
    }
    let segments: Vec<&str> = delta_normalized.split('\n').collect();
    // write.ts:116-119: the first delta segment continues the current last
    // line (the previous content was its prefix).
    let last_index = cache.normalized_lines.len() - 1;
    cache.normalized_lines[last_index].push_str(segments[0]);
    cache.highlighted_lines[last_index] =
        highlight_single_line(&cache.normalized_lines[last_index], cache.lang, theme);
    for segment in &segments[1..] {
        cache.normalized_lines.push((*segment).to_string());
        cache
            .highlighted_lines
            .push(highlight_single_line(segment, cache.lang, theme));
    }
    refresh_write_highlight_prefix(&mut cache, theme);
    Some(cache)
}

/// `trimTrailingEmptyLines` (write.ts:128-134).
fn trim_trailing_empty_lines(mut lines: Vec<String>) -> Vec<String> {
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// `str(args?.file_path ?? args?.path)` (write.ts:143): nullish coalescing —
/// `file_path` wins unless it is null/undefined; `str` maps strings to
/// themselves, null/undefined to `""`, anything else to `None` (invalid arg).
fn file_path_or_path(args: &Value) -> Option<String> {
    match args.get("file_path") {
        None | Some(Value::Null) => str_value(args.get("path")),
        Some(value) => str_value(Some(value)),
    }
}

/// `formatWriteCall` (write.ts:136-167).
fn format_write_call(
    args: &Value,
    expanded: bool,
    theme: &Theme,
    cache: Option<&WriteHighlightCache>,
    cwd: &str,
) -> String {
    let raw_path = file_path_or_path(args);
    let file_content = str_value(args.get("content"));
    let path_display = render_tool_path(raw_path.clone(), theme, cwd, None);
    let mut text = format!(
        "{} {path_display}",
        theme.fg("toolTitle", &Theme::bold("write"))
    );

    let Some(file_content) = file_content else {
        // write.ts:148-149: a non-string content is invalid.
        text.push_str(&format!(
            "\n\n{}",
            theme.fg("error", "[invalid content arg - expected string]")
        ));
        return text;
    };
    // write.ts:150: empty content is falsy — no preview.
    if !file_content.is_empty() {
        let lang = raw_path.as_deref().and_then(get_language_from_path);
        // `renderedLines` (write.ts:152-154): cached (or whole-text
        // highlighted) lines for a recognized language, else the normalized
        // raw lines.
        let rendered_lines: Vec<String> = if lang.is_some() {
            match cache {
                Some(cache) => cache.highlighted_lines.clone(),
                None => highlight_code(
                    &replace_tabs(&normalize_display_text(&file_content)),
                    lang,
                    theme,
                ),
            }
        } else {
            normalize_display_text(&file_content)
                .split('\n')
                .map(str::to_string)
                .collect()
        };
        let lines = trim_trailing_empty_lines(rendered_lines);
        let total_lines = lines.len();
        let max_lines = if expanded {
            lines.len()
        } else {
            WRITE_PREVIEW_LINES
        };
        let remaining = lines.len().saturating_sub(max_lines);
        // write.ts:160: highlighted lines pass through as-is; plain lines get
        // per-line `toolOutput` coloring with tab replacement.
        let body = lines
            .iter()
            .take(max_lines)
            .map(|line| {
                if lang.is_some() {
                    line.clone()
                } else {
                    theme.fg("toolOutput", &replace_tabs(line))
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        text.push_str(&format!("\n\n{body}"));
        if remaining > 0 {
            // write.ts:162: `... (N more lines, M total, <key> to expand)`.
            text.push_str(&format!(
                "{} {}{}",
                theme.fg(
                    "muted",
                    &format!("\n... ({remaining} more lines, {total_lines} total,")
                ),
                key_hint(theme, "app.tools.expand", "to expand"),
                theme.fg("muted", ")")
            ));
        }
    }
    text
}

/// `formatWriteResult` (write.ts:169-184): `None` on success (no result
/// line), the error text on error.
fn format_write_result(result: &ToolResultState, theme: &Theme, is_error: bool) -> Option<String> {
    if !is_error {
        return None;
    }
    // `result.content.filter(c => c.type === "text").map(c => c.text ||
    // "").join("\n")` (write.ts:176-179) — the raw join, without the
    // sanitize/strip-ANSI pass of `getTextOutput`.
    let output = result
        .content
        .iter()
        .filter(|c| c.kind == "text")
        .map(|c| c.text.as_deref().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    if output.is_empty() {
        return None;
    }
    Some(format!("\n{}", theme.fg("error", &output)))
}

/// The write tool's render definition (write.ts:186-268).
pub struct WriteToolRenderer;

impl ToolDefinition for WriteToolRenderer {
    fn render_call(
        &self,
        args: &Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        let state = context.state.get_or_init::<WriteRenderState>();
        let raw_path = file_path_or_path(args);
        let file_content = str_value(args.get("content"));
        let mut cache = lock_recover(&state.cache);
        // write.ts:238-244: a null content clears the cache; otherwise the
        // cache is rebuilt in full once the args are complete, and updated
        // incrementally while the content streams.
        if let Some(file_content) = file_content.as_deref() {
            if context.args_complete {
                *cache = rebuild_write_highlight_cache_full(raw_path, file_content, theme);
            } else {
                *cache = update_write_highlight_cache_incremental(
                    cache.take(),
                    raw_path,
                    file_content,
                    theme,
                );
            }
        } else {
            *cache = None;
        }
        Some(Box::new(Text::new(
            format_write_call(args, context.expanded, theme, cache.as_ref(), &context.cwd),
            0,
            0,
            None,
        )))
    }

    fn render_result(
        &self,
        result: &ToolResultState,
        _options: ResultRenderOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        // write.ts:256-266: success renders an empty container (a fresh one
        // per update, T17); the error text renders as a `Text`.
        match format_write_result(result, theme, context.is_error) {
            Some(output) => Some(Box::new(Text::new(output, 0, 0, None))),
            None => Some(Box::new(Container::new())),
        }
    }

    fn render_shell(&self) -> Option<RenderShell> {
        // No `renderShell` in the upstream definition → `undefined`.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::components::tool_execution::{
        RendererStateSlot, ToolResultContentLoose,
    };
    use rpi_tui::tui::RenderHandle;
    use serde_json::json;

    fn theme() -> Theme {
        load_theme("dark", None).expect("builtin dark theme")
    }

    fn context(state: &RendererStateSlot) -> ToolRenderContext {
        ToolRenderContext {
            args: json!({}),
            tool_call_id: "call_1".to_owned(),
            render_handle: RenderHandle::new(|| {}),
            state: state.clone(),
            cwd: "/cwd".to_owned(),
            execution_started: true,
            args_complete: true,
            is_partial: true,
            expanded: false,
            show_images: false,
            is_error: false,
        }
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' if chars.peek() == Some(&'[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == 'm' {
                            break;
                        }
                    }
                }
                // OSC 8 hyperlink (`ESC]8;;..ESC\`): strip it too, so exact
                // assertions are independent of the process-global terminal
                // capability cache that other tests mutate.
                '\u{1b}' if chars.peek() == Some(&']') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == '\u{1b}' {
                            chars.next(); // `\` of the `ESC\` terminator
                            break;
                        }
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }

    /// Render the call component and strip ANSI + per-line width padding.
    fn call_text(
        renderer: &WriteToolRenderer,
        args: &Value,
        theme: &Theme,
        ctx: &ToolRenderContext,
    ) -> String {
        let component = renderer
            .render_call(args, theme, ctx)
            .expect("call component");
        component
            .render(80)
            .iter()
            .map(|line| strip_ansi(line).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_shell_is_default() {
        // No `renderShell` in the upstream definition → `None` (undefined),
        // which the merge resolves to the default box.
        assert_eq!(WriteToolRenderer.render_shell(), None);
    }

    #[test]
    fn call_renders_title_and_path_variants() {
        let theme = theme();
        // Empty content: header only.
        let text = format_write_call(
            &json!({"path": "src/main.rs", "content": ""}),
            false,
            &theme,
            None,
            "/cwd",
        );
        assert_eq!(strip_ansi(&text), "write src/main.rs");
        // Missing content is `str(undefined)` → "" → same.
        let text = format_write_call(&json!({"path": "src/main.rs"}), false, &theme, None, "/cwd");
        assert_eq!(strip_ansi(&text), "write src/main.rs");
        // file_path wins over path.
        let text = format_write_call(
            &json!({"path": "a.rs", "file_path": "b.rs", "content": "x"}),
            false,
            &theme,
            None,
            "/cwd",
        );
        assert_eq!(strip_ansi(&text), "write b.rs\n\nx");
        // file_path null falls back to path.
        let text = format_write_call(
            &json!({"path": "a.rs", "file_path": null, "content": "x"}),
            false,
            &theme,
            None,
            "/cwd",
        );
        assert_eq!(strip_ansi(&text), "write a.rs\n\nx");
        // Missing path → muted `...`; non-string path → invalid-arg text.
        let missing = format_write_call(&json!({"content": "x"}), false, &theme, None, "/cwd");
        assert!(strip_ansi(&missing).contains("write ..."));
        let invalid = format_write_call(
            &json!({"path": 42, "content": "x"}),
            false,
            &theme,
            None,
            "/cwd",
        );
        assert!(strip_ansi(&invalid).contains("[invalid arg]"));
    }

    #[test]
    fn call_shows_invalid_content_arg_only_for_non_strings() {
        let theme = theme();
        // A non-string content is invalid (write.ts:148-149).
        let invalid = format_write_call(
            &json!({"path": "a.rs", "content": 42}),
            false,
            &theme,
            None,
            "/cwd",
        );
        let stripped = strip_ansi(&invalid);
        assert!(stripped.starts_with("write a.rs\n\n"));
        assert!(stripped.ends_with("[invalid content arg - expected string]"));
        // `str(null)` is "" → falsy, no error line.
        let null_content = format_write_call(
            &json!({"path": "a.rs", "content": null}),
            false,
            &theme,
            None,
            "/cwd",
        );
        assert_eq!(strip_ansi(&null_content), "write a.rs");
    }

    #[test]
    fn call_collapses_to_10_lines_with_verbatim_hint() {
        let theme = theme();
        let content = (1..=15)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format_write_call(
            &json!({"path": "notes.txt", "content": content}),
            false,
            &theme,
            None,
            "/cwd",
        );
        let stripped = strip_ansi(&text);
        assert!(stripped.contains("line1"));
        assert!(stripped.contains("line10"));
        assert!(!stripped.contains("line11"));
        assert!(stripped.contains("\n... (5 more lines, 15 total, ctrl+o to expand)"));
    }

    #[test]
    fn call_expanded_shows_all_lines_without_hint() {
        let theme = theme();
        let content = (1..=15)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format_write_call(
            &json!({"path": "notes.txt", "content": content}),
            true,
            &theme,
            None,
            "/cwd",
        );
        let stripped = strip_ansi(&text);
        assert!(stripped.contains("line15"));
        assert!(!stripped.contains("more lines"));
    }

    #[test]
    fn call_trims_trailing_empty_lines_and_replaces_tabs() {
        let theme = theme();
        // Trailing empty lines are trimmed; the total counts trimmed lines.
        let text = format_write_call(
            &json!({"path": "notes.txt", "content": "a\nb\n\n\n"}),
            false,
            &theme,
            None,
            "/cwd",
        );
        assert_eq!(strip_ansi(&text), "write notes.txt\n\na\nb");
        // 12 real lines + trailing empties → trimmed total of 12.
        let mut content = (1..=12)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        content.push_str("\n\n\n");
        let text = format_write_call(
            &json!({"path": "notes.txt", "content": content}),
            false,
            &theme,
            None,
            "/cwd",
        );
        assert!(strip_ansi(&text).contains("\n... (2 more lines, 12 total, ctrl+o to expand)"));
        // Tabs become three spaces (no lang: per-line toolOutput coloring).
        let tabbed = format_write_call(
            &json!({"path": "notes.txt", "content": "a\tb"}),
            false,
            &theme,
            None,
            "/cwd",
        );
        assert_eq!(strip_ansi(&tabbed), "write notes.txt\n\na   b");
        assert!(tabbed.contains(&theme.fg("toolOutput", "a   b")));
        // The trim helper itself.
        assert_eq!(
            trim_trailing_empty_lines(vec!["a".to_string(), "".to_string()]),
            vec!["a".to_string()]
        );
        assert!(trim_trailing_empty_lines(vec!["".to_string(), "".to_string()]).is_empty());
    }

    #[test]
    fn call_highlights_known_languages_and_strips_back_to_source() {
        let theme = theme();
        let content = "fn main() {\n    let x = 1;\n}";
        // Recognized language (src.rs): syntax-colored lines whose stripped
        // form equals the source verbatim.
        let text = format_write_call(
            &json!({"path": "src.rs", "content": content}),
            false,
            &theme,
            None,
            "/cwd",
        );
        assert_eq!(
            strip_ansi(&text),
            "write src.rs\n\nfn main() {\n    let x = 1;\n}"
        );
        assert!(
            text.contains(theme.get_fg_ansi("syntaxType")),
            "fn/let must be syntaxType"
        );
        assert!(
            text.contains(theme.get_fg_ansi("syntaxNumber")),
            "1 must be syntaxNumber"
        );
        // No recognized language (notes.txt): plain lines, toolOutput colored.
        let plain = format_write_call(
            &json!({"path": "notes.txt", "content": content}),
            false,
            &theme,
            None,
            "/cwd",
        );
        assert_eq!(
            strip_ansi(&plain),
            "write notes.txt\n\nfn main() {\n    let x = 1;\n}"
        );
        for line in content.split('\n') {
            assert!(
                plain.contains(&theme.fg("toolOutput", line)),
                "line must be toolOutput colored: {line}"
            );
        }
    }

    #[test]
    fn streaming_preview_grows_incrementally_and_clamps_at_10_lines() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = WriteToolRenderer;
        let mut ctx = context(&state);
        ctx.args_complete = false;

        // Feed the content one line at a time without argsComplete: the
        // incremental cache (write.ts:95-126) must track the preview line by
        // line while the collapsed height stays clamped at 10 lines.
        let mut content = String::new();
        for i in 1..=15 {
            content.push_str(&format!("line{i}\n"));
            ctx.args = json!({"path": "src.rs", "content": content});
            let stripped = call_text(&renderer, &ctx.args, &theme, &ctx);
            assert!(stripped.contains("write src.rs"), "title stays: {stripped}");
            let body_lines = stripped.lines().filter(|l| l.starts_with("line")).count();
            assert_eq!(body_lines, i.min(10), "step {i}: {stripped}");
            let shown: Vec<&str> = stripped.lines().collect();
            if i <= 10 {
                assert!(
                    shown.contains(&format!("line{i}").as_str()),
                    "step {i} must show the new line"
                );
            } else {
                assert!(
                    !shown.contains(&format!("line{i}").as_str()),
                    "step {i}: the new line must stay clamped"
                );
                assert!(
                    stripped.contains("more lines"),
                    "step {i}: the clamp hint must show"
                );
            }
        }

        // Final collapsed state: 10 lines + the verbatim expand hint.
        let stripped = call_text(&renderer, &ctx.args, &theme, &ctx);
        assert!(stripped.contains("\n... (5 more lines, 15 total, ctrl+o to expand)"));

        // The incremental cache converged on the full highlight: with < 50
        // lines the prefix refresh re-highlights everything, so the cache
        // matches a fresh rebuild line for line. The cache guard must drop
        // before the argsComplete render below re-locks the same mutex.
        {
            let render_state = state.get_or_init::<WriteRenderState>();
            let cache = lock_recover(&render_state.cache);
            let cache = cache.as_ref().expect("streaming cache");
            let fresh =
                rebuild_write_highlight_cache_full(Some("src.rs".to_string()), &content, &theme)
                    .expect("fresh cache");
            assert_eq!(cache.raw_content, fresh.raw_content);
            assert_eq!(cache.normalized_lines, fresh.normalized_lines);
            assert_eq!(cache.highlighted_lines, fresh.highlighted_lines);
        }

        // argsComplete switches to the full-rebuild path (write.ts:239-240)
        // without changing the rendered bytes.
        ctx.args_complete = true;
        let stripped_full = call_text(&renderer, &ctx.args, &theme, &ctx);
        assert_eq!(stripped_full, stripped);
    }

    #[test]
    fn incremental_cache_rebuild_conditions() {
        let theme = theme();
        // No recognized language → no cache (write.ts:82-83, 100-101).
        assert!(
            rebuild_write_highlight_cache_full(Some("notes.txt".to_string()), "x", &theme)
                .is_none()
        );
        assert!(update_write_highlight_cache_incremental(
            None,
            Some("notes.txt".to_string()),
            "x",
            &theme
        )
        .is_none());
        // An empty path resolves to no language either (`rawPath ? …`).
        assert!(
            update_write_highlight_cache_incremental(None, Some(String::new()), "x", &theme)
                .is_none()
        );

        // Prefix growth updates the cache in place.
        let cache =
            rebuild_write_highlight_cache_full(Some("src.rs".to_string()), "fn main() {\n", &theme)
                .expect("cache");
        let updated = update_write_highlight_cache_incremental(
            Some(cache),
            Some("src.rs".to_string()),
            "fn main() {\n    let x = 1;\n",
            &theme,
        )
        .expect("cache");
        assert_eq!(updated.raw_content, "fn main() {\n    let x = 1;\n");
        assert_eq!(
            updated.normalized_lines,
            vec!["fn main() {", "    let x = 1;", ""]
        );
        assert_eq!(
            updated.highlighted_lines.len(),
            updated.normalized_lines.len()
        );

        // Content unchanged → the cache is returned as-is.
        let unchanged = update_write_highlight_cache_incremental(
            Some(updated.clone()),
            Some("src.rs".to_string()),
            "fn main() {\n    let x = 1;\n",
            &theme,
        )
        .expect("cache");
        assert_eq!(unchanged, updated);

        // A different path (and language) rebuilds from scratch.
        let rebuilt = update_write_highlight_cache_incremental(
            Some(updated),
            Some("src.py".to_string()),
            "fn main() {\n    let x = 1;\n",
            &theme,
        )
        .expect("cache");
        assert_eq!(rebuilt.lang, "python");
        assert_eq!(rebuilt.raw_path.as_deref(), Some("src.py"));

        // Non-prefix growth rebuilds (an append would corrupt the line).
        let cache = rebuild_write_highlight_cache_full(Some("src.rs".to_string()), "a\nb", &theme)
            .expect("cache");
        let rebuilt = update_write_highlight_cache_incremental(
            Some(cache),
            Some("src.rs".to_string()),
            "a\nc",
            &theme,
        )
        .expect("cache");
        assert_eq!(rebuilt.normalized_lines, vec!["a", "c"]);
    }

    #[test]
    fn result_success_renders_empty_container() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = WriteToolRenderer;
        // The generic fallback would print this success text; the write
        // renderer must swallow it (write.ts:258-262).
        let result = ToolResultState {
            content: vec![ToolResultContentLoose::text(
                "Successfully wrote 42 bytes to /tmp/a.txt",
            )],
            is_error: false,
            details: None,
        };
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &context(&state),
            )
            .expect("result component");
        assert!(component.render(80).is_empty());
        assert!(format_write_result(&result, &theme, false).is_none());
    }

    #[test]
    fn result_error_renders_error_text() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = WriteToolRenderer;
        let error_text = "Could not write file: /tmp/a.txt. Error code: EACCES.";
        let result = ToolResultState {
            content: vec![ToolResultContentLoose::text(error_text)],
            is_error: true,
            details: None,
        };
        let mut ctx = context(&state);
        ctx.is_error = true;
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &ctx,
            )
            .expect("error component");
        let stripped = strip_ansi(&component.render(80).join("\n"));
        // The leading "\n" renders as a blank (space-padded) first line.
        let first_line = stripped.lines().next().unwrap_or_default();
        assert!(
            first_line.trim().is_empty(),
            "leading blank line before the error text: {first_line:?}"
        );
        assert!(stripped.contains(error_text));
        // Direct function contract (write.ts:169-184).
        assert_eq!(
            format_write_result(&result, &theme, true),
            Some(format!("\n{}", theme.fg("error", error_text)))
        );
        // No text content → no result line; success → no result line.
        let empty = ToolResultState {
            content: vec![],
            is_error: true,
            details: None,
        };
        assert!(format_write_result(&empty, &theme, true).is_none());
        assert!(format_write_result(&result, &theme, false).is_none());
    }
}
