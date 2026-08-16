//! read tool renderer — port of the `renderCall`/`renderResult` hooks in
//! `packages/coding-agent/src/core/tools/read.ts` (formatReadLineRange :72-77,
//! formatReadCall :79-82, getCompactReadClassification :122-143,
//! formatCompactReadCall :145-167, formatReadResult :169-206, hooks :334-350)
//! @ pi 0.82.1 (2efa728) (T17).
//!
//! Intentional differences:
//! - Upstream reuses a shared `Text` via `context.lastComponent`; here the
//!   component is rebuilt on every update — the rendered bytes are identical.
//! - The renderer is stateless: the format helpers only read their inputs, so
//!   no `RendererStateSlot` state is needed (same as the grep renderer).
//! - `getPiDocsClassification` anchors at rpi's own package root
//!   (`config::get_package_dir`, config.ts:367-388 rename) instead of
//!   upstream's `getPackageDir()`.
//! - `resolveToCwd`/`resolvePath` come from `tools::path_utils` (same port,
//!   same behavior; `resolveToCwd` includes the `normalizeUnicodeSpaces` /
//!   `stripAtPrefix` options of path-utils.ts:48-50).
//! - Non-numeric `offset`/`limit` args are treated as absent instead of
//!   producing JS `NaN` arithmetic — unreachable through the read schema
//!   (TypeBox numbers).

use std::path::{Path, PathBuf};

use rpi_tui::components::text::Text;
use rpi_tui::tui::Component;
use serde_json::Value;

