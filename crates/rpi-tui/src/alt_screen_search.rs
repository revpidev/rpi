//! Fullscreen transcript search — port of
//! `packages/tui/src/alt-screen-search.ts` @ pi `9841914`
//! (introduced by `00121ed99` #7913, UI reworked in `7d399e7be` #8800,
//! linear-time matching in `2d4116333`).
//!
//! Intentional differences (FR-A R8, internal-only):
//! - The upstream ASCII fast path (`PRINTABLE_ASCII`, alt-screen-search.ts:32)
//!   indexes complete non-space runs per span exactly like upstream; the
//!   per-cell GC avoidance rationale does not apply to Rust, but the span
//!   shape (`linear_columns`) drives the column arithmetic and is preserved
//!   verbatim so multi-row matches map back to identical columns.
//! - `new RegExp(escapeRegExp(query), "giu")` becomes the `regex` crate with
//!   `case_insensitive(true)` + `unicode(true)` — same simple-fold matching
//!   semantics, no per-call allocation of a JS RegExp object.
//! - `AltScreenSearchIndex::search` returns an `Arc`-shared match list
//!   instead of a live JS array reference (same reuse semantics).

use std::cell::Cell;
use std::sync::Arc;

use regex::RegexBuilder;

use crate::components::input::{Input, PlaceholderStyleFn};
use crate::keybindings::get_keybindings;
use crate::tui::{Component, Focusable};
use crate::utils::{
    get_grapheme_segmenter, strip_terminal_sequences, truncate_to_width, visible_width,
};

// =============================================================================
// Search corpus (alt-screen-search.ts:14-86)
// =============================================================================

/// `SearchSourceSpan` (alt-screen-search.ts:8-14). Offsets index
/// [`SearchCorpus::text`] (byte offsets in Rust; UTF-16 units upstream — the
/// difference is unit-internal, never observable: spans partition the corpus
/// and the linear-column arithmetic only ever runs inside all-ASCII spans
/// where bytes, chars, and columns are 1:1).
#[derive(Debug, Clone)]
struct SearchSourceSpan {
    text_start: usize,
    text_end: usize,
    row: usize,
    start_col: usize,
    end_col: usize,
    linear_columns: bool,
}

/// `SearchCorpus` (alt-screen-search.ts:16-19).
#[derive(Debug, Clone, Default)]
struct SearchCorpus {
    text: String,
    spans: Vec<SearchSourceSpan>,
}

/// `PRINTABLE_ASCII` (alt-screen-search.ts:32): the fast path applies when
/// the whole rendered line is printable ASCII.
fn is_printable_ascii(line: &str) -> bool {
    line.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}

/// Pending-separator helper shared by both corpus paths
/// (`appendSeparator`, alt-screen-search.ts:29-33).
struct CorpusBuilder {
    chunks: Vec<String>,
    spans: Vec<SearchSourceSpan>,
    text_length: usize,
    pending_separator: bool,
}

impl CorpusBuilder {
    fn append_separator(&mut self) {
        if !self.pending_separator {
            return;
        }
        self.chunks.push(" ".to_string());
        self.text_length += 1;
        self.pending_separator = false;
    }
}

