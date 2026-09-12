//! Dialog layout assembly + footer hints + scroll window.
//!
//! Port of upstream `view/dialog-builder.ts` @ `338b264c`: sticky tab bar +
//! heading, the active body (single-select options — composed with the
//! markdown preview pane per the 100-column breakpoint when previews exist —
//! / multi-select / Submit review), the notes editor when open, and one
//! clipped footer hint line. When the natural frame is taller than the
//! terminal, the body scrolls between the sticky regions and overflow
//! indicators (`↑`/`↓`/`↕`, dim) mark the clipped direction (FR-Q3-B).

use crate::config::format_key_spec_for_display;
use crate::i18n::I18n;
use crate::state::build::QuestionItem;
use crate::state::reducer::QuestionnaireState;
use crate::state::row_intent::RowKind;
use crate::tool::types::QuestionData;
use crate::view::inline_input;
use crate::view::theme::Theme;
use crate::view::{multi_select, preview, submit, tab_bar, truncate_line, RenderedFrame};

/// Hint literals (canonical-English fallbacks of the `hint.*` locale keys).
pub const HINT_PART_ENTER: &str = "Enter to select";
/// `↑/↓ to navigate`.
pub const HINT_PART_NAV: &str = "↑/↓ to navigate";
/// `Shift+Enter for newline`.
pub const HINT_PART_NEW_LINE: &str = "Shift+Enter for newline";
/// `Ctrl+U to clear`.
pub const HINT_PART_CLEAR: &str = "Ctrl+U to clear";
/// `Space to toggle`.
pub const HINT_PART_TOGGLE: &str = "Space to toggle";
/// `n to add notes`.
pub const HINT_PART_NOTES: &str = "n to add notes";
/// `Tab to switch questions`.
pub const HINT_PART_TAB: &str = "Tab to switch questions";
/// `Esc to cancel`.
pub const HINT_PART_CANCEL: &str = "Esc to cancel";
/// `{key} to collapse`.
pub const HINT_PART_COLLAPSE_TEMPLATE: &str = "{key} to collapse";
/// `{key} to expand · Esc to cancel` (`hint.expand_line`; the collapsed row).
pub const COLLAPSED_HINT_TEMPLATE: &str = "{key} to expand · Esc to cancel";
/// `n to add a note` (Submit-tab hint).
pub const REVIEW_GLOBAL_HINT: &str = "n to add a note";
/// Collapse key sentinel meaning "no shortcut".
pub const COLLAPSE_KEY_OFF: &str = "off";
/// `notes.header` canonical English.
pub const NOTES_HEADER: &str = "Notes:";
/// `notes.global_header` canonical English.
pub const NOTES_GLOBAL_HEADER: &str = "Global note:";

/// Overflow indicator glyphs (dim).
pub const OVERFLOW_UP: &str = "↑";
/// Down indicator.
pub const OVERFLOW_DOWN: &str = "↓";
/// Both directions (single-row middle).
pub const OVERFLOW_BOTH: &str = "↕";

/// Inputs for one dialog frame.
pub struct DialogModel<'a> {
    /// Canonical state.
    pub state: &'a QuestionnaireState,
    /// All questions.
    pub questions: &'a [QuestionData],
    /// Per-tab row lists.
    pub items_by_tab: &'a [Vec<QuestionItem>],
    /// Locale table.
    pub i18n: &'a I18n,
    /// Theme palette.
    pub theme: &'a Theme,
    /// Live inline-input draft.
    pub input_text: &'a str,
    /// Cursor char offset inside `input_text`.
    pub input_cursor: Option<usize>,
    /// Live notes-editor draft (when `state.notes_visible`).
    pub notes_text: &'a str,
    /// Cursor char offset inside `notes_text`.
    pub notes_cursor: Option<usize>,
    /// Resolved collapse key spec (`"ctrl+]"`, `"alt+o"`, or `"off"`).
    pub collapse_key: &'a str,
    /// Frame width in columns (the pane width).
    pub width: usize,
    /// Terminal width in columns (the preview breakpoint gate; the overlay
    /// is 100% wide so this defaults to the pane width).
    pub terminal_width: usize,
    /// Available content height; frames taller than this scroll between the
    /// sticky regions (`None` disables the scroll window).
    pub height: Option<usize>,
}

/// `QuestionTabStrategy.footerRowCount` (tab-content-strategy.ts:96-97):
/// `Spacer(1)` + the one-line hint = 2 rendered rows.
const QUESTION_FOOTER_ROWS: usize = 2;

/// `SubmitTabStrategy.footerRowCount` (tab-content-strategy.ts:171-172):
/// `Spacer(1)` + prompt + submit picker (2 rows) + hint = 5 rendered rows.
const SUBMIT_FOOTER_ROWS: usize = 5;