use super::render_utils::{render_tool_path, replace_tabs, resolve_to_cwd, str_value};
use crate::config::get_package_dir;
use crate::core::highlight::{get_language_from_path, highlight_code};
use crate::core::themes::Theme;
use crate::modes::interactive::components::keybinding_hints::{key_hint, key_text};
use crate::modes::interactive::components::tool_execution::{
    get_text_output, RenderShell, ResultRenderOptions, ToolDefinition, ToolRenderContext,
    ToolResultState,
};
use crate::tools::path_utils::resolve_path;
use crate::tools::truncate::{format_size, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

/// `COMPACT_RESOURCE_FILE_NAMES` (read.ts:42): file names classified as
/// "resource" in the collapsed call.
const COMPACT_RESOURCE_FILE_NAMES: &[&str] = &[
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

/// `ReadRenderArgs`-shaped path arg: `str(args?.file_path ?? args?.path)`
/// (read.ts:70). `None` = `str()` returned `null` (non-string arg / invalid).
fn read_render_path(args: &Value) -> Option<String> {
    match args.get("file_path") {
        // A present non-null `file_path` wins (empty string included — `""`
        // is not nullish for `??`).
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => str_value(args.get("path")),
        // `str(42)` → null (read.ts:70 renders it as an invalid arg).
        Some(_) => None,
    }
}

/// JS `Number#toString` for interpolated numbers: integral values print
/// without a decimal point (same helper as the bash renderer's).
fn format_js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// `formatReadLineRange` (read.ts:72-77): the `:start-end` suffix for the
/// `offset`/`limit` args. No suffix when both are absent; `limit` is
/// 1-based-inclusive, so `offset: 10, limit: 11` renders `:10-20`.
fn format_read_line_range(args: &Value, theme: &Theme) -> String {
    let offset = args.get("offset").and_then(Value::as_f64);
    let limit = args.get("limit").and_then(Value::as_f64);
    if offset.is_none() && limit.is_none() {
        return String::new();
    }
    // read.ts:74: `startLine = args.offset ?? 1`.
    let start_line = offset.unwrap_or(1.0);
    // read.ts:75: `endLine = startLine + args.limit - 1`, and the JS
    // truthiness of the computed end line decides the suffix (`0` is falsy,
    // so `offset: 1, limit: 0` renders `:1`).
    let end_line = match limit.map(|l| start_line + l - 1.0) {
        Some(v) if v != 0.0 => format_js_number(v),
        _ => String::new(),
    };
    let mut range = format!(":{}", format_js_number(start_line));
    if !end_line.is_empty() {
        range.push_str(&format!("-{end_line}"));
    }
    theme.fg("warning", &range)
}

/// `formatReadCall` (read.ts:79-82): the expanded call line.
fn format_read_call(args: &Value, theme: &Theme, cwd: &str) -> String {
    // read.ts:80-81: `renderToolPath(str(file_path ?? path), theme, cwd)`
    // with no `emptyFallback` — an empty path renders muted `...`.
    let path_display = render_tool_path(read_render_path(args), theme, cwd, None);
    format!(
        "{} {}{}",
        theme.fg("toolTitle", &Theme::bold("read")),
        path_display,
        format_read_line_range(args, theme)
    )
}

/// `trimTrailingEmptyLines` (read.ts:84-90): drop trailing `""` elements.
fn trim_trailing_empty_lines(mut lines: Vec<String>) -> Vec<String> {
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// `toPosixPath` (read.ts:99-101): platform separator → `/`.
fn to_posix_path(path: &str) -> String {
    path.replace(std::path::MAIN_SEPARATOR, "/")
}

/// `CompactReadClassification` (read.ts:37-40) — `kind` narrowed to a Rust
/// enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactKind {
    Docs,
    Resource,
    Skill,
}

struct CompactReadClassification {
    kind: CompactKind,
    label: String,
}

/// `getPiDocsClassification` (read.ts:103-120): whether the absolute path
/// lives in the pi/rpi package's own `README.md` / `docs/` / `examples/`.
fn get_pi_docs_classification(
    absolute_path: &Path,
    package_root: &Path,
) -> Option<CompactReadClassification> {
    // read.ts:104: `packageRoot = dirname(getReadmePath())` — the package
    // dir (config.ts:367-388); normalized like upstream `resolvePath(packageRoot)`
    // (node `resolve` defaults its base to the current dir).
    let package_root = resolve_path(
        &package_root.to_string_lossy(),
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    );
    // read.ts:105-113: `relative(packageRoot, absolutePath)`; paths outside
    // the package root (relative `..`, absolute) do not classify.
    let relative_path = absolute_path.strip_prefix(&package_root).ok()?;
    let label = to_posix_path(&relative_path.to_string_lossy());
    // read.ts:115-117.
    if label == "README.md" || label.starts_with("docs/") || label.starts_with("examples/") {
        return Some(CompactReadClassification {
            kind: CompactKind::Docs,
            label,
        });
    }
    None
}

/// `getCompactReadClassification` (read.ts:122-143).
fn get_compact_read_classification(
    args: &Value,
    cwd: &str,
    package_root: &Path,
) -> Option<CompactReadClassification> {
    // read.ts:126-127: `str(args?.file_path ?? args?.path)` — an empty or
    // invalid path never classifies (`!rawPath`).
    let raw_path = read_render_path(args)?;
    if raw_path.is_empty() {
        return None;
    }
    let absolute_path = resolve_to_cwd(&raw_path, cwd);
    // read.ts:130-132: `SKILL.md` classifies as the parent directory name
    // (`basename(dirname(absolutePath)) || fileName`).
    if absolute_path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
        let label = absolute_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string)
            // A root-level `SKILL.md` has an empty parent basename.
            .unwrap_or_else(|| "SKILL.md".to_string());
        return Some(CompactReadClassification {
            kind: CompactKind::Skill,
            label,
        });
    }

    if let Some(classification) = get_pi_docs_classification(&absolute_path, package_root) {
        return Some(classification);
    }

    // read.ts:138-140: AGENTS/CLAUDE resource files, labeled relative to the
    // cwd when inside it.
    let file_name = absolute_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if COMPACT_RESOURCE_FILE_NAMES.contains(&file_name) {
        return Some(CompactReadClassification {
            kind: CompactKind::Resource,
            label: format_path_relative_to_cwd_or_absolute(&absolute_path, cwd),
        });
    }
    None
}

/// `formatPathRelativeToCwdOrAbsolute` (paths.ts:119-122): cwd-relative when
/// the path is inside the cwd (`.` for the cwd itself), else absolute.
fn format_path_relative_to_cwd_or_absolute(absolute_path: &Path, cwd: &str) -> String {
    // `getCwdRelativePath` (paths.ts:108-117): `resolvePath(cwd)` first.
    let resolved_cwd = resolve_to_cwd(cwd, cwd);
    match absolute_path.strip_prefix(&resolved_cwd) {
        // `relativePath || "."` (paths.ts:116).
        Ok(relative) if !relative.as_os_str().is_empty() => {
            to_posix_path(&relative.to_string_lossy())
        }
        Ok(_) => ".".to_string(),
        Err(_) => to_posix_path(&absolute_path.to_string_lossy()),
    }
}

