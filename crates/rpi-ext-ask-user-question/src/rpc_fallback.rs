//! RPC / dialog-primitive fallback for `ask_user_question` (R-Q6).
//!
//! Port of upstream `packages/rpiv-ask-user-question/rpc-fallback.ts` @
//! `338b264c`. The canonical TUI path renders a tabbed overlay that needs a
//! real terminal; RPC-mode hosts (VSCode pendant, ACP clients such as Zed or
//! Paseo — upstream issue #78) report `hasUI: true` because the dialog
//! sub-protocol works, but `ui.custom` cannot render there. This module walks
//! the questions sequentially with the `ui.select`/`ui.input` host dialogs
//! and returns the same `QuestionnaireResult` shapes the TUI path produces,
//! feeding the shared envelope ([`crate::tool::envelope`]).
//!
//! Parity trade-offs vs the TUI (inherent to the select/input surface, kept
//! identical to upstream): no side-by-side preview pane (previews fold into
//! the select title, each truncated at [`MAX_PREVIEW_CHARS`] UTF-16 units),
//! one dialog per question, multi-select is a free-text numbers input. The
//! "Type something." escape is preserved — multi-select treats any non-index
//! input as a typed custom answer.
//!
//! rpi adaptations (host-call bridge vs the upstream `ctx.ui` object,
//! documented per item; behavior is byte-identical where both sides run):
//! - [`HostUi`] models the upstream structural probe `hasDialogUI(ctx.ui)`;
//!   the rpi method table is static, so the host-call-backed probe reads
//!   `ctx.hasUI` (a real bridge always implements both primitives — every
//!   rpi mode bridge does; the null bridge is excluded by `hasUI == false`).
//! - Dialog transport failures surface as [`DialogOutcome::Err`] instead of
//!   a rejected promise; `tool::execute` maps them to an `isError` result
//!   (the rpi equivalent of upstream's thrown `execute` wrapped by the host).
//! - Locale strings resolve through an explicit [`I18n`] table (upstream
//!   reads the live i18n-bridge scope at dialog time; an injected table is
//!   the testable equivalent — same `t(key, fallback)` semantics).

use crate::i18n::I18n;
use crate::state::row_intent::RowKind;
use crate::tool::types::{
    AnswerKind, OptionData, QuestionAnswer, QuestionData, QuestionParams, QuestionnaireResult,
};
use crate::{HostCall, HostError};

/// `MULTI_SELECT_INSTRUCTIONS` (canonical-English fallback of the
/// `rpc.multi_instructions` locale key).
pub const MULTI_SELECT_INSTRUCTIONS: &str =
	"Enter the numbers of all that apply, comma-separated (e.g. \"1,3\"), or type a custom answer as plain text.";

/// `CUSTOM_ANSWER_TITLE` (canonical-English fallback of the
/// `rpc.custom_answer_title` locale key).
pub const CUSTOM_ANSWER_TITLE: &str = "Type your answer:";

/// `MULTI_SELECT_PLACEHOLDER`.
pub const MULTI_SELECT_PLACEHOLDER: &str = "1,3";

/// `MAX_PREVIEW_CHARS` — longest preview slice folded into a select title.
pub const MAX_PREVIEW_CHARS: usize = 600;

/// One dialog outcome. `Ok(None)` = the user dismissed the dialog (Esc →
/// `null` on the host-call bridge, `undefined` upstream) = cancel; `Err` =
/// transport failure (upstream: the promise rejects).
pub type DialogOutcome = Result<Option<String>, HostError>;

/// The dialog-primitive slice of the host UI surface this walker needs
/// (upstream `DialogUI`).
pub trait DialogUi {
    /// Blocking select dialog; returns the chosen entry or `None` (dismiss).
    fn select(&mut self, title: &str, options: &[String]) -> DialogOutcome;
    /// Blocking input dialog; returns the typed text or `None` (dismiss).
    fn input(&mut self, title: &str, placeholder: Option<&str>) -> DialogOutcome;
}

/// The host-UI probe surface for [`has_dialog_ui`] (upstream reads the
/// `ctx.ui` object structurally: `typeof u.select === "function" &&
/// typeof u.input === "function"`).
pub trait HostUi {
    /// `ui.select` primitive availability.
    fn select_available(&self) -> bool;
    /// `ui.input` primitive availability.
    fn input_available(&self) -> bool;
}