/// Body height of one tab with a hypothetical focused option
/// (`PreviewPane.naturalHeight` / `MultiSelectView.naturalHeight` via the
/// per-tab body-height computers, build-questionnaire.ts:65-91): the
/// rendered pane at the model width with `option_index` overridden. Pure —
/// `preview::compose`/`multi_select::render` are stateless functions of
/// the state clone.
///
/// `input_mode` is forced **off** in every probe: the worst-case footprint
/// must stay invariant while the highlight sits on the "Type something."
/// row (where `preview::compose` collapses to the bare full-width list —
/// the input-mode body is provably ≤ the preview-mode body: the list
/// wraps no wider at the full pane width than the side-by-side left
/// column does, and every layout adds the preview block on top). Probing
/// with the live `input_mode` made the worst case itself collapse, so
/// nothing padded the input-mode frame and the overlay jumped as focus
/// entered/left the input row (real-session report after rc.8).
fn tab_body_height(model: &DialogModel<'_>, tab: usize, option_index: usize) -> usize {
    let Some(question) = model.questions.get(tab) else {
        return 0;
    };
    let items = model
        .items_by_tab
        .get(tab)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut state = model.state.clone();
    state.current_tab = tab;
    state.option_index = option_index;
    state.input_mode = false;
    if question.multi_select == Some(true) {
        multi_select::render(
            &state,
            question,
            model.i18n,
            model.theme,
            model.input_text,
            model.input_cursor,
            model.width,
            tab + 1 == model.questions.len(),
        )
        .lines
        .len()
    } else {
        preview::compose(
            &state,
            question,
            items,
            model.questions,
            model.items_by_tab,
            model.i18n,
            model.theme,
            model.input_text,
            model.input_cursor,
            model.terminal_width,
            model.width,
        )
        .lines
        .len()
    }
}

/// `buildHeightComputers.global` (build-questionnaire.ts:232-240): the
/// worst-case body height across **all tabs and options** — "Determines
/// the stable overall dialog footprint" (dialog-builder.ts:123).
/// Multi-select tabs contribute their single natural height
/// (`current == max`, build-questionnaire.ts:85-91); preview tabs
/// contribute the max over every option's pane (`maxNaturalHeight`,
/// preview-pane.ts:176+). Floored at 1 like upstream.
fn global_body_height(model: &DialogModel<'_>) -> usize {
    let mut max = 0usize;
    for tab in 0..model.questions.len() {
        if let Some(question) = model.questions.get(tab) {
            let option_count = question.options.len().max(1);
            for option in 0..option_count {
                max = max.max(tab_body_height(model, tab, option));
            }
        }
    }
    max.max(1)
}

/// `DialogView.render`'s residual spacer (dialog-builder.ts:207-212):
/// `max(0, getBodyHeight + maxFooterRowCount − bodyHeight −
/// footerRowCount)` — pads the currently active (possibly shorter) body up
/// to the worst-case footprint so the total dialog height stays stable
/// across option switches and tab switches (TE31 port gap closed after a
/// real-session report: switching between options whose previews render at
/// different heights made the overlay jump).
fn residual_spacer_rows(
    model: &DialogModel<'_>,
    active_body: usize,
    active_footer: usize,
) -> usize {
    let is_multi = model.questions.len() > 1;
    let max_footer = if is_multi {
        QUESTION_FOOTER_ROWS.max(SUBMIT_FOOTER_ROWS)
    } else {
        QUESTION_FOOTER_ROWS
    };
    (global_body_height(model) + max_footer).saturating_sub(active_body + active_footer)
}

/// `renderFitsTerminal` (dialog-builder.ts:65-67 + 216-218): pad the
/// active body up to the worst-case footprint, but only when the padded
/// frame still fits the height budget — the overflow branch (scroll
/// window) never pads.
///
/// [VARIANT] placement: upstream appends the blank rows **after** the
/// footer hint (`[...natural, ...spacer]`), which is invisible under its
/// bottom border — but the rpi frame has no border and the overlay is
/// bottom-anchored, so trailing blanks left the hint `spacer` rows above
/// the terminal bottom edge (real-session report after rc.8: the
/// Submit-tab hint floated far above the bottom, reading as dead space).
/// Inserting the pad **above** the footer block keeps the row total — the
/// height-stability contract — identical while the hint stays the frame's
/// last row, flush with the overlay's bottom edge.
fn insert_residual_spacer(
    model: &DialogModel<'_>,
    active_body: usize,
    active_footer: usize,
    footer_start: usize,
    lines: &mut Vec<String>,
) -> usize {
    let spacer = residual_spacer_rows(model, active_body, active_footer);
    if spacer == 0 {
        return 0;
    }
    let fits = model
        .height
        .is_none_or(|budget| lines.len() + spacer <= budget);
    if fits {
        lines.splice(
            footer_start..footer_start,
            std::iter::repeat_n(String::new(), spacer),
        );
        spacer
    } else {
        0
    }
}