/// `formatCompactReadCall` (read.ts:145-167): the collapsed call line for a
/// classified path.
fn format_compact_read_call(
    classification: &CompactReadClassification,
    args: &Value,
    theme: &Theme,
) -> String {
    // read.ts:150: ` (${keyText("app.tools.expand")} to expand)`.
    let expand_hint = theme.fg(
        "dim",
        &format!(" ({} to expand)", key_text("app.tools.expand")),
    );
    if classification.kind == CompactKind::Skill {
        // read.ts:151-158: `[skill]` label + name, same styling family as the
        // skill-invocation message (skill-invocation-message.ts).
        return format!(
            "{}{}{}{}",
            theme.fg("customMessageLabel", "\u{1b}[1m[skill]\u{1b}[22m "),
            theme.fg("customMessageText", &classification.label),
            format_read_line_range(args, theme),
            expand_hint
        );
    }
    // read.ts:160-166: `read docs` / `read resource` in bold toolTitle, the
    // label in accent.
    format!(
        "{} {}{}{}",
        theme.fg(
            "toolTitle",
            &Theme::bold(&format!("read {}", classification.kind_str()))
        ),
        theme.fg("accent", &classification.label),
        format_read_line_range(args, theme),
        expand_hint
    )
}

impl CompactReadClassification {
    /// The upstream `kind` string ("docs" | "resource" | "skill").
    fn kind_str(&self) -> &'static str {
        match self.kind {
            CompactKind::Docs => "docs",
            CompactKind::Resource => "resource",
            CompactKind::Skill => "skill",
        }
    }
}

