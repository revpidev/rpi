//! Edit tool renderer — port of the `renderCall`/`renderResult` hooks in
//! `packages/coding-agent/src/core/tools/edit.ts` (renderCall :367-393,
//! renderResult :394-435, helpers :137-295; the preview diff computation is
//! `computeEditsDiff`, edit-diff.ts:518-547) @ pi 0.82.1 (2efa728) (T17).
//!
//! Intentional differences:
//! - The preview computation is synchronous: upstream starts an async
//!   `computeEditsDiff` when `argsComplete` is first seen and invalidates
//!   once the promise settles (edit.ts:381-390), so the preview appears one
//!   frame later; here `render_call` reads the file and runs the ported diff
//!   engine (`crate::tools::edit_diff::compute_edits_diff`) inline, so the
//!   preview is visible in the same frame as `argsComplete`. The
//!   `preview_pending` flag is kept for state-shape parity and is always
//!   false outside that synchronous window. Visible behavior is equivalent.
//! - Upstream keeps the call component in `EditRenderState.callComponent`
//!   and reuses it through `context.lastComponent` (edit.ts:166-178,
//!   tool-execution.ts:117-124); here the state holds only the preview
//!   fields and the visual component is rebuilt from them on every
//!   `ToolExecutionComponent::update_display` (same pattern as the bash
//!   renderer, T17). Because the rebuild is event-driven (update_display is
//!   called by the args/result/… setters, not every frame), a `renderResult`
//!   backfill only reaches the call component when the call component is
//!   BUILT after the result side effect — `update_display` builds the
//!   result component before the call component for exactly this reason
//!   (V13-11, mirroring upstream's in-place rebuild, edit.ts:416-424).
//! - `renderResult` always returns the result [`Container`] — empty when
//!   `formatEditResult` yields nothing — because `None` would fall through
//!   to the generic fallback and print the success text (upstream write/edit
//!   show no result line on success).
//! - The tool renders its own framing (`renderShell: "self"`, edit.ts:310):
//!   the call box carries the header background and the diff body itself.

use std::path::Path;
use std::sync::Mutex;

use rpi_tui::components::r#box::Box as TuiBox;
use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::text::{ColorFn, Text};
use rpi_tui::tui::{Component, Container};
use serde_json::{json, Value};

use super::render_utils::{render_tool_path, str_value};
use crate::core::themes::Theme;
use crate::modes::interactive::components::diff::{render_diff, RenderDiffOptions};
use crate::modes::interactive::components::tool_execution::{
    lock_recover, RenderShell, ResultRenderOptions, ToolDefinition, ToolRenderContext,
    ToolResultState,
};
use crate::tools::edit_diff::{compute_edits_diff, EditReplacement};

/// `EditPreview` (edit.ts:27): `EditDiffResult | EditDiffError`
/// (edit-diff.ts:505-512).
#[derive(Debug, Clone, PartialEq, Eq)]
enum EditPreview {
    Diff {
        diff: String,
        first_changed_line: Option<usize>,
    },
    Error {
        error: String,
    },
}

/// `EditRenderState` (edit.ts:29-31) plus the preview fields of
/// `EditCallRenderComponent` (edit.ts:150-155): per tool call, carried by the
/// [`RendererStateSlot`]. The call component itself is rebuilt from this
/// state on every update (T17).
#[derive(Default)]
pub struct EditRenderState {
    inner: Mutex<EditRenderStateInner>,
}

#[derive(Default)]
struct EditRenderStateInner {
    preview: Option<EditPreview>,
    preview_args_key: Option<String>,
    preview_pending: bool,
    settled_error: bool,
}

/// The `{ path, edits }` input for the preview (edit.ts:137-143).
struct RenderablePreviewInput {
    path: String,
    /// The `edits` value used for the args key: the raw `edits` array from
    /// the args, or the synthesized single-entry array for the legacy
    /// `oldText`/`newText` form (edit.ts:195-200).
    edits: Value,
}