/// `hasDialogUI` — both primitives must be available. `None` models the
/// upstream `undefined`/`null` `ctx.ui` (also `false`).
pub fn has_dialog_ui(ui: Option<&dyn HostUi>) -> bool {
    ui.is_some_and(|ui| ui.select_available() && ui.input_available())
}

/// `formatOptionLine`: `"{index + 1}. {label} — {description}"` (em dash).
pub fn format_option_line(option: &OptionData, index: usize) -> String {
    format!("{}. {} — {}", index + 1, option.label, option.description)
}

/// ECMAScript `\s` character class (`WhiteSpace ∪ LineTerminator`, the exact
/// set `String.prototype.trim` removes and `/[,\s]+/` splits on).
fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\x0b' | '\x0c' | '\r' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// `Number.parseInt(token, 10) - 1` with the upstream bounds check
/// `i >= 0 && i < count`: skip leading whitespace, an optional sign, then
/// the longest ASCII-digit run; no digits → NaN → `None`; a `-` sign can
/// never satisfy `i >= 0`. `parseInt` reads the leading digits of a full
/// option line (`"2. B — b"` → 2).
pub fn parse_index(token: &str, count: usize) -> Option<usize> {
    let mut rest = token.trim_start_matches(is_js_whitespace);
    let negative = rest.starts_with('-');
    if let Some(stripped) = rest.strip_prefix(['-', '+']) {
        rest = stripped;
    }
    let digits = rest
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .unwrap_or("");
    if digits.is_empty() {
        return None; // parseInt → NaN
    }
    // >20 digits overflow u64; such magnitudes are out of bounds for any
    // question (≤ MAX_OPTIONS + 1 rows), so fail the bounds check directly.
    if digits.len() > 20 || negative {
        return None;
    }
    let parsed: u64 = digits.parse().ok()?;
    let index = usize::try_from(parsed.checked_sub(1)?).ok()?;
    (index < count).then_some(index)
}

/// Multi-select index token test (`/^\d+\.?$/`): ASCII digits with an
/// optional single trailing period (`"2"` / `"2."` yes, `"2.5"` / `"+2"` no).
fn is_index_token(token: &str) -> bool {
    let core = token.strip_suffix('.').unwrap_or(token);
    !core.is_empty() && core.bytes().all(|byte| byte.is_ascii_digit())
}

/// Truncate to at most `max_units` UTF-16 code units (JS `String.slice`
/// counts UTF-16 units, not characters). The cut stops at a char boundary:
/// when the limit would split a surrogate pair upstream keeps the lone
/// surrogate (an invalid-UTF-8 artifact Rust cannot hold); dropping the
/// whole astral char is the closest valid-UTF-8 behavior and renders
/// identically. The parity fixture pins a clean boundary case (301 crabs →
/// exactly 300).
fn truncate_utf16(text: &str, max_units: usize) -> String {
    let mut units = 0usize;
    let mut end = 0usize;
    for (byte_index, character) in text.char_indices() {
        let character_units = character.len_utf16();
        if units + character_units > max_units {
            break;
        }
        units += character_units;
        end = byte_index + character.len_utf8();
    }
    text[..end].to_owned()
}

/// `buildPreviewBlock` — previews folded into the select title (RPC has no
/// side-by-side pane): `"\n\n"` + blocks joined by `"\n\n"`, each
/// `"--- {i+1}. {label} preview ---\n{preview[..600]}"`; empty string when
/// no option carries a non-empty preview.
pub fn build_preview_block(question: &QuestionData) -> String {
    let blocks: Vec<String> = question
        .options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            option
                .preview
                .as_ref()
                .filter(|preview| !preview.is_empty())
                .map(|preview| {
                    format!(
                        "--- {}. {} preview ---\n{}",
                        index + 1,
                        option.label,
                        truncate_utf16(preview, MAX_PREVIEW_CHARS)
                    )
                })
        })
        .collect();
    if blocks.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", blocks.join("\n\n"))
    }
}

