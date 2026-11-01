//! Transcript renderCall/renderResult component trees.
//!
//! v0.1.8 (TE51, FR-G): adopted the upstream v2.15.0 receipt
//! (`packages/rpiv-ask-user-question/view/transcript.ts`, #231/#292). The
//! call line is bold `Ask` plus the question headers joined with ` · `; the
//! result is one `✓ answer` line for a single question, an aligned
//! header/answer table for several, `✗ <first line>` for a failure and a
//! dimmed `✗ no answer` for a declined questionnaire, with notes, previews
//! and the global note as detail lines. Layout runs at the render context's
//! `terminalWidth` (hanging wrap/truncate via `rpi_tui::utils`).
//!
//! G2: this replaces the TE41 (rpi#52) rpi-own shape — a collapsed
//! `N questions (headers)` summary and per-answer `question = answer` lines —
//! with the upstream receipt; the change is registered in the TE51 changes
//! entry.
//!
//! Defensive parsing still matters: the call hook fires while args stream
//! and both hooks may see partial envelopes, so every renderer degrades to
//! the bare title/check instead of dumping JSON. Render panics answer
//! `null` at the dispatch entry, never the isError envelope.

use serde_json::{Value, json};

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
    if hex.len() == 6
        && let (Some(r), Some(g), Some(b)) = (
            u8::from_str_radix(&hex[0..2], 16).ok(),
            u8::from_str_radix(&hex[2..4], 16).ok(),
            u8::from_str_radix(&hex[4..6], 16).ok(),
        )
    {
        return format!("\x1b[38;2;{r};{g};{b}m");
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
                let _ = object.and_then(|object| object.get("options"));
                ParsedQuestion {
                    header: string_field("header"),
                    question: string_field("question"),
                }
            })
            .collect(),
    )
}

/// Collapse line terminators/tabs to spaces and replace the remaining
/// C0 controls with a space so a mid-stream `\r` can never fragment the
/// rendered line
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

/// Terminal width for the receipt layout: the render context's viewport
/// width (rpi extension), falling back to 80 columns.
fn render_width(context: &Value) -> usize {
    context
        .get("terminalWidth")
        .and_then(Value::as_u64)
        .map(|width| width as usize)
        .filter(|width| *width > 0)
        .unwrap_or(80)
}

/// One line truncated to the width (`truncateToWidth(text, width, "…")`).
fn truncate_line(text: &str, width: usize) -> String {
    rpi_tui::utils::truncate_to_width(text, width, "…", false)
}

/// `prefix` then `text` wrapped to the width left after it; continuation
/// lines align under the text (`hanging` in transcript.ts).
fn hanging(prefix: &str, text: &str, width: usize) -> Vec<String> {
    let indent = rpi_tui::utils::visible_width(prefix);
    let bound = width.saturating_sub(indent).max(1);
    rpi_tui::utils::wrap_text_with_ansi(text, bound)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                format!("{prefix}{line}")
            } else {
                format!("{}{line}", " ".repeat(indent))
            }
        })
        .collect()
}

/// `label` (transcript.ts:147): drop the author's `(Recommended)` suffix.
fn strip_recommended(text: &str) -> String {
    const SUFFIX: &str = "(Recommended)";
    let trimmed = text.trim_end();
    if let Some(head) = trimmed.strip_suffix(SUFFIX) {
        head.trim_end().to_string()
    } else {
        text.to_string()
    }
}

/// `answerText` (transcript.ts:129-140): the option labels (or the custom
/// text, quoted) for one answer; `None`/missing renders the dim
/// `not answered` marker.
fn answer_text(answer: Option<&Value>, theme: &dyn RenderTheme, i18n: &I18n) -> String {
    let Some(answer) = answer else {
        return theme.fg("dim", i18n.t("transcript.not_answered", "not answered"));
    };
    let typed = answer
        .get("answer")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| format!("\"{}\"", sanitize_inline(text)));
    let picks: Vec<String> = match answer.get("kind").and_then(Value::as_str) {
        Some("option") => vec![strip_recommended(
            answer.get("answer").and_then(Value::as_str).unwrap_or(""),
        )],
        Some("custom") => typed.into_iter().collect(),
        Some("multi") => {
            let mut picks: Vec<String> = answer
                .get("selected")
                .and_then(Value::as_array)
                .map(|selected| {
                    selected
                        .iter()
                        .filter_map(Value::as_str)
                        .map(strip_recommended)
                        .collect()
                })
                .unwrap_or_default();
            picks.extend(typed);
            picks
        }
        _ => Vec::new(),
    };
    let text = picks
        .into_iter()
        .filter(|pick| !pick.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let text = if text.is_empty() {
        crate::tool::envelope::NO_INPUT_PLACEHOLDER.to_owned()
    } else {
        text
    };
    theme.fg("toolOutput", &text)
}

