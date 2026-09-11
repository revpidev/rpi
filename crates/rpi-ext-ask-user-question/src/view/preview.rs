//! Markdown preview pane (side-by-side / stacked layouts).
//!
//! Port of upstream `view/components/preview/{preview-layout-decider,
//! preview-box-renderer,preview-block-renderer,markdown-content-cache}.ts` @
//! `338b264c` (v2.9.0+): the layout math is ported line-for-line (it is pure
//! and parity-pinned by the `preview` harness group); the markdown body uses
//! the rpi-tui `Markdown` component with the identity theme (the guest has no
//! host markdown theme — visual [VARIANT], TE-D40).
//!
//! Layout contract (R-Q5.3 / 附录 A.4): single-select questions with any
//! `preview` option split into a side-by-side HStack (options left, bordered
//! markdown box right) when **both** the terminal and the pane are ≥100
//! columns; otherwise the preview stacks below the options. Multi-select
//! questions never show a preview.

use std::collections::HashMap;

use rpi_tui::components::markdown::{Markdown, MarkdownTheme};
use rpi_tui::tui::Component as _;
use rpi_tui::utils::{truncate_to_width, visible_width};

use crate::i18n::I18n;
use crate::state::build::QuestionItem;
use crate::state::reducer::QuestionnaireState;
use crate::tool::types::QuestionData;
use crate::view::option_list;
use crate::view::option_list::BodyRender;
use crate::view::theme::Theme;
use crate::view::{spaces, truncate_line};

// ----- preview-layout-decider.ts -----

/// Min terminal/pane width for the side-by-side layout to engage.
pub const PREVIEW_MIN_WIDTH: usize = 100;
/// Visual gap between the options column and the preview column.
pub const PREVIEW_COLUMN_GAP: usize = 2;
/// 1 col padding inside the preview column (between gap and `│`).
pub const PREVIEW_PADDING_LEFT: usize = 1;
/// Empty rows between options and preview blocks in the stacked layout.
pub const STACKED_GAP_ROWS: usize = 1;
/// Floor for the adaptive left column width.
pub const MIN_LEFT: usize = 30;
/// Ceiling ratio: the left column never exceeds this fraction of the pane.
pub const MAX_LEFT_RATIO: f64 = 0.5;
/// Floor for the preview column width.
pub const MIN_PREVIEW_WIDTH: usize = 45;
/// `visibleWidth(" ✔")` — reserved on the longest-label measurement.
pub const CONFIRMED_OVERHEAD: usize = 2;

/// Layout mode (`PreviewLayoutMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewLayoutMode {
    /// Options and bordered preview side by side.
    SideBySide,
    /// Preview stacked underneath the options.
    Stacked,
}

impl PreviewLayoutMode {
    /// Wire name (parity report).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SideBySide => "side-by-side",
            Self::Stacked => "stacked",
        }
    }
}

/// `decideLayout` — the AND check of terminal and pane widths.
pub fn decide_layout(terminal_width: usize, pane_width: usize) -> PreviewLayoutMode {
    if terminal_width >= PREVIEW_MIN_WIDTH && pane_width >= PREVIEW_MIN_WIDTH {
        PreviewLayoutMode::SideBySide
    } else {
        PreviewLayoutMode::Stacked
    }
}

/// `adaptiveLeftWidth` — adaptive left-column width from the option labels.
pub fn adaptive_left_width(
    items: &[QuestionItem],
    total_for_numbering: usize,
    pane_width: usize,
) -> usize {
    let prefix_width = total_for_numbering.max(1).to_string().len() + 4; // digits + "❯ " + ". "
    let mut max_label = 0usize;
    for item in items {
        let width = visible_width(&item.label);
        if width > max_label {
            max_label = width;
        }
    }
    let desired = max_label + prefix_width + CONFIRMED_OVERHEAD;
    let ratio_capped = desired.min((pane_width as f64 * MAX_LEFT_RATIO).floor() as usize);
    let available = pane_width.saturating_sub(PREVIEW_COLUMN_GAP + MIN_PREVIEW_WIDTH);
    MIN_LEFT.max(ratio_capped.min(available.max(1)))
}