/// `buildHintText` — the question-tab footer.
pub fn build_hint_text(
    question: Option<&QuestionData>,
    is_multi: bool,
    state: &QuestionnaireState,
    collapse_key: &str,
    i18n: &I18n,
) -> String {
    let mut parts = vec![
        i18n.t("hint.enter", HINT_PART_ENTER).to_owned(),
        i18n.t("hint.navigate", HINT_PART_NAV).to_owned(),
    ];
    if question.is_some_and(|question| question.multi_select == Some(true)) {
        parts.push(i18n.t("hint.toggle", HINT_PART_TOGGLE).to_owned());
    }
    if question.is_some() && !state.notes_visible && !state.input_mode {
        parts.push(i18n.t("hint.notes", HINT_PART_NOTES).to_owned());
    }
    if is_multi {
        parts.push(i18n.t("hint.tab", HINT_PART_TAB).to_owned());
    }
    parts.push(i18n.t("hint.cancel", HINT_PART_CANCEL).to_owned());
    if collapse_key != COLLAPSE_KEY_OFF && !collapse_key.is_empty() {
        parts.push(
            i18n.t("hint.collapse", HINT_PART_COLLAPSE_TEMPLATE)
                .replace("{key}", &format_key_spec_for_display(collapse_key)),
        );
    }
    if state.notes_visible || state.input_mode {
        parts.push(i18n.t("hint.newline", HINT_PART_NEW_LINE).to_owned());
    }
    if state.input_mode {
        parts.push(i18n.t("hint.clear", HINT_PART_CLEAR).to_owned());
    }
    parts.join(" · ")
}

/// `buildSubmitHintText` — the Submit-tab footer.
pub fn build_submit_hint_text(state: &QuestionnaireState, i18n: &I18n) -> String {
    let mut parts = vec![
        i18n.t("hint.enter", HINT_PART_ENTER).to_owned(),
        i18n.t("hint.navigate", HINT_PART_NAV).to_owned(),
    ];
    if !state.notes_visible {
        parts.push(i18n.t("review.global_hint", REVIEW_GLOBAL_HINT).to_owned());
    }
    parts.push(i18n.t("hint.cancel", HINT_PART_CANCEL).to_owned());
    if state.notes_visible {
        parts.push(i18n.t("hint.newline", HINT_PART_NEW_LINE).to_owned());
    }
    parts.join(" · ")
}

/// `buildCollapsedRender` — the single dim row shown while collapsed. The rpi
/// host hides the overlay entirely (R-U4.2), so this row matters only as the
/// visible fallback when `setComponentHidden` cannot run; it keeps the
/// upstream shape (`buildCollapsedRender`, `COLLAPSED_HINT_TEMPLATE`).
pub fn collapsed_row(collapse_key: &str, i18n: &I18n, theme: &Theme) -> String {
    let hint = if collapse_key == COLLAPSE_KEY_OFF || collapse_key.is_empty() {
        i18n.t("hint.cancel", HINT_PART_CANCEL).to_owned()
    } else {
        i18n.t("hint.expand_line", COLLAPSED_HINT_TEMPLATE)
            .replace("{key}", &format_key_spec_for_display(collapse_key))
    };
    theme.dim(&format!(" {hint} "))
}

