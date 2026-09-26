//! Transcript renderCall/renderResult component trees — the rpi#52
//! [RPI-OWN] render governance surface (TE41 FR-A…FR-E).
//!
//! Upstream registers no render hooks for `ask_user_question` (in pi the
//! args never land in the transcript — the questionnaire lives in the
//! interactive overlay only, so the bold-title fallback suffices). rpi's
//! host renders hook-less extension tools through the generic
//! pretty-printed-args branch (render-slot inheritance, kept intentionally
//! — issue #52 "the host behavior stays"), so the plugin provides the
//! hooks itself: this module has **no upstream counterpart** and is out of
//! the parity-harness surface by design ([RPI-OWN] plugin-layer
//! enhancement, task TE41 §5 — no deviation number, rpi-statusline
//! own-surface precedent).
//!
//! Shapes (ComponentTree v1, `rpi.component-tree.v1`):
//! - collapsed call: one `text` node — bold `ask_user_question`
//!   (`toolTitle`) + muted `N questions (Header1, Header2, …)` summary
//!   (issue #52 §1), `truncate` so the summary stays a single line;
//! - expanded call (`context.expanded`, the `Ctrl+O`
//!   `app.tools.expand` toggle): the summary line, a spacer, then one
//!   block per question — `  i. Header — question [multi]` plus
//!   `       j. label — description` option lines (issue #52 §3 example,
//!   minimized per the 2026-09-20 layout decision: no option preview
//!   blocks, no preview markers);
//! - result: one line per state — `✓ N answered` (success, fg
//!   `success`), `declined`/`declined (N partial)` (cancel, fg `muted`),
//!   `✗ <error text>` (fg `error`, wraps); expanded adds per-answer lines
//!   and the global note.
//!
//! Multi-color single lines are not expressible as declarative v1 props
//! (one `fg` per text node, no `row`), so the segments are embedded as
//! ANSI wraps inside the text value — the same channel as the upstream
//! `Text` component's pre-wrapped strings (rpiv-todo `view/format.ts`
//! port precedent in `rpi-ext-todo/src/view.rs`).
//!
//! Streaming tolerance (FR-A): `renderCall` fires while args are still
//! streaming (`context.argsComplete === false`), so every parse here is
//! defensive — missing/partial/malformed `questions` degrade to the bare
//! title node (never an error, never the JSON dump); a truncated array
//! counts what is there and appends `…` until complete. Render failures
//! degrade through the host's per-hook chain to the pi-identical bold
//! title line (FR-D): this module returns `null` for unknown render
//! requests, and a panic is converted to `null` at the dispatch entry
//! (`lib.rs`) — the isError tool-result envelope must never leak into a
//! render answer.
//!
//! All string inputs are model-authored and may arrive mid-stream, so
//! every rendered string passes through [`sanitize_inline`]: line
//! terminators and tabs collapse to spaces and the remaining C0 controls
//! are dropped (single-line integrity for the summary line; the execute
//! path's full #192 normalization is unrelated and runs later).

use serde_json::{json, Value};

use crate::i18n::I18n;
use crate::tool::TOOL_NAME;

// ---------------------------------------------------------------------------
// Theme abstraction (rpi-ext-todo `view.rs` precedent, kept crate-local —
// plugin crates are independent)
// ---------------------------------------------------------------------------

/// The theme surface the renderers use (`fg`/`bold`, mirroring the host
/// `Theme::fg`/`bold` ANSI shapes).
pub trait RenderTheme {
    /// Wrap `text` in a foreground color (semantic token name).
    fn fg(&self, color: &str, text: &str) -> String;
    /// Bold text.
    fn bold(&self, text: &str) -> String;
}

/// Identity theme — the snapshot/test form: every wrap returns the text
/// unchanged (assertions compare plain text).
pub struct IdentityRenderTheme;

impl RenderTheme for IdentityRenderTheme {
    fn fg(&self, _color: &str, text: &str) -> String {
        text.to_owned()
    }
    fn bold(&self, text: &str) -> String {
        text.to_owned()
    }
}

/// Runtime theme over the `ctx.ui.theme` JSON: `colors` maps semantic
/// tokens to a theme var name / hex / 256-index, `vars` maps var names to
/// hex. Colors render as truecolor SGR (the host default
/// `ColorMode::TrueColor`); unresolvable tokens render unstyled — the
/// host's lenient fallback, never an error.
pub struct AnsiRenderTheme {
    prefixes: std::collections::HashMap<String, String>,
}