/// `crossTabMaxLeftWidth` — widest `adaptiveLeftWidth` over every tab.
pub fn cross_tab_max_left_width(
    questions: &[QuestionData],
    items_by_tab: &[Vec<QuestionItem>],
    pane_width: usize,
) -> usize {
    let mut max = MIN_LEFT;
    for (index, question) in questions.iter().enumerate() {
        let _ = question;
        let items = items_by_tab.get(index).cloned().unwrap_or_default();
        let tab_width = adaptive_left_width(&items, items.len(), pane_width);
        if tab_width > max {
            max = tab_width;
        }
    }
    max
}

/// `previewSourceWidth` — widest source line across all options' previews
/// (0 when no option carries a preview).
pub fn preview_source_width(question: &QuestionData) -> usize {
    let mut max = 0usize;
    for option in &question.options {
        let Some(text) = option.preview.as_deref() else {
            continue;
        };
        for line in text.split('\n') {
            let width = visible_width(line);
            if width > max {
                max = width;
            }
        }
    }
    max
}

/// `crossTabPreviewBudget` — cross-tab/cross-option preview budget.
pub fn cross_tab_preview_budget(questions: &[QuestionData], pane_width: usize) -> usize {
    let mut max = MIN_PREVIEW_WIDTH;
    for question in questions {
        let raw_width = preview_source_width(question);
        let capped = raw_width.min(pane_width.saturating_sub(PREVIEW_COLUMN_GAP + MIN_LEFT));
        let budget = capped
            + super::preview::BORDER_HORIZONTAL_OVERHEAD
            + 2 * super::preview::BORDER_INNER_PADDING_HORIZONTAL
            + PREVIEW_PADDING_LEFT;
        if budget > max {
            max = budget;
        }
    }
    max
}

/// `crossTabLeftWidthWithDonation` — label-driven width plus the slack
/// donated by narrow previews (the left column stays stable across tabs).
pub fn cross_tab_left_width_with_donation(
    questions: &[QuestionData],
    items_by_tab: &[Vec<QuestionItem>],
    pane_width: usize,
) -> usize {
    let label_driven = cross_tab_max_left_width(questions, items_by_tab, pane_width);
    let preview_budget = cross_tab_preview_budget(questions, pane_width);
    let slack_donation =
        (pane_width as i64).saturating_sub(PREVIEW_COLUMN_GAP as i64 + preview_budget as i64);
    let preview_safety_ceiling =
        (pane_width.saturating_sub(PREVIEW_COLUMN_GAP + MIN_PREVIEW_WIDTH)) as i64;
    let ratio_ceiling = (pane_width as f64 * MAX_LEFT_RATIO).floor() as i64;
    let ceiling = preview_safety_ceiling.min(ratio_ceiling);
    (label_driven as i64)
        .max(slack_donation)
        .min(ceiling.max(1)) as usize
}

/// `columnWidths` — the side-by-side split.
pub fn column_widths(pane_width: usize, adaptive_left: usize) -> (usize, usize, usize) {
    let gap = PREVIEW_COLUMN_GAP;
    let left_width = adaptive_left.min(pane_width.saturating_sub(gap + 1).max(1));
    let right_width = (pane_width.saturating_sub(left_width + gap)).max(1);
    (left_width, right_width, gap)
}

/// `bodyWidths` — the widths passed to the option list and the preview block.
pub fn body_widths(
    pane_width: usize,
    mode: PreviewLayoutMode,
    adaptive_left: usize,
) -> (usize, usize) {
    match mode {
        PreviewLayoutMode::Stacked => (pane_width, pane_width),
        PreviewLayoutMode::SideBySide => {
            let (left_width, right_width, _) = column_widths(pane_width, adaptive_left);
            (
                left_width,
                right_width.saturating_sub(PREVIEW_PADDING_LEFT).max(1),
            )
        }
    }
}

// ----- preview-box-renderer.ts -----

/// Top + bottom border rows consumed by [`render_bordered_box`].
pub const BORDER_VERTICAL_OVERHEAD: usize = 2;
/// Left + right vertical bar columns (`│ ... │`).
pub const BORDER_HORIZONTAL_OVERHEAD: usize = 2;
/// Inner horizontal padding between each border bar and the content area.
pub const BORDER_INNER_PADDING_HORIZONTAL: usize = 1;
/// Floor for the preview box's inner content width (CC parity).
pub const BOX_MIN_CONTENT_WIDTH: usize = 40;

const FENCE_MARKER_PREFIX: &str = "```";