/// Render one dialog frame at `width`.
pub fn render(model: &DialogModel<'_>) -> RenderedFrame {
    if model.state.collapsed {
        return RenderedFrame {
            lines: vec![truncate_line(
                &collapsed_row(model.collapse_key, model.i18n, model.theme),
                model.width,
            )],
            cursor: None,
        };
    }

    let is_multi = model.questions.len() > 1;
    let active_question = model.state.current_tab;
    let mut lines: Vec<String> = Vec::new();
    let mut cursor = None;

    if is_multi {
        lines.extend(tab_bar::render(
            model.state,
            model.questions,
            model.i18n,
            model.theme,
            model.width,
        ));
        lines.push(String::new());
    }

    // Everything above the body is sticky; the body (+ notes) scrolls.
    let mut focused_range: Option<(usize, usize)> = None;

    if is_multi && active_question == model.questions.len() {
        // Submit tab.
        lines.push(truncate_line(
            &model
                .theme
                .accent_bold(model.i18n.t("review.heading", submit::REVIEW_HEADING)),
            model.width,
        ));
        lines.push(String::new());
        let answers_start = lines.len();
        lines.extend(submit::render_answers(
            model.state,
            model.questions,
            model.i18n,
            model.theme,
            model.width,
        ));
        let answers_rows = lines.len() - answers_start;
        let footer_start = lines.len();
        lines.push(String::new());
        lines.push(submit::render_prompt(
            model.state,
            model.questions,
            model.i18n,
            model.theme,
            model.width,
        ));
        let picker_start = lines.len();
        let picker = submit::render_picker(model.state, model.i18n, model.theme, model.width);
        let picker_rows = picker.lines.len();
        lines.extend(picker.lines);
        if let Some((start, end)) = picker.focused_range {
            focused_range = Some((picker_start + start, picker_start + end));
        }
        if model.state.notes_visible {
            let (range, notes_cursor) = push_notes_editor(model, &mut lines, true);
            if let Some(range) = range {
                focused_range = Some(range);
            }
            cursor = cursor.or(notes_cursor);
        }
        lines.push(truncate_line(
            &model
                .theme
                .dim(&build_submit_hint_text(model.state, model.i18n)),
            model.width,
        ));
        // Submit-tab footer = blank + prompt + picker rows + hint (the
        // notes editor above is mid-rows, excluded like upstream's
        // `midRows`, tab-content-strategy.ts:181-186). Body = answers.
        let submit_footer_rows = 1 + 1 + picker_rows + 1;
        let inserted = insert_residual_spacer(
            model,
            answers_rows,
            submit_footer_rows,
            footer_start,
            &mut lines,
        );
        // The pad lands above the footer block, so every row anchor at or
        // below `footer_start` (picker focus, notes cursor) shifts down.
        let picker_start = picker_start + inserted;
        focused_range = focused_range.map(|(start, end)| (start + inserted, end + inserted));
        cursor = cursor.map(|(row, column)| (row + inserted, column));
        return finish(model, lines, cursor, focused_range, picker_start);
    }

    let Some(question) = model.questions.get(active_question) else {
        return RenderedFrame { lines, cursor };
    };

    if !is_multi && !question.header.is_empty() {
        lines.push(truncate_line(
            &model.theme.selected(&format!(" {} ", question.header)),
            model.width,
        ));
        lines.push(String::new());
    }
    for segment in rpi_tui::utils::wrap_text_with_ansi(&question.question, model.width.max(1)) {
        lines.push(truncate_line(&model.theme.bold(&segment), model.width));
    }
    lines.push(String::new());

    let items = model
        .items_by_tab
        .get(active_question)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let body_start = lines.len();
    let body = if question.multi_select == Some(true) {
        multi_select::render(
            model.state,
            question,
            model.i18n,
            model.theme,
            model.input_text,
            model.input_cursor,
            model.width,
            active_question + 1 == model.questions.len(),
        )
    } else {
        preview::compose(
            model.state,
            question,
            items,
            model.questions,
            model.items_by_tab,
            model.i18n,
            model.theme,
            model.input_text,
            model.input_cursor,
            model.terminal_width,
            model.width,
        )
    };
    if let Some((row, column)) = body.cursor {
        cursor = Some((body_start + row, column));
    }
    if let Some((start, end)) = body.focused_range {
        focused_range = Some((body_start + start, body_start + end));
    }
    let body_rows = body.lines.len();
    lines.extend(body.lines);
    if model.state.notes_visible {
        let (range, notes_cursor) = push_notes_editor(model, &mut lines, false);
        if let Some(range) = range {
            focused_range = Some(range);
        }
        cursor = cursor.or(notes_cursor);
    }
    let footer_start = lines.len();
    lines.push(String::new());
    lines.push(truncate_line(
        &model.theme.dim(&build_hint_text(
            Some(question),
            is_multi,
            model.state,
            model.collapse_key,
            model.i18n,
        )),
        model.width,
    ));

    // Height stabilization (dialog-builder.ts:207-218): body = the pane
    // rows just rendered; footer = blank + one-line hint (the notes editor
    // above the footer is mid-rows, excluded from the residual math).
    insert_residual_spacer(
        model,
        body_rows,
        QUESTION_FOOTER_ROWS,
        footer_start,
        &mut lines,
    );

    finish(model, lines, cursor, focused_range, body_start)
}

/// Append the notes editor (header + wrapped buffer + cursor) below the
/// body; returns the editor's line range (the scroll focus while open) and
/// the editor's cursor position.
type NotesRange = Option<(usize, usize)>;
type NotesCursor = Option<(usize, usize)>;

fn push_notes_editor(
    model: &DialogModel<'_>,
    lines: &mut Vec<String>,
    global: bool,
) -> (NotesRange, NotesCursor) {
    let header = if global {
        model
            .i18n
            .t("notes.global_header", NOTES_GLOBAL_HEADER)
            .to_owned()
    } else {
        model.i18n.t("notes.header", NOTES_HEADER).to_owned()
    };
    lines.push(truncate_line(
        &model.theme.accent_bold(&header),
        model.width,
    ));
    lines.push(String::new());
    let start = lines.len();
    let rendered = inline_input::render_inline_input(
        model.notes_text,
        model.notes_cursor,
        "",
        "",
        model.width.max(1),
        |line| model.theme.fg(model.theme.text, line),
    );
    let cursor = rendered.cursor.map(|(row, column)| (start + row, column));
    lines.extend(rendered.lines);
    (Some((start.saturating_sub(2), lines.len())), cursor)
}

/// Apply the scroll window (when configured and needed) and produce the
/// final frame.
fn finish(
    model: &DialogModel<'_>,
    lines: Vec<String>,
    cursor: Option<(usize, usize)>,
    focused_range: Option<(usize, usize)>,
    top_fixed: usize,
) -> RenderedFrame {
    let Some(height) = model.height else {
        return RenderedFrame { lines, cursor };
    };
    let bottom_fixed = 1usize;
    let (lines, cursor) = apply_scroll_window(
        lines,
        cursor,
        focused_range,
        top_fixed,
        bottom_fixed,
        height,
        model.theme,
    );
    RenderedFrame { lines, cursor }
}