impl RenderablePreviewInput {
    /// The validated replacements in `edits` array order — the array position
    /// is what error messages report as `edits[i]`
    /// (edit-diff.ts `applyEditsToNormalizedContent`).
    fn replacements(&self) -> Vec<EditReplacement> {
        let Some(items) = self.edits.as_array() else {
            return Vec::new();
        };
        items
            .iter()
            .enumerate()
            .map(|(i, edit)| EditReplacement {
                old_text: edit
                    .get("oldText")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                new_text: edit
                    .get("newText")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                edit_index: i,
            })
            .collect()
    }
}

/// `str(args?.file_path ?? args?.path)` (edit.ts:206, 217): nullish
/// coalescing — `file_path` wins unless it is null/undefined; `str` maps
/// strings to themselves, null/undefined to `""`, anything else to `None`
/// (invalid arg).
fn file_path_or_path(args: &Value) -> Option<String> {
    match args.get("file_path") {
        None | Some(Value::Null) => str_value(args.get("path")),
        Some(value) => str_value(Some(value)),
    }
}

/// `getRenderablePreviewInput` (edit.ts:180-203): the `{path, edits}` input
/// for the preview, or `None` when the args cannot render a preview.
fn get_renderable_preview_input(args: &Value) -> Option<RenderablePreviewInput> {
    // `typeof args.path === "string" ? args.path : typeof args.file_path ===
    // "string" ? args.file_path : null` (edit.ts:185-188).
    let path = match args.get("path") {
        Some(Value::String(s)) => s.clone(),
        _ => match args.get("file_path") {
            Some(Value::String(s)) => s.clone(),
            _ => return None,
        },
    };
    // `if (!path) return null` (edit.ts:189): an empty string is falsy in
    // JS — no preview for an empty path.
    if path.is_empty() {
        return None;
    }
    // `Array.isArray(edits) && edits.length > 0 && every(edit =>
    // typeof edit?.oldText === "string" && typeof edit?.newText === "string")`
    // (edit.ts:190-196).
    let edits_array_valid = match args.get("edits") {
        Some(Value::Array(items)) if !items.is_empty() => items.iter().all(|edit| {
            edit.get("oldText").is_some_and(Value::is_string)
                && edit.get("newText").is_some_and(Value::is_string)
        }),
        _ => false,
    };
    if edits_array_valid {
        return Some(RenderablePreviewInput {
            path,
            edits: args.get("edits").cloned().unwrap_or(Value::Null),
        });
    }
    // Legacy single edit (edit.ts:198-200).
    if let (Some(old_text), Some(new_text)) = (
        args.get("oldText").and_then(Value::as_str),
        args.get("newText").and_then(Value::as_str),
    ) {
        return Some(RenderablePreviewInput {
            path,
            edits: json!([{ "oldText": old_text, "newText": new_text }]),
        });
    }
    None
}

/// `JSON.stringify({ path, edits })` (edit.ts:370-372) — insertion order
/// `path`, `edits`. Only ever compared for equality, so the canonical JSON
/// bytes are equivalent to upstream's.
fn preview_args_key(path: &str, edits: &Value) -> String {
    format!(
        "{{\"path\":{},\"edits\":{}}}",
        serde_json::to_string(path).unwrap_or_default(),
        serde_json::to_string(edits).unwrap_or_default()
    )
}

/// `setEditPreview` (edit.ts:277-295): store the preview and report whether
/// the rendered diff changed.
fn set_edit_preview(
    state: &mut EditRenderStateInner,
    preview: EditPreview,
    args_key: Option<&str>,
) -> bool {
    let changed = match &state.preview {
        None => true,
        Some(current) => match (current, &preview) {
            (EditPreview::Error { error: a }, EditPreview::Error { error: b }) => a != b,
            (
                EditPreview::Diff {
                    diff: a,
                    first_changed_line: fa,
                },
                EditPreview::Diff {
                    diff: b,
                    first_changed_line: fb,
                },
            ) => a != b || fa != fb,
            _ => true,
        },
    };
    state.preview = Some(preview);
    state.preview_args_key = args_key.map(str::to_string);
    state.preview_pending = false;
    changed
}