/// Drop fenced-code-block marker lines (` ``` ` opener/closer) from rendered
/// markdown (pi-tui's Markdown emits them literally around code blocks).
pub fn strip_fence_markers(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| {
            let clean = strip_sgr_and_osc8(line);
            !clean.starts_with(FENCE_MARKER_PREFIX)
        })
        .cloned()
        .collect()
}

/// `ANSI_SGR_RE` + `ANSI_OSC8_RE` combined strip (test/fence probe helper).
fn strip_sgr_and_osc8(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            match chars.peek() {
                // SGR: ESC [ ... letter
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                // OSC 8 hyperlink: ESC ] 8 ; ... (BEL | ESC \)
                Some(']') => {
                    chars.next();
                    let mut terminated = false;
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            terminated = true;
                            break;
                        }
                        if next == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            terminated = true;
                            break;
                        }
                    }
                    let _ = terminated;
                }
                _ => out.push(character),
            }
            continue;
        }
        out.push(character);
    }
    out
}

/// Wrap `lines` in a 4-sided ASCII border with 1 col of inner padding
/// (`renderBorderedBox`). When `hidden > 0`, the bottom dash run becomes
/// ` ✂ ── N lines hidden ── ` (corners stay).
pub fn render_bordered_box(
    lines: &[String],
    width: usize,
    color: impl Fn(&str) -> String,
    hidden: usize,
) -> Vec<String> {
    let dash_span = width.saturating_sub(BORDER_HORIZONTAL_OVERHEAD).max(1);
    let content_inner = dash_span
        .saturating_sub(2 * BORDER_INNER_PADDING_HORIZONTAL)
        .max(1);
    let pad = spaces(BORDER_INNER_PADDING_HORIZONTAL);
    let mut out = vec![color(&format!("┌{}┐", "─".repeat(dash_span)))];
    for line in lines {
        let padded = truncate_to_width(line, content_inner, "", true);
        out.push(format!("{}{pad}{padded}{pad}{}", color("│"), color("│")));
    }
    if hidden > 0 {
        let indicator = format!(" ✂ ── {hidden} lines hidden ── ");
        let indicator_len = indicator.chars().count();
        let space = dash_span.saturating_sub(indicator_len);
        let left_fill = "─".repeat(space / 2);
        let right_fill = "─".repeat(dash_span.saturating_sub(left_fill.len() + indicator_len));
        out.push(color(&format!("└{left_fill}{indicator}{right_fill}┘")));
    } else {
        out.push(color(&format!("└{}┘", "─".repeat(dash_span))));
    }
    out
}

/// `computeBoxDimensions` — inner width and total box width from content
/// lines (trailing whitespace is stripped before measuring; the floor is
/// `BOX_MIN_CONTENT_WIDTH`).
pub fn compute_box_dimensions(content_lines: &[String], max_inner_width: usize) -> (usize, usize) {
    let mut widest = BOX_MIN_CONTENT_WIDTH.min(max_inner_width);
    for line in content_lines {
        let trimmed = line.trim_end();
        let width = visible_width(trimmed);
        if width > widest {
            widest = width;
        }
    }
    let inner_width = widest.min(max_inner_width);
    let box_width = inner_width + BORDER_HORIZONTAL_OVERHEAD + 2 * BORDER_INNER_PADDING_HORIZONTAL;
    (inner_width, box_width)
}

// ----- markdown-content-cache.ts -----

/// CC parity in side-by-side layout.
pub const MAX_PREVIEW_HEIGHT_SIDE_BY_SIDE: usize = 20;
/// Narrow-terminal protection in stacked layout.
pub const MAX_PREVIEW_HEIGHT_STACKED: usize = 15;
/// `preview.no_preview` canonical English.
pub const NO_PREVIEW_TEXT: &str = "No preview available";
/// 1 blank separator + 1 affordance row reserved when a preview exists.
pub const NOTES_AFFORDANCE_OVERHEAD: usize = 2;

// ----- preview-block-renderer.ts -----

/// `preview.notes_affordance` canonical English.
pub const NOTES_AFFORDANCE_TEXT: &str = "Notes: press n to add notes";

/// Content row budget for a layout mode: preview cap minus border +
/// affordance overhead.
fn content_budget_for(mode: PreviewLayoutMode) -> usize {
    let cap = match mode {
        PreviewLayoutMode::SideBySide => MAX_PREVIEW_HEIGHT_SIDE_BY_SIDE,
        PreviewLayoutMode::Stacked => MAX_PREVIEW_HEIGHT_STACKED,
    };
    cap.saturating_sub(BORDER_VERTICAL_OVERHEAD + NOTES_AFFORDANCE_OVERHEAD)
        .max(1)
}