/// `computeScrollStart` + `decorateOverflow` — the 3-region partition:
/// sticky top (`top_fixed` lines) + window over the middle + sticky footer
/// (`bottom_fixed` lines). The window centers on the focused row
/// (top-anchored without focus); the first/last window rows carry the
/// overflow indicators.
pub fn apply_scroll_window(
    lines: Vec<String>,
    cursor: Option<(usize, usize)>,
    focused_range: Option<(usize, usize)>,
    top_fixed: usize,
    bottom_fixed: usize,
    height: usize,
    theme: &Theme,
) -> (Vec<String>, Option<(usize, usize)>) {
    let total = lines.len();
    if total <= height || height == 0 {
        return (lines, cursor);
    }
    // Safety clamp: the sticky regions never leave the terminal.
    let top_fixed = top_fixed.min(height);
    let bottom_fixed = bottom_fixed.min(height.saturating_sub(top_fixed));
    let middle_rows = total.saturating_sub(top_fixed + bottom_fixed);
    let available = height.saturating_sub(top_fixed + bottom_fixed);
    if available == 0 {
        // Terminal too small for any middle content — chrome only.
        let mut chrome: Vec<String> = lines[..top_fixed].to_vec();
        chrome.extend_from_slice(&lines[total - bottom_fixed..]);
        chrome.truncate(height);
        return (chrome, None);
    }

    // Scroll start, centered on the focused row (upstream computeScrollStart).
    let scroll_start = match focused_range {
        None => 0,
        Some((start, end)) => {
            let focused_height = end.saturating_sub(start);
            let ideal = (start as i64) - ((available.saturating_sub(focused_height)) / 2) as i64;
            ideal
                .clamp(0, (middle_rows.saturating_sub(available)) as i64)
                .max(0) as usize
        }
    };

    let window_start = top_fixed + scroll_start;
    let window_end = (window_start + available).min(total - bottom_fixed);
    let mut middle: Vec<String> = lines[window_start..window_end].to_vec();
    let has_up = scroll_start > 0;
    let has_down = window_end < total - bottom_fixed;
    if has_up && has_down && middle.len() == 1 {
        middle[0] = theme.dim(OVERFLOW_BOTH);
    } else {
        if has_up && !middle.is_empty() {
            middle[0] = theme.dim(OVERFLOW_UP);
        }
        if has_down && !middle.is_empty() {
            let last = middle.len() - 1;
            middle[last] = theme.dim(OVERFLOW_DOWN);
        }
    }

    let mut out: Vec<String> = lines[..top_fixed].to_vec();
    out.extend(middle);
    out.extend_from_slice(&lines[total - bottom_fixed..]);

    // Remap or drop the cursor: rows inside the window shift by the scroll
    // offset; clipped rows lose the cursor.
    let cursor = cursor.and_then(|(row, column)| {
        let absolute_row = row;
        if absolute_row < top_fixed {
            return Some((absolute_row, column));
        }
        if absolute_row >= window_start && absolute_row < window_end {
            return Some((top_fixed + (absolute_row - window_start), column));
        }
        None
    });
    (out, cursor)
}