/// `buildSearchCorpus` (alt-screen-search.ts:22-70): linearize rendered
/// lines into a searchable text plus a (row, column) span table. Runs of
/// whitespace — including the line joins — collapse to single separator
/// spaces, so queries match across wrapped rows (the corpus is rebuilt from
/// the already-rendered screen, never shown to the user).
fn build_search_corpus(lines: &[String]) -> SearchCorpus {
    let mut builder = CorpusBuilder {
        chunks: Vec::new(),
        spans: Vec::new(),
        text_length: 0,
        pending_separator: false,
    };

    for (row, raw_line) in lines.iter().enumerate() {
        let line = strip_terminal_sequences(raw_line);
        let mut column: usize = 0;

        if is_printable_ascii(&line) {
            // Rendered transcripts are overwhelmingly ASCII. Index complete
            // non-space runs at once instead of allocating one mapping per
            // cell (alt-screen-search.ts:41-68).
            let mut index = 0;
            let bytes = line.as_bytes();
            while index < bytes.len() {
                if bytes[index] == 0x20 {
                    if builder.text_length > 0 {
                        builder.pending_separator = true;
                    }
                    column += 1;
                    index += 1;
                    continue;
                }
                let mut end = index + 1;
                while end < bytes.len() && bytes[end] != 0x20 {
                    end += 1;
                }
                builder.append_separator();
                let text = &line[index..end];
                builder.chunks.push(text.to_string());
                builder.spans.push(SearchSourceSpan {
                    text_start: builder.text_length,
                    text_end: builder.text_length + text.len(),
                    row,
                    start_col: column,
                    end_col: column + text.len(),
                    linear_columns: true,
                });
                builder.text_length += text.len();
                column += text.len();
                index = end;
            }
        } else {
            // Grapheme path (alt-screen-search.ts:69-86): one span per
            // grapheme cluster, columns by visible width.
            for grapheme in get_grapheme_segmenter().segment(&line) {
                let width = visible_width(grapheme);
                // `/^\s+$/u.test(text)` on a single cluster: whitespace iff
                // every char is whitespace.
                if !grapheme.is_empty() && grapheme.chars().all(char::is_whitespace) {
                    if builder.text_length > 0 {
                        builder.pending_separator = true;
                    }
                    column += width;
                    continue;
                }
                builder.append_separator();
                builder.chunks.push(grapheme.to_string());
                builder.spans.push(SearchSourceSpan {
                    text_start: builder.text_length,
                    text_end: builder.text_length + grapheme.len(),
                    row,
                    start_col: column,
                    end_col: column + width,
                    linear_columns: false,
                });
                builder.text_length += grapheme.len();
                column += width;
            }
        }
        if builder.text_length > 0 {
            builder.pending_separator = true;
        }
    }

    SearchCorpus {
        text: builder.chunks.concat(),
        spans: builder.spans,
    }
}

// =============================================================================
// Query normalization and matching (alt-screen-search.ts:88-148)
// =============================================================================

/// `normalizeQuery` (alt-screen-search.ts:108-110): collapse whitespace runs
/// to a single space and trim.
fn normalize_query(query: &str) -> String {
    let mut result = String::with_capacity(query.len());
    let mut pending_space = false;
    for ch in query.chars() {
        if ch.is_whitespace() {
            pending_space = !result.is_empty();
        } else {
            if pending_space {
                result.push(' ');
                pending_space = false;
            }
            result.push(ch);
        }
    }
    result
}

/// `escapeRegExp` (alt-screen-search.ts:112-114).
fn escape_reg_exp(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(
            ch,
            '.' | '*' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\'
        ) {
            result.push('\\');
        }
        result.push(ch);
    }
    result
}

/// `AltScreenSearchSegment` (alt-screen-search.ts:21-25): a (row, column)
/// highlight range in the rendered transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltScreenSearchSegment {
    pub row: usize,
    pub start_col: usize,
    pub end_col: usize,
}

/// `AltScreenSearchMatch` (alt-screen-search.ts:27-29).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltScreenSearchMatch {
    pub segments: Vec<AltScreenSearchSegment>,
}