/// Inner (padding-aware) content width for a total block width.
fn inner_width_for(width: usize) -> usize {
    width
        .saturating_sub(BORDER_HORIZONTAL_OVERHEAD + 2 * BORDER_INNER_PADDING_HORIZONTAL)
        .max(1)
}

/// Per-question cache of rendered markdown previews (`MarkdownContentCache`):
/// one `Markdown` per option, lazy, width-keyed.
pub struct MarkdownContentCache {
    preview_texts: HashMap<usize, String>,
    markdown_cache: HashMap<usize, Markdown>,
    cached_width: Option<usize>,
    theme: Theme,
    i18n: I18n,
}

impl MarkdownContentCache {
    /// Build the cache for `question` (options carrying a non-empty preview).
    pub fn new(question: &QuestionData, theme: Theme, i18n: I18n) -> Self {
        let mut preview_texts = HashMap::new();
        for (index, option) in question.options.iter().enumerate() {
            if let Some(raw) = option.preview.as_deref() {
                if !raw.is_empty() {
                    preview_texts.insert(index, raw.to_owned());
                }
            }
        }
        Self {
            preview_texts,
            markdown_cache: HashMap::new(),
            cached_width: None,
            theme,
            i18n,
        }
    }

    /// Whether any option of the question carries a preview.
    pub fn has_any_preview(&self) -> bool {
        !self.preview_texts.is_empty()
    }

    /// Whether option `index` carries a preview.
    pub fn has(&self, option_index: usize) -> bool {
        self.preview_texts.contains_key(&option_index)
    }

    /// Body lines for one option at `inner_width` (width changes invalidate
    /// the per-Markdown render cache).
    pub fn body_for(&mut self, option_index: usize, inner_width: usize) -> Vec<String> {
        if self.cached_width != Some(inner_width) {
            for markdown in self.markdown_cache.values_mut() {
                markdown.invalidate();
            }
            self.cached_width = Some(inner_width);
        }
        let Some(text) = self.preview_texts.get(&option_index) else {
            let placeholder = self
                .theme
                .dim(self.i18n.t("preview.no_preview", NO_PREVIEW_TEXT));
            let pad = inner_width.saturating_sub(visible_width(&placeholder));
            return vec![format!("{placeholder}{}", spaces(pad))];
        };
        let markdown = self.markdown_cache.entry(option_index).or_insert_with(|| {
            Markdown::new(
                text.clone(),
                0,
                0,
                std::sync::Arc::new(MarkdownTheme::identity()),
                None,
                None,
            )
        });
        strip_fence_markers(&markdown.render(inner_width))
    }

    /// Invalidate every cached render (`invalidate`).
    pub fn invalidate(&mut self) {
        for markdown in self.markdown_cache.values_mut() {
            markdown.invalidate();
        }
        self.cached_width = None;
    }
}

/// Renders the bordered markdown preview block for one question
/// (`PreviewBlockRenderer`): bordered box + blank separator + affordance row
/// (the affordance row is always emitted — empty when gated — so the row
/// count is height-stable).
pub struct PreviewBlockRenderer {
    cache: MarkdownContentCache,
    theme: Theme,
    i18n: I18n,
}

impl PreviewBlockRenderer {
    /// New renderer for `question`.
    pub fn new(question: &QuestionData, theme: Theme, i18n: I18n) -> Self {
        Self {
            cache: MarkdownContentCache::new(question, theme, i18n.clone()),
            theme,
            i18n,
        }
    }

    /// Whether the question has any preview.
    pub fn has_any_preview(&self) -> bool {
        self.cache.has_any_preview()
    }

    /// Height contribution of the preview block (always equals
    /// `render_block(...).lines.len()`).
    pub fn block_height(
        &mut self,
        width: usize,
        option_index: usize,
        mode: PreviewLayoutMode,
    ) -> usize {
        let content_budget = content_budget_for(mode);
        let inner_width = inner_width_for(width);
        let raw_rows = self.cache.body_for(option_index, inner_width).len();
        let content_rows = raw_rows.min(content_budget);
        BORDER_VERTICAL_OVERHEAD + content_rows + NOTES_AFFORDANCE_OVERHEAD
    }