/// Sentinel label lookup used by tests (dialog re-exports the table lookup).
pub fn sentinel_label(i18n: &I18n, kind: RowKind) -> String {
    i18n.display_label(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::build::build_items_for_question;
    use crate::tool::types::OptionData;

    fn question(multi_select: bool) -> QuestionData {
        QuestionData {
            question: "Pick one?".to_owned(),
            header: "H".to_owned(),
            options: vec![
                OptionData {
                    label: "A".to_owned(),
                    description: "a".to_owned(),
                    preview: None,
                },
                OptionData {
                    label: "B".to_owned(),
                    description: "b".to_owned(),
                    preview: None,
                },
            ],
            multi_select: multi_select.then_some(true),
        }
    }

    #[allow(clippy::too_many_arguments)] // test helper mirrors the render context
    fn model<'a>(
        state: &'a QuestionnaireState,
        questions: &'a [QuestionData],
        items: &'a [Vec<QuestionItem>],
        i18n: &'a I18n,
        theme: &'a Theme,
        input_text: &'a str,
        collapse_key: &'a str,
        width: usize,
    ) -> DialogModel<'a> {
        DialogModel {
            state,
            questions,
            items_by_tab: items,
            i18n,
            theme,
            input_text,
            input_cursor: None,
            notes_text: "",
            notes_cursor: None,
            collapse_key,
            width,
            terminal_width: width,
            height: None,
        }
    }

    /// Preview-bearing fixture whose worst-case preview body is clearly
    /// taller than the bare option list (the input-mode body).
    fn preview_question() -> QuestionData {
        QuestionData {
            question: "Pick one?".to_owned(),
            header: "H".to_owned(),
            options: vec![
                OptionData {
                    label: "Short".to_owned(),
                    description: "one-line summary".to_owned(),
                    preview: Some("# Long\n\n## Header\n\n- Item one\n- Item two\n- Item three\n\n```\ncode block row\n```\n\nTrailing paragraph.".to_owned()),
                },
                OptionData {
                    label: "Tiny".to_owned(),
                    description: "tiny preview".to_owned(),
                    preview: Some("# Tiny\n\n`ok`".to_owned()),
                },
            ],
            multi_select: None,
        }
    }

    #[test]
    fn single_question_layout_has_header_heading_options_and_hint() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let questions = vec![question(false)];
        let items = vec![build_items_for_question(&questions[0], &i18n)];
        let state = QuestionnaireState::initial();
        let frame = render(&model(
            &state, &questions, &items, &i18n, &theme, "", "ctrl+]", 100,
        ));
        let plain: Vec<String> = frame.lines.iter().map(|line| strip_ansi(line)).collect();
        assert_eq!(plain[0], " H ");
        assert_eq!(plain[1], "");
        assert_eq!(plain[2], "Pick one?");
        assert_eq!(plain[4], "→ 1. A");
        assert_eq!(plain[8], "  3. Type something.");
        assert!(plain[10].starts_with("Enter to select"), "{plain:?}");
        assert!(plain[10].contains("Ctrl+] to collapse"), "{plain:?}");
    }

    #[test]
    fn multi_question_layout_has_tab_bar_and_submit_tab() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let questions = vec![question(false), question(false)];
        let items = vec![
            build_items_for_question(&questions[0], &i18n),
            build_items_for_question(&questions[1], &i18n),
        ];
        let mut state = QuestionnaireState::initial();
        state.current_tab = 2;
        let frame = render(&model(
            &state, &questions, &items, &i18n, &theme, "", "off", 60,
        ));
        let plain: Vec<String> = frame.lines.iter().map(|line| strip_ansi(line)).collect();
        assert!(plain[0].contains("□ H"), "{plain:?}");
        assert_eq!(plain[2], "Review your answers");
        // The residual spacer pads the empty answers body up to the
        // worst-case footprint ABOVE the footer block, so the prompt /
        // picker / hint anchor at the bottom of the frame — locate them by
        // content, not fixed offsets.
        let prompt = plain
            .iter()
            .position(|line| line.starts_with("⚠ Answer remaining questions"))
            .expect("prompt");
        assert_eq!(
            plain[prompt],
            "⚠ Answer remaining questions before submitting: H, H"
        );
        assert!(
            plain[prompt + 1].starts_with("→ 1. Submit answers"),
            "{plain:?}"
        );
        assert!(plain[prompt + 2].starts_with("  2. Cancel"), "{plain:?}");
        let hint = plain.last().expect("hint");
        assert!(hint.starts_with("Enter to select"), "{plain:?}");
        assert!(!hint.contains("collapse"), "off disables the hint");
        // Height stability across tabs: the submit frame is as tall as the
        // worst-case question frame (spacer rows included).
        let question_state = QuestionnaireState::initial();
        let question_frame = render(&model(
            &question_state,
            &questions,
            &items,
            &i18n,
            &theme,
            "",
            "off",
            60,
        ));
        assert_eq!(
            frame.lines.len(),
            question_frame.lines.len(),
            "submit and question frames share the stable footprint"
        );
    }

    #[test]
    fn hint_drops_notes_and_adds_newline_and_clear_in_input_mode() {
        let i18n = I18n::for_locale("en");
        let mut state = QuestionnaireState::initial();
        state.input_mode = true;
        let hint = build_hint_text(None, false, &state, "ctrl+]", &i18n);
        assert!(!hint.contains("n to add notes"), "{hint}");
        assert!(hint.contains("Shift+Enter for newline"), "{hint}");
        assert!(hint.contains("Ctrl+U to clear"), "{hint}");
    }

    #[test]
    fn submit_hint_swaps_the_note_part_when_the_editor_is_open() {
        let i18n = I18n::for_locale("en");
        let state = QuestionnaireState::initial();
        let resting = build_submit_hint_text(&state, &i18n);
        assert!(resting.contains("n to add a note"), "{resting}");
        assert!(!resting.contains("Shift+Enter"), "{resting}");
        let mut editing = QuestionnaireState::initial();
        editing.notes_visible = true;
        let editing = build_submit_hint_text(&editing, &i18n);
        assert!(!editing.contains("n to add a note"), "{editing}");
        assert!(editing.contains("Shift+Enter for newline"), "{editing}");
    }

    /// Real-session regression (post-rc.8): moving the highlight onto the
    /// "Type something." row enters input mode, which hides the preview
    /// pane — the worst-case footprint must stay computed WITHOUT input
    /// mode so the residual spacer absorbs the shorter body and the total
    /// frame height does not jump. Both layouts (side-by-side 120,
    /// stacked 80) and the hint-pinned-at-bottom invariant are covered.
    #[test]
    fn focus_on_type_something_keeps_total_frame_height() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let questions = vec![preview_question()];
        let items = vec![build_items_for_question(&questions[0], &i18n)];
        for width in [80, 120] {
            let resting = render(&model(
                &QuestionnaireState::initial(),
                &questions,
                &items,
                &i18n,
                &theme,
                "",
                "ctrl+]",
                width,
            ));
            let mut input = QuestionnaireState::initial();
            input.option_index = 2; // onto "Type something."
            input.input_mode = true;
            let typing = render(&model(
                &input, &questions, &items, &i18n, &theme, "", "ctrl+]", width,
            ));
            assert_eq!(
                resting.lines.len(),
                typing.lines.len(),
                "width {width}: focus entering the input row must not change the frame height"
            );
            // The preview really is hidden in input mode (the body IS
            // shorter) — the equality above is the spacer doing its job.
            assert!(
                resting.lines.iter().any(|line| line.contains('┌')),
                "width {width}: resting frame shows the preview box"
            );
            assert!(
                !typing.lines.iter().any(|line| line.contains('┌')),
                "width {width}: input-mode frame hides the preview box"
            );
            // The residual pad sits ABOVE the footer: the hint is the
            // frame's last row, flush with the overlay's bottom edge.
            for frame in [&resting, &typing] {
                let last = strip_ansi(frame.lines.last().expect("hint"));
                assert!(
                    last.starts_with("Enter to select"),
                    "width {width}: hint pinned to the bottom, got {last:?}"
                );
            }
        }
    }

    /// Real-session regression (post-rc.8): the Submit tab's residual pad
    /// used to trail BELOW the hint, floating it far above the overlay's
    /// bottom edge. The pad now lands between the answers and the footer
    /// block, and every row anchor below the insertion (picker focus,
    /// notes cursor) shifts with it.
    #[test]
    fn submit_tab_spacer_pads_above_the_footer_and_shifts_cursors() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let questions = vec![preview_question(), question(false)];
        let items = vec![
            build_items_for_question(&questions[0], &i18n),
            build_items_for_question(&questions[1], &i18n),
        ];
        let mut state = QuestionnaireState::initial();
        state.current_tab = 2; // Submit
        state.notes_visible = true;
        let mut m = model(&state, &questions, &items, &i18n, &theme, "", "off", 100);
        m.notes_text = "global remark";
        m.notes_cursor = Some(0);
        let frame = render(&m);
        let plain: Vec<String> = frame.lines.iter().map(|line| strip_ansi(line)).collect();
        let hint = plain.last().expect("hint");
        assert!(hint.starts_with("Enter to select"), "{plain:?}");
        assert!(
            !plain.iter().rev().take(2).any(|line| line.is_empty()),
            "no blank rows below the footer block: {plain:?}"
        );
        // The notes-editor cursor row must land on the draft text after the
        // spacer insertion shifted the footer block down.
        if let Some((row, _)) = frame.cursor {
            assert!(
                plain[row].contains("global remark"),
                "cursor row {row} must sit on the notes draft: {plain:?}"
            );
        } else {
            panic!("submit-tab notes editor must report a cursor");
        }
    }

    #[test]
    fn collapsed_frame_is_one_dim_expand_hint_row() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let questions = vec![question(false)];
        let items = vec![build_items_for_question(&questions[0], &i18n)];
        let mut state = QuestionnaireState::initial();
        state.collapsed = true;
        let frame = render(&model(
            &state, &questions, &items, &i18n, &theme, "", "ctrl+]", 80,
        ));
        let plain = strip_ansi(&frame.lines[0]);
        assert_eq!(frame.lines.len(), 1);
        assert_eq!(plain, " Ctrl+] to expand · Esc to cancel ");
        // "off" falls back to the cancel-only line (upstream parity).
        let off = collapsed_row("off", &i18n, &theme);
        assert_eq!(strip_ansi(&off), " Esc to cancel ");
    }

    #[test]
    fn notes_editor_renders_header_buffer_and_cursor_below_the_body() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let questions = vec![question(false)];
        let items = vec![build_items_for_question(&questions[0], &i18n)];
        let mut state = QuestionnaireState::initial();
        state.notes_visible = true;
        let mut m = model(&state, &questions, &items, &i18n, &theme, "", "ctrl+]", 60);
        m.notes_text = "line one\nline two";
        m.notes_cursor = Some(8);
        let frame = render(&m);
        let plain: Vec<String> = frame.lines.iter().map(|line| strip_ansi(line)).collect();
        let header = plain
            .iter()
            .position(|line| line == "Notes:")
            .expect("notes header");
        assert_eq!(plain[header + 1], "");
        assert_eq!(plain[header + 2], "line one");
        assert_eq!(plain[header + 3], "line two");
        assert!(frame.cursor.is_some(), "notes editor reports a cursor");
        // The hint builder swaps the notes part for the newline part; at this
        // width the frame's clipped footer keeps only the core prefix.
        let hint = build_hint_text(questions.first(), false, &state, "ctrl+]", &i18n);
        assert!(hint.contains("Shift+Enter for newline"), "{hint}");
        assert!(!hint.contains("n to add notes"), "{hint}");
        let footer = plain.last().expect("hint");
        assert!(footer.starts_with("Enter to select"), "{footer}");
        assert!(!footer.contains("n to add notes"), "{footer}");
    }

    #[test]
    fn submit_tab_notes_editor_uses_the_global_header() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let questions = vec![question(false), question(false)];
        let items = vec![
            build_items_for_question(&questions[0], &i18n),
            build_items_for_question(&questions[1], &i18n),
        ];
        let mut state = QuestionnaireState::initial();
        state.current_tab = 2;
        state.notes_visible = true;
        let m = model(&state, &questions, &items, &i18n, &theme, "", "ctrl+]", 60);
        let frame = render(&m);
        let plain: Vec<String> = frame.lines.iter().map(|line| strip_ansi(line)).collect();
        assert!(plain.iter().any(|line| line == "Global note:"), "{plain:?}");
    }

    #[test]
    fn scroll_window_marks_clip_directions_and_keeps_the_footer() {
        let theme = Theme::dark();
        let mut lines: Vec<String> = (0..30).map(|index| index.to_string()).collect();
        lines.push(String::new());
        lines.push("hint".to_owned()); // footer (bottom_fixed = 1… the blank is middle)
                                       // No focus: top-anchored window — only the bottom is clipped.
        let (out, _) = apply_scroll_window(lines.clone(), None, None, 2, 1, 10, &theme);
        assert_eq!(out.len(), 10);
        assert_eq!(strip_ansi(&out[0]), "0");
        assert_eq!(strip_ansi(&out[1]), "1");
        assert_eq!(strip_ansi(&out[2]), "2", "nothing above the window");
        assert_eq!(strip_ansi(&out[8]), "↓", "content below is clipped");
        assert_eq!(strip_ansi(&out[9]), "hint", "sticky footer survives");
        // Focus near the bottom: the window scrolls down — only the top is
        // clipped.
        let (out, _) = apply_scroll_window(lines.clone(), None, Some((26, 27)), 2, 1, 10, &theme);
        assert_eq!(strip_ansi(&out[2]), "↑", "content above is clipped");
        assert_eq!(strip_ansi(&out[9]), "hint", "sticky footer survives");
        // Fits → unchanged.
        let short = vec!["a".to_owned(), "b".to_owned()];
        let (out, _) = apply_scroll_window(short.clone(), None, None, 1, 1, 10, &theme);
        assert_eq!(out, short);
    }

    #[test]
    fn scroll_window_single_row_middle_shows_both_arrow() {
        let theme = Theme::dark();
        let lines: Vec<String> = (0..12).map(|index| index.to_string()).collect();
        // height 12 - top 4 - bottom 4 = 4 middle rows, 1 visible: focus at
        // middle row 1 → both directions clipped → combined ↕.
        let (out, _) = apply_scroll_window(lines, None, Some((1, 2)), 4, 4, 9, &theme);
        assert_eq!(strip_ansi(&out[4]), "↕");
    }

    #[test]
    fn scroll_window_remaps_and_clips_the_cursor() {
        let theme = Theme::dark();
        let lines: Vec<String> = (0..20).map(|index| index.to_string()).collect();
        // Cursor inside the window shifts by the scroll offset.
        let (out, cursor) = apply_scroll_window(lines.clone(), Some((5, 2)), None, 2, 1, 8, &theme);
        assert_eq!(
            cursor,
            Some((5, 2)),
            "row 5 stays at window row 5 (start 0)"
        );
        assert_eq!(out.len(), 8);
        // Cursor in the sticky header region survives (the header never
        // scrolls); a cursor below the window is dropped.
        let (_, cursor) = apply_scroll_window(lines.clone(), Some((1, 2)), None, 2, 1, 8, &theme);
        assert_eq!(cursor, Some((1, 2)));
        let (_, cursor) =
            apply_scroll_window(lines, Some((15, 2)), Some((14, 15)), 2, 1, 8, &theme);
        // Focus (14,15) centers the window there; row 15 sits inside.
        assert!(cursor.is_some());
    }

    #[test]
    fn tall_frames_scroll_with_overflow_indicator_via_render() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let long_description = "word ".repeat(30);
        let questions = vec![QuestionData {
            question: "Pick one?".to_owned(),
            header: "H".to_owned(),
            options: vec![
                OptionData {
                    label: "A".to_owned(),
                    description: long_description.clone(),
                    preview: None,
                },
                OptionData {
                    label: "B".to_owned(),
                    description: long_description,
                    preview: None,
                },
            ],
            multi_select: None,
        }];
        let items = vec![build_items_for_question(&questions[0], &i18n)];
        let state = QuestionnaireState::initial();
        let mut m = model(&state, &questions, &items, &i18n, &theme, "", "off", 80);
        m.height = Some(8);
        let frame = render(&m);
        assert_eq!(frame.lines.len(), 8, "frame clamps to the height");
        let plain: Vec<String> = frame.lines.iter().map(|line| strip_ansi(line)).collect();
        assert!(plain
            .iter()
            .any(|line| line == "↓" || line == "↑" || line == "↕"));
        assert!(
            plain.last().expect("hint").starts_with("Enter to select"),
            "sticky footer survives"
        );
    }

    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(character) = chars.next() {
            if character == '\u{1b}' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                continue;
            }
            out.push(character);
        }
        out
    }
}