/// `formatReadResult` (read.ts:169-206).
fn format_read_result(
    args: &Value,
    result: &ToolResultState,
    options: ResultRenderOptions,
    theme: &Theme,
    show_images: bool,
    is_error: bool,
) -> String {
    // read.ts:178-180: collapsed, non-error results render nothing.
    if !options.expanded && !is_error {
        return String::new();
    }

    let raw_path = read_render_path(args);
    let output = get_text_output(Some(result), show_images);
    // read.ts:184: language detection only for non-error reads with a path.
    let lang = if !is_error {
        raw_path.as_deref().and_then(get_language_from_path)
    } else {
        None
    };
    // read.ts:185: highlighted lines (tabs replaced before highlighting) or
    // the raw output split on newlines.
    let rendered_lines: Vec<String> = match lang {
        Some(lang) => highlight_code(&replace_tabs(&output), Some(lang), theme),
        None => output.split('\n').map(str::to_string).collect(),
    };
    let lines = trim_trailing_empty_lines(rendered_lines);
    // read.ts:187-189: full when expanded, else a 10-line preview.
    let max_lines = if options.expanded { lines.len() } else { 10 };
    let display_lines = &lines[..max_lines.min(lines.len())];
    let remaining = lines.len().saturating_sub(max_lines);
    // read.ts:190: leading `\n`, then highlighted lines as-is or per-line
    // `toolOutput` coloring (the second `replaceTabs` is a no-op on the
    // highlighted branch, whose input was already replaced).
    let mut text = format!(
        "\n{}",
        display_lines
            .iter()
            .map(|line| match lang {
                Some(_) => replace_tabs(line),
                None => theme.fg("toolOutput", &replace_tabs(line)),
            })
            .collect::<Vec<_>>()
            .join("\n")
    );
    if remaining > 0 {
        // read.ts:191-193: `... (N more lines, <key> to expand)`.
        text.push_str(&format!(
            "{} {}{}",
            theme.fg("muted", &format!("\n... ({remaining} more lines,")),
            key_hint(theme, "app.tools.expand", "to expand"),
            theme.fg("muted", ")")
        ));
    }

    // read.ts:195-204: truncation warnings from `details.truncation`.
    let truncation = result.details.as_ref().and_then(|d| d.get("truncation"));
    let truncated = truncation
        .and_then(|t| t.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if truncated {
        let first_line_exceeds = truncation
            .and_then(|t| t.get("firstLineExceedsLimit"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let truncated_by_lines = truncation
            .and_then(|t| t.get("truncatedBy"))
            .and_then(Value::as_str)
            == Some("lines");
        let max_bytes = truncation
            .and_then(|t| t.get("maxBytes"))
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let max_lines = truncation
            .and_then(|t| t.get("maxLines"))
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let output_lines = truncation
            .and_then(|t| t.get("outputLines"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let total_lines = truncation
            .and_then(|t| t.get("totalLines"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if first_line_exceeds {
            // read.ts:197-198.
            text.push_str(&theme.fg(
                "warning",
                &format!(
                    "\n[First line exceeds {} limit]",
                    format_size(max_bytes.unwrap_or(DEFAULT_MAX_BYTES))
                ),
            ));
        } else if truncated_by_lines {
            // read.ts:199-200.
            text.push_str(&theme.fg(
                "warning",
                &format!(
                    "\n[Truncated: showing {output_lines} of {total_lines} lines ({} line limit)]",
                    max_lines.unwrap_or(DEFAULT_MAX_LINES)
                ),
            ));
        } else {
            // read.ts:201-202.
            text.push_str(&theme.fg(
                "warning",
                &format!(
                    "\n[Truncated: {output_lines} lines shown ({} limit)]",
                    format_size(max_bytes.unwrap_or(DEFAULT_MAX_BYTES))
                ),
            ));
        }
    }
    text
}

/// The read tool's render definition (read.ts:334-350).
pub struct ReadToolRenderer;

impl ToolDefinition for ReadToolRenderer {
    fn render_call(
        &self,
        args: &Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        // read.ts:335-340: collapsed calls with a classified path get the
        // compact format; everything else (expanded, or unclassified) gets
        // the full format.
        let classification = if !context.expanded {
            get_compact_read_classification(args, &context.cwd, &get_package_dir())
        } else {
            None
        };
        let text = match classification {
            Some(classification) => format_compact_read_call(&classification, args, theme),
            None => format_read_call(args, theme, &context.cwd),
        };
        Some(Box::new(Text::new(text, 0, 0, None)))
    }

    fn render_result(
        &self,
        result: &ToolResultState,
        options: ResultRenderOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        // read.ts:344-350: always a `Text` — the collapsed non-error case
        // returns an empty component (never `None`, which would fall through
        // to the generic fallback printing the raw file contents).
        Some(Box::new(Text::new(
            format_read_result(
                &context.args,
                result,
                options,
                theme,
                context.show_images,
                context.is_error,
            ),
            0,
            0,
            None,
        )))
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
    use crate::tools::test_helpers::TempDir;
    use rpi_tui::tui::RenderHandle;
    use serde_json::json;

    /// Serializes tests that mutate the process-global terminal-image
    /// capability cache (shared with the `tool_execution.rs` tests — same
    /// lock name marks the convention, tool-execution tests CAPS_LOCK).
    static CAPS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
            terminal_width: 0,
        }
    }

    /// Strip CSI SGR (`ESC[..m`) and OSC 8 hyperlink (`ESC]8;;..ESC\`)
    /// sequences so assertions are independent of terminal capabilities
    /// (same helper as the ls renderer's tests; the capability cache is a
    /// process global other tests mutate).
    fn strip_escape_sequences(input: &str) -> String {
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

    fn result_with(text: &str, details: Option<Value>) -> ToolResultState {
        ToolResultState {
            content: vec![ToolResultContentLoose::text(text.to_string())],
            is_error: false,
            details,
        }
    }

    /// Text pads rendered lines to the full width (text.rs); trim each line
    /// for exact comparisons.
    fn pad_trim(rendered: &str) -> String {
        rendered
            .lines()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // --- formatReadLineRange ------------------------------------------------

    #[test]
    fn line_range_renders_offset_and_limit_variants() {
        let theme = theme();
        // No range args → no suffix.
        assert_eq!(
            strip_escape_sequences(&format_read_line_range(&json!({}), &theme)),
            ""
        );
        // offset + limit → `:start-(start+limit-1)`.
        let ranged = format_read_line_range(&json!({"offset": 10, "limit": 11}), &theme);
        assert_eq!(strip_escape_sequences(&ranged), ":10-20");
        // limit only → starts at 1; offset only → no end.
        assert_eq!(
            strip_escape_sequences(&format_read_line_range(&json!({"limit": 5}), &theme)),
            ":1-5"
        );
        assert_eq!(
            strip_escape_sequences(&format_read_line_range(&json!({"offset": 5}), &theme)),
            ":5"
        );
        // JS falsy-0 end line: offset 1 + limit 0 → `:1` (no `-0`).
        assert_eq!(
            strip_escape_sequences(&format_read_line_range(
                &json!({"offset": 1, "limit": 0}),
                &theme
            )),
            ":1"
        );
        // offset 0 is not nullish (`?? 1` does not fire) → `:0-9`.
        assert_eq!(
            strip_escape_sequences(&format_read_line_range(
                &json!({"offset": 0, "limit": 10}),
                &theme
            )),
            ":0-9"
        );
    }

    // --- formatReadCall ------------------------------------------------------

    #[test]
    fn call_full_format_renders_path_and_range() {
        let theme = theme();
        let text = format_read_call(&json!({"path": "src/main.rs"}), &theme, "/cwd");
        assert_eq!(strip_escape_sequences(&text), "read src/main.rs");
        let ranged = format_read_call(
            &json!({"path": "src/main.rs", "offset": 10, "limit": 11}),
            &theme,
            "/cwd",
        );
        assert_eq!(strip_escape_sequences(&ranged), "read src/main.rs:10-20");
        // file_path wins over path (read.ts:70).
        let aliased = format_read_call(
            &json!({"file_path": "a.txt", "path": "b.txt"}),
            &theme,
            "/cwd",
        );
        assert_eq!(strip_escape_sequences(&aliased), "read a.txt");
        // Missing/empty path → muted `...`; non-string path → invalid-arg text.
        let missing = format_read_call(&json!({}), &theme, "/cwd");
        assert!(strip_escape_sequences(&missing).contains("read ..."));
        let invalid = format_read_call(&json!({"path": 42}), &theme, "/cwd");
        assert!(strip_escape_sequences(&invalid).contains("[invalid arg]"));
    }

    #[test]
    fn call_shortens_home_path() {
        let theme = theme();
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        let text = format_read_call(&json!({"path": format!("{home}/notes.md")}), &theme, "/cwd");
        assert_eq!(strip_escape_sequences(&text), "read ~/notes.md");
    }

    // --- compact classification ----------------------------------------------

    #[test]
    fn classification_skill_uses_parent_dir_name() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.path().join("skills/foo")).unwrap();
        let classification = get_compact_read_classification(
            &json!({"path": "skills/foo/SKILL.md"}),
            &tmp.path().to_string_lossy(),
            tmp.path(),
        );
        let classification = classification.expect("SKILL.md classifies");
        assert_eq!(classification.kind, CompactKind::Skill);
        assert_eq!(classification.label, "foo");
        // A root-level SKILL.md (dirname's basename is empty) falls back to
        // the file name (read.ts:131).
        let root = get_compact_read_classification(&json!({"path": "SKILL.md"}), "/", tmp.path());
        assert_eq!(root.expect("root SKILL.md classifies").label, "SKILL.md");
    }

    #[test]
    fn classification_docs_matches_readme_and_docs_dir() {
        let pkg = TempDir::new();
        let cwd = TempDir::new();
        let cwd_str = cwd.path().to_string_lossy();
        let pkg_str = pkg.path().to_string_lossy();
        std::fs::create_dir_all(pkg.path().join("docs")).unwrap();
        std::fs::create_dir_all(pkg.path().join("examples")).unwrap();

        // README.md at the package root.
        let readme = get_compact_read_classification(
            &json!({"path": pkg.path().join("README.md")}),
            &cwd_str,
            pkg.path(),
        )
        .expect("README.md classifies as docs");
        assert_eq!(readme.kind, CompactKind::Docs);
        assert_eq!(readme.label, "README.md");

        // Anything under docs/ or examples/.
        let doc = get_compact_read_classification(
            &json!({"path": pkg.path().join("docs/design.md")}),
            &cwd_str,
            pkg.path(),
        )
        .expect("docs/ path classifies");
        assert_eq!(doc.kind, CompactKind::Docs);
        assert_eq!(doc.label, "docs/design.md");
        let example = get_compact_read_classification(
            &json!({"path": pkg.path().join("examples/foo.rs")}),
            &cwd_str,
            pkg.path(),
        );
        assert_eq!(
            example.expect("examples/ path classifies").label,
            "examples/foo.rs"
        );

        // Paths outside the package root never classify as docs.
        let outside = TempDir::new();
        std::fs::create_dir_all(outside.path().join("docs")).unwrap();
        let not_docs = get_compact_read_classification(
            &json!({"path": outside.path().join("docs/design.md")}),
            &cwd_str,
            pkg.path(),
        );
        assert!(not_docs.is_none(), "outside the package root");
        // `docs/` elsewhere in the tree does not classify either.
        let other_docs = get_compact_read_classification(
            &json!({"path": format!("{pkg_str}/src/docs/design.md")}),
            &cwd_str,
            pkg.path(),
        );
        assert!(other_docs.is_none());
    }

    #[test]
    fn classification_resource_labels_relative_to_cwd() {
        let tmp = TempDir::new();
        let cwd_str = tmp.path().to_string_lossy();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        for name in [
            "AGENTS.override.md",
            "AGENTS.md",
            "AGENTS.MD",
            "CLAUDE.md",
            "CLAUDE.MD",
        ] {
            let classification = get_compact_read_classification(
                &json!({"path": format!("sub/{name}")}),
                &cwd_str,
                tmp.path(),
            )
            .unwrap_or_else(|| panic!("{name} must classify as resource"));
            assert_eq!(classification.kind, CompactKind::Resource);
            assert_eq!(classification.label, format!("sub/{name}"));
        }
        // At the cwd root the label is the bare file name.
        let root =
            get_compact_read_classification(&json!({"path": "AGENTS.md"}), &cwd_str, tmp.path());
        assert_eq!(root.expect("AGENTS.md classifies").label, "AGENTS.md");
        // Outside the cwd → absolute label.
        let outside = TempDir::new();
        std::fs::write(outside.path().join("AGENTS.md"), "").unwrap();
        let abs = get_compact_read_classification(
            &json!({"path": outside.path().join("AGENTS.md")}),
            &cwd_str,
            tmp.path(),
        )
        .expect("absolute AGENTS.md classifies");
        assert_eq!(
            abs.label,
            outside.path().join("AGENTS.md").to_string_lossy()
        );
    }

    #[test]
    fn classification_none_for_unclassifiable_or_invalid() {
        let tmp = TempDir::new();
        let cwd_str = tmp.path().to_string_lossy();
        // Ordinary file, missing path, empty path, non-string path.
        assert!(get_compact_read_classification(
            &json!({"path": "src/main.rs"}),
            &cwd_str,
            tmp.path()
        )
        .is_none());
        assert!(get_compact_read_classification(&json!({}), &cwd_str, tmp.path()).is_none());
        assert!(
            get_compact_read_classification(&json!({"path": ""}), &cwd_str, tmp.path()).is_none()
        );
        assert!(
            get_compact_read_classification(&json!({"path": 42}), &cwd_str, tmp.path()).is_none()
        );
        // `file_path` wins: an unclassifiable file_path hides a classifiable path.
        assert!(get_compact_read_classification(
            &json!({"file_path": "notes.txt", "path": "AGENTS.md"}),
            &cwd_str,
            tmp.path()
        )
        .is_none());
    }

    // --- renderCall (collapsed vs expanded) ----------------------------------

    #[test]
    fn render_call_collapsed_uses_compact_classification() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.path().join("skills/foo")).unwrap();
        let state = RendererStateSlot::default();
        let renderer = ReadToolRenderer;
        let mut ctx = context(&state);
        ctx.cwd = tmp.path().to_string_lossy().into_owned();
        ctx.args = json!({"path": "skills/foo/SKILL.md"});

        let component = renderer
            .render_call(&ctx.args, &theme(), &ctx)
            .expect("call component");
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        // `[skill]` label + parent-dir name + dim expand hint (ctrl+o default).
        assert_eq!(stripped.trim_end(), "[skill] foo (ctrl+o to expand)");

        // Resource classification with the range suffix.
        std::fs::write(tmp.path().join("AGENTS.md"), "").unwrap();
        ctx.args = json!({"path": "AGENTS.md", "offset": 3});
        let component = renderer
            .render_call(&ctx.args, &theme(), &ctx)
            .expect("call component");
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        assert_eq!(
            stripped.trim_end(),
            "read resource AGENTS.md:3 (ctrl+o to expand)"
        );
    }

    #[test]
    fn render_call_expanded_skips_compact_classification() {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.path().join("skills/foo")).unwrap();
        let state = RendererStateSlot::default();
        let renderer = ReadToolRenderer;
        let mut ctx = context(&state);
        ctx.cwd = tmp.path().to_string_lossy().into_owned();
        ctx.expanded = true;
        ctx.args = json!({"path": "skills/foo/SKILL.md"});

        // Expanded: always the full format, never the `[skill]` classification.
        let component = renderer
            .render_call(&ctx.args, &theme(), &ctx)
            .expect("call component");
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        assert_eq!(stripped.trim_end(), "read skills/foo/SKILL.md");
    }

    // --- renderResult: collapsed/expanded preview -----------------------------

    #[test]
    fn result_collapsed_non_error_is_empty_component() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = ReadToolRenderer;
        let context = context(&state);
        let result = result_with("file contents\nline2", None);
        // Must be `Some` (an empty Text) — `None` would fall through to the
        // generic fallback, which prints the raw file contents.
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &context,
            )
            .expect("collapsed non-error result still yields a component");
        assert!(component.render(80).is_empty());
    }

    #[test]
    fn result_error_collapsed_previews_10_lines_with_hint() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = ReadToolRenderer;
        let mut ctx = context(&state);
        ctx.is_error = true;
        ctx.args = json!({"path": "data.txt"});
        let result = ToolResultState {
            content: vec![ToolResultContentLoose::text(
                (1..=15)
                    .map(|i| format!("line{i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )],
            is_error: true,
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
                &ctx,
            )
            .expect("result component");
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        // Error results skip the collapsed-empty shortcut (read.ts:178), so
        // the collapsed state shows the 10-line preview: first 10 lines,
        // then the verbatim expand hint (ctrl+o default).
        assert!(stripped.contains("line1"));
        assert!(stripped.contains("line10"));
        assert!(!stripped.contains("line11"));
        assert!(stripped.contains("\n... (5 more lines, ctrl+o to expand)"));
    }

    #[test]
    fn result_expanded_shows_all_lines_without_hint() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = ReadToolRenderer;
        let mut ctx = context(&state);
        ctx.args = json!({"path": "data.txt"});
        let result = result_with(
            &(1..=15)
                .map(|i| format!("line{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            None,
        );
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: true,
                    is_partial: false,
                },
                &theme,
                &ctx,
            )
            .expect("result component");
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        assert!(stripped.contains("line15"));
        assert!(!stripped.contains("more lines"));
    }

    #[test]
    fn result_error_collapsed_exactly_10_lines_has_no_hint() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = ReadToolRenderer;
        let mut ctx = context(&state);
        ctx.is_error = true;
        ctx.args = json!({"path": "data.txt"});
        let result = ToolResultState {
            content: vec![ToolResultContentLoose::text(
                (1..=10)
                    .map(|i| format!("line{i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )],
            is_error: true,
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
                &ctx,
            )
            .expect("result component");
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        assert!(stripped.contains("line10"));
        assert!(!stripped.contains("more lines"));
    }

    // --- renderResult: highlight vs plain ------------------------------------

    #[test]
    fn result_highlights_supported_language_paths() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = ReadToolRenderer;
        let mut ctx = context(&state);
        // The raw path drives language detection (read.ts:184).
        ctx.args = json!({"path": "src/main.rs"});
        let code = "fn main() {\n    if x == 1 {\n        let s = \"hi\"; // note\n    }\n}\n";
        let result = result_with(code, None);

        // Expanded (non-error): syntax colors present, stripping back to the
        // original code. Assert structure + color presence, not the exact
        // ANSI sequence (ADR-0008 / D-051: syntect ≠ hljs token boundaries).
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: true,
                    is_partial: false,
                },
                &theme,
                &ctx,
            )
            .expect("result component");
        let rendered = component.render(80).join("\n");
        assert!(rendered.contains('\u{1b}'), "highlighted output has ANSI");
        assert!(
            rendered.contains(theme.get_fg_ansi("syntaxKeyword")),
            "rust keywords must be highlighted"
        );
        assert!(
            rendered.contains(theme.get_fg_ansi("syntaxType")),
            "rust fn/let must be highlighted"
        );
        assert_eq!(
            pad_trim(&strip_escape_sequences(&rendered)),
            format!("\n{}", pad_trim(code)),
            "highlighted output must round-trip (leading \\n is the format's contract, read.ts:190)"
        );
    }

    #[test]
    fn result_plain_path_uses_tool_output_coloring() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = ReadToolRenderer;
        let mut ctx = context(&state);
        ctx.args = json!({"path": "data.txt"});
        let result = result_with("alpha\nbeta", None);
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: true,
                    is_partial: false,
                },
                &theme,
                &ctx,
            )
            .expect("result component");
        let rendered = component.render(80).join("\n");
        assert!(rendered.contains(theme.get_fg_ansi("toolOutput")));
        assert!(!rendered.contains(theme.get_fg_ansi("syntaxKeyword")));
        assert_eq!(
            pad_trim(&strip_escape_sequences(&rendered)),
            "\nalpha\nbeta"
        );
    }

    // --- renderResult: error branch ------------------------------------------

    #[test]
    fn result_error_branch_renders_collapsed_without_highlight() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = ReadToolRenderer;
        let mut ctx = context(&state);
        // The hook reads `isError` from the render context (read.ts:347) —
        // error results skip the collapsed-empty shortcut AND language
        // detection (read.ts:178, :184).
        ctx.is_error = true;
        ctx.args = json!({"path": "src/main.rs"});
        let result = ToolResultState {
            content: vec![ToolResultContentLoose::text(
                "Error: permission denied".to_string(),
            )],
            is_error: true,
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
                &ctx,
            )
            .expect("error result component");
        let rendered = component.render(80).join("\n");
        let stripped = strip_escape_sequences(&rendered);
        assert!(stripped.contains("Error: permission denied"));
        assert!(
            !rendered.contains(theme.get_fg_ansi("syntaxKeyword")),
            "no syntax highlighting for error output"
        );
        assert!(rendered.contains(theme.get_fg_ansi("toolOutput")));
    }

    // --- renderResult: truncation warnings -----------------------------------

    #[test]
    fn result_truncation_warnings_verbatim() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = ReadToolRenderer;
        let mut ctx = context(&state);
        ctx.args = json!({"path": "big.txt"});
        let result = result_with(
            "line1",
            Some(json!({
                "truncation": {
                    "truncated": true,
                    "truncatedBy": "lines",
                    "outputLines": 2000,
                    "totalLines": 2500,
                    "maxLines": 2000,
                    "maxBytes": 51200
                }
            })),
        );
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: true,
                    is_partial: false,
                },
                &theme,
                &ctx,
            )
            .expect("result component");
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        assert!(stripped.contains("\n[Truncated: showing 2000 of 2500 lines (2000 line limit)]"));

        // Byte truncation branch.
        let bytes = result_with(
            "line1",
            Some(json!({
                "truncation": {
                    "truncated": true,
                    "truncatedBy": "bytes",
                    "outputLines": 500,
                    "totalLines": 2500
                }
            })),
        );
        let component = renderer
            .render_result(
                &bytes,
                ResultRenderOptions {
                    expanded: true,
                    is_partial: false,
                },
                &theme,
                &ctx,
            )
            .expect("result component");
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        assert!(stripped.contains("\n[Truncated: 500 lines shown (50.0KB limit)]"));

        // First-line-exceeds-limit branch (maxBytes defaults to 50KB).
        let first_line = result_with(
            "",
            Some(json!({
                "truncation": {
                    "truncated": true,
                    "truncatedBy": "bytes",
                    "firstLineExceedsLimit": true
                }
            })),
        );
        let component = renderer
            .render_result(
                &first_line,
                ResultRenderOptions {
                    expanded: true,
                    is_partial: false,
                },
                &theme,
                &ctx,
            )
            .expect("result component");
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        assert!(stripped.contains("\n[First line exceeds 50.0KB limit]"));

        // Untruncated (or no details) → no warning line.
        let plain = result_with("line1", Some(json!({"truncation": {"truncated": false}})));
        let component = renderer
            .render_result(
                &plain,
                ResultRenderOptions {
                    expanded: true,
                    is_partial: false,
                },
                &theme,
                &ctx,
            )
            .expect("result component");
        assert!(!strip_escape_sequences(&component.render(80).join("\n")).contains("Truncated"));
    }

    // --- renderResult: image branch -------------------------------------------

    #[test]
    fn result_image_blocks_render_fallback_text_without_caps() {
        // The terminal-image capability cache is a process global; serialize
        // with the tool_execution.rs tests (same CAPS_LOCK convention).
        let _guard = CAPS_LOCK.lock().unwrap();
        rpi_tui::terminal_image::set_capabilities(rpi_tui::terminal_image::TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        });
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = ReadToolRenderer;
        let mut ctx = context(&state);
        ctx.args = json!({"path": "photo.png"});
        let result = ToolResultState {
            content: vec![
                ToolResultContentLoose::text("Read image file [image/png]".to_string()),
                ToolResultContentLoose::image("aGVsbG8=", "image/png"),
            ],
            is_error: false,
            details: None,
        };
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: true,
                    is_partial: false,
                },
                &theme,
                &ctx,
            )
            .expect("result component");
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        // getTextOutput appends the image-fallback indicator when the
        // terminal cannot display images.
        assert!(stripped.contains("Read image file [image/png]"));
        assert!(stripped.contains("[Image: [image/png]]"));
    }

    #[test]
    fn result_image_blocks_skip_indicator_with_kitty_caps() {
        // Same process-global capability cache as above; serialize.
        let _guard = CAPS_LOCK.lock().unwrap();
        rpi_tui::terminal_image::set_capabilities(rpi_tui::terminal_image::TerminalCapabilities {
            images: Some(rpi_tui::terminal_image::ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: false,
        });
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = ReadToolRenderer;
        let mut ctx = context(&state);
        ctx.show_images = true;
        ctx.args = json!({"path": "photo.png"});
        let result = ToolResultState {
            content: vec![
                ToolResultContentLoose::text("Read image file [image/png]".to_string()),
                ToolResultContentLoose::image("aGVsbG8=", "image/png"),
            ],
            is_error: false,
            details: None,
        };
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: true,
                    is_partial: false,
                },
                &theme,
                &ctx,
            )
            .expect("result component");
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        // With image caps + show_images the text output carries only the
        // text block (the real image is rendered by the shell, not here).
        assert!(stripped.contains("Read image file [image/png]"));
        assert!(!stripped.contains("[Image:"));
    }
}