/// Walk the questionnaire one native dialog at a time
/// (`runRpcQuestionnaire`). Dismissing any dialog cancels the whole
/// questionnaire — mirroring Esc in the TUI — keeping the answers collected
/// so far; the shared envelope emits DECLINE for cancels. A
/// [`QuestionAnswer`] is produced per question otherwise, so the envelope is
/// identical to the TUI path's.
pub fn run_rpc_questionnaire(
    ui: &mut dyn DialogUi,
    params: &QuestionParams,
    i18n: &I18n,
) -> Result<QuestionnaireResult, HostError> {
    let mut answers: Vec<QuestionAnswer> = Vec::new();
    for (question_index, question) in params.questions.iter().enumerate() {
        let header = if question.header.is_empty() {
            String::new()
        } else {
            format!("[{}] ", question.header)
        };
        let answer = if question.multi_select.unwrap_or(false) {
            ask_multi_select(ui, question, question_index, &header, i18n)?
        } else {
            ask_single_select(ui, question, question_index, &header, i18n)?
        };
        let Some(answer) = answer else {
            return Ok(QuestionnaireResult {
                answers,
                cancelled: true,
                global_note: None,
                error: None,
            });
        };
        answers.push(answer);
    }
    Ok(QuestionnaireResult {
        answers,
        cancelled: false,
        global_note: None,
        error: None,
    })
}

/// `askSingleSelect` — `None` means the user dismissed the dialog (cancel).
fn ask_single_select(
    ui: &mut dyn DialogUi,
    question: &QuestionData,
    question_index: usize,
    header: &str,
    i18n: &I18n,
) -> Result<Option<QuestionAnswer>, HostError> {
    let mut options: Vec<String> = question
        .options
        .iter()
        .enumerate()
        .map(|(index, option)| format_option_line(option, index))
        .collect();
    options.push(format!(
        "{}. {}",
        question.options.len() + 1,
        i18n.display_label(RowKind::Other)
    ));
    let chosen = ui.select(
        &format!(
            "{header}{}{}",
            question.question,
            build_preview_block(question)
        ),
        &options,
    )?;
    // A host returning something outside the offered list (null included —
    // the bridge maps it to `None`, and a non-string value parses to no
    // index) is indistinguishable from a dismissal — treat it as one rather
    // than fabricate an answer.
    let index = parse_index(chosen.as_deref().unwrap_or(""), options.len());
    let Some(index) = index else {
        return Ok(None);
    };
    if index < question.options.len() {
        let option = &question.options[index];
        return Ok(Some(QuestionAnswer {
            question_index,
            question: question.question.clone(),
            kind: AnswerKind::Option,
            answer: Some(option.label.clone()),
            // `preview: o.preview && o.preview.length > 0 ? o.preview : undefined`
            preview: option
                .preview
                .as_ref()
                .filter(|preview| !preview.is_empty())
                .cloned(),
            selected: None,
            notes: None,
        }));
    }
    // "Type something." sentinel → free-text follow-up (empty placeholder).
    let typed = ui.input(
        &format!(
            "{header}{}\n\n{}",
            question.question,
            i18n.t("rpc.custom_answer_title", CUSTOM_ANSWER_TITLE)
        ),
        Some(""),
    )?;
    let Some(typed) = typed else {
        return Ok(None);
    };
    Ok(Some(QuestionAnswer {
        question_index,
        question: question.question.clone(),
        kind: AnswerKind::Custom,
        answer: Some(typed),
        selected: None,
        notes: None,
        preview: None,
    }))
}