impl AnsiRenderTheme {
    /// Resolve the prefix table from a `ctx.ui.theme` JSON value
    /// (`null`/non-object → empty table, i.e. fully unstyled).
    pub fn from_theme_json(theme: &Value) -> Self {
        let mut prefixes = std::collections::HashMap::new();
        let Some(colors) = theme.get("colors").and_then(Value::as_object) else {
            return AnsiRenderTheme { prefixes };
        };
        let vars = theme.get("vars").and_then(Value::as_object);
        for (token, value) in colors {
            let prefix = match value {
                Value::String(raw) => {
                    if let Some(hex) = raw.strip_prefix('#') {
                        hex_prefix(hex)
                    } else if raw.is_empty() {
                        // An empty color value is a bare reset.
                        "\x1b[39m".to_owned()
                    } else {
                        // Variable reference: resolve through `vars`.
                        vars.and_then(|vars| vars.get(raw))
                            .and_then(|resolved| match resolved {
                                Value::String(hex) => hex.strip_prefix('#').map(hex_prefix),
                                _ => None,
                            })
                            .unwrap_or_default()
                    }
                }
                Value::Number(number) => number
                    .as_u64()
                    .filter(|index| *index <= 255)
                    .map(|index| format!("\x1b[38;5;{index}m"))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            prefixes.insert(token.clone(), prefix);
        }
        AnsiRenderTheme { prefixes }
    }
}

/// `#rrggbb` → truecolor SGR prefix (host `fg_ansi` truecolor arm).
fn hex_prefix(hex: &str) -> String {
    if hex.len() == 6 {
        if let (Some(r), Some(g), Some(b)) = (
            u8::from_str_radix(&hex[0..2], 16).ok(),
            u8::from_str_radix(&hex[2..4], 16).ok(),
            u8::from_str_radix(&hex[4..6], 16).ok(),
        ) {
            return format!("\x1b[38;2;{r};{g};{b}m");
        }
    }
    String::new()
}

impl RenderTheme for AnsiRenderTheme {
    fn fg(&self, color: &str, text: &str) -> String {
        let prefix = self.prefixes.get(color).map(String::as_str).unwrap_or("");
        format!("{prefix}{text}\x1b[39m")
    }
    fn bold(&self, text: &str) -> String {
        format!("\x1b[1m{text}\x1b[22m")
    }
}

// ---------------------------------------------------------------------------
// Defensive parsing (streaming tolerance)
// ---------------------------------------------------------------------------

/// One question as far as the render can see it. `header`/`question`/
/// `options` are `None` when the streaming payload has not produced a
/// usable value yet — the summary still counts the item, the expanded
/// view skips its body block.
struct ParsedQuestion {
    header: Option<String>,
    question: Option<String>,
    options: Vec<Option<(String, String)>>,
    multi: bool,
}

/// Parse the render-side view of `args.questions`. `None` = no usable
/// array at all (missing / not an array / empty) → the caller falls back
/// to the bare title node.
fn parse_questions(args: &Value) -> Option<Vec<ParsedQuestion>> {
    let items = args.get("questions")?.as_array()?;
    if items.is_empty() {
        return None;
    }
    Some(
        items
            .iter()
            .map(|item| {
                let object = item.as_object();
                let string_field = |key: &str| {
                    object
                        .and_then(|object| object.get(key))
                        .and_then(Value::as_str)
                        // JS truthiness: an empty string is falsy and does
                        // not contribute (the schema requires non-empty
                        // anyway; a partial stream may still carry one).
                        .filter(|text| !text.is_empty())
                        .map(sanitize_inline)
                };
                ParsedQuestion {
                    header: string_field("header"),
                    question: string_field("question"),
                    options: object
                        .and_then(|object| object.get("options"))
                        .and_then(Value::as_array)
                        .map(|options| {
                            options
                                .iter()
                                .map(|option| {
                                    let label = option
                                        .get("label")
                                        .and_then(Value::as_str)
                                        .filter(|label| !label.is_empty())
                                        .map(sanitize_inline)?;
                                    let description = option
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .map(sanitize_inline)
                                        .unwrap_or_default();
                                    Some((label, description))
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    multi: object
                        .and_then(|object| object.get("multiSelect"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }
            })
            .collect(),
    )
}

/// Collapse line terminators/tabs to spaces and drop the remaining C0
/// controls so a mid-stream `\r` can never fragment the rendered line
/// (render runs before the execute path's #192 normalization).
fn sanitize_inline(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

/// The `N question(s)` count segment (`render.question` / `render.questions`).
fn count_segment(i18n: &I18n, count: usize) -> String {
    let key = if count == 1 {
        "render.question"
    } else {
        "render.questions"
    };
    i18n.t(key, "{count} questions")
        .replace("{count}", &count.to_string())
}

/// The bare title node — the pi-identical fallback shape (`createCallFallback`
/// renders the same bold tool name; here the plugin's own floor for
/// unusable args, so the host chain and this floor converge visually).
fn title_node(theme: &dyn RenderTheme) -> Value {
    json!({
        "type": "text",
        "props": { "text": title_text(theme) }
    })
}

fn title_text(theme: &dyn RenderTheme) -> String {
    theme.fg("toolTitle", &theme.bold(TOOL_NAME))
}

/// The collapsed summary line text:
/// `{bold tool} {muted: N questions (H1, H2, …)}` (issue #52 §1).
fn summary_text(
    questions: &[ParsedQuestion],
    args_complete: bool,
    theme: &dyn RenderTheme,
    i18n: &I18n,
) -> String {
    let mut headers: Vec<String> = questions
        .iter()
        .filter_map(|question| question.header.clone())
        .collect();
    if !args_complete {
        headers.push("…".to_owned());
    }
    let summary = if headers.is_empty() {
        // Complete args always carry headers (schema-required); an empty
        // header list therefore only occurs mid-stream before the first
        // header arrives — count only.
        count_segment(i18n, questions.len())
    } else {
        format!(
            "{} ({})",
            count_segment(i18n, questions.len()),
            headers.join(", ")
        )
    };
    format!("{} {}", title_text(theme), theme.fg("muted", &summary))
}

/// Whether a question has enough shape to render an expanded block
/// (header + question text; options are optional lines within the block).
fn expandable(question: &ParsedQuestion) -> bool {
    question.header.is_some() && question.question.is_some()
}

// ---------------------------------------------------------------------------
// renderCall (FR-A collapsed / FR-B expanded)
// ---------------------------------------------------------------------------

/// The `renderCall` tree. Collapsed (default): the single summary line
/// (`truncate` keeps it one line at narrow widths, ANSI-preserving).
/// Expanded (`context.expanded`, the existing `Ctrl+O` toggle): summary +
/// per-question detail blocks (issue #52 §3 example, minimized).
pub fn render_call(args: &Value, context: &Value, theme: &dyn RenderTheme, i18n: &I18n) -> Value {
    let Some(questions) = parse_questions(args) else {
        return title_node(theme);
    };
    let args_complete = context
        .get("argsComplete")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let expanded = context
        .get("expanded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let summary = summary_text(&questions, args_complete, theme, i18n);
    if !expanded {
        return json!({
            "type": "text",
            "props": { "text": summary, "truncate": true }
        });
    }

    let mut children = vec![
        json!({ "type": "text", "props": { "text": summary, "truncate": true } }),
        json!({ "type": "spacer", "props": { "lines": 1 } }),
    ];
    for (index, question) in questions.iter().enumerate() {
        if !expandable(question) {
            // A mid-stream partial question body is skipped; the trailing
            // `…` line below marks the open tail while args stream.
            continue;
        }
        let header = question.header.clone().unwrap_or_default();
        let text = question.question.clone().unwrap_or_default();
        let mut line = format!(
            "  {}. {} — {}",
            index + 1,
            theme.fg("accent", &header),
            text
        );
        if question.multi {
            line.push(' ');
            line.push_str(&theme.fg("dim", i18n.t("render.multi", "[multi]")));
        }
        children.push(json!({ "type": "text", "props": { "text": line } }));
        for (option_index, option) in question.options.iter().enumerate() {
            let Some((label, description)) = option else {
                continue;
            };
            let option_line = if description.is_empty() {
                format!("       {}. {}", option_index + 1, label)
            } else {
                format!(
                    "       {}. {} — {}",
                    option_index + 1,
                    label,
                    theme.fg("dim", description)
                )
            };
            children.push(json!({ "type": "text", "props": { "text": option_line } }));
        }
    }
    if !args_complete {
        children.push(json!({
            "type": "text",
            "props": { "text": theme.fg("dim", "…") }
        }));
    }
    json!({ "type": "column", "props": {}, "children": children })
}

// ---------------------------------------------------------------------------
// renderResult (FR-C)
// ---------------------------------------------------------------------------

/// The first text block of a tool result (`content[0].text` shape).
fn result_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks.iter().find_map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            })
        })
        .map(sanitize_inline)
        .unwrap_or_default()
}

/// The answers array of a questionnaire result envelope (`details.answers`).
fn result_answers(result: &Value) -> &[Value] {
    result
        .get("details")
        .and_then(|details| details.get("answers"))
        .and_then(Value::as_array)
        .map(|answers| answers.as_slice())
        .unwrap_or(&[])
}

/// `format_answer_scalar`-equivalent over the answer JSON (kind: option →
/// `answer`; custom → `answer` or `(no input)`; multi → `selected` joined
/// or `(no input)`).
fn answer_scalar(answer: &Value) -> String {
    match answer.get("kind").and_then(Value::as_str) {
        Some("multi") => answer
            .get("selected")
            .and_then(Value::as_array)
            .filter(|selected| !selected.is_empty())
            .map(|selected| {
                // Module invariant: EVERY rendered string passes through
                // sanitize_inline — the joined selection included (a
                // replayed/hand-built envelope can carry raw control
                // characters in any answer field).
                sanitize_inline(
                    &selected
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            })
            .unwrap_or_else(|| crate::tool::envelope::NO_INPUT_PLACEHOLDER.to_owned()),
        Some("custom") => answer
            .get("answer")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(sanitize_inline)
            .unwrap_or_else(|| crate::tool::envelope::NO_INPUT_PLACEHOLDER.to_owned()),
        // Option answers: `a.answer ?? NO_INPUT_PLACEHOLDER` (an
        // empty-string option answer stays empty).
        _ => answer
            .get("answer")
            .and_then(Value::as_str)
            .map(sanitize_inline)
            .unwrap_or_else(|| crate::tool::envelope::NO_INPUT_PLACEHOLDER.to_owned()),
    }
}

/// One per-answer line for the expanded result:
/// `{glyph} {question} = {answer}`.
fn answer_line(glyph: &str, answer: &Value, color: &str, theme: &dyn RenderTheme) -> Value {
    let question = answer
        .get("question")
        .and_then(Value::as_str)
        .map(sanitize_inline)
        .unwrap_or_default();
    let text = format!("{glyph} {question} = {}", answer_scalar(answer));
    json!({
        "type": "text",
        "props": { "text": theme.fg(color, &text) }
    })
}

/// The `renderResult` tree — the three states of issue #52 §4 plus the
/// minimal expanded detail (per-answer lines + global note). Collapsed
/// states are one short line each; the error line wraps (no truncate) so
/// the failure text stays readable.
pub fn render_result(
    result: &Value,
    options: &Value,
    context: &Value,
    theme: &dyn RenderTheme,
    i18n: &I18n,
) -> Value {
    let is_error = context
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_error {
        let text = result_text(result);
        let line = if text.is_empty() {
            "✗".to_owned()
        } else {
            format!("✗ {text}")
        };
        return json!({
            "type": "text",
            "props": { "text": theme.fg("error", &line) }
        });
    }

    let cancelled = result
        .get("details")
        .and_then(|details| details.get("cancelled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let answers = result_answers(result);
    let expanded = options
        .get("expanded")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if cancelled {
        let headline = if answers.is_empty() {
            i18n.t("render.declined", "declined").to_owned()
        } else {
            i18n.t("render.declined_partial", "declined ({count} partial)")
                .replace("{count}", &answers.len().to_string())
        };
        if !expanded {
            return json!({
                "type": "text",
                "props": { "text": theme.fg("muted", &headline) }
            });
        }
        let mut children = vec![json!({
            "type": "text",
            "props": { "text": theme.fg("muted", &headline) }
        })];
        for answer in answers {
            children.push(answer_line("·", answer, "muted", theme));
        }
        return json!({ "type": "column", "props": {}, "children": children });
    }

    let headline = i18n
        .t("render.answered", "✓ {count} answered")
        .replace("{count}", &answers.len().to_string());
    if !expanded {
        return json!({
            "type": "text",
            "props": { "text": theme.fg("success", &headline) }
        });
    }
    let mut children = vec![json!({
        "type": "text",
        "props": { "text": theme.fg("success", &headline) }
    })];
    for answer in answers {
        children.push(answer_line("✓", answer, "success", theme));
    }
    if let Some(note) = result
        .get("details")
        .and_then(|details| details.get("globalNote"))
        .and_then(Value::as_str)
        .filter(|note| !note.is_empty())
    {
        let note = i18n
            .t("render.global_note", "global note: {text}")
            .replace("{text}", &sanitize_inline(note));
        children.push(json!({
            "type": "text",
            "props": { "text": theme.fg("dim", &note) }
        }));
    }
    json!({ "type": "column", "props": {}, "children": children })
}

// ---------------------------------------------------------------------------
// Dispatch arm
// ---------------------------------------------------------------------------

/// Test seam for the FR-D panic red line: when set, the render dispatch
/// panics on entry so the guard at the `rpi_dispatch` boundary (which must
/// answer `null`, not the isError envelope) is observable.
#[cfg(test)]
pub(crate) static FORCE_PANIC: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The `"render"` dispatch (`{"kind":"render","what":…}`): resolves the
/// theme (`ui.theme`, a synchronous read — the rpiv-todo render-dispatch
/// precedent) and the process locale, then routes to the pure builders.
/// Unknown `what`/`toolName` answer `null` — the host `render_call`
/// closure turns `null` into an error and degrades to the per-hook
/// fallback chain (the pi-identical title line), never the JSON dump.
pub fn dispatch_render(host: &dyn crate::HostCall, message: &Value) -> Value {
    #[cfg(test)]
    {
        if FORCE_PANIC.load(std::sync::atomic::Ordering::SeqCst) {
            panic!("render dispatch forced panic (FR-D red-line seam)");
        }
    }
    if message.get("toolName").and_then(Value::as_str) != Some(TOOL_NAME) {
        return Value::Null;
    }
    let theme = match host.call("ui.theme", json!({})) {
        Ok(theme) => AnsiRenderTheme::from_theme_json(&theme),
        Err(_) => AnsiRenderTheme::from_theme_json(&Value::Null),
    };
    let i18n = I18n::detect();
    match message.get("what").and_then(Value::as_str) {
        Some("toolCall") => {
            let context = message.get("context").cloned().unwrap_or(Value::Null);
            let args = context.get("args").cloned().unwrap_or(Value::Null);
            render_call(&args, &context, &theme, &i18n)
        }
        Some("toolResult") => {
            let result = message.get("result").cloned().unwrap_or(Value::Null);
            let options = message.get("options").cloned().unwrap_or(Value::Null);
            let context = message.get("context").cloned().unwrap_or(Value::Null);
            render_result(&result, &options, &context, &theme, &i18n)
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn en() -> I18n {
        I18n::for_locale("en")
    }

    fn three_questions() -> Value {
        // The issue #52 §1 example: Scope / Priority / Framework.
        json!({
            "questions": [
                {
                    "question": "Which storage backend should the CLI default to?",
                    "header": "Scope",
                    "options": [
                        {"label": "Local SQLite", "description": "zero setup, single machine"},
                        {"label": "Postgres", "description": "networked, concurrent access"}
                    ]
                },
                {
                    "question": "How urgent is the migration?",
                    "header": "Priority",
                    "options": [
                        {"label": "Now", "description": "this sprint"},
                        {"label": "Later", "description": "next quarter"}
                    ]
                },
                {
                    "question": "Which UI framework?",
                    "header": "Framework",
                    "multiSelect": true,
                    "options": [
                        {"label": "React", "description": ""},
                        {"label": "Vue", "description": ""}
                    ]
                }
            ]
        })
    }

    fn complete_context(expanded: bool) -> Value {
        json!({
            "toolCallId": "call-1",
            "cwd": "/tmp",
            "executionStarted": true,
            "argsComplete": true,
            "isPartial": false,
            "expanded": expanded,
            "showImages": false,
            "isError": false,
            "terminalWidth": 100
        })
    }

    fn text_of(node: &Value) -> &str {
        node["props"]["text"].as_str().unwrap_or("")
    }

    /// Strip the wrapping codes an unstyled `AnsiRenderTheme` still emits
    /// (`\x1b[1m/22m` bold, bare `\x1b[39m` resets — the host `Theme`
    /// channel shape, rpiv-todo overlay.rs test precedent).
    fn strip_ansi(line: &str) -> String {
        line.replace("\u{1b}[39m", "")
            .replace("\u{1b}[22m", "")
            .replace("\u{1b}[1m", "")
    }

    // ===== FR-A: collapsed summary snapshots =====

    #[test]
    fn collapsed_summary_matches_issue_example() {
        let tree = render_call(
            &three_questions(),
            &complete_context(false),
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(
            tree,
            json!({
                "type": "text",
                "props": {
                    "text": "ask_user_question 3 questions (Scope, Priority, Framework)",
                    "truncate": true
                }
            })
        );
    }

    #[test]
    fn collapsed_summary_singular_and_tree_is_width_independent() {
        let args = json!({"questions": [{
            "question": "Pick?", "header": "Pick",
            "options": [{"label": "A", "description": "a"}, {"label": "B", "description": "b"}]
        }]});
        let narrow = complete_context(false);
        let wide = json!({
            "toolCallId": "call-1", "cwd": "/tmp", "executionStarted": true,
            "argsComplete": true, "isPartial": false, "expanded": false,
            "showImages": false, "isError": false, "terminalWidth": 200
        });
        let no_width = json!({
            "toolCallId": "call-1", "cwd": "/tmp", "executionStarted": true,
            "argsComplete": true, "isPartial": false, "expanded": false,
            "showImages": false, "isError": false
        });
        let tree = render_call(&args, &narrow, &IdentityRenderTheme, &en());
        assert_eq!(text_of(&tree), "ask_user_question 1 question (Pick)");
        // The tree is width-independent: truncation is the host's
        // `truncate` prop clipping at the live render width.
        assert_eq!(tree, render_call(&args, &wide, &IdentityRenderTheme, &en()));
        assert_eq!(
            tree,
            render_call(&args, &no_width, &IdentityRenderTheme, &en())
        );
    }

    #[test]
    fn collapsed_summary_localizes_count_copy() {
        let zh = I18n::for_locale("zh");
        let tree = render_call(
            &three_questions(),
            &complete_context(false),
            &IdentityRenderTheme,
            &zh,
        );
        assert_eq!(
            text_of(&tree),
            "ask_user_question 3 个问题 (Scope, Priority, Framework)"
        );
    }

    // ===== FR-A: streaming tolerance matrix (argsComplete=false) =====

    #[test]
    fn streaming_args_count_what_is_there_and_ellipsis_until_complete() {
        let mut context = complete_context(false);
        context["argsComplete"] = json!(false);
        let tree = render_call(&three_questions(), &context, &IdentityRenderTheme, &en());
        assert_eq!(
            text_of(&tree),
            "ask_user_question 3 questions (Scope, Priority, Framework, …)"
        );

        // A truncated array simply counts fewer questions.
        let mut truncated = three_questions();
        truncated["questions"].as_array_mut().unwrap().pop();
        let tree = render_call(&truncated, &context, &IdentityRenderTheme, &en());
        assert_eq!(
            text_of(&tree),
            "ask_user_question 2 questions (Scope, Priority, …)"
        );
    }

    #[test]
    fn malformed_or_missing_questions_fall_back_to_bare_title() {
        for args in [
            Value::Null,
            json!({}),
            json!({"questions": null}),
            json!({"questions": "not-an-array"}),
            json!({"questions": []}),
            json!({"questions": {}}),
        ] {
            let tree = render_call(&args, &complete_context(false), &IdentityRenderTheme, &en());
            assert_eq!(
                tree,
                json!({
                    "type": "text",
                    "props": {"text": "ask_user_question"}
                }),
                "args {args} must degrade to the bare title node"
            );
        }
    }

    #[test]
    fn malformed_items_count_without_headers_and_stream_mid_object() {
        let mut context = complete_context(false);
        context["argsComplete"] = json!(false);
        // One complete question + one half-streamed object (no header yet)
        // + one non-object item: count 3, headers Scope only, ellipsis
        // while streaming.
        let args = json!({
            "questions": [
                {"question": "Pick?", "header": "Scope", "options": []},
                {"question": "partial"},
                "garbage"
            ]
        });
        let tree = render_call(&args, &context, &IdentityRenderTheme, &en());
        assert_eq!(text_of(&tree), "ask_user_question 3 questions (Scope, …)");

        // Mid-stream before any header arrives: count + the open tail.
        let args = json!({"questions": [{"question": "partial"}]});
        let tree = render_call(&args, &context, &IdentityRenderTheme, &en());
        assert_eq!(text_of(&tree), "ask_user_question 1 question (…)");

        // The same payload once complete keeps the (schema-invalid)
        // headerless shape without the ellipsis.
        let args = json!({"questions": [{"question": "partial"}]});
        let tree = render_call(&args, &complete_context(false), &IdentityRenderTheme, &en());
        assert_eq!(text_of(&tree), "ask_user_question 1 question");
    }

    #[test]
    fn control_characters_never_fragment_the_summary_line() {
        let mut args = three_questions();
        args["questions"][0]["header"] = json!("Sc\r\nop\te");
        args["questions"][0]["question"] = json!("Which \u{0000}backend?");
        let tree = render_call(&args, &complete_context(false), &IdentityRenderTheme, &en());
        // Every control char maps 1:1 to a space (no run collapsing —
        // the render only guarantees single-line integrity).
        assert_eq!(
            text_of(&tree),
            "ask_user_question 3 questions (Sc  op e, Priority, Framework)"
        );
        assert!(!text_of(&tree).contains('\r'));
        assert!(!text_of(&tree).contains('\n'));
    }

    // ===== FR-B: expanded detail view =====

    #[test]
    fn expanded_tree_matches_issue_example_shape() {
        let tree = render_call(
            &three_questions(),
            &complete_context(true),
            &IdentityRenderTheme,
            &en(),
        );
        // Root is a column: summary + spacer + per-question blocks.
        assert_eq!(tree["type"], "column");
        let children = tree["children"].as_array().expect("children");
        // summary + spacer + (1 line + 2 options) × 3 questions.
        assert_eq!(children.len(), 2 + 9);
        assert_eq!(
            text_of(&children[0]),
            "ask_user_question 3 questions (Scope, Priority, Framework)"
        );
        assert_eq!(children[1]["props"]["lines"], json!(1));
        assert_eq!(
            text_of(&children[2]),
            "  1. Scope — Which storage backend should the CLI default to?"
        );
        assert_eq!(
            text_of(&children[3]),
            "       1. Local SQLite — zero setup, single machine"
        );
        assert_eq!(
            text_of(&children[4]),
            "       2. Postgres — networked, concurrent access"
        );
        assert_eq!(
            text_of(&children[5]),
            "  2. Priority — How urgent is the migration?"
        );
        // multiSelect question carries the localized marker at the line end.
        assert_eq!(
            text_of(&children[8]),
            "  3. Framework — Which UI framework? [multi]"
        );
        // An empty description renders label only (no dangling dash).
        assert_eq!(text_of(&children[9]), "       1. React");
    }

    #[test]
    fn expanded_tree_multi_marker_localizes() {
        let tree = render_call(
            &three_questions(),
            &complete_context(true),
            &IdentityRenderTheme,
            &I18n::for_locale("zh"),
        );
        let children = tree["children"].as_array().expect("children");
        assert_eq!(
            text_of(&children[8]),
            "  3. Framework — Which UI framework? [多选]"
        );
    }

    #[test]
    fn expanded_streaming_skips_partial_bodies_and_marks_the_tail() {
        let mut context = complete_context(true);
        context["argsComplete"] = json!(false);
        let mut args = three_questions();
        args["questions"].as_array_mut().unwrap().truncate(2);
        args["questions"]
            .as_array_mut()
            .unwrap()
            .push(json!({"question": "half"}));
        let tree = render_call(&args, &context, &IdentityRenderTheme, &en());
        let children = tree["children"].as_array().expect("children");
        let last = children.last().expect("tail marker");
        assert_eq!(text_of(last), "…");
        // The half-streamed question contributes no body block:
        // summary + spacer + (1+2) + (1+2) + tail marker.
        assert_eq!(children.len(), 2 + 6 + 1);
    }

    #[test]
    fn expanded_tree_uses_ansi_theme_channels() {
        let theme = AnsiRenderTheme::from_theme_json(&json!({
            "colors": {
                "toolTitle": "#ff0000",
                "muted": "mutedVar",
                "accent": "#00ff00",
                "dim": "dimVar"
            },
            "vars": {"mutedVar": "#808080", "dimVar": "#404040"}
        }));
        let tree = render_call(&three_questions(), &complete_context(false), &theme, &en());
        assert_eq!(
            text_of(&tree),
            "\u{1b}[38;2;255;0;0m\u{1b}[1mask_user_question\u{1b}[22m\u{1b}[39m \
             \u{1b}[38;2;128;128;128m3 questions (Scope, Priority, Framework)\u{1b}[39m"
        );
        let expanded = render_call(&three_questions(), &complete_context(true), &theme, &en());
        assert_eq!(
            text_of(&expanded["children"][2]),
            "  1. \u{1b}[38;2;0;255;0mScope\u{1b}[39m — Which storage backend should the CLI default to?"
        );
        // Unresolvable tokens render unstyled (lenient fallback; the bare
        // resets the channel always emits are invisible).
        let bare = AnsiRenderTheme::from_theme_json(&Value::Null);
        let tree = render_call(&three_questions(), &complete_context(false), &bare, &en());
        assert_eq!(
            strip_ansi(text_of(&tree)),
            "ask_user_question 3 questions (Scope, Priority, Framework)"
        );
    }

    #[test]
    fn multi_selected_control_characters_are_sanitized() {
        // Regression: the multi arm's joined `selected` used to bypass
        // `sanitize_inline` (the module invariant) — a replayed/hand-built
        // envelope could fragment the rendered line with raw `\r`/`\n`.
        let scalar = answer_scalar(&json!({
            "questionIndex": 0,
            "question": "Q?",
            "kind": "multi",
            "selected": ["a\rb", "c\nd\te"]
        }));
        assert_eq!(scalar, "a b, c d e");
        assert!(!scalar.contains('\r'));
        assert!(!scalar.contains('\n'));
        assert!(!scalar.contains('\t'));
    }

    // ===== FR-C: renderResult states =====

    fn answered_result(answers: Value, global_note: Option<&str>) -> Value {
        let mut details = json!({"answers": answers, "cancelled": false});
        if let Some(note) = global_note {
            details["globalNote"] = json!(note);
        }
        json!({
            "content": [{
                "type": "text",
                "text": "User has answered your questions: \"Q?\"=\"A\". You can now continue with the user's answers in mind."
            }],
            "details": details
        })
    }

    #[test]
    fn result_success_single_line_and_expanded_detail() {
        let answers = json!([
            {"questionIndex": 0, "question": "Q1?", "kind": "option", "answer": "A"},
            {"questionIndex": 1, "question": "Q2?", "kind": "custom", "answer": "typed"},
            {"questionIndex": 2, "question": "Q3?", "kind": "multi", "selected": ["X", "Y"]}
        ]);
        let result = answered_result(answers, Some("see note"));
        let tree = render_result(
            &result,
            &json!({"expanded": false, "isPartial": false}),
            &complete_context(false),
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(
            tree,
            json!({"type": "text", "props": {"text": "✓ 3 answered"}})
        );

        let expanded = render_result(
            &result,
            &json!({"expanded": true, "isPartial": false}),
            &complete_context(false),
            &IdentityRenderTheme,
            &en(),
        );
        let children = expanded["children"].as_array().expect("children");
        assert_eq!(text_of(&children[0]), "✓ 3 answered");
        assert_eq!(text_of(&children[1]), "✓ Q1? = A");
        assert_eq!(text_of(&children[2]), "✓ Q2? = typed");
        // The multi answer renders the canonical scalar (joined labels).
        assert_eq!(text_of(&children[3]), "✓ Q3? = X, Y");
        assert_eq!(text_of(&children[4]), "global note: see note");
    }

    #[test]
    fn result_cancelled_declined_and_partial_shapes() {
        let declined = json!({
            "content": [{"type": "text", "text": "User declined to answer questions"}],
            "details": {"answers": [], "cancelled": true}
        });
        let tree = render_result(
            &declined,
            &json!({"expanded": false}),
            &complete_context(false),
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(text_of(&tree), "declined");

        let partial = json!({
            "content": [{"type": "text", "text": "User declined to answer questions"}],
            "details": {
                "answers": [{"questionIndex": 0, "question": "Q1?", "kind": "option", "answer": "A"}],
                "cancelled": true
            }
        });
        let tree = render_result(
            &partial,
            &json!({"expanded": false}),
            &complete_context(false),
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(text_of(&tree), "declined (1 partial)");
        let expanded = render_result(
            &partial,
            &json!({"expanded": true}),
            &complete_context(false),
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(text_of(&expanded["children"][1]), "· Q1? = A");
    }

    #[test]
    fn result_error_carries_error_text_and_localizes() {
        let error = json!({
            "content": [{
                "type": "text",
                "text": "Error: UI not available (running in non-interactive mode)"
            }],
            "details": {"answers": [], "cancelled": true}
        });
        let mut context = complete_context(false);
        context["isError"] = json!(true);
        let tree = render_result(
            &error,
            &json!({"expanded": false}),
            &context,
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(
            text_of(&tree),
            "✗ Error: UI not available (running in non-interactive mode)"
        );

        // An error result without text degrades to the bare glyph.
        let tree = render_result(
            &json!({"content": [], "details": {}}),
            &json!({"expanded": false}),
            &context,
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(text_of(&tree), "✗");

        // Success/declined copy localizes.
        let zh = I18n::for_locale("zh");
        let answered = answered_result(
            json!([{"questionIndex": 0, "question": "Q?", "kind": "option", "answer": "A"}]),
            None,
        );
        let tree = render_result(
            &answered,
            &json!({"expanded": false}),
            &complete_context(false),
            &IdentityRenderTheme,
            &zh,
        );
        assert_eq!(text_of(&tree), "✓ 已回答 1 题");
    }

    #[test]
    fn result_answer_scalars_match_envelope_semantics() {
        // Empty custom answer and empty multi → the envelope placeholder.
        let answers = json!([
            {"questionIndex": 0, "question": "Q1?", "kind": "custom", "answer": ""},
            {"questionIndex": 1, "question": "Q2?", "kind": "multi", "selected": []}
        ]);
        let result = answered_result(answers, None);
        let expanded = render_result(
            &result,
            &json!({"expanded": true}),
            &complete_context(false),
            &IdentityRenderTheme,
            &en(),
        );
        let children = expanded["children"].as_array().expect("children");
        assert_eq!(text_of(&children[1]), "✓ Q1? = (no input)");
        assert_eq!(text_of(&children[2]), "✓ Q2? = (no input)");
    }

    #[test]
    fn theme_wraps_result_lines_in_color() {
        let theme = AnsiRenderTheme::from_theme_json(&json!({
            "colors": {"success": "#00ff00", "error": "#ff0000", "muted": "#808080"}
        }));
        let result = answered_result(
            json!([{"questionIndex": 0, "question": "Q?", "kind": "option", "answer": "A"}]),
            None,
        );
        let tree = render_result(
            &result,
            &json!({"expanded": false}),
            &complete_context(false),
            &theme,
            &en(),
        );
        assert_eq!(text_of(&tree), "\u{1b}[38;2;0;255;0m✓ 1 answered\u{1b}[39m");
    }

    // ===== FR-D: dispatch nulls (the host chain degrades to the title) =====

    #[test]
    fn dispatch_render_answers_null_for_foreign_tools_and_kinds() {
        assert_eq!(
            dispatch_render(
                &FailingHost,
                &json!({"kind": "render", "what": "toolCall", "toolName": "other"}),
            ),
            Value::Null
        );
        assert_eq!(
            dispatch_render(
                &FailingHost,
                &json!({"kind": "render", "what": "unknown", "toolName": TOOL_NAME}),
            ),
            Value::Null
        );
    }

    /// A host whose every call fails (`ui.theme` unreachable): the render
    /// still succeeds with the unstyled theme — a theme read failure must
    /// not kill the render (lenient fallback).
    struct FailingHost;

    impl crate::HostCall for FailingHost {
        fn call(&self, _method: &str, _args: Value) -> Result<Value, crate::HostError> {
            Err(crate::HostError {
                kind: "unknownMethod".to_owned(),
                message: "no host".to_owned(),
            })
        }
    }

    #[test]
    fn dispatch_render_survives_theme_read_failure() {
        let tree = dispatch_render(
            &FailingHost,
            &json!({
                "kind": "render", "what": "toolCall", "toolName": TOOL_NAME,
                "context": {"args": three_questions(), "argsComplete": true, "expanded": false}
            }),
        );
        assert_eq!(
            strip_ansi(text_of(&tree)),
            "ask_user_question 3 questions (Scope, Priority, Framework)"
        );
    }
}