/// `findSearchCorpusMatches` (alt-screen-search.ts:116-148): case-insensitive
/// Unicode substring matches over the corpus, mapped back to (row, column)
/// segments. Spans that a match crosses completely (line joins) merge into
/// per-row segments; adjacent same-row segments merge.
fn find_search_corpus_matches(
    corpus: &SearchCorpus,
    normalized_query: &str,
) -> Vec<AltScreenSearchMatch> {
    if normalized_query.is_empty() {
        return Vec::new();
    }
    // `/${escapeRegExp(query)}/giu` — a builder failure means the escaped
    // pattern was rejected (not reachable for escaped text); degrade to no
    // matches instead of panicking in the render path.
    let Ok(expression) = RegexBuilder::new(&escape_reg_exp(normalized_query))
        .case_insensitive(true)
        .unicode(true)
        .build()
    else {
        return Vec::new();
    };

    let mut matches: Vec<AltScreenSearchMatch> = Vec::new();
    let mut span_index = 0usize;

    for mat in expression.find_iter(&corpus.text) {
        let start = mat.start();
        let end = mat.end();
        while span_index < corpus.spans.len() && corpus.spans[span_index].text_end <= start {
            span_index += 1;
        }

        let mut segments: Vec<AltScreenSearchSegment> = Vec::new();
        for span in &corpus.spans[span_index..] {
            if span.text_start >= end {
                break;
            }
            if span.text_end <= start {
                continue;
            }
            let start_col = if span.linear_columns {
                span.start_col + (start.max(span.text_start) - span.text_start)
            } else {
                span.start_col
            };
            let end_col = if span.linear_columns {
                span.start_col + (end.min(span.text_end) - span.text_start)
            } else {
                span.end_col
            };
            let merge = segments
                .last_mut()
                .filter(|previous| previous.row == span.row && start_col <= previous.end_col);
            match merge {
                Some(previous) => previous.end_col = previous.end_col.max(end_col),
                None => segments.push(AltScreenSearchSegment {
                    row: span.row,
                    start_col,
                    end_col,
                }),
            }
        }
        while span_index < corpus.spans.len() && corpus.spans[span_index].text_end <= end {
            span_index += 1;
        }
        if !segments.is_empty() {
            matches.push(AltScreenSearchMatch { segments });
        }
    }

    matches
}

/// `AltScreenSearchResult` (alt-screen-search.ts:150-153). The match list is
/// `Arc`-shared: an unchanged search returns the same instance (upstream
/// returns the live array).
#[derive(Debug, Clone)]
pub struct AltScreenSearchResult {
    pub matches: Arc<Vec<AltScreenSearchMatch>>,
    pub changed: bool,
}

/// Cache the searchable corpus and matches while rendered transcript lines
/// remain unchanged (`AltScreenSearchIndex`, alt-screen-search.ts:158-182,
/// linear-time per `2d4116333`).
#[derive(Default)]
pub struct AltScreenSearchIndex {
    source_lines: Option<Vec<String>>,
    corpus: Option<SearchCorpus>,
    normalized_query: Option<String>,
    matches: Arc<Vec<AltScreenSearchMatch>>,
}

impl AltScreenSearchIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// `search` (alt-screen-search.ts:168-182).
    pub fn search(&mut self, lines: &[String], query: &str) -> AltScreenSearchResult {
        let mut source_changed = match &self.source_lines {
            Some(source) => source.len() != lines.len(),
            None => true,
        };
        if !source_changed {
            if let Some(source) = &self.source_lines {
                for (index, line) in lines.iter().enumerate() {
                    if &source[index] != line {
                        source_changed = true;
                        break;
                    }
                }
            }
        }
        if source_changed || self.corpus.is_none() {
            self.source_lines = Some(lines.to_vec());
            self.corpus = Some(build_search_corpus(lines));
        }

        let normalized_query = normalize_query(query);
        let changed = source_changed || Some(&normalized_query) != self.normalized_query.as_ref();
        if changed {
            let matches = self
                .corpus
                .as_ref()
                .map(|corpus| find_search_corpus_matches(corpus, &normalized_query))
                .unwrap_or_default();
            self.normalized_query = Some(normalized_query);
            self.matches = Arc::new(matches);
        }
        AltScreenSearchResult {
            matches: Arc::clone(&self.matches),
            changed,
        }
    }
}