    /// Render the full preview block at `width`.
    #[allow(clippy::too_many_arguments)] // mirrors the upstream signature
    pub fn render_block(
        &mut self,
        width: usize,
        option_index: usize,
        mode: PreviewLayoutMode,
        focused: bool,
        notes_visible: bool,
    ) -> Vec<String> {
        let content_budget = content_budget_for(mode);
        let max_inner_width = inner_width_for(width);

        let raw = self.cache.body_for(option_index, max_inner_width);
        let truncated = raw.len() > content_budget;
        let hidden = if truncated {
            raw.len() - content_budget
        } else {
            0
        };
        let content_lines: Vec<String> = if truncated {
            raw.into_iter().take(content_budget).collect()
        } else {
            raw
        };

        let (_, box_width) = compute_box_dimensions(&content_lines, max_inner_width);
        let color = |text: &str| self.theme.accent(text);
        let boxed_lines = render_bordered_box(&content_lines, box_width, color, hidden);

        let show_affordance = focused && !notes_visible && self.cache.has(option_index);
        let affordance = if show_affordance {
            self.theme.muted(
                self.i18n
                    .t("preview.notes_affordance", NOTES_AFFORDANCE_TEXT),
            )
        } else {
            String::new()
        };
        let mut out = boxed_lines;
        out.push(String::new());
        out.push(affordance);
        out
    }
}

// ----- preview-pane.ts (composition) -----

/// Whether any option of `question` carries a non-empty preview.
pub fn has_any_preview(question: &QuestionData) -> bool {
    question
        .options
        .iter()
        .any(|option| option.preview.as_deref().is_some_and(|p| !p.is_empty()))
}

/// The effective layout mode for the active question, or `None` when no
/// preview is shown (multi-select, no previews, or `input_mode` — the inline
/// editor expands to the full pane width).
pub fn preview_mode(
    question: &QuestionData,
    input_mode: bool,
    terminal_width: usize,
    pane_width: usize,
) -> Option<PreviewLayoutMode> {
    if question.multi_select == Some(true) || input_mode || !has_any_preview(question) {
        return None;
    }
    Some(decide_layout(terminal_width, pane_width))
}

/// Compose the question body with its preview (`PreviewPane.render`).
///
/// `None` mode renders the bare option list at the full width (no preview).
#[allow(clippy::too_many_arguments)] // render context is threaded explicitly (no component graph)
pub fn compose(
    state: &QuestionnaireState,
    question: &QuestionData,
    items: &[QuestionItem],
    questions: &[QuestionData],
    items_by_tab: &[Vec<QuestionItem>],
    i18n: &I18n,
    theme: &Theme,
    input_text: &str,
    input_cursor: Option<usize>,
    terminal_width: usize,
    width: usize,
) -> BodyRender {
    let Some(mode) = preview_mode(question, state.input_mode, terminal_width, width) else {
        return option_list::render(
            state,
            question,
            items,
            i18n,
            theme,
            input_text,
            input_cursor,
            width,
        );
    };
    let mut block = PreviewBlockRenderer::new(question, *theme, i18n.clone());
    let focused = !state.notes_visible;
    match mode {
        PreviewLayoutMode::Stacked => {
            let body = option_list::render(
                state,
                question,
                items,
                i18n,
                theme,
                input_text,
                input_cursor,
                width,
            );
            let focused_range = body.focused_range;
            let mut lines = body.lines;
            let cursor = body.cursor;
            for _ in 0..STACKED_GAP_ROWS {
                lines.push(String::new());
            }
            lines.extend(block.render_block(
                width,
                state.option_index,
                mode,
                focused,
                state.notes_visible,
            ));
            BodyRender {
                lines,
                cursor,
                focused_range,
            }
        }
        PreviewLayoutMode::SideBySide => {
            let adaptive_left = cross_tab_left_width_with_donation(questions, items_by_tab, width);
            let (left_width, right_width, gap) = column_widths(width, adaptive_left);
            let left = option_list::render(
                state,
                question,
                items,
                i18n,
                theme,
                input_text,
                input_cursor,
                left_width,
            );
            // The side-by-side compose keeps one output line per row pair, so
            // the focused row range maps 1:1 onto the joined output.
            let focused_range = left.focused_range;
            let right = render_padded_preview_lines(
                &mut block,
                right_width,
                state.option_index,
                mode,
                focused,
                state.notes_visible,
            );
            let rows = left.lines.len().max(right.len());
            let gap_str = spaces(gap);
            let mut lines = Vec::with_capacity(rows);
            for index in 0..rows {
                let left_raw = left.lines.get(index).cloned().unwrap_or_default();
                let right_raw = right.get(index).cloned().unwrap_or_default();
                let left_clamped = truncate_to_width(&left_raw, left_width, "", false);
                let left_pad = spaces(left_width.saturating_sub(visible_width(&left_clamped)));
                let joined = format!("{left_clamped}{left_pad}{gap_str}{right_raw}");
                lines.push(truncate_line(&joined, width));
            }
            BodyRender {
                lines,
                cursor: None, // preview modes never have an inline editor cursor
                focused_range,
            }
        }
    }
}