/// `formatEditCall` (edit.ts:205-208).
fn format_edit_call(args: &Value, theme: &Theme, cwd: &str) -> String {
    let path_display = render_tool_path(file_path_or_path(args), theme, cwd, None);
    format!(
        "{} {path_display}",
        theme.fg("toolTitle", &Theme::bold("edit"))
    )
}

/// `formatEditResult` (edit.ts:210-237). `None` means the result line is
/// skipped — on success the diff is already shown in the call preview.
fn format_edit_result(
    args: &Value,
    preview: Option<&EditPreview>,
    result: &ToolResultState,
    theme: &Theme,
    is_error: bool,
) -> Option<String> {
    let raw_path = file_path_or_path(args);
    let preview_diff = match preview {
        Some(EditPreview::Diff { diff, .. }) => Some(diff.as_str()),
        _ => None,
    };
    let preview_error = match preview {
        Some(EditPreview::Error { error }) => Some(error.as_str()),
        _ => None,
    };
    if is_error {
        // `result.content.filter(c => c.type === "text").map(c => c.text ||
        // "").join("\n")` (edit.ts:221-224) — the raw join, without the
        // sanitize/strip-ANSI pass of `getTextOutput`.
        let error_text = result
            .content
            .iter()
            .filter(|c| c.kind == "text")
            .map(|c| c.text.as_deref().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        if error_text.is_empty() || Some(error_text.as_str()) == preview_error {
            return None;
        }
        return Some(theme.fg("error", &error_text));
    }
    let result_diff = result
        .details
        .as_ref()
        .and_then(|d| d.get("diff"))
        .and_then(Value::as_str);
    // `if (resultDiff && resultDiff !== previewDiff)` (edit.ts:231-234): an
    // empty-string diff is falsy in JS.
    if let Some(result_diff) = result_diff {
        if !result_diff.is_empty() && preview_diff != Some(result_diff) {
            return Some(render_diff(
                result_diff,
                theme,
                RenderDiffOptions {
                    file_path: raw_path,
                },
            ));
        }
    }
    None
}

/// `getEditHeaderBg` (edit.ts:239-254).
fn get_edit_header_bg(
    preview: Option<&EditPreview>,
    settled_error: bool,
    theme: &Theme,
) -> ColorFn {
    let color = match preview {
        Some(EditPreview::Error { .. }) => "toolErrorBg",
        Some(EditPreview::Diff { .. }) => "toolSuccessBg",
        None if settled_error => "toolErrorBg",
        None => "toolPendingBg",
    };
    let theme = theme.clone();
    Box::new(move |text: &str| theme.bg(color, text))
}

/// `buildEditCallComponent` (edit.ts:256-275): the header (title + path) and,
/// once a preview exists, a spacer plus the diff body.
fn build_edit_call_component(
    state: &EditRenderStateInner,
    args: &Value,
    theme: &Theme,
    cwd: &str,
) -> TuiBox {
    // `new Box(1, 1, ...)` + `setBgFn(getEditHeaderBg(...))` (edit.ts:157-164,
    // 262).
    let mut component = TuiBox::new(
        1,
        1,
        Some(get_edit_header_bg(
            state.preview.as_ref(),
            state.settled_error,
            theme,
        )),
    );
    component.add_child(Box::new(Text::new(
        format_edit_call(args, theme, cwd),
        0,
        0,
        None,
    )));
    let Some(preview) = &state.preview else {
        return component;
    };
    // `"error" in preview ? fg(error, preview.error) : renderDiff(...)`
    // (edit.ts:270-271).
    let body = match preview {
        EditPreview::Error { error } => theme.fg("error", error),
        EditPreview::Diff { diff, .. } => render_diff(diff, theme, RenderDiffOptions::default()),
    };
    component.add_child(Box::new(Spacer::new(1)));
    component.add_child(Box::new(Text::new(body, 0, 0, None)));
    component
}

/// The edit tool's render definition (edit.ts:297-437).
pub struct EditToolRenderer;

impl ToolDefinition for EditToolRenderer {
    fn render_call(
        &self,
        args: &Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        let state = context.state.get_or_init::<EditRenderState>();
        let preview_input = get_renderable_preview_input(args);
        let args_key = preview_input
            .as_ref()
            .map(|input| preview_args_key(&input.path, &input.edits));

        let mut inner = lock_recover(&state.inner);
        // `component.previewArgsKey !== argsKey` → reset (edit.ts:374-379).
        if inner.preview_args_key != args_key {
            inner.preview = None;
            inner.preview_args_key = args_key.clone();
            inner.preview_pending = false;
            inner.settled_error = false;
        }

        // `context.argsComplete && previewInput && !preview && !previewPending`
        // (edit.ts:381): computed synchronously — see the module doc.
        if context.args_complete && inner.preview.is_none() && !inner.preview_pending {
            if let Some(input) = preview_input.as_ref() {
                inner.preview_pending = true;
                let request_key = args_key.clone();
                let result =
                    compute_edits_diff(&input.path, &input.replacements(), Path::new(&context.cwd));
                // The requestKey race guard (edit.ts:385-388) is trivially
                // satisfied while the lock is held (single-threaded render
                // loop); kept for parity.
                if inner.preview_args_key == request_key {
                    let preview = match result.error {
                        Some(error) => EditPreview::Error { error },
                        None => EditPreview::Diff {
                            diff: result.diff.unwrap_or_default(),
                            first_changed_line: result.first_changed_line,
                        },
                    };
                    set_edit_preview(&mut inner, preview, request_key.as_deref());
                } else {
                    inner.preview_pending = false;
                }
            }
        }

        Some(Box::new(build_edit_call_component(
            &inner,
            args,
            theme,
            &context.cwd,
        )))
    }

    fn render_result(
        &self,
        result: &ToolResultState,
        _options: ResultRenderOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        let state = context.state.get_or_init::<EditRenderState>();
        let preview_input = get_renderable_preview_input(&context.args);
        let args_key = preview_input
            .as_ref()
            .map(|input| preview_args_key(&input.path, &input.edits));

        let mut inner = lock_recover(&state.inner);
        // Backfill the call preview from the settled result (edit.ts:400-411):
        // `details.diff` / `details.firstChangedLine` (camelCase, T06).
        if !context.is_error {
            if let Some(result_diff) = result
                .details
                .as_ref()
                .and_then(|d| d.get("diff"))
                .and_then(Value::as_str)
            {
                let first_changed_line = result
                    .details
                    .as_ref()
                    .and_then(|d| d.get("firstChangedLine"))
                    .and_then(Value::as_u64)
                    .map(|v| v as usize);
                set_edit_preview(
                    &mut inner,
                    EditPreview::Diff {
                        diff: result_diff.to_string(),
                        first_changed_line,
                    },
                    args_key.as_deref(),
                );
            }
        }
        // `callComponent.settledError !== context.isError` (edit.ts:412-415).
        if inner.settled_error != context.is_error {
            inner.settled_error = context.is_error;
        }

        // Upstream rebuilds the call component when the preview/settledError
        // changed (edit.ts:416-424). Here the backfill takes effect through
        // `update_display`'s result-before-call build order (V13-11): this
        // `renderResult` runs while building the RESULT component, and the
        // call component is built afterwards from the backfilled state —
        // building it first (as before V13-11) froze the pre-result preview
        // on screen forever (rpi#18).

        let output = format_edit_result(
            &context.args,
            inner.preview.as_ref(),
            result,
            theme,
            context.is_error,
        );
        // `(context.lastComponent as Container | undefined) ?? new Container()`
        // (edit.ts:427): a fresh container per update — the renderer's
        // components are rebuilt anyway (T17).
        let mut component = Container::new();
        if let Some(output) = output {
            component.add_child(Box::new(Spacer::new(1)));
            component.add_child(Box::new(Text::new(output, 1, 0, None)));
        }
        Some(Box::new(component))
    }

    fn render_shell(&self) -> Option<RenderShell> {
        // `renderShell: "self"` (edit.ts:310).
        Some(RenderShell::Self_)
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
    use std::path::PathBuf;

    fn theme() -> Theme {
        load_theme("dark", None).expect("builtin dark theme")
    }

    fn context(state: &RendererStateSlot, args: Value) -> ToolRenderContext {
        ToolRenderContext {
            args,
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

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "rpi-edit-renderer-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock before epoch")
                    .as_nanos(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn call_component_lines(
        renderer: &EditToolRenderer,
        args: &Value,
        theme: &Theme,
        ctx: &ToolRenderContext,
    ) -> (Vec<String>, String) {
        let component = renderer
            .render_call(args, theme, ctx)
            .expect("call component");
        let lines = component.render(80);
        let stripped = strip_ansi(&lines.join("\n"));
        (lines, stripped)
    }

    #[test]
    fn render_shell_is_self() {
        assert_eq!(EditToolRenderer.render_shell(), Some(RenderShell::Self_));
    }

    #[test]
    fn call_shows_path_and_preview_after_args_complete() {
        let tmp = TempDir::new();
        std::fs::write(tmp.path().join("src.txt"), "Hello, world!").unwrap();
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = EditToolRenderer;
        let args =
            json!({"path": "src.txt", "edits": [{"oldText": "world", "newText": "testing"}]});
        let mut ctx = context(&state, args.clone());
        ctx.cwd = tmp.path().to_string_lossy().into_owned();
        ctx.args_complete = false;

        // Before argsComplete: header with the relative path, no preview.
        let (lines, stripped) = call_component_lines(&renderer, &args, &theme, &ctx);
        assert!(stripped.contains("edit src.txt"), "stripped: {stripped}");
        assert!(!stripped.contains("Hello, testing!"));
        assert!(lines[0].starts_with(theme.get_bg_ansi("toolPendingBg")));

        // After argsComplete the diff preview appears in the same frame.
        ctx.args_complete = true;
        let (lines, stripped) = call_component_lines(&renderer, &args, &theme, &ctx);
        assert!(stripped.contains("edit src.txt"), "stripped: {stripped}");
        assert!(
            stripped.contains("-1 Hello, world!"),
            "stripped: {stripped}"
        );
        assert!(
            stripped.contains("+1 Hello, testing!"),
            "stripped: {stripped}"
        );
        assert!(lines[0].starts_with(theme.get_bg_ansi("toolSuccessBg")));
    }

    #[test]
    fn preview_error_shows_error_text_and_header_bg() {
        let tmp = TempDir::new();
        std::fs::write(tmp.path().join("src.txt"), "Hello, world!").unwrap();
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = EditToolRenderer;
        let args =
            json!({"path": "src.txt", "edits": [{"oldText": "missing", "newText": "testing"}]});
        let mut ctx = context(&state, args.clone());
        ctx.cwd = tmp.path().to_string_lossy().into_owned();

        let (lines, stripped) = call_component_lines(&renderer, &args, &theme, &ctx);
        assert!(
            stripped.contains("Could not find the exact text in src.txt."),
            "stripped: {stripped}"
        );
        assert!(lines[0].starts_with(theme.get_bg_ansi("toolErrorBg")));
    }

    #[test]
    fn args_key_change_resets_and_recomputes_preview() {
        let tmp = TempDir::new();
        std::fs::write(tmp.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "beta").unwrap();
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = EditToolRenderer;
        let mut ctx = context(&state, json!({}));
        ctx.cwd = tmp.path().to_string_lossy().into_owned();

        let args_a = json!({"path": "a.txt", "edits": [{"oldText": "alpha", "newText": "ALPHA"}]});
        let (_, stripped) = call_component_lines(&renderer, &args_a, &theme, &ctx);
        assert!(stripped.contains("+1 ALPHA"), "stripped: {stripped}");

        let args_b = json!({"path": "b.txt", "edits": [{"oldText": "beta", "newText": "BETA"}]});
        let (_, stripped) = call_component_lines(&renderer, &args_b, &theme, &ctx);
        assert!(stripped.contains("+1 BETA"), "stripped: {stripped}");
        assert!(!stripped.contains("ALPHA"), "stale preview: {stripped}");
    }

    #[test]
    fn result_backfills_preview_and_skips_result_line() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = EditToolRenderer;
        let args =
            json!({"path": "src.txt", "edits": [{"oldText": "world", "newText": "testing"}]});
        let mut ctx = context(&state, args.clone());
        ctx.args_complete = false;

        // No preview computed at call time (args incomplete).
        let (_, stripped) = call_component_lines(&renderer, &args, &theme, &ctx);
        assert!(!stripped.contains("-1 Hello, world!"));

        // The result backfills `details.diff` into the call preview.
        let result = ToolResultState {
            content: vec![],
            is_error: false,
            details: Some(json!({
                "diff": "-1 Hello, world!\n+1 Hello, testing!",
                "firstChangedLine": 1,
                "patch": "--- a\n+++ b\n@@ -1 +1 @@\n-Hello, world!\n+Hello, testing!\n"
            })),
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
        // The result line is empty: the diff equals the backfilled preview.
        assert!(strip_ansi(&component.render(80).join("\n")).is_empty());

        // The call component now carries the backfilled diff.
        let (_, stripped) = call_component_lines(&renderer, &args, &theme, &ctx);
        assert!(
            stripped.contains("-1 Hello, world!"),
            "stripped: {stripped}"
        );
        assert!(
            stripped.contains("+1 Hello, testing!"),
            "stripped: {stripped}"
        );
    }

    #[test]
    fn result_replaces_error_preview_with_result_diff() {
        let tmp = TempDir::new();
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = EditToolRenderer;
        let args =
            json!({"path": "src.txt", "edits": [{"oldText": "world", "newText": "testing"}]});
        let mut ctx = context(&state, args.clone());
        ctx.cwd = tmp.path().to_string_lossy().into_owned();

        // argsComplete + missing file → error preview in the call component.
        let (_, stripped) = call_component_lines(&renderer, &args, &theme, &ctx);
        assert!(
            stripped.contains("Could not edit file: src.txt."),
            "stripped: {stripped}"
        );

        // A successful result replaces the error preview with the result diff
        // (edit.ts:400-411) and renders no result line — on success the diff
        // is only shown in the call preview.
        let result = ToolResultState {
            content: vec![],
            is_error: false,
            details: Some(json!({
                "diff": "-1 Hello, world!\n+1 Hello, testing!",
                "firstChangedLine": 1
            })),
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
        assert!(strip_ansi(&component.render(80).join("\n")).is_empty());

        let (lines, stripped) = call_component_lines(&renderer, &args, &theme, &ctx);
        assert!(
            stripped.contains("-1 Hello, world!"),
            "stripped: {stripped}"
        );
        assert!(
            stripped.contains("+1 Hello, testing!"),
            "stripped: {stripped}"
        );
        assert!(
            !stripped.contains("Could not edit file:"),
            "stripped: {stripped}"
        );
        assert!(lines[0].starts_with(theme.get_bg_ansi("toolSuccessBg")));
    }

    #[test]
    fn settled_error_flips_header_bg() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = EditToolRenderer;
        let args = json!({"path": "src.txt", "edits": [{"oldText": "a", "newText": "b"}]});
        let mut ctx = context(&state, args.clone());
        ctx.args_complete = false;

        // No preview: the header bg follows settledError.
        let (lines, _) = call_component_lines(&renderer, &args, &theme, &ctx);
        assert!(lines[0].starts_with(theme.get_bg_ansi("toolPendingBg")));

        // Error result flips settledError → error bg, and renders the error
        // text (it differs from the absent preview error).
        let err_result = ToolResultState {
            content: vec![ToolResultContentLoose::text(
                "Could not edit file: src.txt. Error code: ENOENT.",
            )],
            is_error: true,
            details: None,
        };
        let mut err_ctx = ctx.clone();
        err_ctx.is_error = true;
        let component = renderer
            .render_result(
                &err_result,
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &err_ctx,
            )
            .expect("error result component");
        let stripped = strip_ansi(&component.render(80).join("\n"));
        assert!(
            stripped.contains("Could not edit file: src.txt. Error code: ENOENT."),
            "stripped: {stripped}"
        );

        let (lines, _) = call_component_lines(&renderer, &args, &theme, &ctx);
        assert!(lines[0].starts_with(theme.get_bg_ansi("toolErrorBg")));

        // Success result flips it back.
        let ok_result = ToolResultState {
            content: vec![],
            is_error: false,
            details: None,
        };
        let _ = renderer.render_result(
            &ok_result,
            ResultRenderOptions {
                expanded: false,
                is_partial: false,
            },
            &theme,
            &ctx,
        );
        let (lines, _) = call_component_lines(&renderer, &args, &theme, &ctx);
        assert!(lines[0].starts_with(theme.get_bg_ansi("toolPendingBg")));
    }

    #[test]
    fn format_edit_result_branches() {
        let theme = theme();
        let args = json!({"path": "a.txt", "edits": [{"oldText": "a", "newText": "b"}]});
        let diff = "-1 a\n+1 b";
        let result = ToolResultState {
            content: vec![],
            is_error: false,
            details: Some(json!({ "diff": diff })),
        };

        // Success, no preview → renders the diff.
        let out = format_edit_result(&args, None, &result, &theme, false).expect("diff output");
        assert_eq!(strip_ansi(&out), diff);
        assert_eq!(out, render_diff(diff, &theme, RenderDiffOptions::default()));

        // Success, preview diff equal → no result line (no change to show).
        let preview = EditPreview::Diff {
            diff: diff.to_string(),
            first_changed_line: Some(1),
        };
        assert!(format_edit_result(&args, Some(&preview), &result, &theme, false).is_none());

        // Success, preview diff different → renders the result diff.
        let other = EditPreview::Diff {
            diff: "-1 x\n+1 y".to_string(),
            first_changed_line: None,
        };
        let out = format_edit_result(&args, Some(&other), &result, &theme, false).expect("diff");
        assert_eq!(strip_ansi(&out), diff);

        // Empty diff string is falsy in JS → no result line.
        let empty = ToolResultState {
            content: vec![],
            is_error: false,
            details: Some(json!({ "diff": "" })),
        };
        assert!(format_edit_result(&args, None, &empty, &theme, false).is_none());

        // Error branch: error text renders when it differs from the preview.
        let error_text = "Could not edit file: a.txt. Error code: EACCES.";
        let err_result = ToolResultState {
            content: vec![ToolResultContentLoose::text(error_text)],
            is_error: true,
            details: None,
        };
        let out = format_edit_result(&args, None, &err_result, &theme, true).expect("error");
        assert_eq!(out, theme.fg("error", error_text));

        // Error with no text output → no result line.
        let empty_err = ToolResultState {
            content: vec![],
            is_error: true,
            details: None,
        };
        assert!(format_edit_result(&args, None, &empty_err, &theme, true).is_none());

        // Error text equal to the preview error → no result line.
        let preview_err = EditPreview::Error {
            error: error_text.to_string(),
        };
        assert!(format_edit_result(&args, Some(&preview_err), &err_result, &theme, true).is_none());

        // Error text differs from the preview error → renders.
        let other_err = EditPreview::Error {
            error: "old error".to_string(),
        };
        let out =
            format_edit_result(&args, Some(&other_err), &err_result, &theme, true).expect("error");
        assert_eq!(out, theme.fg("error", error_text));
    }

    #[test]
    fn format_edit_call_path_variants() {
        let theme = theme();
        // Relative path is shown as given.
        let out = format_edit_call(&json!({"path": "src/main.rs"}), &theme, "/cwd");
        assert_eq!(strip_ansi(&out), "edit src/main.rs", "out: {out}");
        // file_path wins over path for display.
        let out = format_edit_call(
            &json!({"path": "src/main.rs", "file_path": "other.rs"}),
            &theme,
            "/cwd",
        );
        assert_eq!(strip_ansi(&out), "edit other.rs");
        // Missing path → muted `...`; non-string path → invalid-arg text.
        let missing = format_edit_call(
            &json!({"edits": [{"oldText": "a", "newText": "b"}]}),
            &theme,
            "/cwd",
        );
        assert!(strip_ansi(&missing).contains("..."));
        let invalid = format_edit_call(&json!({"path": 42}), &theme, "/cwd");
        assert!(strip_ansi(&invalid).contains("[invalid arg]"));
    }

    #[test]
    fn renderable_preview_input_variants() {
        // Edits array wins over legacy fields.
        let input = get_renderable_preview_input(&json!({
            "path": "a.txt",
            "edits": [{"oldText": "x", "newText": "y"}],
            "oldText": "z",
            "newText": "w"
        }))
        .expect("array input");
        assert_eq!(input.path, "a.txt");
        assert_eq!(input.edits, json!([{"oldText": "x", "newText": "y"}]));
        // The replacement index is the array position.
        let input = get_renderable_preview_input(&json!({
            "path": "a.txt",
            "edits": [
                {"oldText": "x", "newText": "y"},
                {"oldText": "p", "newText": "q"}
            ]
        }))
        .expect("array input");
        assert_eq!(
            input
                .replacements()
                .iter()
                .map(|r| r.edit_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        // Legacy single edit synthesizes a one-entry array.
        let input = get_renderable_preview_input(&json!({
            "path": "a.txt",
            "oldText": "x",
            "newText": "y"
        }))
        .expect("legacy input");
        assert_eq!(input.edits, json!([{"oldText": "x", "newText": "y"}]));
        // file_path is the fallback when path is not a string.
        let input = get_renderable_preview_input(&json!({
            "file_path": "b.txt",
            "edits": [{"oldText": "x", "newText": "y"}]
        }))
        .expect("file_path input");
        assert_eq!(input.path, "b.txt");
        // An empty-string path is falsy in JS (`if (!path) return null`,
        // edit.ts:189) → no preview.
        assert!(get_renderable_preview_input(&json!({
            "path": "",
            "edits": [{"oldText": "x", "newText": "y"}]
        }))
        .is_none());
        // Invalid inputs → None.
        assert!(get_renderable_preview_input(
            &json!({"edits": [{"oldText": "x", "newText": "y"}]})
        )
        .is_none());
        assert!(get_renderable_preview_input(&json!({"path": "a.txt", "edits": "nope"})).is_none());
        assert!(get_renderable_preview_input(&json!({"path": "a.txt", "edits": []})).is_none());
        assert!(get_renderable_preview_input(
            &json!({"path": "a.txt", "edits": [{"oldText": 1, "newText": "y"}]})
        )
        .is_none());
        assert!(get_renderable_preview_input(&json!({"path": "a.txt", "oldText": "x"})).is_none());
        assert!(get_renderable_preview_input(&json!({})).is_none());
    }
}