/// `detailLines` (transcript.ts:100-116): the question text and chosen
/// preview when expanded, then the answer's note.
fn detail_lines(
    answer: Option<&Value>,
    question_text: Option<&str>,
    indent: &str,
    theme: &dyn RenderTheme,
    width: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(question) = question_text.filter(|text| !text.is_empty()) {
        lines.extend(hanging(
            indent,
            &theme.fg("dim", &sanitize_inline(question)),
            width,
        ));
    }
    if question_text.is_some_and(|text| !text.is_empty())
        && let Some(preview) = answer
            .and_then(|answer| answer.get("preview"))
            .and_then(Value::as_str)
            .filter(|preview| !preview.is_empty())
    {
        for line in preview.split('\n') {
            lines.push(truncate_line(
                &format!("{indent}{}", theme.fg("dim", &format!("│ {line}"))),
                width,
            ));
        }
    }
    if let Some(note) = answer
        .and_then(|answer| answer.get("notes"))
        .and_then(Value::as_str)
        .filter(|note| !note.is_empty())
    {
        lines.extend(hanging(
            indent,
            &theme.fg("dim", &format!("✎ {}", sanitize_inline(note))),
            width,
        ));
    }
    lines
}

/// `globalNoteLines` (transcript.ts:118-120).
fn global_note_lines(details: &Value, theme: &dyn RenderTheme, width: usize) -> Vec<String> {
    details
        .get("globalNote")
        .and_then(Value::as_str)
        .filter(|note| !note.is_empty())
        .map(|note| {
            hanging(
                "  ",
                &theme.fg("dim", &format!("✎ {}", sanitize_inline(note))),
                width,
            )
        })
        .unwrap_or_default()
}

/// The questionnaire result envelope (`isQuestionnaireResult`): an
/// `answers` array and a boolean `cancelled`.
fn questionnaire_details(result: &Value) -> Option<&Value> {
    let details = result.get("details").filter(|details| !details.is_null())?;
    details
        .get("answers")
        .is_some_and(Value::is_array)
        .then_some(details)
        .filter(|details| details.get("cancelled").is_some_and(Value::is_boolean))
}

/// The first non-empty text block of a tool result, unsanitized.
fn raw_result_text(result: &Value) -> String {
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
        .unwrap_or("")
        .to_string()
}

/// First line of the model-facing text content (`firstTextLine`).
fn first_text_line(result: &Value) -> String {
    raw_result_text(result)
        .split('\n')
        .next()
        .unwrap_or("")
        .to_string()
}