/// `findAltScreenSearchMatches` (alt-screen-search.ts:185-187): one-shot
/// search without caching.
pub fn find_alt_screen_search_matches(lines: &[String], query: &str) -> Vec<AltScreenSearchMatch> {
    let normalized_query = normalize_query(query);
    if normalized_query.is_empty() {
        return Vec::new();
    }
    find_search_corpus_matches(&build_search_corpus(lines), &normalized_query)
}

/// `getAltScreenSearchMatchKey` (alt-screen-search.ts:190-194): stable
/// identity for the currently selected match (survives corpus rebuilds).
pub fn get_alt_screen_search_match_key(search_match: &AltScreenSearchMatch) -> String {
    match (search_match.segments.first(), search_match.segments.last()) {
        (Some(first), Some(last)) => {
            format!(
                "{}:{}:{}:{}",
                first.row, first.start_col, last.row, last.end_col
            )
        }
        _ => String::new(),
    }
}

// =============================================================================
// Search component (alt-screen-search.ts:196-338)
// =============================================================================

/// `onQueryChange` callback (alt-screen-search.ts:198).
pub type SearchQueryChangeFn = Arc<dyn Fn(&str) + Send + Sync>;

/// `navigationButtonStyle` (alt-screen-search.ts:204-219): styles a
/// navigation button label, `hovered` marking the one under the pointer.
pub type NavigationButtonStyleFn = Arc<dyn Fn(&str, bool) -> String + Send + Sync>;

fn identity_navigation_button_style(text: &str, _hovered: bool) -> String {
    text.to_string()
}

/// `AltScreenSearchComponent` (alt-screen-search.ts:196): the bordered
/// query box with placeholder, `n/m` counter, and clickable ↑/↓ buttons.
pub struct AltScreenSearchComponent {
    input: Input,
    on_query_change: SearchQueryChangeFn,
    navigation_button_style: NavigationButtonStyleFn,
    result_count: usize,
    result_index: i64,
    /// Navigation-button hit columns, written by `render` (upstream assigns
    /// the private fields inside render; interior mutability because render
    /// takes `&self`).
    previous_button_start: Cell<i32>,
    previous_button_end: Cell<i32>,
    next_button_start: Cell<i32>,
    next_button_end: Cell<i32>,
    hovered_navigation_direction: Option<i32>,
    focused: bool,
    /// Test injection for upstream's `process.platform === "darwin"`
    /// reads (alt-screen-search.ts:268); production uses
    /// `cfg!(target_os = "macos")`.
    darwin_override: Option<bool>,
}

impl AltScreenSearchComponent {
    /// Constructor (alt-screen-search.ts:198-221).
    pub fn new(
        on_query_change: SearchQueryChangeFn,
        navigation_button_style: Option<NavigationButtonStyleFn>,
    ) -> Self {
        let placeholder_style: PlaceholderStyleFn =
            Arc::new(|text: &str| format!("\x1b[2m{text}\x1b[22m"));
        Self {
            input: Input::with_options(crate::components::input::InputOptions {
                prompt: Some(" ".to_string()),
                placeholder: Some("Find in transcript".to_string()),
                placeholder_style: Some(placeholder_style),
            }),
            on_query_change,
            navigation_button_style: navigation_button_style
                .unwrap_or_else(|| Arc::new(identity_navigation_button_style)),
            result_count: 0,
            result_index: -1,
            previous_button_start: Cell::new(-1),
            previous_button_end: Cell::new(-1),
            next_button_start: Cell::new(-1),
            next_button_end: Cell::new(-1),
            hovered_navigation_direction: None,
            focused: false,
            darwin_override: None,
        }
    }

    /// Test injection for `process.platform === "darwin"`.
    #[doc(hidden)]
    pub fn set_darwin_override(&mut self, darwin: Option<bool>) {
        self.darwin_override = darwin;
    }

    /// `setResult` (alt-screen-search.ts:232-235).
    pub fn set_result(&mut self, index: i64, count: usize) {
        self.result_index = index;
        self.result_count = count;
    }