/// `askMultiSelect` — `None` means the user dismissed the dialog (cancel).
fn ask_multi_select(
    ui: &mut dyn DialogUi,
    question: &QuestionData,
    question_index: usize,
    header: &str,
    i18n: &I18n,
) -> Result<Option<QuestionAnswer>, HostError> {
    let list = question
        .options
        .iter()
        .enumerate()
        .map(|(index, option)| format_option_line(option, index))
        .collect::<Vec<_>>()
        .join("\n");
    let value = ui.input(
        &format!(
            "{header}{}\n\n{list}\n\n{}",
            question.question,
            i18n.t("rpc.multi_instructions", MULTI_SELECT_INSTRUCTIONS)
        ),
        Some(MULTI_SELECT_PLACEHOLDER),
    )?;
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim_matches(is_js_whitespace);
    if trimmed.is_empty() {
        // Deliberate empty commit — same as pressing "Next" with nothing
        // toggled.
        return Ok(Some(QuestionAnswer {
            question_index,
            question: question.question.clone(),
            kind: AnswerKind::Multi,
            answer: None,
            selected: Some(Vec::new()),
            notes: None,
            preview: None,
        }));
    }
    let tokens: Vec<&str> = trimmed
        .split(|character: char| character == ',' || is_js_whitespace(character))
        .filter(|token| !token.is_empty())
        .collect();
    // `/^\d+\.?$/` tokens go through parseIndex (out-of-range numbers
    // become null there); anything else is null → custom answer.
    let indices: Option<Vec<usize>> = tokens
        .iter()
        .map(|token| {
            if is_index_token(token) {
                parse_index(token, question.options.len())
            } else {
                None
            }
        })
        .collect();
    if let Some(indices) = indices {
        let mut selected: Vec<String> = Vec::new();
        for index in indices {
            let label = &question.options[index].label;
            if !selected.contains(label) {
                selected.push(label.clone());
            }
        }
        return Ok(Some(QuestionAnswer {
            question_index,
            question: question.question.clone(),
            kind: AnswerKind::Multi,
            answer: None,
            selected: Some(selected),
            notes: None,
            preview: None,
        }));
    }
    // Any non-index token (words, or an out-of-range number like "13" for
    // three options) means the user typed an answer, not a selection.
    // Preserve it verbatim as a custom answer instead of silently dropping
    // their input — this is also the multi-select "Type something." escape.
    Ok(Some(QuestionAnswer {
        question_index,
        question: question.question.clone(),
        kind: AnswerKind::Custom,
        answer: Some(trimmed.to_owned()),
        selected: None,
        notes: None,
        preview: None,
    }))
}

/// Host-UI probe over the host-call bridge (`hasDialogUI(ctx.ui)`): the rpi
/// method table is static — `ui.select`/`ui.input` dispatch whenever the
/// `ui` capability is granted and a real bridge is bound — so both
/// primitives are available exactly when `ctx.hasUI` is true (every mode
/// bridge implements them; the null bridge is what `hasUI == false`
/// excludes). Probe failures (stale host) count as unavailable.
pub struct HostCallUi {
    has_ui: bool,
}

impl HostCallUi {
    /// Probe the host once (`ctx.hasUI`).
    pub fn probe(host: &dyn HostCall) -> Self {
        Self {
            has_ui: host
                .call("ctx.hasUI", serde_json::json!({}))
                .ok()
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
        }
    }

    /// Construct from a known `ctx.hasUI` value (avoids a second probe when
    /// the caller already read it).
    pub fn from_has_ui(has_ui: bool) -> Self {
        Self { has_ui }
    }
}

impl HostUi for HostCallUi {
    fn select_available(&self) -> bool {
        self.has_ui
    }

    fn input_available(&self) -> bool {
        self.has_ui
    }
}

/// Dialog transport over the host-call bridge: `ui.select`/`ui.input` are
/// blocking oneshot dialogs on the host side (Esc resolves `null`), mapped
/// here to `Ok(None)`. A non-string reply is a host returning something
/// outside the offered list — also a dismissal (see `ask_single_select`).
pub struct HostCallDialogUi<'a> {
    host: &'a dyn HostCall,
}

impl<'a> HostCallDialogUi<'a> {
    /// Wrap a host-call channel.
    pub fn new(host: &'a dyn HostCall) -> Self {
        Self { host }
    }
}