/// `renderPaddedPreviewLines` — right-align the box inside the column with
/// ≥1 col of left padding; a line wider than the box slides left.
#[allow(clippy::too_many_arguments)] // mirrors the upstream signature
fn render_padded_preview_lines(
    block: &mut PreviewBlockRenderer,
    col_width: usize,
    selected_index: usize,
    mode: PreviewLayoutMode,
    focused: bool,
    notes_visible: bool,
) -> Vec<String> {
    let inner = col_width.saturating_sub(PREVIEW_PADDING_LEFT).max(1);
    let content_lines = block.render_block(inner, selected_index, mode, focused, notes_visible);
    let box_width = content_lines
        .first()
        .map(|line| visible_width(line))
        .unwrap_or(0)
        .max(1);
    let box_aligned_pad = PREVIEW_PADDING_LEFT.max(col_width.saturating_sub(box_width));
    content_lines
        .into_iter()
        .map(|line| {
            if line.is_empty() {
                return line;
            }
            let pad = PREVIEW_PADDING_LEFT
                .min(box_aligned_pad.min(col_width.saturating_sub(visible_width(&line))));
            let content = truncate_to_width(&line, col_width.saturating_sub(pad), "", false);
            format!("{}{content}", spaces(pad))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::types::OptionData;

    fn option(label: &str, preview: Option<&str>) -> OptionData {
        OptionData {
            label: label.to_owned(),
            description: "d".to_owned(),
            preview: preview.map(str::to_owned),
        }
    }

    fn question(multi: bool) -> QuestionData {
        QuestionData {
            question: "Pick one".to_owned(),
            header: "H".to_owned(),
            options: vec![option("A", Some("# A\n\nfirst")), option("B", None)],
            multi_select: multi.then_some(true),
        }
    }

    #[test]
    fn decide_layout_and_gates_match_the_upstream_matrix() {
        assert_eq!(decide_layout(100, 100), PreviewLayoutMode::SideBySide);
        assert_eq!(decide_layout(120, 100), PreviewLayoutMode::SideBySide);
        assert_eq!(decide_layout(99, 120), PreviewLayoutMode::Stacked);
        assert_eq!(decide_layout(120, 99), PreviewLayoutMode::Stacked);
    }

    #[test]
    fn preview_mode_gates_multi_input_and_missing_previews() {
        let single = question(false);
        assert_eq!(
            preview_mode(&single, false, 100, 100),
            Some(PreviewLayoutMode::SideBySide)
        );
        assert_eq!(preview_mode(&single, true, 120, 120), None);
        let multi = question(true);
        assert_eq!(preview_mode(&multi, false, 120, 120), None);
        let bare = QuestionData {
            question: "q".to_owned(),
            header: "H".to_owned(),
            options: vec![option("A", None), option("B", None)],
            multi_select: None,
        };
        assert_eq!(preview_mode(&bare, false, 120, 120), None);
    }

    #[test]
    fn bordered_box_pads_truncates_and_reports_hidden() {
        let lines = vec!["hello".to_owned(), "world".to_owned()];
        let plain: Vec<String> = render_bordered_box(&lines, 11, |text| text.to_owned(), 0)
            .iter()
            .map(|line| strip_ansi(line))
            .collect();
        assert_eq!(plain[0], "┌─────────┐");
        assert_eq!(plain[1], "│ hello   │");
        assert_eq!(plain[2], "│ world   │");
        assert_eq!(plain[3], "└─────────┘");
        let hidden = render_bordered_box(&lines, 16, |text| text.to_owned(), 3);
        let bottom = strip_ansi(hidden.last().expect("bottom"));
        assert!(bottom.contains("✂ ── 3 lines hidden ──"), "{bottom}");
        assert!(bottom.starts_with('└') && bottom.ends_with('┘'));
    }

    #[test]
    fn fence_markers_are_stripped_after_ansi_cleaning() {
        let lines = vec![
            "\u{1b}[32m```rust\u{1b}[0m".to_owned(),
            "let x = 1;".to_owned(),
            "```".to_owned(),
            "text".to_owned(),
        ];
        let stripped = strip_fence_markers(&lines);
        assert_eq!(stripped.len(), 2);
        assert_eq!(stripped[0], "let x = 1;");
    }

    #[test]
    fn box_dimensions_floor_and_cap() {
        let narrow = vec!["ab".to_owned()];
        assert_eq!(
            compute_box_dimensions(&narrow, 80),
            (BOX_MIN_CONTENT_WIDTH, BOX_MIN_CONTENT_WIDTH + 4)
        );
        let wide = ["x".repeat(90)];
        assert_eq!(compute_box_dimensions(&wide, 60), (60, 64));
        let padded = vec!["ab  ".to_owned()];
        assert_eq!(
            compute_box_dimensions(&padded, 4),
            (4, 8),
            "trailing whitespace is not measured"
        );
    }

    #[test]
    fn block_renderer_caps_heights_and_gates_the_affordance() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let long_preview = "para\n\n".to_owned() + &"line\n".repeat(40);
        let question = QuestionData {
            question: "q".to_owned(),
            header: "H".to_owned(),
            options: vec![option("A", Some(&long_preview)), option("B", None)],
            multi_select: None,
        };
        let mut block = PreviewBlockRenderer::new(&question, theme, i18n);
        let stacked = block.render_block(60, 0, PreviewLayoutMode::Stacked, true, false);
        // Cap 15 - borders 2 - affordance 2 = 11 content rows + chrome 4.
        assert_eq!(stacked.len(), 15);
        let bottom = strip_ansi(stacked.get(12).expect("bottom border"));
        assert!(bottom.contains("lines hidden"), "{bottom}");
        let affordance = stacked.last().expect("affordance row");
        assert!(affordance.contains("Notes: press n to add notes"));

        let gated = block.render_block(60, 0, PreviewLayoutMode::Stacked, true, true);
        assert_eq!(gated.last().map(String::as_str), Some(""));

        let placeholder = block.render_block(60, 1, PreviewLayoutMode::Stacked, true, false);
        assert!(placeholder
            .iter()
            .any(|line| strip_ansi(line).contains("No preview available")));
    }

    #[test]
    fn compose_side_by_side_places_preview_right_of_options() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let single = question(false);
        let items = crate::state::build::build_items_for_question(&single, &i18n);
        let state = QuestionnaireState::initial();
        let body = compose(
            &state,
            &single,
            &items,
            std::slice::from_ref(&single),
            std::slice::from_ref(&items),
            &i18n,
            &theme,
            "",
            None,
            120,
            120,
        );
        let plain: Vec<String> = body.lines.iter().map(|line| strip_ansi(line)).collect();
        assert!(plain[0].contains("1. A"), "{plain:?}");
        assert!(
            plain.iter().any(|line| line.contains('┌')),
            "bordered preview box on the right: {plain:?}"
        );
        for line in &body.lines {
            assert!(visible_width(line) <= 120, "{line}");
        }
    }

    #[test]
    fn compose_stacked_places_preview_below_options() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let single = question(false);
        let items = crate::state::build::build_items_for_question(&single, &i18n);
        let state = QuestionnaireState::initial();
        let body = compose(
            &state,
            &single,
            &items,
            std::slice::from_ref(&single),
            std::slice::from_ref(&items),
            &i18n,
            &theme,
            "",
            None,
            80,
            80,
        );
        let plain: Vec<String> = body.lines.iter().map(|line| strip_ansi(line)).collect();
        let option_rows = plain
            .iter()
            .position(|line| line.contains("Type something."));
        let box_top = plain.iter().position(|line| line.contains('┌'));
        let (option_rows, box_top) = (option_rows.expect("rows"), box_top.expect("box"));
        assert!(box_top > option_rows, "preview below options: {plain:?}");
        // One blank gap row between options and the box.
        assert_eq!(plain[box_top - 1], "");
    }

    fn strip_ansi(text: &str) -> String {
        strip_sgr_and_osc8(text)
    }
}