/// The answers array of a questionnaire result envelope.
fn answers(details: &Value) -> &[Value] {
    details
        .get("answers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// The answer for question `index` (`answers.find(a => a.questionIndex === i)`).
fn answer_for(answers: &[Value], index: usize) -> Option<&Value> {
    answers
        .iter()
        .find(|answer| answer.get("questionIndex").and_then(Value::as_u64) == Some(index as u64))
}

/// `renderAskCall` (transcript.ts:31-38): bold `Ask` and the question
/// headers joined with ` · `, truncated to one line.
pub fn render_call(args: &Value, context: &Value, theme: &dyn RenderTheme, i18n: &I18n) -> Value {
    let _ = i18n;
    let headers: Vec<String> = parse_questions(args)
        .map(|questions| {
            questions
                .into_iter()
                .filter_map(|question| question.header)
                .collect()
        })
        .unwrap_or_default();
    let title = theme.fg("toolTitle", &theme.bold("Ask"));
    let text = if headers.is_empty() {
        title
    } else {
        format!("{title} {}", theme.fg("accent", &headers.join(" · ")))
    };
    json!({
        "type": "text",
        "props": { "text": truncate_line(&text, render_width(context)) }
    })
}

/// The single-question receipt (`singleLines`, transcript.ts:63-72).
fn single_lines(
    details: &Value,
    question_text: Option<&str>,
    theme: &dyn RenderTheme,
    expanded: bool,
    width: usize,
) -> Vec<String> {
    let list = answers(details);
    let answer = answer_for(list, 0);
    let prefix = format!("{} ", theme.fg("success", "✓"));
    let mut lines = hanging(&prefix, &answer_text(answer, theme, &I18n::detect()), width);
    let question = expanded.then_some(question_text).flatten();
    lines.extend(detail_lines(answer, question, "  ", theme, width));
    lines.extend(global_note_lines(details, theme, width));
    lines
}

/// The multi-question receipt table (`tableLines`, transcript.ts:74-92).
fn table_lines(
    details: &Value,
    questions: &[ParsedQuestion],
    theme: &dyn RenderTheme,
    expanded: bool,
    width: usize,
) -> Vec<String> {
    let list = answers(details);
    let headers: Vec<String> = questions
        .iter()
        .map(|question| question.header.clone().unwrap_or_default())
        .collect();
    let header_width = headers
        .iter()
        .map(|header| rpi_tui::utils::visible_width(header))
        .max()
        .unwrap_or(0);
    let answer_indent = " ".repeat(2 + header_width + 2);
    let mut lines = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        let answer = answer_for(list, index);
        let header = headers.get(index).cloned().unwrap_or_default();
        let pad = header_width.saturating_sub(rpi_tui::utils::visible_width(&header));
        let prefix = format!("  {}{}  ", theme.fg("muted", &header), " ".repeat(pad));
        lines.extend(hanging(
            &prefix,
            &answer_text(answer, theme, &I18n::detect()),
            width,
        ));
        let question_text = expanded.then_some(question.question.as_deref()).flatten();
        lines.extend(detail_lines(
            answer,
            question_text,
            &answer_indent,
            theme,
            width,
        ));
    }
    lines.extend(global_note_lines(details, theme, width));
    lines
}

/// The `renderResult` receipt (`renderAskResult`, transcript.ts:40-61):
/// success answers, a failure line, or the declined marker.
pub fn render_result(
    result: &Value,
    options: &Value,
    context: &Value,
    theme: &dyn RenderTheme,
    i18n: &I18n,
) -> Value {
    let expanded = options
        .get("expanded")
        .and_then(Value::as_bool)
        .or_else(|| context.get("expanded").and_then(Value::as_bool))
        .unwrap_or(false);
    let is_error = context
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let width = render_width(context);
    let details = questionnaire_details(result);
    let lines: Vec<String> = if is_error
        || details
            .and_then(|d| d.get("error"))
            .is_some_and(|e| !e.is_null())
    {
        let reason = first_text_line(result);
        vec![theme.fg("error", &format!("✗ {reason}"))]
    } else {
        let Some(details) = details else {
            return json!({
                "type": "text",
                "props": { "text": theme.fg("success", "✓") }
            });
        };
        if details.get("cancelled").and_then(Value::as_bool) == Some(true) {
            return json!({
                "type": "text",
                "props": {
                    "text": theme.fg(
                        "dim",
                        &format!("✗ {}", i18n.t("transcript.no_answer", "no answer"))
                    )
                }
            });
        }
        let args = context.get("args").cloned().unwrap_or(Value::Null);
        let questions = parse_questions(&args).unwrap_or_default();
        if questions.len() > 1 {
            table_lines(details, &questions, theme, expanded, width)
        } else {
            single_lines(
                details,
                questions
                    .first()
                    .and_then(|question| question.question.as_deref()),
                theme,
                expanded,
                width,
            )
        }
    };
    json!({
        "type": "text",
        "props": { "text": lines.join("\n") }
    })
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
/// theme (`ui.theme`, a synchronous read — the rpi-todo render-dispatch
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
        json!({
            "questions": [
                {
                    "question": "Which storage backend should the CLI default to?",
                    "header": "Scope",
                    "options": [
                        {"label": "Local SQLite", "description": "zero setup"},
                        {"label": "Postgres", "description": "networked"}
                    ]
                },
                {
                    "question": "How urgent is the migration?",
                    "header": "Priority",
                    "options": [{"label": "Now", "description": "this sprint"}]
                },
                {
                    "question": "Which UI framework?",
                    "header": "Framework",
                    "multiSelect": true,
                    "options": [{"label": "React", "description": ""}]
                }
            ]
        })
    }

    fn context(expanded: bool, width: u64) -> Value {
        json!({
            "toolCallId": "call-1",
            "argsComplete": true,
            "expanded": expanded,
            "isError": false,
            "terminalWidth": width
        })
    }

    fn text_of(node: &Value) -> &str {
        node["props"]["text"].as_str().unwrap_or("")
    }

    /// Strip the wrapping codes an unstyled `AnsiRenderTheme` still emits.
    fn strip_ansi(line: &str) -> String {
        line.replace("\u{1b}[39m", "")
            .replace("\u{1b}[22m", "")
            .replace("\u{1b}[1m", "")
            .replace("\u{1b}[0m", "")
    }

    fn details(answers: Value) -> Value {
        json!({ "answers": answers, "cancelled": false })
    }

    fn result(details: Value, content: &str) -> Value {
        json!({ "content": [{ "type": "text", "text": content }], "details": details })
    }

    // ===== renderCall (transcript.ts:31-38) =====

    #[test]
    fn call_lists_the_question_headers() {
        let tree = render_call(
            &three_questions(),
            &context(false, 100),
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(text_of(&tree), "Ask Scope · Priority · Framework");
        // No questions array → the bare title.
        let tree = render_call(
            &json!({}),
            &context(false, 100),
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(text_of(&tree), "Ask");
    }

    #[test]
    fn call_truncates_to_the_context_width() {
        let tree = render_call(
            &three_questions(),
            &context(false, 12),
            &IdentityRenderTheme,
            &en(),
        );
        let text = strip_ansi(text_of(&tree));
        assert!(text.ends_with('…'), "{text}");
        assert!(rpi_tui::utils::visible_width(&text) <= 12, "{text}");
    }

    // ===== renderResult failure / declined / empty (transcript.ts:40-61) =====

    #[test]
    fn result_failure_uses_the_first_content_line() {
        let tree = render_result(
            &result(details(json!([])), "Error: nope\nsecond line"),
            &json!({}),
            &json!({ "isError": true, "terminalWidth": 80 }),
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(text_of(&tree), "✗ Error: nope");
        // A reducer error in `details.error` takes the same branch.
        let tree = render_result(
            &result(
                json!({ "answers": [], "cancelled": false, "error": "bad batch" }),
                "Error: bad batch",
            ),
            &json!({}),
            &context(false, 80),
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(text_of(&tree), "✗ Error: bad batch");
    }

    #[test]
    fn result_without_details_is_a_check_and_cancelled_is_no_answer() {
        let tree = render_result(
            &json!({ "content": [] }),
            &json!({}),
            &context(false, 80),
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(text_of(&tree), "✓");
        let tree = render_result(
            &result(json!({ "answers": [], "cancelled": true }), ""),
            &json!({}),
            &context(false, 80),
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(text_of(&tree), "✗ no answer");
    }

    // ===== single-question receipt (transcript.ts:63-72) =====

    #[test]
    fn single_answer_strips_recommended_and_hides_details_when_collapsed() {
        let questions = json!({"questions": [{
            "question": "Pick one",
            "header": "H",
            "options": [{"label": "A", "description": "a"}, {"label": "B", "description": "b"}]
        }]});
        let ctx =
            json!({ "args": questions, "expanded": false, "terminalWidth": 80, "isError": false });
        let tree = render_result(
            &result(
                details(json!([{
                    "questionIndex": 0,
                    "question": "Pick one",
                    "kind": "option",
                    "answer": "B (Recommended)",
                    "notes": "keep it",
                    "preview": "preview line"
                }])),
                "",
            ),
            &json!({}),
            &ctx,
            &IdentityRenderTheme,
            &en(),
        );
        let text = text_of(&tree);
        // Notes stay visible when collapsed; the question and its preview are
        // expanded-only (upstream `detailLines`).
        assert_eq!(text, "✓ B\n  ✎ keep it", "collapsed: {text}");

        let ctx =
            json!({ "args": questions, "expanded": true, "terminalWidth": 80, "isError": false });
        let tree = render_result(
            &result(
                details(json!([{
                    "questionIndex": 0,
                    "question": "Pick one",
                    "kind": "option",
                    "answer": "B (Recommended)",
                    "notes": "keep it",
                    "preview": "preview line"
                }])),
                "",
            ),
            &json!({}),
            &ctx,
            &IdentityRenderTheme,
            &en(),
        );
        let text = text_of(&tree);
        assert!(text.contains("✓ B"), "{text}");
        assert!(text.contains("Pick one"), "{text}");
        assert!(text.contains("│ preview line"), "{text}");
        assert!(text.contains("✎ keep it"), "{text}");
    }

    #[test]
    fn single_answer_custom_and_multi_shapes() {
        let questions = json!({"questions": [{"question": "Q", "header": "H", "options": []}]});
        let ctx = |answer: Value| json!({ "args": questions, "expanded": false, "terminalWidth": 80, "isError": false, "answer": answer });
        for (answer, expected) in [
            (
                json!({"questionIndex":0,"question":"Q","kind":"custom","answer":"typed it"}),
                "✓ \"typed it\"",
            ),
            (
                json!({"questionIndex":0,"question":"Q","kind":"multi","answer":"own","selected":["A","C"]}),
                "✓ A, C, \"own\"",
            ),
            (
                json!({"questionIndex":0,"question":"Q","kind":"option","answer":null}),
                "✓ (no input)",
            ),
        ] {
            let tree = render_result(
                &result(details(json!([answer])), ""),
                &json!({}),
                &ctx(json!({})),
                &IdentityRenderTheme,
                &en(),
            );
            assert_eq!(text_of(&tree), expected);
        }
    }

    #[test]
    fn missing_answer_renders_not_answered() {
        let questions = json!({"questions": [{"question": "Q", "header": "H", "options": []}]});
        let tree = render_result(
            &result(details(json!([])), ""),
            &json!({}),
            &json!({ "args": questions, "expanded": false, "terminalWidth": 80, "isError": false }),
            &IdentityRenderTheme,
            &en(),
        );
        assert_eq!(text_of(&tree), "✓ not answered");
    }

    // ===== multi-question table (transcript.ts:74-92) =====

    #[test]
    fn multi_question_table_aligns_headers_and_appends_the_global_note() {
        let questions = json!({"questions": [
            {"question": "Pick one", "header": "Scope", "options": []},
            {"question": "Pick two", "header": "Priority", "options": []}
        ]});
        let tree = render_result(
            &result(
                json!({
                    "answers": [
                        {"questionIndex":0,"question":"Pick one","kind":"option","answer":"A"},
                        {"questionIndex":1,"question":"Pick two","kind":"custom","answer":"now"}
                    ],
                    "cancelled": false,
                    "globalNote": "overall"
                }),
                "",
            ),
            &json!({}),
            &json!({ "args": questions, "expanded": false, "terminalWidth": 80, "isError": false }),
            &IdentityRenderTheme,
            &en(),
        );
        let text = text_of(&tree);
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines[0], "  Scope     A", "{text}");
        assert_eq!(lines[1], "  Priority  \"now\"", "{text}");
        assert_eq!(lines[2], "  ✎ overall", "{text}");
    }

    #[test]
    fn expanded_multi_table_includes_each_question() {
        let questions = json!({"questions": [
            {"question": "Pick one", "header": "Scope", "options": []},
            {"question": "Pick two", "header": "Priority", "options": []}
        ]});
        let tree = render_result(
            &result(
                details(json!([
                    {"questionIndex":0,"question":"Pick one","kind":"option","answer":"A"}
                ])),
                "",
            ),
            &json!({"expanded": true}),
            &json!({ "args": questions, "expanded": false, "terminalWidth": 80, "isError": false }),
            &IdentityRenderTheme,
            &en(),
        );
        let text = text_of(&tree);
        assert!(text.contains("Pick one"), "{text}");
        assert!(text.contains("Pick two"), "{text}");
    }

    #[test]
    fn result_lines_wrap_to_the_context_width() {
        let questions = json!({"questions": [{"question": "Q", "header": "H", "options": []}]});
        let answer = "x".repeat(120);
        let tree = render_result(
            &result(
                details(json!([{
                    "questionIndex":0,"question":"Q","kind":"custom","answer":answer
                }])),
                "",
            ),
            &json!({}),
            &json!({ "args": questions, "expanded": false, "terminalWidth": 30, "isError": false }),
            &IdentityRenderTheme,
            &en(),
        );
        let text = text_of(&tree);
        for line in text.split('\n') {
            assert!(
                rpi_tui::utils::visible_width(line) <= 30,
                "line over width: {line:?}"
            );
        }
        assert!(
            text.contains("\n  "),
            "continuation is hanging-indented: {text}"
        );
    }

    // ===== dispatch =====

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
                "context": {"args": three_questions(), "argsComplete": true, "expanded": false, "terminalWidth": 100}
            }),
        );
        assert_eq!(
            strip_ansi(text_of(&tree)),
            "Ask Scope · Priority · Framework"
        );
    }
}