    /// `getNavigationDirectionAt` (alt-screen-search.ts:237-243): component-
    /// local coordinates; the buttons live on the third rendered row.
    pub fn get_navigation_direction_at(&self, row: i32, column: i32) -> Option<i32> {
        if row != 2 {
            return None;
        }
        if column >= self.previous_button_start.get() && column < self.previous_button_end.get() {
            return Some(-1);
        }
        if column >= self.next_button_start.get() && column < self.next_button_end.get() {
            return Some(1);
        }
        None
    }

    /// `setHoveredNavigationDirection` (alt-screen-search.ts:245-249):
    /// returns whether the hover state changed (drives a re-render).
    pub fn set_hovered_navigation_direction(&mut self, direction: Option<i32>) -> bool {
        if direction == self.hovered_navigation_direction {
            return false;
        }
        self.hovered_navigation_direction = direction;
        true
    }

    /// `handleInput` (alt-screen-search.ts:251-257): forward to the inner
    /// input and report query changes.
    pub fn handle_search_input(&mut self, data: &str) {
        let previous = self.input.get_value().to_string();
        self.input.handle_input(data);
        let query = self.input.get_value().to_string();
        if query != previous {
            (self.on_query_change)(&query);
        }
    }

    /// `formatKey` (alt-screen-search.ts:236-247): capitalize each `+`-part,
    /// renaming `alt` to `Option` on macOS; `Unbound` without a key.
    fn format_key(&self, key: Option<&str>) -> String {
        let Some(key) = key else {
            return "Unbound".to_string();
        };
        let darwin = self.darwin_override.unwrap_or(cfg!(target_os = "macos"));
        key.split('+')
            .map(|part| {
                if darwin && part.eq_ignore_ascii_case("alt") {
                    "Option".to_string()
                } else {
                    let mut chars = part.chars();
                    match chars.next() {
                        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                        None => String::new(),
                    }
                }
            })
            .collect::<Vec<_>>()
            .join("+")
    }
}