impl DialogUi for HostCallDialogUi<'_> {
    fn select(&mut self, title: &str, options: &[String]) -> DialogOutcome {
        let reply = self.host.call(
            "ui.select",
            serde_json::json!({ "title": title, "options": options }),
        )?;
        Ok(reply.as_str().map(str::to_owned))
    }

    fn input(&mut self, title: &str, placeholder: Option<&str>) -> DialogOutcome {
        let reply = self.host.call(
            "ui.input",
            serde_json::json!({ "title": title, "placeholder": placeholder }),
        )?;
        Ok(reply.as_str().map(str::to_owned))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::I18n;
    use crate::tool::types::OptionData;
    use serde_json::json;

    /// Flagged probe surface (the upstream structural judgment table).
    struct FlagUi {
        select: bool,
        input: bool,
    }

    impl HostUi for FlagUi {
        fn select_available(&self) -> bool {
            self.select
        }

        fn input_available(&self) -> bool {
            self.input
        }
    }

    /// Scripted dialog UI: pops one reply per call (`None` = dismiss),
    /// recording every (method, title, options/placeholder) invocation.
    #[derive(Default)]
    struct ScriptUi {
        replies: std::collections::VecDeque<DialogOutcome>,
        calls: Vec<(String, String, Vec<String>, Option<String>)>,
    }

    impl ScriptUi {
        fn push(&mut self, outcome: DialogOutcome) {
            self.replies.push_back(outcome);
        }

        fn pop(
            &mut self,
            method: &str,
            title: &str,
            extra: Vec<String>,
            placeholder: Option<String>,
        ) -> DialogOutcome {
            self.calls
                .push((method.to_owned(), title.to_owned(), extra, placeholder));
            self.replies.pop_front().unwrap_or(Ok(None)) // exhausted script = dismiss (mock default)
        }
    }

    impl DialogUi for ScriptUi {
        fn select(&mut self, title: &str, options: &[String]) -> DialogOutcome {
            let options = options.to_vec();
            self.pop("select", title, options, None)
        }

        fn input(&mut self, title: &str, placeholder: Option<&str>) -> DialogOutcome {
            let placeholder = placeholder.map(str::to_owned);
            self.pop("input", title, Vec::new(), placeholder)
        }
    }

    fn english() -> I18n {
        I18n::for_locale("en")
    }

    fn option(label: &str, description: &str, preview: Option<&str>) -> OptionData {
        OptionData {
            label: label.to_owned(),
            description: description.to_owned(),
            preview: preview.map(str::to_owned),
        }
    }

    fn single_params(preview: Option<&str>) -> QuestionParams {
        QuestionParams {
            questions: vec![QuestionData {
                question: "Which?".to_owned(),
                header: "Pick".to_owned(),
                options: vec![option("A", "a", preview), option("B", "b", None)],
                multi_select: None,
            }],
        }
    }

    fn multi_params() -> QuestionParams {
        QuestionParams {
            questions: vec![QuestionData {
                question: "Pick colors?".to_owned(),
                header: "Colors".to_owned(),
                options: vec![
                    option("red", "r", None),
                    option("green", "g", None),
                    option("blue", "b", None),
                ],
                multi_select: Some(true),
            }],
        }
    }

    #[test]
    fn format_option_line_matches_upstream() {
        assert_eq!(format_option_line(&option("A", "a", None), 0), "1. A — a");
        assert_eq!(
            format_option_line(&option("B", "b — with dash", None), 1),
            "2. B — b — with dash"
        );
    }

    /// Upstream `hasDialogUI` judgment table (rpc-fallback.test.ts).
    #[test]
    fn has_dialog_ui_requires_both_primitives() {
        assert!(has_dialog_ui(Some(&FlagUi {
            select: true,
            input: true
        })));
        assert!(!has_dialog_ui(Some(&FlagUi {
            select: true,
            input: false
        })));
        assert!(!has_dialog_ui(Some(&FlagUi {
            select: false,
            input: true
        })));
        assert!(!has_dialog_ui(Some(&FlagUi {
            select: false,
            input: false
        })));
        assert!(!has_dialog_ui(None), "undefined ctx.ui is false");
        // The host-call probe maps the static method table onto ctx.hasUI.
        assert!(HostUi::select_available(&HostCallUi::from_has_ui(true)));
        assert!(HostUi::input_available(&HostCallUi::from_has_ui(true)));
        assert!(!HostUi::select_available(&HostCallUi::from_has_ui(false)));
    }

    #[test]
    fn parse_index_follows_js_parseint_semantics() {
        // Leading digits of a full option line.
        assert_eq!(parse_index("2. B — b", 3), Some(1));
        assert_eq!(parse_index("1. A — a", 3), Some(0));
        // No digits → NaN → null.
        assert_eq!(parse_index("banana", 3), None);
        assert_eq!(parse_index("", 3), None);
        assert_eq!(parse_index("  ", 3), None);
        // Bounds: 0-based against `count` (a 3-row list accepts 1..=3).
        assert_eq!(parse_index("0", 3), None, "0 → i = -1 fails i >= 0");
        assert_eq!(
            parse_index("3", 3),
            Some(2),
            "the sentinel row of a 2-option select"
        );
        assert_eq!(parse_index("4", 3), None, "out of range");
        assert_eq!(parse_index("13", 3), None);
        // Whitespace skipped, signs honored (both JS parseInt behaviors).
        assert_eq!(parse_index(" 2", 3), Some(1));
        assert_eq!(parse_index("+2", 3), Some(1));
        assert_eq!(parse_index("-2", 3), None);
        // Hex-looking tokens stop at the 'x' with explicit radix 10
        // (parseInt → 0 → i = -1 → out of bounds, same as "0").
        assert_eq!(parse_index("0x2", 3), None);
        // Astral leading char → no digits.
        assert_eq!(parse_index("🦀2", 3), None);
        // Digit runs beyond any question size fail the bounds check.
        assert_eq!(parse_index("99999999999999999999999", 3), None);
    }

    #[test]
    fn preview_block_folds_formats_and_truncates() {
        let mut params = single_params(Some("short"));
        params.questions[0].options[1].preview = Some("x".repeat(700));
        let block = build_preview_block(&params.questions[0]);
        assert!(
            block.starts_with("\n\n--- 1. A preview ---\nshort\n\n--- 2. B preview ---\n"),
            "{block}"
        );
        let second: String = "x".repeat(600);
        assert!(block.ends_with(&second), "second preview truncated to 600");
        assert!(!block.ends_with(&"x".repeat(601)));

        // No non-empty previews → empty string (absent and empty both skip).
        assert_eq!(build_preview_block(&single_params(None).questions[0]), "");
        let mut empty_preview = single_params(Some(""));
        empty_preview.questions[0].options[0].preview = Some(String::new());
        assert_eq!(build_preview_block(&empty_preview.questions[0]), "");
    }

    #[test]
    fn preview_truncation_counts_utf16_units() {
        // 301 crabs = 602 UTF-16 units → exactly 300 crabs survive.
        let crabs = "🦀".repeat(301);
        let mut params = single_params(None);
        params.questions[0].options[0].preview = Some(crabs);
        let block = build_preview_block(&params.questions[0]);
        let expected = format!("--- 1. A preview ---\n{}", "🦀".repeat(300));
        assert_eq!(block, format!("\n\n{expected}"));
        // ASCII cut is exact.
        let ascii: String = "y".repeat(601);
        params.questions[0].options[0].preview = Some(ascii);
        assert_eq!(
            build_preview_block(&params.questions[0]),
            format!("\n\n--- 1. A preview ---\n{}", "y".repeat(600))
        );
    }

    #[test]
    fn single_select_title_folds_header_and_preview() {
        let params = single_params(Some("PREVIEW-A"));
        let mut ui = ScriptUi::default();
        ui.push(Ok(Some("1. A — a".to_owned())));
        let result =
            run_rpc_questionnaire(&mut ui, &params, &english()).expect("scripted ui cannot fail");
        assert!(!result.cancelled);
        let (method, title, options, placeholder) = &ui.calls[0];
        assert_eq!(method, "select");
        assert_eq!(
            title, "[Pick] Which?\n\n--- 1. A preview ---\nPREVIEW-A",
            "header prefix + preview block"
        );
        assert_eq!(
            options,
            &vec![
                "1. A — a".to_owned(),
                "2. B — b".to_owned(),
                "3. Type something.".to_owned()
            ],
            "sentinel row appended from ROW_INTENT_META via displayLabel"
        );
        assert_eq!(placeholder, &None, "select carries no placeholder");

        // Empty header → no prefix.
        let mut no_header = params.clone();
        no_header.questions[0].header = String::new();
        let mut ui = ScriptUi::default();
        ui.push(Ok(Some("1. A — a".to_owned())));
        let _ = run_rpc_questionnaire(&mut ui, &no_header, &english()).expect("run");
        assert_eq!(ui.calls[0].1, "Which?\n\n--- 1. A preview ---\nPREVIEW-A");
    }

    #[test]
    fn single_select_returns_option_or_custom() {
        // Option answer carries the matched preview only when non-empty.
        let params = single_params(Some("PREVIEW-A"));
        let mut ui = ScriptUi::default();
        ui.push(Ok(Some("1. A — a".to_owned())));
        let result =
            run_rpc_questionnaire(&mut ui, &params, &english()).expect("scripted ui cannot fail");
        assert_eq!(result.answers.len(), 1);
        let answer = &result.answers[0];
        assert_eq!(answer.kind, AnswerKind::Option);
        assert_eq!(answer.answer.as_deref(), Some("A"));
        assert_eq!(answer.preview.as_deref(), Some("PREVIEW-A"));

        // Sentinel → follow-up input (empty placeholder) → custom answer.
        let mut ui = ScriptUi::default();
        ui.push(Ok(Some("3. Type something.".to_owned())));
        ui.push(Ok(Some("typed it".to_owned())));
        let result =
            run_rpc_questionnaire(&mut ui, &params, &english()).expect("scripted ui cannot fail");
        let answer = &result.answers[0];
        assert_eq!(answer.kind, AnswerKind::Custom);
        assert_eq!(answer.answer.as_deref(), Some("typed it"));
        assert_eq!(answer.preview, None);
        let (_, title, _, placeholder) = &ui.calls[1];
        assert_eq!(
            title, "[Pick] Which?\n\nType your answer:",
            "custom title = header + question + localized prompt"
        );
        assert_eq!(placeholder.as_deref(), Some(""), "upstream passes \"\"");
    }

    #[test]
    fn select_out_of_range_treated_as_cancel() {
        for reply in ["9. weird", "banana", ""] {
            let params = single_params(None);
            let mut ui = ScriptUi::default();
            ui.push(Ok(if reply.is_empty() {
                None
            } else {
                Some(reply.to_owned())
            }));
            let result = run_rpc_questionnaire(&mut ui, &params, &english())
                .expect("scripted ui cannot fail");
            assert!(result.cancelled, "{reply} must cancel, not fabricate");
            assert!(result.answers.is_empty());
        }
    }

    #[test]
    fn multi_select_parses_indices_and_dedupes() {
        for input in ["1,3", "1 3", "1. 3.", "1, 3,1"] {
            let params = multi_params();
            let mut ui = ScriptUi::default();
            ui.push(Ok(Some(input.to_owned())));
            let result = run_rpc_questionnaire(&mut ui, &params, &english())
                .expect("scripted ui cannot fail");
            assert!(!result.cancelled, "{input}");
            let answer = &result.answers[0];
            assert_eq!(answer.kind, AnswerKind::Multi);
            assert_eq!(answer.answer, None);
            assert_eq!(
                answer.selected,
                Some(vec!["red".to_owned(), "blue".to_owned()]),
                "{input} → dedup preserving first occurrence"
            );
        }
        // Title: header + question + full option list + instructions.
        let params = multi_params();
        let mut ui = ScriptUi::default();
        ui.push(Ok(Some("2".to_owned())));
        let _ = run_rpc_questionnaire(&mut ui, &params, &english()).expect("run");
        let (_, title, _, placeholder) = &ui.calls[0];
        assert_eq!(
			title,
			"[Colors] Pick colors?\n\n1. red — r\n2. green — g\n3. blue — b\n\nEnter the numbers of all that apply, comma-separated (e.g. \"1,3\"), or type a custom answer as plain text."
		);
        assert_eq!(placeholder.as_deref(), Some("1,3"));
    }

    #[test]
    fn multi_select_empty_commit_is_empty_selection() {
        let params = multi_params();
        let mut ui = ScriptUi::default();
        ui.push(Ok(Some("   ".to_owned())));
        let result =
            run_rpc_questionnaire(&mut ui, &params, &english()).expect("scripted ui cannot fail");
        assert!(!result.cancelled);
        let answer = &result.answers[0];
        assert_eq!(answer.kind, AnswerKind::Multi);
        assert_eq!(answer.selected, Some(Vec::new()));
        assert_eq!(answer.answer, None);
    }

    #[test]
    fn multi_select_non_index_token_is_custom() {
        for input in ["red, something else entirely", "13", "2.5"] {
            let params = multi_params();
            let mut ui = ScriptUi::default();
            ui.push(Ok(Some(input.to_owned())));
            let result = run_rpc_questionnaire(&mut ui, &params, &english())
                .expect("scripted ui cannot fail");
            let answer = &result.answers[0];
            assert_eq!(answer.kind, AnswerKind::Custom, "{input}");
            assert_eq!(answer.answer.as_deref(), Some(input), "verbatim");
            assert_eq!(answer.selected, None);
        }
    }

    #[test]
    fn any_dialog_cancel_cancels_whole_questionnaire() {
        // Two questions; the second dialog dismisses → cancel with the first
        // answer preserved.
        let params = QuestionParams {
            questions: vec![
                QuestionData {
                    question: "Which?".to_owned(),
                    header: "Pick".to_owned(),
                    options: vec![option("A", "a", None), option("B", "b", None)],
                    multi_select: None,
                },
                QuestionData {
                    question: "Pick colors?".to_owned(),
                    header: "Colors".to_owned(),
                    options: vec![option("red", "r", None), option("green", "g", None)],
                    multi_select: Some(true),
                },
            ],
        };
        let mut ui = ScriptUi::default();
        ui.push(Ok(Some("1. A — a".to_owned())));
        ui.push(Ok(None)); // dismiss the multi input
        let result =
            run_rpc_questionnaire(&mut ui, &params, &english()).expect("scripted ui cannot fail");
        assert!(result.cancelled);
        assert_eq!(result.answers.len(), 1, "answered questions survive");
        assert_eq!(result.answers[0].answer.as_deref(), Some("A"));

        // Multi answered, then a follow-up single-select dismisses (multi
        // first so both dialog kinds are exercised in both orders).
        let swapped = QuestionParams {
            questions: vec![params.questions[1].clone(), params.questions[0].clone()],
        };
        let mut ui = ScriptUi::default();
        ui.push(Ok(Some("2".to_owned()))); // multi → green
        ui.push(Ok(None)); // dismiss the second question's select
        let result =
            run_rpc_questionnaire(&mut ui, &swapped, &english()).expect("scripted ui cannot fail");
        assert!(result.cancelled);
        assert_eq!(result.answers.len(), 1);
        assert_eq!(result.answers[0].selected, Some(vec!["green".to_owned()]));
    }

    #[test]
    fn multi_question_sequential_walk_one_dialog_each() {
        let params = QuestionParams {
            questions: vec![
                QuestionData {
                    question: "Which?".to_owned(),
                    header: "Pick".to_owned(),
                    options: vec![option("A", "a", None), option("B", "b", None)],
                    multi_select: None,
                },
                QuestionData {
                    question: "Pick colors?".to_owned(),
                    header: "Colors".to_owned(),
                    options: vec![option("red", "r", None), option("green", "g", None)],
                    multi_select: Some(true),
                },
            ],
        };
        let mut ui = ScriptUi::default();
        ui.push(Ok(Some("1. A — a".to_owned())));
        ui.push(Ok(Some("2".to_owned())));
        let result =
            run_rpc_questionnaire(&mut ui, &params, &english()).expect("scripted ui cannot fail");
        assert!(!result.cancelled);
        assert_eq!(result.answers.len(), 2);
        assert_eq!(result.answers[0].answer.as_deref(), Some("A"));
        assert_eq!(result.answers[1].selected, Some(vec!["green".to_owned()]));
        let methods: Vec<&str> = ui.calls.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(methods, vec!["select", "input"]);
    }

    #[test]
    fn locale_strings_resolve_through_the_injected_table() {
        let zh = I18n::for_locale("zh");
        let params = single_params(None);
        let mut ui = ScriptUi::default();
        ui.push(Ok(Some("3. 输入内容".to_owned())));
        ui.push(Ok(Some("自定义".to_owned())));
        let result = run_rpc_questionnaire(&mut ui, &params, &zh).expect("scripted ui cannot fail");
        assert_eq!(result.answers[0].kind, AnswerKind::Custom);
        // Sentinel row + custom-answer title follow the live locale.
        assert_eq!(ui.calls[0].2[2], "3. 输入内容");
        assert!(
            ui.calls[1].1.ends_with("输入你的回答："),
            "{}",
            ui.calls[1].1
        );
    }

    #[test]
    fn transport_error_propagates_without_fabricating_a_decline() {
        let params = single_params(None);
        let mut ui = ScriptUi::default();
        ui.push(Err(HostError {
            kind: "capabilityDenied".to_owned(),
            message: "ui.select requires capability ui".to_owned(),
        }));
        let error =
            run_rpc_questionnaire(&mut ui, &params, &english()).expect_err("must propagate");
        assert_eq!(error.kind, "capabilityDenied");
    }

    /// Serialization shape pinned by the parity fixtures (`answer: null` for
    /// multi, absent optional keys) — guards the wire form the envelope and
    /// details consumers read.
    #[test]
    fn result_serialization_matches_upstream_keys() {
        let params = multi_params();
        let mut ui = ScriptUi::default();
        ui.push(Ok(Some("1,3".to_owned())));
        let result =
            run_rpc_questionnaire(&mut ui, &params, &english()).expect("scripted ui cannot fail");
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(
            value,
            json!({
                "answers": [{
                    "questionIndex": 0,
                    "question": "Pick colors?",
                    "kind": "multi",
                    "answer": null,
                    "selected": ["red", "blue"]
                }],
                "cancelled": false
            })
        );
    }
}