impl Component for AltScreenSearchComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let safe_width = width.max(1);
        let inner_width = safe_width.saturating_sub(2);

        let (previous_key, next_key) = {
            let keybindings = get_keybindings()
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = keybindings
                .get_keys_by_id("tui.altScreen.searchPrevious")
                .into_iter()
                .next();
            let next = keybindings
                .get_keys_by_id("tui.altScreen.searchNext")
                .into_iter()
                .next();
            (
                self.format_key(previous.as_deref()),
                self.format_key(next.as_deref()),
            )
        };

        let query = self.input.get_value();
        let result: String = if query.is_empty() {
            String::new()
        } else if self.result_count == 0 {
            "No matches".to_string()
        } else {
            format!("{}/{}", self.result_index + 1, self.result_count)
        };
        let result_space = inner_width.saturating_sub(3);
        let visible_result = truncate_to_width(&result, result_space, "", false);
        let result_text = if visible_result.is_empty() {
            String::new()
        } else {
            format!("\x1b[2m {visible_result} \x1b[22m")
        };
        let input_width = inner_width.saturating_sub(visible_width(&result_text));
        let input_line = self
            .input
            .render(input_width.max(1))
            .into_iter()
            .next()
            .unwrap_or_default();
        let input_line = truncate_to_width(&input_line, input_width, "", false);
        let input_padding = " ".repeat(input_width.saturating_sub(visible_width(&input_line)));
        let content = format!("{input_line}{input_padding}{result_text}");

        let mut previous_button = format!("↑ {previous_key}");
        let mut next_button = format!("↓ {next_key}");
        let mut separator = " · ".to_string();
        let outer_gap_width: i32 = 1;
        let available_controls_width =
            (inner_width as i32 - outer_gap_width * 2 - 1).max(0) as usize;
        let mut controls_width = visible_width(&previous_button)
            + visible_width(&separator)
            + visible_width(&next_button);
        if controls_width > available_controls_width {
            previous_button = "↑".to_string();
            next_button = "↓".to_string();
            separator = " ".to_string();
            controls_width = visible_width(&previous_button)
                + visible_width(&separator)
                + visible_width(&next_button);
        }
        let show_buttons = controls_width <= available_controls_width;
        let rendered_buttons = if show_buttons {
            format!(
                "{}{separator}{}",
                (self.navigation_button_style)(
                    &previous_button,
                    self.hovered_navigation_direction == Some(-1)
                ),
                (self.navigation_button_style)(
                    &next_button,
                    self.hovered_navigation_direction == Some(1)
                ),
            )
        } else {
            String::new()
        };
        let outer_gaps_width: i32 = if show_buttons { outer_gap_width * 2 } else { 0 };
        let right_rule_width: i32 = if !rendered_buttons.is_empty()
            && (inner_width as i32) > controls_width as i32 + outer_gaps_width
        {
            1
        } else {
            0
        };
        let left_rule_width = (inner_width as i32
            - if show_buttons {
                controls_width as i32
            } else {
                0
            }
            - outer_gaps_width
            - right_rule_width)
            .max(0);

        let previous_start = 1 + left_rule_width + outer_gap_width;
        if show_buttons {
            let previous_end = previous_start + visible_width(&previous_button) as i32;
            let next_start = previous_end + visible_width(&separator) as i32;
            let next_end = next_start + visible_width(&next_button) as i32;
            self.previous_button_start.set(previous_start);
            self.previous_button_end.set(previous_end);
            self.next_button_start.set(next_start);
            self.next_button_end.set(next_end);
        } else {
            self.previous_button_start.set(-1);
            self.previous_button_end.set(-1);
            self.next_button_start.set(-1);
            self.next_button_end.set(-1);
        }

        if safe_width == 1 {
            return vec!["┌".to_string(), "│".to_string(), "└".to_string()];
        }

        // (The width-1 degenerate form above still records the button
        // ranges — a hidden button keeps start/end -1 — matching upstream's
        // inline assignments.)
        let buttons_space = if show_buttons { " " } else { "" };
        vec![
            format!("┌{}┐", "─".repeat(inner_width)),
            format!("│{content}│"),
            format!(
                "└{}{buttons_space}{rendered_buttons}{buttons_space}{}┘",
                "─".repeat(left_rule_width.max(0) as usize),
                "─".repeat(right_rule_width.max(0) as usize)
            ),
        ]
    }

    fn handle_input(&mut self, data: &str) {
        self.handle_search_input(data);
    }

    fn invalidate(&mut self) {
        self.input.invalidate();
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn as_search_component(&self) -> Option<&Self> {
        Some(self)
    }

    fn as_search_component_mut(&mut self) -> Option<&mut Self> {
        Some(self)
    }
}

impl Focusable for AltScreenSearchComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_query_collapses_whitespace_and_trims() {
        assert_eq!(normalize_query("  foo   bar\nbaz  "), "foo bar baz");
        assert_eq!(normalize_query("\t\t"), "");
        // NBSP is JS \s (and Rust White_Space) — collapses like a space.
        assert_eq!(normalize_query("a\u{00a0}b"), "a b");
    }

    #[test]
    fn searches_normalized_rendered_transcript_text_across_rows() {
        // (tui-alt-screen.test.ts:510)
        let matches = find_alt_screen_search_matches(
            &["alpha QUICK".to_string(), "brown fox".to_string()],
            "quick brown",
        );
        assert_eq!(
            matches,
            vec![AltScreenSearchMatch {
                segments: vec![
                    AltScreenSearchSegment {
                        row: 0,
                        start_col: 6,
                        end_col: 11
                    },
                    AltScreenSearchSegment {
                        row: 1,
                        start_col: 0,
                        end_col: 5
                    },
                ]
            }]
        );
    }

    #[test]
    fn maps_ascii_and_unicode_matches_back_to_rendered_columns() {
        // (tui-alt-screen.test.ts:521): ANSI stripped in the corpus; the
        // grapheme path counts visible columns (界 = 2, 🙂 = 2).
        let matches = find_alt_screen_search_matches(
            &["\x1b[31mfoo  bar\x1b[0m".to_string(), "A界🙂éZ".to_string()],
            "oo   bar\nA界🙂é",
        );
        assert_eq!(
            matches,
            vec![AltScreenSearchMatch {
                segments: vec![
                    AltScreenSearchSegment {
                        row: 0,
                        start_col: 1,
                        end_col: 3
                    },
                    AltScreenSearchSegment {
                        row: 0,
                        start_col: 5,
                        end_col: 8
                    },
                    AltScreenSearchSegment {
                        row: 1,
                        start_col: 0,
                        end_col: 6
                    },
                ]
            }]
        );
    }

    #[test]
    fn case_insensitive_matching_covers_non_ascii_pairs() {
        // `giu` semantics: É/é and Ü/ü fold together (FR-A R3).
        let matches =
            find_alt_screen_search_matches(&["CAFÉ über  Straße".to_string()], "café ÜBER");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].segments[0].start_col, 0);
        assert_eq!(matches[0].segments[0].end_col, 4);
        // "CAFÉ über" — the second match starts at column 5 (É is one
        // column wide).
        assert_eq!(matches[0].segments[1].start_col, 5);
        assert_eq!(matches[0].segments[1].end_col, 9);
    }

    #[test]
    fn regex_metacharacters_in_query_match_literally() {
        let matches = find_alt_screen_search_matches(
            &["a.b*c [x] (y) +z?".to_string(), "abc".to_string()],
            ".b*c [x] (y) +z?",
        );
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].segments[0].row, 0);
        assert_eq!(matches[0].segments[0].start_col, 1);
    }

    #[test]
    fn successive_matches_do_not_overlap() {
        // matchAll semantics: the second `aa` in `aaaa` starts after the
        // first match ends.
        let matches = find_alt_screen_search_matches(&["aaaa".to_string()], "aa");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].segments[0].start_col, 0);
        assert_eq!(matches[1].segments[0].start_col, 2);
    }

    #[test]
    fn index_reuses_matches_until_query_or_lines_change() {
        // (tui-alt-screen.test.ts:534)
        let mut index = AltScreenSearchIndex::new();
        let lines = vec!["alpha needle".to_string(), "omega".to_string()];

        let initial = index.search(&lines, "needle");
        assert!(initial.changed);
        assert_eq!(initial.matches.len(), 1);

        let cached = index.search(&lines, "needle");
        assert!(!cached.changed);
        assert!(Arc::ptr_eq(&cached.matches, &initial.matches));

        let changed_query = index.search(&lines, "omega");
        assert!(changed_query.changed);
        assert!(!Arc::ptr_eq(&changed_query.matches, &initial.matches));
        assert_eq!(
            changed_query.matches[0].segments,
            vec![AltScreenSearchSegment {
                row: 1,
                start_col: 0,
                end_col: 5
            }]
        );

        let changed_lines = index.search(
            &["alpha needle".to_string(), "no match".to_string()],
            "omega",
        );
        assert!(changed_lines.changed);
        assert!(changed_lines.matches.is_empty());
    }

    #[test]
    fn match_key_identifies_first_and_last_segments() {
        let key = get_alt_screen_search_match_key(&AltScreenSearchMatch {
            segments: vec![
                AltScreenSearchSegment {
                    row: 2,
                    start_col: 4,
                    end_col: 6,
                },
                AltScreenSearchSegment {
                    row: 3,
                    start_col: 0,
                    end_col: 2,
                },
            ],
        });
        assert_eq!(key, "2:4:3:2");
        assert_eq!(
            get_alt_screen_search_match_key(&AltScreenSearchMatch { segments: vec![] }),
            ""
        );
    }
}
