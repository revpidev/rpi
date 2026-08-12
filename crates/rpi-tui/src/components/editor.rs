//! Port of `packages/tui/src/components/editor.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - Cursor positions are in characters (upstream UTF-16 code units); every
//!   upstream cursor arithmetic (grapheme lengths, word-navigation results,
//!   string slicing) lands on the same character position in both spaces.
//! - `lastWidth` and `scrollOffset` are `Cell`s: the [`Component::render`]
//!   contract takes `&self` while upstream's render mutates these fields.
//! - Autocomplete provider calls are synchronous. The debounced (`@`
//!   attachment) path runs on a worker thread with a generation token +
//!   abort flag + text/cursor snapshot (the Rust counterpart of upstream's
//!   `startToken` + `AbortController` + snapshot triple check); results are
//!   applied at the next render or `handle_input`. Requests with a zero
//!   debounce (Tab / force) run synchronously inside `handle_input`, so the
//!   single-suggestion auto-apply keeps its exact upstream behavior.
//! - `onSubmit`/`onChange` are `Box<dyn FnMut(&str) + Send>` fields instead
//!   of plain function properties.
//! - The `EditorTheme` is consumed by value (its `borderColor` moves into
//!   the editor, mirroring upstream's `this.borderColor = theme.borderColor`);
//!   `selectList` is an `Arc` shared with each recreated `SelectList`.
//! - The paste marker regex uses `[0-9]` where upstream uses `\d` (JS `\d`
//!   is ASCII-only; Rust regex `\d` would match Unicode digits).
//! - `expandPasteMarkers` replaces with the literal paste content (upstream
//!   `String.replace` with a function); the Rust `Regex::replace_all`
//!   closure avoids `$`-expansion in paste contents.
//! - Paste size thresholds count UTF-16 code units like upstream
//!   (`String.length`), so the `[paste #N N chars]` markers match upstream
//!   exactly for non-ASCII content.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use regex::{Captures, Regex};

use crate::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, GetSuggestionsOptions,
};
use crate::components::select_list::{
    SelectItem, SelectList, SelectListLayoutOptions, SelectListTheme,
};
use crate::keybindings::{get_keybindings, Keybinding};
use crate::keys::{decode_printable_key, matches_key};
use crate::kill_ring::{KillRing, KillRingPushOptions};
use crate::tui::{Component, Focusable, CURSOR_MARKER};
use crate::tui_main_screen::TuiMainScreen;
use crate::undo_stack::UndoStack;
use crate::utils::{
    cjk_break_regex, get_grapheme_segmenter, is_whitespace_char, slice_by_column, visible_width,
};
use crate::word_navigation::{
    find_word_backward, find_word_forward, segment_words, WordNavigationOptions, WordSegment,
};

/// Byte offset of the `chars`-th character in `text`; `chars` == char count
/// maps to `text.len()`. Cursors are always on char boundaries.
fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

/// Regex matching paste markers like `[paste #1 +123 lines]` or
/// `[paste #2 1234 chars]` (upstream `PASTE_MARKER_REGEX`, editor.ts:22).
const PASTE_MARKER_REGEX_STR: &str = r"\[paste #([0-9]+)( (\+[0-9]+ lines|[0-9]+ chars))?\]";

fn paste_marker_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(PASTE_MARKER_REGEX_STR).expect("static paste marker regex"))
}

/// Non-global version for single-segment testing (upstream
/// `PASTE_MARKER_SINGLE`, editor.ts:25).
fn paste_marker_single() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!("^(?:{PASTE_MARKER_REGEX_STR})$")).expect("static paste marker regex")
    })
}

/// Check if a segment is a paste marker (i.e. was merged by
/// `segmentWithMarkers`) (editor.ts:28-30).
fn is_paste_marker(segment: &str) -> bool {
    segment.len() >= 10 && paste_marker_single().is_match(segment)
}

/// The paste id of a marker segment, or `None` when it is not a marker
/// (upstream `Number(isPastedSegmented[1])`, editor.ts:1291-1295).
fn paste_marker_id(segment: &str) -> Option<u32> {
    if !is_paste_marker(segment) {
        return None;
    }
    paste_marker_single()
        .captures(segment)
        .and_then(|caps| caps[1].parse().ok())
}

/// Byte offsets of all marker spans whose numeric id exists in `valid_ids`
/// (upstream `segmentWithMarkers` marker scan, editor.ts:49-55).
fn paste_marker_spans(text: &str, valid_ids: &BTreeMap<u32, String>) -> Vec<(usize, usize)> {
    if valid_ids.is_empty() || !text.contains("[paste #") {
        return Vec::new();
    }
    let mut spans = Vec::new();
    for captures in paste_marker_regex().captures_iter(text) {
        let id: u32 = match captures[1].parse() {
            Ok(id) => id,
            Err(_) => continue,
        };
        if !valid_ids.contains_key(&id) {
            continue;
        }
        let matched = captures.get(0).expect("full match always present");
        let start = text[..matched.start()].chars().count();
        let end = start + text[matched.start()..matched.end()].chars().count();
        spans.push((start, end));
    }
    spans
}

/// A grapheme/word segment with its character offset (upstream
/// `Intl.SegmentData` minus `input`).
#[derive(Debug, Clone, Copy)]
pub struct EditorSegment<'a> {
    /// Character offset of the segment in the segmented text.
    pub index: usize,
    pub segment: &'a str,
}

/// Segment `text` into graphemes, merging segments that fall within valid
/// paste markers into single atomic segments (upstream
/// `segmentWithMarkers` with the grapheme segmenter, editor.ts:39-91).
fn grapheme_segments<'a>(
    text: &'a str,
    valid_ids: &BTreeMap<u32, String>,
) -> Vec<EditorSegment<'a>> {
    let mut segments = Vec::new();
    let mut index = 0usize;
    for segment in get_grapheme_segmenter().segment(text) {
        segments.push(EditorSegment { index, segment });
        index += segment.chars().count();
    }
    merge_marker_segments(text, segments, valid_ids)
}

/// Merge segments covered by valid paste markers into atomic segments
/// (upstream `segmentWithMarkers` merge loop, editor.ts:61-90).
fn merge_marker_segments<'a>(
    text: &'a str,
    base_segments: Vec<EditorSegment<'a>>,
    valid_ids: &BTreeMap<u32, String>,
) -> Vec<EditorSegment<'a>> {
    let markers = paste_marker_spans(text, valid_ids);
    if markers.is_empty() {
        return base_segments;
    }

    let mut result = Vec::with_capacity(base_segments.len());
    let mut marker_idx = 0usize;

    for seg in base_segments {
        // Skip past markers that are entirely before this segment.
        while marker_idx < markers.len() && markers[marker_idx].1 <= seg.index {
            marker_idx += 1;
        }

        let marker = markers.get(marker_idx).copied();

        if let Some((start, end)) = marker {
            if seg.index >= start && seg.index < end {
                // This segment falls inside a marker. If this is the first
                // segment of the marker, emit a merged segment.
                if seg.index == start {
                    let marker_text = &text[char_to_byte(text, start)..char_to_byte(text, end)];
                    result.push(EditorSegment {
                        index: start,
                        segment: marker_text,
                    });
                }
                // Otherwise skip (already merged into the first segment).
                continue;
            }
        }
        result.push(seg);
    }

    result
}

/// Word-granularity segmentation with paste-marker merging (upstream
/// `segment(text, "word")`, editor.ts:361-363). Merged marker segments are
/// not word-like (upstream drops `isWordLike` when merging, editor.ts:78-83).
pub fn word_segments_with_markers<'a>(
    text: &'a str,
    valid_ids: &BTreeMap<u32, String>,
) -> Vec<WordSegment<'a>> {
    let base = segment_words(text);
    if valid_ids.is_empty() || !text.contains("[paste #") {
        return base;
    }

    let markers = paste_marker_spans(text, valid_ids);
    if markers.is_empty() {
        return base;
    }

    // Index the base segments (the ICU-equivalent default word segmenter
    // does not expose character offsets).
    let mut indexed: Vec<(usize, WordSegment<'a>)> = Vec::with_capacity(base.len());
    let mut index = 0usize;
    for seg in base {
        indexed.push((index, seg));
        index += seg.segment.chars().count();
    }

    let mut result = Vec::with_capacity(indexed.len());
    let mut marker_idx = 0usize;

    for (start, seg) in indexed {
        while marker_idx < markers.len() && markers[marker_idx].1 <= start {
            marker_idx += 1;
        }

        let marker = markers.get(marker_idx).copied();

        if let Some((marker_start, marker_end)) = marker {
            if start >= marker_start && start < marker_end {
                if start == marker_start {
                    let marker_text =
                        &text[char_to_byte(text, marker_start)..char_to_byte(text, marker_end)];
                    result.push(WordSegment {
                        segment: marker_text,
                        is_word_like: false,
                    });
                }
                continue;
            }
        }
        result.push(seg);
    }

    result
}

/// Represents a chunk of text for word-wrap layout. Tracks both the text
/// content and its position in the original line (upstream `TextChunk`,
/// editor.ts:97-101).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub start_index: usize,
    pub end_index: usize,
}

/// Split a line into word-wrapped chunks. Wraps at word boundaries when
/// possible, falling back to character-level wrapping for words longer than
/// the available width (upstream `wordWrapLine`, editor.ts:114-206).
///
/// `pre_segmented` optionally provides pre-segmented graphemes (e.g. with
/// paste-marker awareness); when omitted the default grapheme segmenter is
/// used. Indices are character offsets.
pub fn word_wrap_line(
    line: &str,
    max_width: usize,
    pre_segmented: Option<&[EditorSegment<'_>]>,
) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk {
            text: String::new(),
            start_index: 0,
            end_index: 0,
        }];
    }

    let line_width = visible_width(line);
    if line_width <= max_width {
        return vec![TextChunk {
            text: line.to_string(),
            start_index: 0,
            end_index: line.chars().count(),
        }];
    }

    let mut chunks: Vec<TextChunk> = Vec::new();
    let segments: Vec<EditorSegment<'_>> = match pre_segmented {
        Some(pre_segmented) => pre_segmented.to_vec(),
        None => {
            let mut segments = Vec::new();
            let mut index = 0usize;
            for segment in get_grapheme_segmenter().segment(line) {
                segments.push(EditorSegment { index, segment });
                index += segment.chars().count();
            }
            segments
        }
    };

    let mut current_width = 0usize;
    let mut chunk_start = 0usize;

    // Wrap opportunity: the position after the last whitespace before a
    // non-whitespace grapheme, i.e. where a line break is allowed.
    let mut wrap_opp_index: Option<usize> = None;
    let mut wrap_opp_width = 0usize;

    for i in 0..segments.len() {
        let seg = segments[i];
        let grapheme = seg.segment;
        let g_width = visible_width(grapheme);
        let char_index = seg.index;
        let is_ws = !is_paste_marker(grapheme) && is_whitespace_char(grapheme);

        // Overflow check before advancing.
        if current_width + g_width > max_width {
            let mut backtracked = false;
            if let Some(opp) = wrap_opp_index {
                if current_width - wrap_opp_width + g_width <= max_width {
                    // Backtrack to last wrap opportunity (the remaining
                    // content plus the current grapheme still fits within
                    // maxWidth).
                    chunks.push(TextChunk {
                        text: line[char_to_byte(line, chunk_start)..char_to_byte(line, opp)]
                            .to_string(),
                        start_index: chunk_start,
                        end_index: opp,
                    });
                    chunk_start = opp;
                    current_width -= wrap_opp_width;
                    backtracked = true;
                }
            }
            if !backtracked && chunk_start < char_index {
                // No viable wrap opportunity: force-break at current
                // position. This also handles the case where backtracking to
                // a word boundary wouldn't help because the remaining
                // content plus the current grapheme (e.g. a wide character)
                // still exceeds maxWidth.
                chunks.push(TextChunk {
                    text: line[char_to_byte(line, chunk_start)..char_to_byte(line, char_index)]
                        .to_string(),
                    start_index: chunk_start,
                    end_index: char_index,
                });
                chunk_start = char_index;
                current_width = 0;
            }
            wrap_opp_index = None;
        }

        if g_width > max_width {
            // Single atomic segment wider than maxWidth (e.g. a paste marker
            // in a narrow terminal). Re-wrap it at grapheme granularity.
            //
            // The segment remains logically atomic for cursor
            // movement / editing — the split is purely visual for word-wrap
            // layout.
            let sub_chunks = word_wrap_line(grapheme, max_width, None);
            for sub_chunk in &sub_chunks[..sub_chunks.len() - 1] {
                chunks.push(TextChunk {
                    text: sub_chunk.text.clone(),
                    start_index: char_index + sub_chunk.start_index,
                    end_index: char_index + sub_chunk.end_index,
                });
            }
            let last = sub_chunks.last().expect("sub_chunks is never empty");
            chunk_start = char_index + last.start_index;
            current_width = visible_width(&last.text);
            wrap_opp_index = None;
            continue;
        }

        // Advance.
        current_width += g_width;

        // Record wrap opportunity: whitespace followed by non-whitespace
        // (multiple spaces join; the break point is after the last space),
        // or at a boundary where either side is CJK (CJK allows breaking
        // between any adjacent characters).
        let next = segments.get(i + 1).copied();
        if let Some(next) = next {
            if is_ws && (is_paste_marker(next.segment) || !is_whitespace_char(next.segment)) {
                wrap_opp_index = Some(next.index);
                wrap_opp_width = current_width;
            } else if !is_ws && !is_whitespace_char(next.segment) {
                let is_cjk = !is_paste_marker(grapheme) && cjk_break_regex(grapheme);
                let next_is_cjk = !is_paste_marker(next.segment) && cjk_break_regex(next.segment);
                if is_cjk || next_is_cjk {
                    wrap_opp_index = Some(next.index);
                    wrap_opp_width = current_width;
                }
            }
        }
    }

    // Push final chunk.
    chunks.push(TextChunk {
        text: line[char_to_byte(line, chunk_start)..].to_string(),
        start_index: chunk_start,
        end_index: line.chars().count(),
    });

    chunks
}

/// Editor text state (upstream `EditorState`, editor.ts:209-213).
#[derive(Debug, Clone)]
struct EditorState {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
}

/// Undo snapshot: editor text state plus the paste registry (upstream
/// `EditorSnapshot`, editor.ts:216-220).
#[derive(Debug, Clone)]
struct EditorSnapshot {
    state: EditorState,
    pastes: BTreeMap<u32, String>,
    paste_counter: u32,
}

/// A rendered layout line (upstream `LayoutLine`, editor.ts:222-226).
struct LayoutLine {
    text: String,
    has_cursor: bool,
    cursor_pos: Option<usize>,
}

/// Visual line mapping for cursor navigation (upstream
/// `buildVisualLineMap` entries, editor.ts:1732-1757).
#[derive(Debug, Clone, Copy)]
struct VisualLine {
    logical_line: usize,
    start_col: usize,
    length: usize,
}

/// Editor theme (upstream `EditorTheme`, editor.ts:228-231).
pub struct EditorTheme {
    /// Border color function (upstream `borderColor`).
    pub border_color: Box<dyn Fn(&str) -> String + Send + Sync>,
    /// Select list theme shared with each autocomplete list.
    pub select_list: Arc<SelectListTheme>,
}

/// Editor options (upstream `EditorOptions`, editor.ts:233-236).
#[derive(Debug, Default)]
pub struct EditorOptions {
    pub padding_x: Option<usize>,
    pub autocomplete_max_visible: Option<usize>,
}

/// Slash-command autocomplete list layout (editor.ts:238-241).
const SLASH_COMMAND_SELECT_LIST_LAYOUT: SelectListLayoutOptions = SelectListLayoutOptions {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(32),
    truncate_primary: None,
};

/// Attachment autocomplete debounce (editor.ts:243).
const ATTACHMENT_AUTOCOMPLETE_DEBOUNCE_MS: u64 = 20;

/// Default autocomplete trigger characters (editor.ts:244).
const DEFAULT_AUTOCOMPLETE_TRIGGER_CHARACTERS: [char; 2] = ['@', '#'];

/// JS `\s` character class contents (ECMA-262; includes U+FEFF, excludes
/// U+0085) — used to build the trigger/debounce patterns with the same
/// semantics as the upstream `RegExp` literals.
const JS_WHITESPACE_CLASS_CONTENT: &str =
    r"\t\n\v\f\r \u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

/// `escapeCharacterClass` (editor.ts:246-248).
fn escape_character_class(value: char) -> String {
    let mut out = String::with_capacity(2);
    if "\\^$.*+?()[\\]{}|-".contains(value) {
        out.push('\\');
    }
    out.push(value);
    out
}

/// `buildTriggerPattern` (editor.ts:250-252).
fn build_trigger_pattern(trigger_characters: &[char]) -> Regex {
    let class: String = trigger_characters
        .iter()
        .map(|c| escape_character_class(*c))
        .collect();
    Regex::new(&format!(
        "(?:^|[{JS_WHITESPACE_CLASS_CONTENT}])[{class}][^{JS_WHITESPACE_CLASS_CONTENT}]*$"
    ))
    .expect("static trigger pattern")
}

/// `buildDebouncePattern` (editor.ts:254-257).
fn build_debounce_pattern(trigger_characters: &[char]) -> Regex {
    let escaped_without_at: String = trigger_characters
        .iter()
        .filter(|c| **c != '@')
        .map(|c| escape_character_class(*c))
        .collect();
    Regex::new(&format!(
        "(?:^|[ \\t])(?:@(?:\"[^\"]*|[^{JS_WHITESPACE_CLASS_CONTENT}]*)|[{escaped_without_at}][^{JS_WHITESPACE_CLASS_CONTENT}]*)$"
    ))
    .expect("static debounce pattern")
}

/// `createScrollBorder` (editor.ts:259-268): `─── ↑ N more ` padded with
/// `─`, or truncated with an ellipsis when narrower than the indicator.
fn create_scroll_border(direction: &str, hidden_line_count: usize, width: usize) -> String {
    let available_width = width;
    let indicator = format!("─── {direction} {hidden_line_count} more ");
    let remaining = available_width as i64 - visible_width(&indicator) as i64;
    if remaining >= 0 {
        return format!("{indicator}{}", "─".repeat(remaining as usize));
    }

    let ellipsis = "...";
    let ellipsis_width = visible_width(ellipsis);
    let indicator_width = available_width.saturating_sub(ellipsis_width);
    format!(
        "{}{}",
        slice_by_column(&indicator, 0, indicator_width, true),
        ellipsis
    )
}

/// Last editing action, for kill-ring accumulation and undo coalescing
/// (upstream `lastAction`, editor.ts:323).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// Character jump direction (upstream `jumpMode`, editor.ts:326).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JumpDirection {
    Forward,
    Backward,
}

/// Cursor placement for `set_text_internal` (upstream
/// `"start" | "end"`, editor.ts:465).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorPlacement {
    Start,
    End,
}

/// Autocomplete picker mode (upstream `autocompleteState`,
/// editor.ts:299).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutocompleteState {
    Regular,
    Force,
}

/// Autocomplete UI state, grouped for interior mutability: the picker is
/// applied from [`Editor::drain_autocomplete`], which runs from
/// [`Component::render`] (`&self`) as well as `handle_input`.
#[derive(Default)]
struct AutocompleteUi {
    state: Option<AutocompleteState>,
    list: Option<SelectList>,
    prefix: String,
}

/// A debounced autocomplete request result waiting to be applied
/// (upstream `runAutocompleteRequest` result).
struct PendingAutocomplete {
    generation: u64,
    aborted: Arc<AtomicBool>,
    snapshot_text: String,
    snapshot_line: usize,
    snapshot_col: usize,
    suggestions: Option<AutocompleteSuggestions>,
}

/// Request options for [`Editor::request_autocomplete`] (upstream
/// `{ force, explicitTab }`, editor.ts:2165).
#[derive(Debug, Clone, Copy)]
struct RequestOptions {
    force: bool,
    explicit_tab: bool,
}

/// Submit callback (upstream `(text: string) => void`).
pub type SubmitFn = Box<dyn FnMut(&str) + Send>;
/// Change callback (upstream `(text: string) => void`).
pub type ChangeFn = Box<dyn FnMut(&str) + Send>;

/// Multi-line text editor (upstream `Editor`, editor.ts:270).
pub struct Editor {
    state: EditorState,

    /// Focusable interface — set by TUI when focus changes.
    focused: bool,

    tui: TuiMainScreen,
    /// Select list theme shared with each recreated autocomplete list.
    select_list_theme: Arc<SelectListTheme>,
    padding_x: usize,

    /// Last render width, for cursor navigation (upstream `lastWidth`,
    /// editor.ts:285). A `Cell`: render takes `&self`.
    last_width: Cell<usize>,

    /// Vertical scrolling offset (upstream `scrollOffset`, editor.ts:288).
    /// A `Cell`: render takes `&self`.
    scroll_offset: Cell<usize>,

    /// Border color (can be changed dynamically; upstream `borderColor`,
    /// editor.ts:291).
    pub border_color: Box<dyn Fn(&str) -> String + Send + Sync>,

    // Autocomplete support (editor.ts:294-306)
    autocomplete_provider: Option<Arc<dyn AutocompleteProvider>>,
    autocomplete_trigger_characters: Vec<char>,
    autocomplete_trigger_pattern: Regex,
    autocomplete_debounce_pattern: Regex,
    autocomplete_ui: RefCell<AutocompleteUi>,
    autocomplete_max_visible: usize,
    /// Generation token shared with worker threads; bumped on every request
    /// and cancel (upstream `autocompleteStartToken`). A `Cell`: the
    /// cancellation path also runs from `render` (`&self`).
    autocomplete_generation: Cell<u64>,
    /// Abort flag for the in-flight request (upstream `autocompleteAbort`).
    /// A `RefCell`: see `autocomplete_generation`.
    autocomplete_abort: RefCell<Option<Arc<AtomicBool>>>,
    /// Result slot written by the debounced worker thread.
    autocomplete_pending: Arc<Mutex<Option<PendingAutocomplete>>>,

    // Paste tracking for large pastes (editor.ts:309-310)
    pastes: BTreeMap<u32, String>,
    paste_counter: u32,

    // Bracketed paste mode buffering (editor.ts:313-314)
    paste_buffer: String,
    is_in_paste: bool,

    // Prompt history for up/down navigation (editor.ts:317-319)
    history: Vec<String>,
    /// -1 = not browsing, 0 = most recent, 1 = older, etc.
    history_index: i64,
    history_draft: Option<EditorState>,

    // Kill ring for Emacs-style kill/yank operations (editor.ts:322-323)
    kill_ring: KillRing,
    last_action: Option<LastAction>,

    // Character jump mode (editor.ts:326)
    jump_mode: Option<JumpDirection>,

    // Preferred visual column for vertical cursor movement (sticky column)
    // (editor.ts:329)
    preferred_visual_col: Option<usize>,

    // Pre-snap cursor column (editor.ts:336)
    snapped_from_cursor_col: Option<usize>,

    // Undo support (editor.ts:339)
    undo_stack: UndoStack<EditorSnapshot>,

    /// Called on submit (upstream `onSubmit`, editor.ts:341).
    pub on_submit: Option<SubmitFn>,
    /// Called on text change (upstream `onChange`, editor.ts:342).
    pub on_change: Option<ChangeFn>,
    /// When true, Enter is swallowed instead of submitting (upstream
    /// `disableSubmit`, editor.ts:343).
    pub disable_submit: bool,
}

impl Editor {
    pub fn new(tui: TuiMainScreen, theme: EditorTheme, options: EditorOptions) -> Self {
        let EditorTheme {
            border_color,
            select_list,
        } = theme;
        let padding_x = options.padding_x.unwrap_or(0);
        let autocomplete_max_visible = options.autocomplete_max_visible.unwrap_or(5).clamp(3, 20);
        let trigger_characters = DEFAULT_AUTOCOMPLETE_TRIGGER_CHARACTERS.to_vec();
        let trigger_pattern = build_trigger_pattern(&trigger_characters);
        let debounce_pattern = build_debounce_pattern(&trigger_characters);

        Self {
            state: EditorState {
                lines: vec![String::new()],
                cursor_line: 0,
                cursor_col: 0,
            },
            focused: false,
            tui,
            select_list_theme: select_list,
            padding_x,
            last_width: Cell::new(80),
            scroll_offset: Cell::new(0),
            border_color,
            autocomplete_provider: None,
            autocomplete_trigger_characters: trigger_characters,
            autocomplete_trigger_pattern: trigger_pattern,
            autocomplete_debounce_pattern: debounce_pattern,
            autocomplete_ui: RefCell::new(AutocompleteUi::default()),
            autocomplete_max_visible,
            autocomplete_generation: Cell::new(0),
            autocomplete_abort: RefCell::new(None),
            autocomplete_pending: Arc::new(Mutex::new(None)),
            pastes: BTreeMap::new(),
            paste_counter: 0,
            paste_buffer: String::new(),
            is_in_paste: false,
            history: Vec::new(),
            history_index: -1,
            history_draft: None,
            kill_ring: KillRing::new(),
            last_action: None,
            jump_mode: None,
            preferred_visual_col: None,
            snapped_from_cursor_col: None,
            undo_stack: UndoStack::new(),
            on_submit: None,
            on_change: None,
            disable_submit: false,
        }
    }

    /// Set of currently valid paste IDs (upstream `validPasteIds`,
    /// editor.ts:356-358).
    fn valid_paste_ids(&self) -> &BTreeMap<u32, String> {
        &self.pastes
    }

    /// Segment text with paste-marker awareness, only merging markers with
    /// valid IDs (upstream `segment`, editor.ts:361-363).
    fn grapheme_segments<'a>(&self, text: &'a str) -> Vec<EditorSegment<'a>> {
        grapheme_segments(text, self.valid_paste_ids())
    }

    pub fn get_padding_x(&self) -> usize {
        self.padding_x
    }

    pub fn set_padding_x(&mut self, padding: usize) {
        if self.padding_x != padding {
            self.padding_x = padding;
            self.tui.request_render(false);
        }
    }

    pub fn get_autocomplete_max_visible(&self) -> usize {
        self.autocomplete_max_visible
    }

    pub fn set_autocomplete_max_visible(&mut self, max_visible: usize) {
        let new_max_visible = max_visible.clamp(3, 20);
        if self.autocomplete_max_visible != new_max_visible {
            self.autocomplete_max_visible = new_max_visible;
            self.tui.request_render(false);
        }
    }

    pub fn set_autocomplete_provider(&mut self, provider: Arc<dyn AutocompleteProvider>) {
        self.cancel_autocomplete();
        self.autocomplete_provider = Some(provider);
        let trigger_characters: Vec<char> = self
            .autocomplete_provider
            .as_ref()
            .map(|provider| provider.trigger_characters().to_vec())
            .unwrap_or_default();
        self.set_autocomplete_trigger_characters(&trigger_characters);
    }

    /// Add a prompt to history for up/down arrow navigation. Called after
    /// successful submission (upstream `addToHistory`, editor.ts:399-409).
    pub fn add_to_history(&mut self, text: &str) {
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        // Don't add consecutive duplicates.
        if self.history.first().is_some_and(|first| *first == trimmed) {
            return;
        }
        self.history.insert(0, trimmed);
        // Limit history size.
        if self.history.len() > 100 {
            self.history.pop();
        }
    }

    fn is_editor_empty(&self) -> bool {
        self.state.lines.len() == 1 && self.state.lines[0].is_empty()
    }

    fn is_on_first_visual_line(&self) -> bool {
        let visual_lines = self.build_visual_line_map(self.last_width.get());
        self.find_current_visual_line(&visual_lines) == 0
    }

    fn is_on_last_visual_line(&self) -> bool {
        let visual_lines = self.build_visual_line_map(self.last_width.get());
        let current = self.find_current_visual_line(&visual_lines);
        current == visual_lines.len().saturating_sub(1)
    }

    fn navigate_history(&mut self, direction: i64) {
        self.last_action = None;
        if self.history.is_empty() {
            return;
        }

        // Up(-1) increases index, Down(1) decreases.
        let new_index = self.history_index - direction;
        if new_index < -1 || new_index >= self.history.len() as i64 {
            return;
        }

        // Capture state when first entering history browsing mode.
        if self.history_index == -1 && new_index >= 0 {
            self.push_undo_snapshot();
            self.history_draft = Some(self.state.clone());
        }

        self.history_index = new_index;

        if self.history_index == -1 {
            let draft = self.history_draft.take();
            if let Some(draft) = draft {
                self.state = draft;
                self.preferred_visual_col = None;
                self.snapped_from_cursor_col = None;
                self.scroll_offset.set(0);
                self.notify_change();
            } else {
                self.set_text_internal("", CursorPlacement::Start);
            }
        } else {
            let text = self
                .history
                .get(self.history_index as usize)
                .cloned()
                .unwrap_or_default();
            let placement = if direction == -1 {
                CursorPlacement::Start
            } else {
                CursorPlacement::End
            };
            self.set_text_internal(&text, placement);
        }
    }

    fn exit_history_browsing(&mut self) {
        self.history_index = -1;
        self.history_draft = None;
    }

    /// Internal setText that doesn't reset history state — used by
    /// `navigate_history` (upstream `setTextInternal`, editor.ts:465-476).
    fn set_text_internal(&mut self, text: &str, cursor_placement: CursorPlacement) {
        let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        self.state.lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        self.state.cursor_line = match cursor_placement {
            CursorPlacement::Start => 0,
            CursorPlacement::End => self.state.lines.len() - 1,
        };
        let line_length = self
            .state
            .lines
            .get(self.state.cursor_line)
            .map(|line| line.chars().count())
            .unwrap_or(0);
        let cursor_col = match cursor_placement {
            CursorPlacement::Start => 0,
            CursorPlacement::End => line_length,
        };
        self.set_cursor_col(cursor_col);
        // Reset scroll — render() will adjust to show the cursor.
        self.scroll_offset.set(0);

        self.notify_change();
    }

    pub fn get_text(&self) -> String {
        self.state.lines.join("\n")
    }

    /// Expand paste markers to their actual content (upstream
    /// `expandPasteMarkers`, editor.ts:985-992). Replacements are literal —
    /// a function replacement avoids `$`-expansion in paste contents.
    fn expand_paste_markers(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (paste_id, paste_content) in &self.pastes {
            let marker_regex = Regex::new(&format!(
                r"\[paste #{paste_id}( (\+[0-9]+ lines|[0-9]+ chars))?\]"
            ))
            .expect("per-id marker regex");
            // Literal replacement: escape `$` (the only special char in
            // regex-crate replacements) so paste contents like "$1" survive
            // verbatim, mirroring upstream's function replacement.
            result = marker_regex
                .replace_all(&result, paste_content.replace('$', "$$"))
                .into_owned();
        }
        result
    }

    /// Get text with paste markers expanded to their actual content. Use
    /// this when you need the full content (e.g. for an external editor)
    /// (upstream `getExpandedText`, editor.ts:998-1000).
    pub fn get_expanded_text(&self) -> String {
        self.expand_paste_markers(&self.state.lines.join("\n"))
    }

    /// Defensive copy of the current lines (upstream `getLines`,
    /// editor.ts:1002-1004).
    pub fn get_lines(&self) -> Vec<String> {
        self.state.lines.clone()
    }

    pub fn get_cursor(&self) -> (usize, usize) {
        (self.state.cursor_line, self.state.cursor_col)
    }

    pub fn set_text(&mut self, text: &str) {
        self.cancel_autocomplete();
        self.last_action = None;
        self.exit_history_browsing();
        let normalized = self.normalize_text(text);
        // Push undo snapshot if content differs (makes programmatic changes
        // undoable).
        if self.get_text() != normalized {
            self.push_undo_snapshot();
        }
        self.pastes.clear();
        self.paste_counter = 0;
        self.set_text_internal(&normalized, CursorPlacement::End);
    }

    /// Insert text at the current cursor position. Used for programmatic
    /// insertion (e.g. clipboard image markers). This is atomic for undo —
    /// a single undo restores the entire pre-insert state (upstream
    /// `insertTextAtCursor`, editor.ts:1029-1036).
    pub fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.cancel_autocomplete();
        self.push_undo_snapshot();
        self.last_action = None;
        self.exit_history_browsing();
        self.insert_text_at_cursor_internal(text);
    }

    /// Normalize text for editor storage: normalize line endings
    /// (`\r\n` and `\r` -> `\n`) and expand tabs to 4 spaces (upstream
    /// `normalizeText`, editor.ts:1043-1045).
    fn normalize_text(&self, text: &str) -> String {
        text.replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ")
    }

    /// Internal text insertion at cursor. Handles single and multi-line
    /// text. Does not push undo snapshots or trigger autocomplete — the
    /// caller is responsible. Normalizes line endings and calls `on_change`
    /// once at the end (upstream `insertTextAtCursorInternal`,
    /// editor.ts:1052-1093).
    fn insert_text_at_cursor_internal(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        // Normalize line endings and tabs.
        let normalized = self.normalize_text(text);
        let inserted_lines: Vec<&str> = normalized.split('\n').collect();

        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        let before_cursor =
            current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
        let after_cursor =
            current_line[char_to_byte(&current_line, self.state.cursor_col)..].to_string();

        if inserted_lines.len() == 1 {
            // Single line — insert at cursor position.
            self.state.lines[self.state.cursor_line] =
                format!("{before_cursor}{normalized}{after_cursor}");
            self.set_cursor_col(self.state.cursor_col + normalized.chars().count());
        } else {
            // Multi-line insertion.
            let mut new_lines: Vec<String> =
                Vec::with_capacity(self.state.lines.len() + inserted_lines.len() - 1);
            // All lines before the current line.
            new_lines.extend(self.state.lines[..self.state.cursor_line].iter().cloned());
            // The first inserted line merged with text before cursor.
            new_lines.push(format!("{before_cursor}{}", inserted_lines[0]));
            // All middle inserted lines.
            new_lines.extend(
                inserted_lines[1..inserted_lines.len() - 1]
                    .iter()
                    .map(|line| line.to_string()),
            );
            // The last inserted line with text after cursor.
            new_lines.push(format!(
                "{}{after_cursor}",
                inserted_lines[inserted_lines.len() - 1]
            ));
            // All lines after the current line.
            new_lines.extend(
                self.state.lines[self.state.cursor_line + 1..]
                    .iter()
                    .cloned(),
            );

            self.state.lines = new_lines;
            self.state.cursor_line += inserted_lines.len() - 1;
            self.set_cursor_col(inserted_lines[inserted_lines.len() - 1].chars().count());
        }

        self.notify_change();
    }

    /// Insert a single character at the cursor with undo coalescing
    /// (upstream `insertCharacter`, editor.ts:1096-1154). `skip_undo_coalescing`
    /// is set for atomic operations (e.g. `handle_paste`).
    fn insert_character(&mut self, char: &str, skip_undo_coalescing: bool) {
        self.exit_history_browsing();

        // Undo coalescing (fish-style):
        // - Consecutive word chars coalesce into one undo unit
        // - Space captures state before itself (so undo removes space +
        //   following word together)
        // - Each space is separately undoable
        // Skip coalescing when called from atomic operations (e.g. handlePaste).
        if !skip_undo_coalescing {
            if is_whitespace_char(char) || self.last_action != Some(LastAction::TypeWord) {
                self.push_undo_snapshot();
            }
            self.last_action = Some(LastAction::TypeWord);
        }

        let line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        let before = line[..char_to_byte(&line, self.state.cursor_col)].to_string();
        let after = line[char_to_byte(&line, self.state.cursor_col)..].to_string();

        self.state.lines[self.state.cursor_line] = format!("{before}{char}{after}");
        self.set_cursor_col(self.state.cursor_col + char.chars().count());

        self.notify_change();

        // Check if we should trigger or update autocomplete.
        if self.autocomplete_ui.borrow().state.is_none() {
            // Auto-trigger for "/" at the start of a message (slash commands).
            if char == "/" && self.is_at_start_of_message() {
                self.try_trigger_autocomplete(false);
            }
            // Auto-trigger for symbol-based completion like @, #, or
            // provider triggers at token boundaries.
            else if char
                .chars()
                .next()
                .is_some_and(|c| self.autocomplete_trigger_characters.contains(&c))
            {
                let current_line = self
                    .state
                    .lines
                    .get(self.state.cursor_line)
                    .cloned()
                    .unwrap_or_default();
                let text_before_cursor =
                    current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
                let char_count = text_before_cursor.chars().count();
                let char_before_symbol = if char_count >= 2 {
                    text_before_cursor.chars().nth(char_count - 2)
                } else {
                    None
                };
                if char_count == 1
                    || char_before_symbol == Some(' ')
                    || char_before_symbol == Some('\t')
                {
                    self.try_trigger_autocomplete(false);
                }
            }
            // Also auto-trigger when typing letters in a slash command or
            // symbol completion context.
            else if char
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
            {
                let current_line = self
                    .state
                    .lines
                    .get(self.state.cursor_line)
                    .cloned()
                    .unwrap_or_default();
                let text_before_cursor =
                    current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
                // Check if we're in a slash command (with or without space
                // for arguments) or a symbol-based completion context like
                // @, #, or provider triggers.
                if self.is_in_slash_command_context(&text_before_cursor)
                    || self
                        .autocomplete_trigger_pattern
                        .is_match(&text_before_cursor)
                {
                    self.try_trigger_autocomplete(false);
                }
            }
        } else {
            self.update_autocomplete();
        }
    }

    /// Handle a completed bracketed paste (upstream `handlePaste`,
    /// editor.ts:1156-1222).
    pub fn handle_paste(&mut self, pasted_text: &str) {
        self.cancel_autocomplete();
        self.exit_history_browsing();
        self.last_action = None;

        self.push_undo_snapshot();

        // Some terminals (e.g. tmux popups with extended-keys-format=csi-u)
        // re-encode control bytes inside bracketed paste as CSI-u Ctrl+<letter>
        // sequences (ESC [ <codepoint> ; 5 u). Decode those back to their
        // literal byte so the per-char filter below preserves newlines
        // instead of stripping ESC and leaking the printable tail (e.g.
        // "[106;5u") into the editor (editor.ts:1163-1173).
        let decoded_text = csi_u_paste_decode_regex()
            .replace_all(pasted_text, |caps: &Captures<'_>| {
                let cp: u32 = caps[1].parse().unwrap_or(0);
                if (97..=122).contains(&cp) {
                    char::from_u32(cp - 96)
                        .expect("1..=26 is a valid char")
                        .to_string()
                } else if (65..=90).contains(&cp) {
                    char::from_u32(cp - 64)
                        .expect("1..=26 is a valid char")
                        .to_string()
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        // Clean the pasted text: normalize line endings, expand tabs.
        let clean_text = self.normalize_text(&decoded_text);

        // Filter out non-printable characters except newlines.
        let mut filtered_text: String = clean_text
            .chars()
            .filter(|c| *c == '\n' || (*c as u32) >= 32)
            .collect();

        // If pasting a file path (starts with /, ~, or .) and the character
        // before the cursor is a word character, prepend a space for better
        // readability (editor.ts:1184-1192).
        if filtered_text.starts_with('/')
            || filtered_text.starts_with('~')
            || filtered_text.starts_with('.')
        {
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let char_before_cursor = self
                .state
                .cursor_col
                .checked_sub(1)
                .and_then(|index| current_line.chars().nth(index));
            if let Some(char_before_cursor) = char_before_cursor {
                // JS `\w`: ASCII word characters only.
                if char_before_cursor.is_ascii_alphanumeric() || char_before_cursor == '_' {
                    filtered_text = format!(" {filtered_text}");
                }
            }
        }

        // Split into lines to check for large paste.
        let pasted_lines: Vec<&str> = filtered_text.split('\n').collect();

        // Check if this is a large paste (> 10 lines or > 1000 characters).
        // The character count uses UTF-16 code units like upstream
        // `String.length`.
        let total_chars = filtered_text.encode_utf16().count();
        if pasted_lines.len() > 10 || total_chars > 1000 {
            // Store the paste and insert a marker.
            self.paste_counter += 1;
            let paste_id = self.paste_counter;
            self.pastes.insert(paste_id, filtered_text.clone());

            // Insert marker like "[paste #1 +123 lines]" or
            // "[paste #1 1234 chars]".
            let marker = if pasted_lines.len() > 10 {
                format!("[paste #{paste_id} +{} lines]", pasted_lines.len())
            } else {
                format!("[paste #{paste_id} {total_chars} chars]")
            };
            self.insert_text_at_cursor_internal(&marker);
            return;
        }

        // Single line or multi-line paste — insert atomically (do not
        // trigger autocomplete during paste).
        self.insert_text_at_cursor_internal(&filtered_text);
    }

    /// Insert a newline at the cursor, splitting the current line (upstream
    /// `addNewLine`, editor.ts:1224-1247).
    fn add_new_line(&mut self) {
        self.cancel_autocomplete();
        self.exit_history_browsing();
        self.last_action = None;

        self.push_undo_snapshot();

        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        let before = current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
        let after = current_line[char_to_byte(&current_line, self.state.cursor_col)..].to_string();

        // Split current line.
        self.state.lines[self.state.cursor_line] = before;
        self.state.lines.insert(self.state.cursor_line + 1, after);

        // Move cursor to start of new line.
        self.state.cursor_line += 1;
        self.set_cursor_col(0);

        self.notify_change();
    }

    /// `shouldSubmitOnBackslashEnter` (editor.ts:1249-1258): with a
    /// Shift+Enter binding available, Enter after a backslash deletes the
    /// backslash and submits instead of inserting a newline.
    fn should_submit_on_backslash_enter(
        &self,
        data: &str,
        kb: &crate::keybindings::KeybindingsManager,
    ) -> bool {
        if self.disable_submit {
            return false;
        }
        if !matches_key(data, "enter") {
            return false;
        }
        let submit_keys = kb.get_keys(Keybinding::InputSubmit);
        let has_shift_enter = submit_keys
            .iter()
            .any(|key| key == "shift+enter" || key == "shift+return");
        if !has_shift_enter {
            return false;
        }

        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        self.state.cursor_col > 0
            && current_line.chars().nth(self.state.cursor_col - 1) == Some('\\')
    }

    /// Submit the current content (upstream `submitValue`, editor.ts:1260-1274).
    fn submit_value(&mut self) {
        self.cancel_autocomplete();
        let result = self
            .expand_paste_markers(&self.state.lines.join("\n"))
            .trim()
            .to_string();

        self.state = EditorState {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
        };
        self.pastes.clear();
        self.paste_counter = 0;
        self.exit_history_browsing();
        self.scroll_offset.set(0);
        self.undo_stack.clear();
        self.last_action = None;

        if let Some(on_change) = self.on_change.as_mut() {
            on_change("");
        }
        if let Some(on_submit) = self.on_submit.as_mut() {
            on_submit(&result);
        }
    }

    /// Delete the grapheme before the cursor, or merge with the previous
    /// line at column 0. Deleting a paste marker also removes it from the
    /// registry and renumbers the remaining markers (upstream
    /// `handleBackspace`, editor.ts:1276-1359).
    fn handle_backspace(&mut self) {
        self.exit_history_browsing();
        self.last_action = None;

        if self.state.cursor_col > 0 {
            self.push_undo_snapshot();

            // Delete grapheme before cursor (handles emojis, combining
            // characters, etc.).
            let line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let before_cursor = line[..char_to_byte(&line, self.state.cursor_col)].to_string();

            // Find the last grapheme in the text before cursor.
            let graphemes = self.grapheme_segments(&before_cursor);
            let last_grapheme = graphemes.last().copied();
            let grapheme_length = last_grapheme
                .map(|grapheme| grapheme.segment.chars().count())
                .unwrap_or(1);
            let pasted_id = last_grapheme.and_then(|grapheme| paste_marker_id(grapheme.segment));

            if let Some(target_id) = pasted_id {
                // This contains the id part e.g. 4 from [paste #4 +123 lines].
                self.pastes.remove(&target_id);
                self.paste_counter -= 1;

                // Shift registry entries down in ascending id order,
                // independent of marker order in the text ([paste #3] becomes
                // [paste #2] when [paste #1] is removed).
                let higher_ids: Vec<u32> = self
                    .pastes
                    .keys()
                    .copied()
                    .filter(|id| *id > target_id)
                    .collect();
                for id in higher_ids {
                    let content = self.pastes.remove(&id).expect("id present in registry");
                    self.pastes.insert(id - 1, content);
                }

                // Renumber markers with ids greater than the removed one.
                for line in &mut self.state.lines {
                    *line = paste_marker_regex()
                        .replace_all(line, |caps: &Captures<'_>| {
                            let id: u32 = caps[1].parse().unwrap_or(0);
                            if id <= target_id {
                                caps[0].to_string()
                            } else {
                                let suffix = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                                format!("[paste #{}{suffix}]", id - 1)
                            }
                        })
                        .into_owned();
                }
            }

            let line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let before =
                line[..char_to_byte(&line, self.state.cursor_col - grapheme_length)].to_string();
            let after = line[char_to_byte(&line, self.state.cursor_col)..].to_string();

            self.state.lines[self.state.cursor_line] = format!("{before}{after}");
            self.set_cursor_col(self.state.cursor_col - grapheme_length);
        } else if self.state.cursor_line > 0 {
            self.push_undo_snapshot();

            // Merge with previous line.
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let previous_line = self
                .state
                .lines
                .get(self.state.cursor_line - 1)
                .cloned()
                .unwrap_or_default();

            self.state.lines[self.state.cursor_line - 1] = format!("{previous_line}{current_line}");
            self.state.lines.remove(self.state.cursor_line);

            self.state.cursor_line -= 1;
            self.set_cursor_col(previous_line.chars().count());
        }

        self.notify_change();

        // Update or re-trigger autocomplete after backspace.
        if self.autocomplete_ui.borrow().state.is_some() {
            self.update_autocomplete();
        } else {
            // If autocomplete was cancelled (no matches), re-trigger if
            // we're in a completable context.
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let text_before_cursor =
                current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
            // Slash command context or symbol-based completion context like
            // @, #, or provider triggers.
            if self.is_in_slash_command_context(&text_before_cursor)
                || self
                    .autocomplete_trigger_pattern
                    .is_match(&text_before_cursor)
            {
                self.try_trigger_autocomplete(false);
            }
        }
    }

    /// Set cursor column and clear `preferred_visual_col`. Use this for all
    /// non-vertical cursor movements to reset sticky column behavior
    /// (upstream `setCursorCol`, editor.ts:1365-1369).
    fn set_cursor_col(&mut self, col: usize) {
        self.state.cursor_col = col;
        self.preferred_visual_col = None;
        self.snapped_from_cursor_col = None;
    }

    /// Move cursor to a target visual line, applying sticky column logic.
    /// Shared by `move_cursor()` and `page_scroll()` (upstream
    /// `moveToVisualLine`, editor.ts:1375-1455).
    fn move_to_visual_line(
        &mut self,
        visual_lines: &[VisualLine],
        current_visual_line: usize,
        target_visual_line: usize,
    ) {
        let Some(current_vl) = visual_lines.get(current_visual_line).copied() else {
            return;
        };
        let Some(target_vl) = visual_lines.get(target_visual_line).copied() else {
            return;
        };

        // When the cursor was snapped to a segment start, resolve the
        // pre-snap position against the VL it belongs to. This gives the
        // correct visual column even after a resize reshuffles VLs.
        let current_visual_col = if let Some(snapped_from_cursor_col) = self.snapped_from_cursor_col
        {
            let vl_index = self.find_visual_line_at(
                visual_lines,
                current_vl.logical_line,
                snapped_from_cursor_col,
            );
            snapped_from_cursor_col - visual_lines[vl_index].start_col
        } else {
            self.state.cursor_col - current_vl.start_col
        };

        // For non-last segments, clamp to length-1 to stay within the segment.
        let is_last_source_segment = current_visual_line == visual_lines.len() - 1
            || visual_lines[current_visual_line + 1].logical_line != current_vl.logical_line;
        let source_max_visual_col = if is_last_source_segment {
            current_vl.length
        } else {
            current_vl.length.saturating_sub(1)
        };

        let is_last_target_segment = target_visual_line == visual_lines.len() - 1
            || visual_lines[target_visual_line + 1].logical_line != target_vl.logical_line;
        let target_max_visual_col = if is_last_target_segment {
            target_vl.length
        } else {
            target_vl.length.saturating_sub(1)
        };

        let move_to_visual_col = self.compute_vertical_move_column(
            current_visual_col,
            source_max_visual_col,
            target_max_visual_col,
        );

        // Set cursor position.
        self.state.cursor_line = target_vl.logical_line;
        let target_col = target_vl.start_col + move_to_visual_col;
        let logical_line = self
            .state
            .lines
            .get(target_vl.logical_line)
            .cloned()
            .unwrap_or_default();
        self.state.cursor_col = target_col.min(logical_line.chars().count());

        // Snap cursor to atomic segment boundary (e.g. paste markers) so the
        // cursor never lands in the middle of a multi-grapheme unit.
        // Single-grapheme segments don't need snapping.
        let segments = self.grapheme_segments(&logical_line);
        for seg in &segments {
            if seg.index > self.state.cursor_col {
                break;
            }
            if seg.segment.chars().count() <= 1 {
                continue;
            }
            let seg_end = seg.index + seg.segment.chars().count();
            if self.state.cursor_col < seg_end {
                let is_continuation = seg.index < target_vl.start_col;
                let is_moving_down = target_visual_line > current_visual_line;

                if is_continuation && is_moving_down {
                    // The segment started on a previous visual line, and we
                    // already visited it on the way down. Skip all remaining
                    // continuation VLs and land on the first VL past it.
                    let mut next = target_visual_line + 1;
                    while next < visual_lines.len()
                        && visual_lines[next].logical_line == target_vl.logical_line
                        && visual_lines[next].start_col < seg_end
                    {
                        next += 1;
                    }
                    if next < visual_lines.len() {
                        self.move_to_visual_line(visual_lines, current_visual_line, next);
                        return;
                    }
                }

                // Snap to the start of the segment so it gets highlighted.
                // Store the pre-snap position so the next vertical move can
                // resolve it to the correct visual column.
                self.snapped_from_cursor_col = Some(self.state.cursor_col);
                self.state.cursor_col = seg.index;
                return;
            }
        }

        // No snap occurred — we moved out of the atomic segment.
        self.snapped_from_cursor_col = None;
    }

    /// Compute the target visual column for vertical cursor movement.
    /// Implements the sticky column decision table (upstream
    /// `computeVerticalMoveColumn`, editor.ts:1477-1508):
    ///
    /// | P | S | T | U | Scenario                                             | Set Preferred | Move To     |
    /// |---|---|---|---| ---------------------------------------------------- |---------------|-------------|
    /// | 0 | * | 0 | - | Start nav, target fits                               | null          | current     |
    /// | 0 | * | 1 | - | Start nav, target shorter                            | current       | target end  |
    /// | 1 | 0 | 0 | 0 | Clamped, target fits preferred                       | null          | preferred   |
    /// | 1 | 0 | 0 | 1 | Clamped, target longer but still can't fit preferred | keep          | target end  |
    /// | 1 | 0 | 1 | - | Clamped, target even shorter                         | keep          | target end  |
    /// | 1 | 1 | 0 | - | Rewrapped, target fits current                       | null          | current     |
    /// | 1 | 1 | 1 | - | Rewrapped, target shorter than current               | current       | target end  |
    ///
    /// Where:
    /// - P = preferred col is set
    /// - S = cursor in middle of source line (not clamped to end)
    /// - T = target line shorter than current visual col
    /// - U = target line shorter than preferred col
    fn compute_vertical_move_column(
        &mut self,
        current_visual_col: usize,
        source_max_visual_col: usize,
        target_max_visual_col: usize,
    ) -> usize {
        let has_preferred = self.preferred_visual_col.is_some(); // P
        let cursor_in_middle = current_visual_col < source_max_visual_col; // S
        let target_too_short = target_max_visual_col < current_visual_col; // T

        if !has_preferred || cursor_in_middle {
            if target_too_short {
                // Cases 2 and 7.
                self.preferred_visual_col = Some(current_visual_col);
                return target_max_visual_col;
            }

            // Cases 1 and 6.
            self.preferred_visual_col = None;
            return current_visual_col;
        }

        let target_cant_fit_preferred = target_max_visual_col < self.preferred_visual_col.unwrap(); // U
        if target_too_short || target_cant_fit_preferred {
            // Cases 4 and 5.
            return target_max_visual_col;
        }

        // Case 3.
        let result = self.preferred_visual_col.unwrap();
        self.preferred_visual_col = None;
        result
    }

    fn move_to_line_start(&mut self) {
        self.last_action = None;
        self.set_cursor_col(0);
    }

    fn move_to_line_end(&mut self) {
        self.last_action = None;
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        self.set_cursor_col(current_line.chars().count());
    }

    /// Delete from the start of the line to the cursor (upstream
    /// `deleteToStartOfLine`, editor.ts:1521-1554).
    fn delete_to_start_of_line(&mut self) {
        self.exit_history_browsing();

        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        if self.state.cursor_col > 0 {
            self.push_undo_snapshot();

            // Calculate text to be deleted and save to kill ring (backward
            // deletion = prepend).
            let deleted_text =
                current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
            self.kill_ring.push(
                deleted_text,
                KillRingPushOptions {
                    prepend: true,
                    accumulate: self.last_action == Some(LastAction::Kill),
                },
            );
            self.last_action = Some(LastAction::Kill);

            // Delete from start of line up to cursor.
            self.state.lines[self.state.cursor_line] =
                current_line[char_to_byte(&current_line, self.state.cursor_col)..].to_string();
            self.set_cursor_col(0);
        } else if self.state.cursor_line > 0 {
            self.push_undo_snapshot();

            // At start of line — merge with previous line, treating newline
            // as deleted text.
            self.kill_ring.push(
                "\n".to_string(),
                KillRingPushOptions {
                    prepend: true,
                    accumulate: self.last_action == Some(LastAction::Kill),
                },
            );
            self.last_action = Some(LastAction::Kill);

            let previous_line = self
                .state
                .lines
                .get(self.state.cursor_line - 1)
                .cloned()
                .unwrap_or_default();
            self.state.lines[self.state.cursor_line - 1] = format!("{previous_line}{current_line}");
            self.state.lines.remove(self.state.cursor_line);
            self.state.cursor_line -= 1;
            self.set_cursor_col(previous_line.chars().count());
        }

        self.notify_change();
    }

    /// Delete from the cursor to the end of the line (upstream
    /// `deleteToEndOfLine`, editor.ts:1556-1586).
    fn delete_to_end_of_line(&mut self) {
        self.exit_history_browsing();

        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        if self.state.cursor_col < current_line.chars().count() {
            self.push_undo_snapshot();

            // Calculate text to be deleted and save to kill ring (forward
            // deletion = append).
            let deleted_text =
                current_line[char_to_byte(&current_line, self.state.cursor_col)..].to_string();
            self.kill_ring.push(
                deleted_text,
                KillRingPushOptions {
                    prepend: false,
                    accumulate: self.last_action == Some(LastAction::Kill),
                },
            );
            self.last_action = Some(LastAction::Kill);

            // Delete from cursor to end of line.
            self.state.lines[self.state.cursor_line] =
                current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
        } else if self.state.cursor_line < self.state.lines.len() - 1 {
            self.push_undo_snapshot();

            // At end of line — merge with next line, treating newline as
            // deleted text.
            self.kill_ring.push(
                "\n".to_string(),
                KillRingPushOptions {
                    prepend: false,
                    accumulate: self.last_action == Some(LastAction::Kill),
                },
            );
            self.last_action = Some(LastAction::Kill);

            let next_line = self
                .state
                .lines
                .get(self.state.cursor_line + 1)
                .cloned()
                .unwrap_or_default();
            self.state.lines[self.state.cursor_line] = format!("{current_line}{next_line}");
            self.state.lines.remove(self.state.cursor_line + 1);
        }

        self.notify_change();
    }

    /// Delete the word before the cursor (upstream `deleteWordBackwards`,
    /// editor.ts:1588-1631).
    fn delete_word_backwards(&mut self) {
        self.exit_history_browsing();

        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        // If at start of line, behave like backspace at column 0 (merge with
        // previous line).
        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.push_undo_snapshot();

                // Treat newline as deleted text (backward deletion = prepend).
                self.kill_ring.push(
                    "\n".to_string(),
                    KillRingPushOptions {
                        prepend: true,
                        accumulate: self.last_action == Some(LastAction::Kill),
                    },
                );
                self.last_action = Some(LastAction::Kill);

                let previous_line = self
                    .state
                    .lines
                    .get(self.state.cursor_line - 1)
                    .cloned()
                    .unwrap_or_default();
                self.state.lines[self.state.cursor_line - 1] =
                    format!("{previous_line}{current_line}");
                self.state.lines.remove(self.state.cursor_line);
                self.state.cursor_line -= 1;
                self.set_cursor_col(previous_line.chars().count());
            }
        } else {
            self.push_undo_snapshot();

            // Save lastAction before cursor movement (moveWordBackwards
            // resets it).
            let was_kill = self.last_action == Some(LastAction::Kill);

            let old_cursor_col = self.state.cursor_col;
            self.move_word_backwards();
            let delete_from = self.state.cursor_col;
            self.set_cursor_col(old_cursor_col);

            let deleted_text: String = current_line
                .chars()
                .skip(delete_from)
                .take(self.state.cursor_col - delete_from)
                .collect();
            self.kill_ring.push(
                deleted_text,
                KillRingPushOptions {
                    prepend: true,
                    accumulate: was_kill,
                },
            );
            self.last_action = Some(LastAction::Kill);

            self.state.lines[self.state.cursor_line] = format!(
                "{}{}",
                &current_line[..char_to_byte(&current_line, delete_from)],
                &current_line[char_to_byte(&current_line, self.state.cursor_col)..]
            );
            self.set_cursor_col(delete_from);
        }

        self.notify_change();
    }

    /// Delete the word after the cursor (upstream `deleteWordForward`,
    /// editor.ts:1633-1673).
    fn delete_word_forward(&mut self) {
        self.exit_history_browsing();

        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        // If at end of line, merge with next line (delete the newline).
        if self.state.cursor_col >= current_line.chars().count() {
            if self.state.cursor_line < self.state.lines.len() - 1 {
                self.push_undo_snapshot();

                // Treat newline as deleted text (forward deletion = append).
                self.kill_ring.push(
                    "\n".to_string(),
                    KillRingPushOptions {
                        prepend: false,
                        accumulate: self.last_action == Some(LastAction::Kill),
                    },
                );
                self.last_action = Some(LastAction::Kill);

                let next_line = self
                    .state
                    .lines
                    .get(self.state.cursor_line + 1)
                    .cloned()
                    .unwrap_or_default();
                self.state.lines[self.state.cursor_line] = format!("{current_line}{next_line}");
                self.state.lines.remove(self.state.cursor_line + 1);
            }
        } else {
            self.push_undo_snapshot();

            // Save lastAction before cursor movement (moveWordForwards
            // resets it).
            let was_kill = self.last_action == Some(LastAction::Kill);

            let old_cursor_col = self.state.cursor_col;
            self.move_word_forwards();
            let delete_to = self.state.cursor_col;
            self.set_cursor_col(old_cursor_col);

            let deleted_text: String = current_line
                .chars()
                .skip(self.state.cursor_col)
                .take(delete_to - self.state.cursor_col)
                .collect();
            self.kill_ring.push(
                deleted_text,
                KillRingPushOptions {
                    prepend: false,
                    accumulate: was_kill,
                },
            );
            self.last_action = Some(LastAction::Kill);

            self.state.lines[self.state.cursor_line] = format!(
                "{}{}",
                &current_line[..char_to_byte(&current_line, self.state.cursor_col)],
                &current_line[char_to_byte(&current_line, delete_to)..]
            );
        }

        self.notify_change();
    }

    /// Delete the grapheme at the cursor (upstream `handleForwardDelete`,
    /// editor.ts:1675-1723).
    fn handle_forward_delete(&mut self) {
        self.exit_history_browsing();
        self.last_action = None;

        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        if self.state.cursor_col < current_line.chars().count() {
            self.push_undo_snapshot();

            // Delete grapheme at cursor position (handles emojis, combining
            // characters, etc.).
            let after_cursor =
                current_line[char_to_byte(&current_line, self.state.cursor_col)..].to_string();

            // Find the first grapheme at cursor.
            let graphemes = self.grapheme_segments(&after_cursor);
            let first_grapheme = graphemes.first().copied();
            let grapheme_length = first_grapheme
                .map(|grapheme| grapheme.segment.chars().count())
                .unwrap_or(1);

            let before =
                current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
            let after = current_line
                [char_to_byte(&current_line, self.state.cursor_col + grapheme_length)..]
                .to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{after}");
        } else if self.state.cursor_line < self.state.lines.len() - 1 {
            self.push_undo_snapshot();

            // At end of line — merge with next line.
            let next_line = self
                .state
                .lines
                .get(self.state.cursor_line + 1)
                .cloned()
                .unwrap_or_default();
            self.state.lines[self.state.cursor_line] = format!("{current_line}{next_line}");
            self.state.lines.remove(self.state.cursor_line + 1);
        }

        self.notify_change();

        // Update or re-trigger autocomplete after forward delete.
        if self.autocomplete_ui.borrow().state.is_some() {
            self.update_autocomplete();
        } else {
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let text_before_cursor =
                current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
            // Slash command context or symbol-based completion context like
            // @, #, or provider triggers.
            if self.is_in_slash_command_context(&text_before_cursor)
                || self
                    .autocomplete_trigger_pattern
                    .is_match(&text_before_cursor)
            {
                self.try_trigger_autocomplete(false);
            }
        }
    }

    /// Build a mapping from visual lines to logical positions (upstream
    /// `buildVisualLineMap`, editor.ts:1732-1757).
    fn build_visual_line_map(&self, width: usize) -> Vec<VisualLine> {
        let mut visual_lines = Vec::new();

        for (i, line) in self.state.lines.iter().enumerate() {
            let line_vis_width = visible_width(line);
            if line.is_empty() {
                // Empty line still takes one visual line.
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: 0,
                    length: 0,
                });
            } else if line_vis_width <= width {
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: 0,
                    length: line.chars().count(),
                });
            } else {
                // Line needs wrapping — use word-aware wrapping.
                let segments = self.grapheme_segments(line);
                let chunks = word_wrap_line(line, width, Some(&segments));
                for chunk in &chunks {
                    visual_lines.push(VisualLine {
                        logical_line: i,
                        start_col: chunk.start_index,
                        length: chunk.end_index - chunk.start_index,
                    });
                }
            }
        }

        visual_lines
    }

    /// Find the visual line index that contains the given logical position
    /// (upstream `findVisualLineAt`, editor.ts:1762-1779).
    fn find_visual_line_at(&self, visual_lines: &[VisualLine], line: usize, col: usize) -> usize {
        for (i, vl) in visual_lines.iter().enumerate() {
            if vl.logical_line != line {
                continue;
            }
            let offset = col as i64 - vl.start_col as i64;
            // Cursor is in this segment if it's within range. For the last
            // segment of a logical line, cursor can be at length (end
            // position).
            let is_last_segment_of_line =
                i == visual_lines.len() - 1 || visual_lines[i + 1].logical_line != vl.logical_line;
            if offset >= 0
                && (offset < vl.length as i64
                    || (is_last_segment_of_line && offset == vl.length as i64))
            {
                return i;
            }
        }
        visual_lines.len().saturating_sub(1)
    }

    /// Find the visual line index for the current cursor position (upstream
    /// `findCurrentVisualLine`, editor.ts:1784-1788).
    fn find_current_visual_line(&self, visual_lines: &[VisualLine]) -> usize {
        self.find_visual_line_at(visual_lines, self.state.cursor_line, self.state.cursor_col)
    }

    /// Move the cursor by visual lines and/or graphemes (upstream
    /// `moveCursor`, editor.ts:1790-1851).
    fn move_cursor(&mut self, delta_line: i64, delta_col: i64) {
        self.last_action = None;
        let visual_lines = self.build_visual_line_map(self.last_width.get());
        let current_visual_line = self.find_current_visual_line(&visual_lines);

        if delta_line != 0 {
            let target_visual_line = current_visual_line as i64 + delta_line;

            if target_visual_line >= 0 && (target_visual_line as usize) < visual_lines.len() {
                self.move_to_visual_line(
                    &visual_lines,
                    current_visual_line,
                    target_visual_line as usize,
                );
            }
        }

        if delta_col != 0 {
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();

            if delta_col > 0 {
                // Moving right — move by one grapheme (handles emojis,
                // combining characters, etc.).
                if self.state.cursor_col < current_line.chars().count() {
                    let after_cursor = current_line
                        [char_to_byte(&current_line, self.state.cursor_col)..]
                        .to_string();
                    let graphemes = self.grapheme_segments(&after_cursor);
                    let first_grapheme = graphemes.first().copied();
                    self.set_cursor_col(
                        self.state.cursor_col
                            + first_grapheme
                                .map(|g| g.segment.chars().count())
                                .unwrap_or(1),
                    );
                } else if self.state.cursor_line < self.state.lines.len() - 1 {
                    // Wrap to start of next logical line.
                    self.state.cursor_line += 1;
                    self.set_cursor_col(0);
                } else {
                    // At end of last line — can't move, but set
                    // preferredVisualCol for up/down navigation.
                    if let Some(current_vl) = visual_lines.get(current_visual_line).copied() {
                        self.preferred_visual_col =
                            Some(self.state.cursor_col - current_vl.start_col);
                    }
                }
            } else {
                // Moving left — move by one grapheme (handles emojis,
                // combining characters, etc.).
                if self.state.cursor_col > 0 {
                    let before_cursor = current_line
                        [..char_to_byte(&current_line, self.state.cursor_col)]
                        .to_string();
                    let graphemes = self.grapheme_segments(&before_cursor);
                    let last_grapheme = graphemes.last().copied();
                    self.set_cursor_col(
                        self.state.cursor_col
                            - last_grapheme
                                .map(|g| g.segment.chars().count())
                                .unwrap_or(1),
                    );
                } else if self.state.cursor_line > 0 {
                    // Wrap to end of previous logical line.
                    self.state.cursor_line -= 1;
                    let prev_line = self
                        .state
                        .lines
                        .get(self.state.cursor_line)
                        .cloned()
                        .unwrap_or_default();
                    self.set_cursor_col(prev_line.chars().count());
                }
            }
        }

        // Keep an open autocomplete picker in sync with the new cursor
        // position: cursor movement changes the text before the cursor, so a
        // picker computed for the old position is stale. Re-query so it
        // refreshes — or closes when the new position yields no suggestions
        // — mirroring insertCharacter()/handleBackspace() (editor.ts:1848-1850).
        if self.autocomplete_ui.borrow().state.is_some() {
            self.update_autocomplete();
        }
    }

    /// Scroll by a page (direction: -1 for up, 1 for down). Moves the cursor
    /// by the page size while keeping it in bounds (upstream `pageScroll`,
    /// editor.ts:1857-1867).
    fn page_scroll(&mut self, direction: i64) {
        self.last_action = None;
        let terminal_rows = self.tui.terminal_rows() as usize;
        let page_size = std::cmp::max(5, (terminal_rows as f64 * 0.3).floor() as usize);

        let visual_lines = self.build_visual_line_map(self.last_width.get());
        let current_visual_line = self.find_current_visual_line(&visual_lines);
        let target_visual_line = (current_visual_line as i64 + direction * page_size as i64)
            .clamp(0, visual_lines.len().saturating_sub(1) as i64)
            as usize;

        self.move_to_visual_line(&visual_lines, current_visual_line, target_visual_line);
    }

    /// Move the cursor one word backward (upstream `moveWordBackwards`,
    /// editor.ts:1869-1889).
    fn move_word_backwards(&mut self) {
        self.last_action = None;
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        // If at start of line, move to end of previous line.
        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.state.cursor_line -= 1;
                let prev_line = self
                    .state
                    .lines
                    .get(self.state.cursor_line)
                    .cloned()
                    .unwrap_or_default();
                self.set_cursor_col(prev_line.chars().count());
            }
            return;
        }

        let valid_ids = self.valid_paste_ids();
        let segment_fn: &crate::word_navigation::WordSegmentFn<'_> =
            &|text: &str| word_segments_with_markers(text, valid_ids);
        let options = WordNavigationOptions {
            segment: Some(segment_fn),
            is_atomic_segment: Some(&is_paste_marker),
        };
        self.set_cursor_col(find_word_backward(
            &current_line,
            self.state.cursor_col,
            Some(&options),
        ));
    }

    /// Yank (paste) the most recent kill ring entry at the cursor position
    /// (upstream `yank`, editor.ts:1894-1903).
    fn yank(&mut self) {
        if self.kill_ring.is_empty() {
            return;
        }

        self.push_undo_snapshot();

        let text = self.kill_ring.peek().expect("checked above").to_string();
        self.insert_yanked_text(&text);

        self.last_action = Some(LastAction::Yank);
    }

    /// Cycle through the kill ring (only works immediately after yank or
    /// yank-pop). Replaces the last yanked text with the previous entry in
    /// the ring (upstream `yankPop`, editor.ts:1909-1926).
    fn yank_pop(&mut self) {
        // Only works if we just yanked and have more than one entry.
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }

        self.push_undo_snapshot();

        // Delete the previously yanked text (still at end of ring before
        // rotation).
        self.delete_yanked_text();

        // Rotate the ring: move end to front.
        self.kill_ring.rotate();

        // Insert the new most recent entry (now at end after rotation).
        let text = self
            .kill_ring
            .peek()
            .expect("ring is non-empty")
            .to_string();
        self.insert_yanked_text(&text);

        self.last_action = Some(LastAction::Yank);
    }

    /// Insert text at cursor position (used by yank operations) (upstream
    /// `insertYankedText`, editor.ts:1931-1968).
    fn insert_yanked_text(&mut self, text: &str) {
        self.exit_history_browsing();
        let lines: Vec<&str> = text.split('\n').collect();

        if lines.len() == 1 {
            // Single line — insert at cursor.
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let before =
                current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
            let after =
                current_line[char_to_byte(&current_line, self.state.cursor_col)..].to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{text}{after}");
            self.set_cursor_col(self.state.cursor_col + text.chars().count());
        } else {
            // Multi-line insert.
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let before =
                current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
            let after =
                current_line[char_to_byte(&current_line, self.state.cursor_col)..].to_string();

            // First line merges with text before cursor.
            self.state.lines[self.state.cursor_line] = format!("{before}{}", lines[0]);

            // Insert middle lines.
            for (offset, line) in lines[1..lines.len() - 1].iter().enumerate() {
                self.state
                    .lines
                    .insert(self.state.cursor_line + 1 + offset, line.to_string());
            }

            // Last line merges with text after cursor.
            let last_line_index = self.state.cursor_line + lines.len() - 1;
            self.state.lines.insert(
                last_line_index,
                format!("{}{after}", lines[lines.len() - 1]),
            );

            // Update cursor position.
            self.state.cursor_line = last_line_index;
            self.set_cursor_col(lines[lines.len() - 1].chars().count());
        }

        self.notify_change();
    }

    /// Delete the previously yanked text (used by yank-pop). The yanked text
    /// is derived from killRing[end] since it hasn't been rotated yet
    /// (upstream `deleteYankedText`, editor.ts:1974-2010).
    fn delete_yanked_text(&mut self) {
        let Some(yanked_text) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };
        let yank_lines: Vec<&str> = yanked_text.split('\n').collect();

        if yank_lines.len() == 1 {
            // Single line — delete backward from cursor.
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let delete_len = yanked_text.chars().count();
            let before = current_line
                [..char_to_byte(&current_line, self.state.cursor_col - delete_len)]
                .to_string();
            let after =
                current_line[char_to_byte(&current_line, self.state.cursor_col)..].to_string();
            self.state.lines[self.state.cursor_line] = format!("{before}{after}");
            self.set_cursor_col(self.state.cursor_col - delete_len);
        } else {
            // Multi-line delete — cursor is at end of last yanked line.
            let start_line = self.state.cursor_line - (yank_lines.len() - 1);
            let start_col = self
                .state
                .lines
                .get(start_line)
                .map(|line| line.chars().count())
                .unwrap_or(0)
                - yank_lines[0].chars().count();

            // Get text after cursor on current line.
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            let after_cursor =
                current_line[char_to_byte(&current_line, self.state.cursor_col)..].to_string();

            // Get text before yank start position.
            let before_yank_line = self
                .state
                .lines
                .get(start_line)
                .cloned()
                .unwrap_or_default();
            let before_yank =
                before_yank_line[..char_to_byte(&before_yank_line, start_col)].to_string();

            // Remove all lines from startLine to cursorLine and replace with
            // merged line.
            self.state.lines.splice(
                start_line..start_line + yank_lines.len(),
                [format!("{before_yank}{after_cursor}")],
            );

            // Update cursor.
            self.state.cursor_line = start_line;
            self.set_cursor_col(start_col);
        }

        self.notify_change();
    }

    fn push_undo_snapshot(&mut self) {
        self.undo_stack.push(EditorSnapshot {
            state: self.state.clone(),
            pastes: self.pastes.clone(),
            paste_counter: self.paste_counter,
        });
    }

    /// Fire `on_change` with the current text (upstream inline
    /// `this.onChange?.(this.getText())`; factored out so the text is
    /// computed before the callback borrows the field mutably).
    fn notify_change(&mut self) {
        if self.on_change.is_some() {
            let text = self.get_text();
            if let Some(on_change) = self.on_change.as_mut() {
                on_change(&text);
            }
        }
    }

    fn undo(&mut self) {
        self.exit_history_browsing();
        let Some(snapshot) = self.undo_stack.pop() else {
            return;
        };
        self.state = snapshot.state;
        self.pastes = snapshot.pastes;
        self.paste_counter = snapshot.paste_counter;
        self.last_action = None;
        self.preferred_visual_col = None;
        self.notify_change();
    }

    /// Jump to the first occurrence of a character in the specified
    /// direction. Multi-line search. Case-sensitive. Skips the current
    /// cursor position (upstream `jumpToChar`, editor.ts:2034-2062).
    fn jump_to_char(&mut self, char: &str, direction: JumpDirection) {
        self.last_action = None;
        let is_forward = direction == JumpDirection::Forward;
        let end: i64 = if is_forward {
            self.state.lines.len() as i64
        } else {
            -1
        };
        let step: i64 = if is_forward { 1 } else { -1 };

        let mut line_idx = self.state.cursor_line as i64;
        while line_idx != end {
            let line = &self.state.lines[line_idx as usize];
            let is_current_line = line_idx as usize == self.state.cursor_line;

            // Current line: start after/before cursor; other lines: search
            // full line.
            let search_from = if is_current_line {
                if is_forward {
                    Some(self.state.cursor_col + 1)
                } else {
                    self.state.cursor_col.checked_sub(1)
                }
            } else {
                None
            };

            let idx = if is_forward {
                find_str_from(line, char, search_from)
            } else {
                rfind_str_from(line, char, search_from)
            };

            if let Some(idx) = idx {
                self.state.cursor_line = line_idx as usize;
                self.set_cursor_col(idx);
                return;
            }

            line_idx += step;
        }
        // No match found — cursor stays in place.
    }

    /// Move the cursor one word forward (upstream `moveWordForwards`,
    /// editor.ts:2064-2083).
    fn move_word_forwards(&mut self) {
        self.last_action = None;
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();

        // If at end of line, move to start of next line.
        if self.state.cursor_col >= current_line.chars().count() {
            if self.state.cursor_line < self.state.lines.len() - 1 {
                self.state.cursor_line += 1;
                self.set_cursor_col(0);
            }
            return;
        }

        let valid_ids = self.valid_paste_ids();
        let segment_fn: &crate::word_navigation::WordSegmentFn<'_> =
            &|text: &str| word_segments_with_markers(text, valid_ids);
        let options = WordNavigationOptions {
            segment: Some(segment_fn),
            is_atomic_segment: Some(&is_paste_marker),
        };
        self.set_cursor_col(find_word_forward(
            &current_line,
            self.state.cursor_col,
            Some(&options),
        ));
    }

    // Slash menu only allowed on the first line of the editor (editor.ts:2086-2088).
    fn is_slash_menu_allowed(&self) -> bool {
        self.state.cursor_line == 0
    }

    // Helper method to check if cursor is at start of message (for slash
    // command detection) (editor.ts:2091-2096).
    fn is_at_start_of_message(&self) -> bool {
        if !self.is_slash_menu_allowed() {
            return false;
        }
        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        let before_cursor =
            current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
        before_cursor.trim() == "" || before_cursor.trim() == "/"
    }

    /// `isInSlashCommandContext` (editor.ts:2098-2100).
    fn is_in_slash_command_context(&self, text_before_cursor: &str) -> bool {
        self.is_slash_menu_allowed() && text_before_cursor.trim_start().starts_with('/')
    }

    // Autocomplete methods

    /// Find the best autocomplete item index for the given prefix. Returns
    /// `-1` when no match is found (upstream `getBestAutocompleteMatchIndex`,
    /// editor.ts:2114-2130).
    ///
    /// Match priority:
    /// 1. Exact match (prefix === item.value) — always selected
    /// 2. Prefix match — first item whose value starts with prefix
    /// 3. No match — -1 (keep default highlight)
    ///
    /// Matching is case-sensitive and checks item.value only.
    fn get_best_autocomplete_match_index(&self, items: &[SelectItem], prefix: &str) -> i64 {
        if prefix.is_empty() {
            return -1;
        }

        let mut first_prefix_index = -1i64;

        for (i, item) in items.iter().enumerate() {
            if item.value == prefix {
                return i as i64; // Exact match always wins.
            }
            if first_prefix_index == -1 && item.value.starts_with(prefix) {
                first_prefix_index = i as i64;
            }
        }

        first_prefix_index
    }

    /// `createAutocompleteList` (editor.ts:2132-2138).
    fn create_autocomplete_list(&self, prefix: &str, items: &[SelectItem]) -> SelectList {
        let layout = if prefix.starts_with('/') {
            Some(SLASH_COMMAND_SELECT_LIST_LAYOUT)
        } else {
            None
        };
        SelectList::new(
            items.to_vec(),
            self.autocomplete_max_visible,
            Arc::clone(&self.select_list_theme),
            layout,
        )
    }

    /// `tryTriggerAutocomplete` (editor.ts:2140-2142).
    fn try_trigger_autocomplete(&mut self, explicit_tab: bool) {
        self.request_autocomplete(RequestOptions {
            force: false,
            explicit_tab,
        });
    }

    /// `handleTabCompletion` (editor.ts:2144-2155).
    fn handle_tab_completion(&mut self) {
        if self.autocomplete_provider.is_none() {
            return;
        }

        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        let before_cursor =
            current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();

        if self.is_in_slash_command_context(&before_cursor)
            && !before_cursor.trim_start().contains(' ')
        {
            self.handle_slash_command_completion();
        } else {
            self.force_file_autocomplete(true);
        }
    }

    /// `handleSlashCommandCompletion` (editor.ts:2157-2159).
    fn handle_slash_command_completion(&mut self) {
        self.request_autocomplete(RequestOptions {
            force: false,
            explicit_tab: true,
        });
    }

    /// `forceFileAutocomplete` (editor.ts:2161-2163).
    fn force_file_autocomplete(&mut self, explicit_tab: bool) {
        self.request_autocomplete(RequestOptions {
            force: true,
            explicit_tab,
        });
    }

    /// `requestAutocomplete` (editor.ts:2165-2194): starts a debounced
    /// request on a worker thread, or runs it synchronously when the
    /// debounce is zero.
    fn request_autocomplete(&mut self, options: RequestOptions) {
        let Some(provider) = self.autocomplete_provider.clone() else {
            return;
        };

        if options.force {
            let should_trigger = provider.should_trigger_file_completion(
                &self.state.lines,
                self.state.cursor_line,
                self.state.cursor_col,
            );
            if !should_trigger {
                return;
            }
        }

        self.cancel_autocomplete_request();
        self.autocomplete_generation
            .set(self.autocomplete_generation.get() + 1);
        let generation = self.autocomplete_generation.get();
        let abort = Arc::new(AtomicBool::new(false));
        *self.autocomplete_abort.borrow_mut() = Some(Arc::clone(&abort));

        let debounce_ms = self.get_autocomplete_debounce_ms(options);

        if debounce_ms > 0 {
            let lines = self.state.lines.clone();
            let cursor_line = self.state.cursor_line;
            let cursor_col = self.state.cursor_col;
            let pending = Arc::clone(&self.autocomplete_pending);
            let render_handle = self.tui.render_handle();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(debounce_ms));
                if abort.load(Ordering::Relaxed) {
                    return;
                }
                let suggestions = provider.get_suggestions(
                    &lines,
                    cursor_line,
                    cursor_col,
                    &GetSuggestionsOptions {
                        abort: Arc::clone(&abort),
                        force: options.force,
                    },
                );
                if abort.load(Ordering::Relaxed) {
                    return;
                }
                let snapshot_text = lines.join("\n");
                if let Ok(mut pending) = pending.lock() {
                    *pending = Some(PendingAutocomplete {
                        generation,
                        aborted: abort,
                        snapshot_text,
                        snapshot_line: cursor_line,
                        snapshot_col: cursor_col,
                        suggestions,
                    });
                }
                render_handle.request_render();
            });
            return;
        }

        // Zero debounce (Tab / force / slash-command typing): run the
        // request synchronously — the state cannot change mid-query, so the
        // staleness check trivially passes.
        let suggestions = provider.get_suggestions(
            &self.state.lines,
            self.state.cursor_line,
            self.state.cursor_col,
            &GetSuggestionsOptions {
                abort: Arc::clone(&abort),
                force: options.force,
            },
        );

        if !suggestions
            .as_ref()
            .is_some_and(|suggestions| !suggestions.items.is_empty())
        {
            self.cancel_autocomplete();
            self.tui.request_render(false);
            return;
        }
        let suggestions = suggestions.expect("checked above");

        if options.force && options.explicit_tab && suggestions.items.len() == 1 {
            let item = suggestions.items[0].clone();
            self.push_undo_snapshot();
            self.last_action = None;
            let result = provider.apply_completion(
                &self.state.lines,
                self.state.cursor_line,
                self.state.cursor_col,
                &item,
                &suggestions.prefix,
            );
            self.state.lines = result.lines;
            self.state.cursor_line = result.cursor_line;
            self.set_cursor_col(result.cursor_col);
            self.notify_change();
            self.tui.request_render(false);
            return;
        }

        self.apply_autocomplete_suggestions(
            suggestions,
            if options.force {
                AutocompleteState::Force
            } else {
                AutocompleteState::Regular
            },
        );
        self.tui.request_render(false);
    }

    /// `setAutocompleteTriggerCharacters` (editor.ts:2219-2230).
    fn set_autocomplete_trigger_characters(&mut self, trigger_characters: &[char]) {
        let mut next: Vec<char> = DEFAULT_AUTOCOMPLETE_TRIGGER_CHARACTERS.to_vec();
        for &character in trigger_characters {
            // Skip non-single-code-unit characters (upstream
            // `character.length !== 1`, which rejects astral chars) and
            // duplicate/whitespace/`/` triggers.
            if character.len_utf16() != 1
                || character == '/'
                || is_whitespace_char(&character.to_string())
                || next.contains(&character)
            {
                continue;
            }
            next.push(character);
        }
        self.autocomplete_trigger_characters = next.clone();
        self.autocomplete_trigger_pattern = build_trigger_pattern(&next);
        self.autocomplete_debounce_pattern = build_debounce_pattern(&next);
    }

    /// `getAutocompleteDebounceMs` (editor.ts:2232-2240).
    fn get_autocomplete_debounce_ms(&self, options: RequestOptions) -> u64 {
        if options.explicit_tab || options.force {
            return 0;
        }

        let current_line = self
            .state
            .lines
            .get(self.state.cursor_line)
            .cloned()
            .unwrap_or_default();
        let text_before_cursor =
            current_line[..char_to_byte(&current_line, self.state.cursor_col)].to_string();
        if self
            .autocomplete_debounce_pattern
            .is_match(&text_before_cursor)
        {
            ATTACHMENT_AUTOCOMPLETE_DEBOUNCE_MS
        } else {
            0
        }
    }

    /// Apply pending worker-thread results (the upstream
    /// `runAutocompleteRequest` continuation). Runs from both `render`
    /// (`&self` via the [`RefCell`] UI group) and `handle_input`. Stale
    /// results (superseded generation, aborted, or text/cursor moved on) are
    /// discarded.
    fn drain_autocomplete(&self) {
        let pending = match self.autocomplete_pending.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        let Some(pending) = pending else {
            return;
        };

        // Upstream `isAutocompleteRequestCurrent` (editor.ts:2294-2308):
        // request id + abort flag + text snapshot + cursor snapshot.
        if pending.generation != self.autocomplete_generation.get()
            || pending.aborted.load(Ordering::Relaxed)
        {
            return;
        }
        if pending.snapshot_text != self.get_text()
            || pending.snapshot_line != self.state.cursor_line
            || pending.snapshot_col != self.state.cursor_col
        {
            return;
        }

        let suggestions = pending.suggestions;
        let empty = suggestions
            .as_ref()
            .is_none_or(|suggestions| suggestions.items.is_empty());
        if empty {
            self.cancel_autocomplete();
            self.tui.request_render(false);
            return;
        }
        let suggestions = suggestions.expect("checked above");

        self.apply_autocomplete_suggestions(suggestions, AutocompleteState::Regular);
        self.tui.request_render(false);
    }

    /// `applyAutocompleteSuggestions` (editor.ts:2310-2320).
    fn apply_autocomplete_suggestions(
        &self,
        suggestions: AutocompleteSuggestions,
        state: AutocompleteState,
    ) {
        let items: Vec<SelectItem> = suggestions
            .items
            .into_iter()
            .map(|item| SelectItem {
                value: item.value,
                label: item.label,
                description: item.description,
            })
            .collect();
        let mut ui = self.autocomplete_ui.borrow_mut();
        ui.prefix = suggestions.prefix.clone();
        ui.list = Some(self.create_autocomplete_list(&suggestions.prefix, &items));

        let best_match_index = self.get_best_autocomplete_match_index(&items, &suggestions.prefix);
        if best_match_index >= 0 {
            if let Some(list) = ui.list.as_mut() {
                list.set_selected_index(best_match_index as usize);
            }
        }

        ui.state = Some(state);
    }

    /// `cancelAutocompleteRequest` (editor.ts:2322-2330): invalidate the
    /// in-flight request (generation bump + abort flag) and clear the
    /// pending result slot. Takes `&self` — it also runs from
    /// [`Editor::drain_autocomplete`] (via `render`).
    fn cancel_autocomplete_request(&self) {
        self.autocomplete_generation
            .set(self.autocomplete_generation.get() + 1);
        if let Some(abort) = self.autocomplete_abort.borrow_mut().take() {
            abort.store(true, Ordering::Relaxed);
        }
        if let Ok(mut pending) = self.autocomplete_pending.lock() {
            *pending = None;
        }
    }

    /// `clearAutocompleteUi` (editor.ts:2332-2336).
    fn clear_autocomplete_ui(&self) {
        let mut ui = self.autocomplete_ui.borrow_mut();
        ui.state = None;
        ui.list = None;
        ui.prefix.clear();
    }

    /// `cancelAutocomplete` (editor.ts:2338-2341).
    fn cancel_autocomplete(&self) {
        self.cancel_autocomplete_request();
        self.clear_autocomplete_ui();
    }

    /// `isShowingAutocomplete` (editor.ts:2343-2345).
    pub fn is_showing_autocomplete(&self) -> bool {
        self.autocomplete_ui.borrow().state.is_some()
    }

    /// `updateAutocomplete` (editor.ts:2347-2350).
    fn update_autocomplete(&mut self) {
        if self.autocomplete_ui.borrow().state.is_none() || self.autocomplete_provider.is_none() {
            return;
        }
        let force = self.autocomplete_ui.borrow().state == Some(AutocompleteState::Force);
        self.request_autocomplete(RequestOptions {
            force,
            explicit_tab: false,
        });
    }
}

/// Find `needle` in `text` starting at the `from`-th character (JS
/// `String.indexOf(needle, from)` with UTF-16 positions; `None` = from 0).
fn find_str_from(text: &str, needle: &str, from: Option<usize>) -> Option<usize> {
    let from = from.unwrap_or(0);
    let mut pos = from;
    while pos <= text.chars().count() {
        let byte = char_to_byte(text, pos);
        if text[byte..].starts_with(needle) {
            return Some(pos);
        }
        pos += 1;
    }
    None
}

/// Find the last `needle` in `text` starting at or before the `from`-th
/// character (JS `String.lastIndexOf(needle, from)`; `None` = from the end).
fn rfind_str_from(text: &str, needle: &str, from: Option<usize>) -> Option<usize> {
    let mut pos = from.unwrap_or(text.chars().count());
    loop {
        let byte = char_to_byte(text, pos);
        if text[byte..].starts_with(needle) {
            return Some(pos);
        }
        if pos == 0 {
            return None;
        }
        pos -= 1;
    }
}

/// CSI-u Ctrl+letter sequences inside bracketed paste (editor.ts:1168).
fn csi_u_paste_decode_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[([0-9]+);5u").expect("static CSI-u regex"))
}

/// Layout the text into visual lines (upstream `layoutText`,
/// editor.ts:893-979).
impl Editor {
    fn layout_text(&self, content_width: usize) -> Vec<LayoutLine> {
        let mut layout_lines: Vec<LayoutLine> = Vec::new();

        if self.state.lines.is_empty()
            || (self.state.lines.len() == 1 && self.state.lines[0].is_empty())
        {
            // Empty editor.
            layout_lines.push(LayoutLine {
                text: String::new(),
                has_cursor: true,
                cursor_pos: Some(0),
            });
            return layout_lines;
        }

        // Process each logical line.
        for (i, line) in self.state.lines.iter().enumerate() {
            let is_current_line = i == self.state.cursor_line;
            let line_visible_width = visible_width(line);

            if line_visible_width <= content_width {
                // Line fits in one layout line.
                if is_current_line {
                    layout_lines.push(LayoutLine {
                        text: line.clone(),
                        has_cursor: true,
                        cursor_pos: Some(self.state.cursor_col),
                    });
                } else {
                    layout_lines.push(LayoutLine {
                        text: line.clone(),
                        has_cursor: false,
                        cursor_pos: None,
                    });
                }
            } else {
                // Line needs wrapping — use word-aware wrapping.
                let segments = self.grapheme_segments(line);
                let chunks = word_wrap_line(line, content_width, Some(&segments));

                for (chunk_index, chunk) in chunks.iter().enumerate() {
                    let cursor_pos = self.state.cursor_col;
                    let is_last_chunk = chunk_index == chunks.len() - 1;

                    // Determine if cursor is in this chunk.
                    // For word-wrapped chunks, we need to handle the case
                    // where the cursor might be in trimmed whitespace at end
                    // of chunk.
                    let mut has_cursor_in_chunk = false;
                    let mut adjusted_cursor_pos = 0usize;

                    if is_current_line {
                        if is_last_chunk {
                            // Last chunk: cursor belongs here if >= startIndex.
                            has_cursor_in_chunk = cursor_pos >= chunk.start_index;
                            // Upstream computes `cursorPos - chunk.startIndex`
                            // even when the cursor is before the chunk (JS
                            // yields a negative float that is never used);
                            // saturate instead of overflowing.
                            adjusted_cursor_pos = cursor_pos.saturating_sub(chunk.start_index);
                        } else {
                            // Non-last chunk: cursor belongs here if in range
                            // [startIndex, endIndex). But we need to handle
                            // the visual position in the trimmed text.
                            has_cursor_in_chunk =
                                cursor_pos >= chunk.start_index && cursor_pos < chunk.end_index;
                            if has_cursor_in_chunk {
                                adjusted_cursor_pos = cursor_pos - chunk.start_index;
                                // Clamp to text length (in case cursor was in
                                // trimmed whitespace).
                                if adjusted_cursor_pos > chunk.text.chars().count() {
                                    adjusted_cursor_pos = chunk.text.chars().count();
                                }
                            }
                        }
                    }

                    if has_cursor_in_chunk {
                        layout_lines.push(LayoutLine {
                            text: chunk.text.clone(),
                            has_cursor: true,
                            cursor_pos: Some(adjusted_cursor_pos),
                        });
                    } else {
                        layout_lines.push(LayoutLine {
                            text: chunk.text.clone(),
                            has_cursor: false,
                            cursor_pos: None,
                        });
                    }
                }
            }
        }

        layout_lines
    }
}

impl Component for Editor {
    fn invalidate(&mut self) {
        // No cached state to invalidate currently.
    }

    fn render(&self, width: usize) -> Vec<String> {
        // Apply any pending debounced autocomplete result before rendering.
        self.drain_autocomplete();

        let max_padding = width.saturating_sub(1) / 2;
        let padding_x = self.padding_x.min(max_padding);
        let content_width = (width - padding_x * 2).max(1);

        // Layout width: with padding the cursor can overflow into it,
        // without padding we reserve 1 column for the cursor.
        let layout_width = (content_width - if padding_x > 0 { 0 } else { 1 }).max(1);

        // Store for cursor navigation (must match wrapping width).
        self.last_width.set(layout_width);

        let horizontal = (self.border_color)("─");

        // Layout the text.
        let layout_lines = self.layout_text(layout_width);

        // Calculate max visible lines: 30% of terminal height, minimum 5
        // lines.
        let terminal_rows = self.tui.terminal_rows() as usize;
        let max_visible_lines = std::cmp::max(5, (terminal_rows as f64 * 0.3).floor() as usize);

        // Find the cursor line index in layoutLines.
        let cursor_line_index = layout_lines
            .iter()
            .position(|line| line.has_cursor)
            .unwrap_or(0);

        // Adjust scroll offset to keep cursor visible.
        let mut scroll_offset = self.scroll_offset.get();
        if cursor_line_index < scroll_offset {
            scroll_offset = cursor_line_index;
        } else if cursor_line_index >= scroll_offset + max_visible_lines {
            scroll_offset = cursor_line_index - max_visible_lines + 1;
        }

        // Clamp scroll offset to valid range.
        let max_scroll_offset = layout_lines.len().saturating_sub(max_visible_lines);
        scroll_offset = scroll_offset.min(max_scroll_offset);
        self.scroll_offset.set(scroll_offset);

        // Get visible lines slice.
        let visible_lines = &layout_lines
            [scroll_offset..(scroll_offset + max_visible_lines).min(layout_lines.len())];

        let mut result: Vec<String> = Vec::new();
        let left_padding = " ".repeat(padding_x);
        let right_padding = left_padding.clone();

        // Render top border (with scroll indicator if scrolled down).
        if scroll_offset > 0 {
            let border = create_scroll_border("↑", scroll_offset, width);
            result.push((self.border_color)(&border));
        } else {
            result.push(horizontal.repeat(width));
        }

        // Render each visible layout line.
        // Emit hardware cursor marker when focused so TUI can position the
        // hardware cursor for IME candidate-window placement even while
        // autocomplete (e.g. slash-command menu) is visible.
        let emit_cursor_marker = self.focused;

        for layout_line in visible_lines {
            let mut display_text = layout_line.text.clone();
            let mut line_visible_width = visible_width(&layout_line.text);
            let mut cursor_in_padding = false;

            // Add cursor if this line has it.
            if layout_line.has_cursor {
                if let Some(cursor_pos) = layout_line.cursor_pos {
                    let before =
                        display_text[..char_to_byte(&display_text, cursor_pos)].to_string();
                    let after = display_text[char_to_byte(&display_text, cursor_pos)..].to_string();

                    // Hardware cursor marker (zero-width, emitted before fake
                    // cursor for IME positioning).
                    let marker = if emit_cursor_marker {
                        CURSOR_MARKER
                    } else {
                        ""
                    };

                    if !after.is_empty() {
                        // Cursor is on a character (grapheme) — replace it
                        // with highlighted version.
                        let after_graphemes = self.grapheme_segments(&after);
                        let first_grapheme = after_graphemes
                            .first()
                            .map(|grapheme| grapheme.segment)
                            .unwrap_or("");
                        let rest_after = after[first_grapheme.len()..].to_string();
                        let cursor = format!("\x1b[7m{first_grapheme}\x1b[0m");
                        display_text = format!("{before}{marker}{cursor}{rest_after}");
                        // lineVisibleWidth stays the same — we're replacing,
                        // not adding.
                    } else {
                        // Cursor is at the end — add highlighted space.
                        let cursor = "\x1b[7m \x1b[0m";
                        display_text = format!("{before}{marker}{cursor}");
                        line_visible_width += 1;
                        // If cursor overflows content width into the padding,
                        // flag it.
                        if line_visible_width > content_width && padding_x > 0 {
                            cursor_in_padding = true;
                        }
                    }
                }
            }

            // Calculate padding based on actual visible width.
            let padding = " ".repeat(content_width.saturating_sub(line_visible_width));
            let line_right_padding = if cursor_in_padding {
                &right_padding[1..]
            } else {
                &right_padding
            };

            // Render the line (no side borders, just horizontal lines above
            // and below).
            result.push(format!(
                "{left_padding}{display_text}{padding}{line_right_padding}"
            ));
        }

        // Render bottom border (with scroll indicator if more content below).
        let lines_below = layout_lines.len() - (scroll_offset + visible_lines.len());
        if lines_below > 0 {
            let border = create_scroll_border("↓", lines_below, width);
            result.push((self.border_color)(&border));
        } else {
            result.push(horizontal.repeat(width));
        }

        // Add autocomplete list if active.
        if self.autocomplete_ui.borrow().state.is_some() {
            let autocomplete_result = {
                let ui = self.autocomplete_ui.borrow();
                ui.list
                    .as_ref()
                    .map(|list| list.render(content_width))
                    .unwrap_or_default()
            };
            for line in autocomplete_result {
                let line_width = visible_width(&line);
                let line_padding = " ".repeat(content_width.saturating_sub(line_width));
                result.push(format!("{left_padding}{line}{line_padding}{right_padding}"));
            }
        }

        result
    }

    fn handle_input(&mut self, data: &str) {
        // Apply any pending debounced autocomplete result before processing
        // the key (upstream applies results between input events).
        self.drain_autocomplete();

        let kb = get_keybindings();
        let kb = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());

        // Handle bracketed paste mode.
        let mut data = std::borrow::Cow::Borrowed(data);
        if data.contains("\x1b[200~") {
            self.is_in_paste = true;
            self.paste_buffer.clear();
            data = std::borrow::Cow::Owned(data.replacen("\x1b[200~", "", 1));
        }

        if self.is_in_paste {
            self.paste_buffer.push_str(&data);
            let end_index = self.paste_buffer.find("\x1b[201~");
            if let Some(end_index) = end_index {
                let paste_content = self.paste_buffer[..end_index].to_string();
                if !paste_content.is_empty() {
                    self.handle_paste(&paste_content);
                }
                self.is_in_paste = false;
                let remaining = self.paste_buffer[end_index + 6..].to_string();
                self.paste_buffer.clear();
                if !remaining.is_empty() {
                    self.handle_input(&remaining);
                }
                return;
            }
            return;
        }

        // Handle character jump mode (awaiting next character to jump to).
        if self.jump_mode.is_some() {
            // Cancel if the hotkey is pressed again.
            if kb.matches(&data, Keybinding::EditorJumpForward)
                || kb.matches(&data, Keybinding::EditorJumpBackward)
            {
                self.jump_mode = None;
                return;
            }

            let printable: Option<String> = match decode_printable_key(&data) {
                Some(character) => Some(character.to_string()),
                None => data
                    .chars()
                    .next()
                    .filter(|character| (*character as u32) >= 32)
                    .map(|_| data.to_string()),
            };
            if let Some(printable) = printable {
                // Printable character — perform the jump.
                let direction = self.jump_mode.take().expect("checked above");
                self.jump_to_char(&printable, direction);
                return;
            }

            // Control character — cancel and fall through to normal handling.
            self.jump_mode = None;
        }

        // Ctrl+C — let parent handle (exit/clear).
        if kb.matches(&data, Keybinding::InputCopy) {
            return;
        }

        // Undo.
        if kb.matches(&data, Keybinding::EditorUndo) {
            self.undo();
            return;
        }

        // Handle autocomplete mode.
        if self.autocomplete_ui.borrow().state.is_some()
            && self.autocomplete_ui.borrow().list.is_some()
        {
            if kb.matches(&data, Keybinding::SelectCancel) {
                self.cancel_autocomplete();
                return;
            }

            if kb.matches(&data, Keybinding::SelectUp) || kb.matches(&data, Keybinding::SelectDown)
            {
                let data_owned = data.to_string();
                let mut ui = self.autocomplete_ui.borrow_mut();
                if let Some(list) = ui.list.as_mut() {
                    list.handle_input(&data_owned);
                }
                return;
            }

            if kb.matches(&data, Keybinding::InputTab) {
                let selected = self
                    .autocomplete_ui
                    .borrow()
                    .list
                    .as_ref()
                    .and_then(|list| list.get_selected_item())
                    .cloned();
                if let (Some(selected), Some(provider)) =
                    (selected, self.autocomplete_provider.clone())
                {
                    let item = AutocompleteItem {
                        value: selected.value,
                        label: selected.label,
                        description: selected.description,
                    };
                    self.push_undo_snapshot();
                    self.last_action = None;
                    let prefix = self.autocomplete_ui.borrow().prefix.clone();
                    let result = provider.apply_completion(
                        &self.state.lines,
                        self.state.cursor_line,
                        self.state.cursor_col,
                        &item,
                        &prefix,
                    );
                    self.state.lines = result.lines;
                    self.state.cursor_line = result.cursor_line;
                    self.set_cursor_col(result.cursor_col);
                    self.cancel_autocomplete();
                    self.notify_change();
                }
                return;
            }

            if kb.matches(&data, Keybinding::SelectConfirm) {
                let selected = self
                    .autocomplete_ui
                    .borrow()
                    .list
                    .as_ref()
                    .and_then(|list| list.get_selected_item())
                    .cloned();
                if let (Some(selected), Some(provider)) =
                    (selected, self.autocomplete_provider.clone())
                {
                    let item = AutocompleteItem {
                        value: selected.value,
                        label: selected.label,
                        description: selected.description,
                    };
                    self.push_undo_snapshot();
                    self.last_action = None;
                    let prefix = self.autocomplete_ui.borrow().prefix.clone();
                    let result = provider.apply_completion(
                        &self.state.lines,
                        self.state.cursor_line,
                        self.state.cursor_col,
                        &item,
                        &prefix,
                    );
                    self.state.lines = result.lines;
                    self.state.cursor_line = result.cursor_line;
                    self.set_cursor_col(result.cursor_col);

                    if prefix.starts_with('/') {
                        self.cancel_autocomplete();
                        // Fall through to submit.
                    } else {
                        self.cancel_autocomplete();
                        self.notify_change();
                        return;
                    }
                }
            }
        }

        // Tab — trigger completion.
        if kb.matches(&data, Keybinding::InputTab) && self.autocomplete_ui.borrow().state.is_none()
        {
            self.handle_tab_completion();
            return;
        }

        // Deletion actions.
        if kb.matches(&data, Keybinding::EditorDeleteToLineEnd) {
            self.delete_to_end_of_line();
            return;
        }
        if kb.matches(&data, Keybinding::EditorDeleteToLineStart) {
            self.delete_to_start_of_line();
            return;
        }
        if kb.matches(&data, Keybinding::EditorDeleteWordBackward) {
            self.delete_word_backwards();
            return;
        }
        if kb.matches(&data, Keybinding::EditorDeleteWordForward) {
            self.delete_word_forward();
            return;
        }
        if kb.matches(&data, Keybinding::EditorDeleteCharBackward)
            || matches_key(&data, "shift+backspace")
        {
            self.handle_backspace();
            return;
        }
        if kb.matches(&data, Keybinding::EditorDeleteCharForward)
            || matches_key(&data, "shift+delete")
        {
            self.handle_forward_delete();
            return;
        }

        // Kill ring actions.
        if kb.matches(&data, Keybinding::EditorYank) {
            self.yank();
            return;
        }
        if kb.matches(&data, Keybinding::EditorYankPop) {
            self.yank_pop();
            return;
        }

        // Dedicated history actions always browse entries instead of moving
        // the cursor (editor.ts:767-778 @ 4181f66, 16ad96ae8).
        if kb.matches(&data, Keybinding::EditorHistoryPrevious) {
            self.cancel_autocomplete();
            self.navigate_history(-1);
            return;
        }
        if kb.matches(&data, Keybinding::EditorHistoryNext) {
            self.cancel_autocomplete();
            self.navigate_history(1);
            return;
        }

        // Cursor movement actions.
        if kb.matches(&data, Keybinding::EditorCursorLineStart) {
            self.move_to_line_start();
            return;
        }
        if kb.matches(&data, Keybinding::EditorCursorLineEnd) {
            self.move_to_line_end();
            return;
        }
        if kb.matches(&data, Keybinding::EditorCursorWordLeft) {
            self.move_word_backwards();
            return;
        }
        if kb.matches(&data, Keybinding::EditorCursorWordRight) {
            self.move_word_forwards();
            return;
        }

        // New line.
        if kb.matches(&data, Keybinding::InputNewLine)
            || (data.starts_with('\n') && data.chars().count() > 1)
            || data == "\x1b\r"
            || data == "\x1b[13;2~"
            || (data.chars().count() > 1 && data.contains('\x1b') && data.contains('\r'))
            || (data == "\n" && data.chars().count() == 1)
        {
            if self.should_submit_on_backslash_enter(&data, &kb) {
                self.handle_backspace();
                self.submit_value();
                return;
            }
            self.add_new_line();
            return;
        }

        // Submit (Enter).
        if kb.matches(&data, Keybinding::InputSubmit) {
            if self.disable_submit {
                return;
            }

            // Workaround for terminals without Shift+Enter support:
            // If char before cursor is \, delete it and insert newline
            // instead of submitting.
            let current_line = self
                .state
                .lines
                .get(self.state.cursor_line)
                .cloned()
                .unwrap_or_default();
            if self.state.cursor_col > 0
                && current_line.chars().nth(self.state.cursor_col - 1) == Some('\\')
            {
                self.handle_backspace();
                self.add_new_line();
                return;
            }

            self.submit_value();
            return;
        }

        // Arrow key navigation (with history support).
        if kb.matches(&data, Keybinding::EditorCursorUp) {
            if self.is_on_first_visual_line()
                && (self.is_editor_empty() || self.history_index > -1 || self.state.cursor_col == 0)
            {
                self.navigate_history(-1);
            } else if self.is_on_first_visual_line() {
                // Already at top — jump to start of line.
                self.move_to_line_start();
            } else {
                self.move_cursor(-1, 0);
            }
            return;
        }
        if kb.matches(&data, Keybinding::EditorCursorDown) {
            if self.history_index > -1 && self.is_on_last_visual_line() {
                self.navigate_history(1);
            } else if self.is_on_last_visual_line() {
                // Already at bottom — jump to end of line.
                self.move_to_line_end();
            } else {
                self.move_cursor(1, 0);
            }
            return;
        }
        if kb.matches(&data, Keybinding::EditorCursorRight) {
            self.move_cursor(0, 1);
            return;
        }
        if kb.matches(&data, Keybinding::EditorCursorLeft) {
            self.move_cursor(0, -1);
            return;
        }

        // Page up/down — scroll by page and move cursor.
        if kb.matches(&data, Keybinding::EditorPageUp) {
            self.page_scroll(-1);
            return;
        }
        if kb.matches(&data, Keybinding::EditorPageDown) {
            self.page_scroll(1);
            return;
        }

        // Character jump mode triggers.
        if kb.matches(&data, Keybinding::EditorJumpForward) {
            self.jump_mode = Some(JumpDirection::Forward);
            return;
        }
        if kb.matches(&data, Keybinding::EditorJumpBackward) {
            self.jump_mode = Some(JumpDirection::Backward);
            return;
        }

        // Shift+Space — insert regular space.
        if matches_key(&data, "shift+space") {
            self.insert_character(" ", false);
            return;
        }

        let printable = decode_printable_key(&data);
        if let Some(printable) = printable {
            self.insert_character(&printable.to_string(), false);
            return;
        }

        // Regular characters.
        if data.chars().next().is_some_and(|c| (c as u32) >= 32) {
            self.insert_character(&data, false);
        }
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for Editor {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

impl crate::components::editor_component::EditorComponent for Editor {
    fn get_text(&self) -> String {
        Editor::get_text(self)
    }

    fn set_text(&mut self, text: &str) {
        Editor::set_text(self, text);
    }

    fn handle_input(&mut self, data: &str) {
        Component::handle_input(self, data);
    }

    fn on_submit(&mut self) -> Option<&mut SubmitFn> {
        self.on_submit.as_mut()
    }

    fn on_change(&mut self) -> Option<&mut ChangeFn> {
        self.on_change.as_mut()
    }

    fn add_to_history(&mut self, text: &str) {
        Editor::add_to_history(self, text);
    }

    fn insert_text_at_cursor(&mut self, text: &str) {
        Editor::insert_text_at_cursor(self, text);
    }

    fn get_expanded_text(&self) -> String {
        Editor::get_expanded_text(self)
    }

    fn set_autocomplete_provider(&mut self, provider: Arc<dyn AutocompleteProvider>) {
        Editor::set_autocomplete_provider(self, provider);
    }

    fn border_color(&self) -> Option<&dyn Fn(&str) -> String> {
        Some(&*self.border_color)
    }

    fn set_padding_x(&mut self, padding: usize) {
        Editor::set_padding_x(self, padding);
    }

    fn set_autocomplete_max_visible(&mut self, max_visible: usize) {
        Editor::set_autocomplete_max_visible(self, max_visible);
    }
}

#[cfg(test)]
mod tests {
    //! Ports of `test/editor.test.ts` @ pi 0.82.1 (2efa728): all describe
    //! groups. Key sequences go through `handle_input` exactly like the
    //! upstream tests; matching goes through the global default keybindings.
    //! The debounced autocomplete path (worker thread) is flushed with
    //! [`flush_autocomplete`], the Rust counterpart of the upstream
    //! `flushAutocomplete()` + `setTimeout` waits.

    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use rpi_test_support::vt::strip_ansi;

    use super::*;
    use crate::autocomplete::{CompletionResult, SlashCommand, SlashCommandOrItem};
    use crate::terminal::Terminal;
    use crate::tui_main_screen::TuiMainScreen;

    /// Virtual terminal for tests (upstream test `VirtualTerminal`,
    /// virtual-terminal.ts): fixed size, Kitty protocol always active.
    struct TestTerminal {
        columns: u16,
        rows: u16,
    }

    impl TestTerminal {
        fn new(columns: u16, rows: u16) -> Self {
            Self { columns, rows }
        }
    }

    impl Terminal for TestTerminal {
        fn start(
            &mut self,
            _on_input: crate::terminal::InputHandler,
            _on_resize: crate::terminal::ResizeHandler,
        ) {
        }
        fn stop(&mut self) {}
        fn drain_input(
            &mut self,
            _max_ms: Option<u64>,
            _idle_ms: Option<u64>,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }
        fn write(&mut self, _data: &str) {}
        fn columns(&self) -> u16 {
            self.columns
        }
        fn rows(&self) -> u16 {
            self.rows
        }
        fn kitty_protocol_active(&self) -> bool {
            true
        }
        fn move_by(&mut self, _lines: i32) {}
        fn hide_cursor(&mut self) {}
        fn show_cursor(&mut self) {}
        fn clear_line(&mut self) {}
        fn clear_from_cursor(&mut self) {}
        fn clear_screen(&mut self) {}
        fn set_title(&mut self, _title: &str) {}
        fn set_progress(&mut self, _active: bool) {}
    }

    /// `createTestTUI` (editor.test.ts:11-14).
    fn test_tui(columns: usize, rows: usize) -> TuiMainScreen {
        TuiMainScreen::new(Box::new(TestTerminal::new(columns as u16, rows as u16)))
    }

    /// `defaultEditorTheme` with identity styling (test-themes.ts:35-38,
    /// simplified so render output carries no color codes).
    fn test_theme() -> EditorTheme {
        EditorTheme {
            border_color: Box::new(|text: &str| text.to_string()),
            select_list: Arc::new(SelectListTheme::identity()),
        }
    }

    fn editor() -> Editor {
        Editor::new(test_tui(80, 24), test_theme(), EditorOptions::default())
    }

    fn editor_at(columns: usize, rows: usize) -> Editor {
        Editor::new(
            test_tui(columns, rows),
            test_theme(),
            EditorOptions::default(),
        )
    }

    fn type_text(editor: &mut Editor, text: &str) {
        for ch in text.chars() {
            editor.handle_input(&ch.to_string());
        }
    }

    /// Standard applyCompletion used by the mock providers (editor.test.ts:16-34).
    fn mock_apply_completion(
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        let line = lines.get(cursor_line).cloned().unwrap_or_default();
        let before = line[..char_to_byte(&line, cursor_col - prefix.chars().count())].to_string();
        let after = line[char_to_byte(&line, cursor_col)..].to_string();
        let mut new_lines = lines.to_vec();
        new_lines[cursor_line] = format!("{before}{}{after}", item.value);
        CompletionResult {
            lines: new_lines,
            cursor_line,
            cursor_col: cursor_col - prefix.chars().count() + item.value.chars().count(),
        }
    }

    /// Mock provider driving `get_suggestions` from a closure (upstream
    /// inline `AutocompleteProvider` object literals).
    struct MockProvider {
        trigger: Vec<char>,
        #[allow(clippy::type_complexity)] // mirrors the trait method signature
        get_suggestions: Box<
            dyn Fn(&[String], usize, usize, bool) -> Option<AutocompleteSuggestions> + Send + Sync,
        >,
    }

    impl MockProvider {
        fn new(
            get_suggestions: impl Fn(&[String], usize, usize, bool) -> Option<AutocompleteSuggestions>
                + Send
                + Sync
                + 'static,
        ) -> Self {
            Self {
                trigger: Vec::new(),
                get_suggestions: Box::new(get_suggestions),
            }
        }

        fn with_trigger(mut self, trigger: &[char]) -> Self {
            self.trigger = trigger.to_vec();
            self
        }
    }

    impl AutocompleteProvider for MockProvider {
        fn trigger_characters(&self) -> &[char] {
            &self.trigger
        }

        fn get_suggestions(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            options: &GetSuggestionsOptions,
        ) -> Option<AutocompleteSuggestions> {
            (self.get_suggestions)(lines, cursor_line, cursor_col, options.force)
        }

        fn apply_completion(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> CompletionResult {
            mock_apply_completion(lines, cursor_line, cursor_col, item, prefix)
        }
    }

    /// `flushAutocomplete` (editor.test.ts:36-39): wait for the debounced
    /// worker thread and drain its result through a render.
    fn flush_autocomplete(editor: &mut Editor) {
        std::thread::sleep(Duration::from_millis(60));
        editor.render(80);
    }

    /// `positionCursor` (editor.test.ts:3072-3080).
    fn position_cursor(editor: &mut Editor, line: usize, col: usize) {
        // Go to line 0 first.
        for _ in 0..20 {
            editor.handle_input("\x1b[A");
        }
        // Go to target line.
        for _ in 0..line {
            editor.handle_input("\x1b[B");
        }
        // Go to target col.
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..col {
            editor.handle_input("\x1b[C");
        }
    }

    /// `pasteWithMarker` (editor.test.ts:3574-3579): simulate a large paste
    /// that creates a marker.
    fn paste_with_marker(editor: &mut Editor) -> String {
        let big_content = "line\n".repeat(20);
        let big_content = big_content.trim_end().to_string();
        editor.handle_input(&format!("\x1b[200~{big_content}\x1b[201~"));
        editor.get_text()
    }

    /// `bigPaste` (editor.test.ts:3582-3584): 12-line paste content with a
    /// distinguishing tag.
    fn big_paste(tag: &str) -> String {
        (0..12)
            .map(|i| format!("{tag}{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_cursor(editor: &Editor, line: usize, col: usize) {
        assert_eq!(
            editor.get_cursor(),
            (line, col),
            "cursor mismatch: {}",
            editor.get_text()
        );
    }

    // --- Prompt history navigation (editor.test.ts:42-285) ---------------

    #[test]
    fn does_nothing_on_up_arrow_when_history_is_empty() {
        let mut editor = editor();

        editor.handle_input("\x1b[A"); // Up arrow

        assert_eq!(editor.get_text(), "");
    }

    #[test]
    fn shows_most_recent_history_entry_on_up_arrow_when_editor_is_empty() {
        let mut editor = editor();

        editor.add_to_history("first prompt");
        editor.add_to_history("second prompt");

        editor.handle_input("\x1b[A"); // Up arrow

        assert_eq!(editor.get_text(), "second prompt");
    }

    #[test]
    fn cycles_through_history_entries_on_repeated_up_arrow() {
        let mut editor = editor();

        editor.add_to_history("first");
        editor.add_to_history("second");
        editor.add_to_history("third");

        editor.handle_input("\x1b[A"); // Up - shows "third"
        assert_eq!(editor.get_text(), "third");

        editor.handle_input("\x1b[A"); // Up - shows "second"
        assert_eq!(editor.get_text(), "second");

        editor.handle_input("\x1b[A"); // Up - shows "first"
        assert_eq!(editor.get_text(), "first");

        editor.handle_input("\x1b[A"); // Up - stays at "first" (oldest)
        assert_eq!(editor.get_text(), "first");
    }

    #[test]
    fn jumps_to_start_before_entering_history_from_a_non_empty_draft() {
        let mut editor = editor();

        editor.add_to_history("prompt");
        editor.set_text("draft");
        editor.handle_input("\x1b[D");
        editor.handle_input("\x1b[D");

        editor.handle_input("\x1b[A"); // Up - jumps to start before history browsing
        assert_eq!(editor.get_text(), "draft");
        assert_cursor(&editor, 0, 0);

        editor.handle_input("\x1b[A"); // Up at start - shows "prompt"
        assert_eq!(editor.get_text(), "prompt");

        editor.handle_input("\x1b[B"); // Down - restores draft
        assert_eq!(editor.get_text(), "draft");
        assert_cursor(&editor, 0, 0);
    }

    #[test]
    fn navigates_forward_through_history_with_down_arrow() {
        let mut editor = editor();

        editor.add_to_history("first");
        editor.add_to_history("second");
        editor.add_to_history("third");
        editor.set_text("draft");

        // Go to oldest.
        editor.handle_input("\x1b[A"); // start of draft
        editor.handle_input("\x1b[A"); // third
        editor.handle_input("\x1b[A"); // second
        editor.handle_input("\x1b[A"); // first

        // Navigate back.
        editor.handle_input("\x1b[B"); // second
        assert_eq!(editor.get_text(), "second");

        editor.handle_input("\x1b[B"); // third
        assert_eq!(editor.get_text(), "third");

        editor.handle_input("\x1b[B"); // draft
        assert_eq!(editor.get_text(), "draft");
    }

    // --- Editor prompt history keybindings
    //     (editor-history-keybindings.test.ts @ 4181f66, 16ad96ae8) ----------

    /// Serializes tests that replace the process-global keybinding registry
    /// (cargo runs tests on parallel threads).
    static KEYBINDINGS_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn browses_history_directly_without_first_moving_the_cursor() {
        use crate::keybindings::{
            set_keybindings, tui_keybindings, KeyBindingValue, KeybindingsConfig,
            KeybindingsManager,
        };

        let _guard = KEYBINDINGS_LOCK.lock().unwrap();
        let mut user_bindings = KeybindingsConfig::new();
        user_bindings.insert(
            "tui.editor.historyPrevious".to_string(),
            KeyBindingValue::Single("ctrl+p".to_string()),
        );
        user_bindings.insert(
            "tui.editor.historyNext".to_string(),
            KeyBindingValue::Single("ctrl+n".to_string()),
        );
        set_keybindings(KeybindingsManager::new(
            tui_keybindings().to_vec(),
            user_bindings,
        ));

        let mut editor = editor();
        editor.add_to_history("older prompt");
        editor.add_to_history("newer\nmultiline prompt");
        editor.set_text("draft");
        editor.handle_input("\x1b[D");
        editor.handle_input("\x1b[D");

        editor.handle_input("\x10"); // Ctrl+P
        assert_eq!(editor.get_text(), "newer\nmultiline prompt");
        assert_cursor(&editor, 0, 0);

        editor.handle_input("\x10"); // Ctrl+P
        assert_eq!(editor.get_text(), "older prompt");

        editor.handle_input("\x0e"); // Ctrl+N
        assert_eq!(editor.get_text(), "newer\nmultiline prompt");
        assert_cursor(&editor, 1, 16);

        editor.handle_input("\x0e"); // Ctrl+N
        assert_eq!(editor.get_text(), "draft");
        assert_cursor(&editor, 0, 3);

        // Restore the default table (upstream `afterEach`).
        set_keybindings(KeybindingsManager::with_defaults());
    }

    #[test]
    fn exits_history_mode_when_typing_a_character() {
        let mut editor = editor();

        editor.add_to_history("old prompt");

        editor.handle_input("\x1b[A"); // Up - shows "old prompt"
        editor.handle_input("x"); // Type a character - exits history mode

        assert_eq!(editor.get_text(), "xold prompt");
    }

    #[test]
    fn exits_history_mode_on_set_text() {
        let mut editor = editor();

        editor.add_to_history("first");
        editor.add_to_history("second");

        editor.handle_input("\x1b[A"); // Up - shows "second"
        editor.set_text(""); // External clear

        // Up should start fresh from most recent.
        editor.handle_input("\x1b[A");
        assert_eq!(editor.get_text(), "second");
    }

    #[test]
    fn does_not_add_empty_strings_to_history() {
        let mut editor = editor();

        editor.add_to_history("");
        editor.add_to_history("   ");
        editor.add_to_history("valid");

        editor.handle_input("\x1b[A");
        assert_eq!(editor.get_text(), "valid");

        // Should not have more entries.
        editor.handle_input("\x1b[A");
        assert_eq!(editor.get_text(), "valid");
    }

    #[test]
    fn does_not_add_consecutive_duplicates_to_history() {
        let mut editor = editor();

        editor.add_to_history("same");
        editor.add_to_history("same");
        editor.add_to_history("same");

        editor.handle_input("\x1b[A"); // "same"
        assert_eq!(editor.get_text(), "same");

        editor.handle_input("\x1b[A"); // stays at "same" (only one entry)
        assert_eq!(editor.get_text(), "same");
    }

    #[test]
    fn allows_non_consecutive_duplicates_in_history() {
        let mut editor = editor();

        editor.add_to_history("first");
        editor.add_to_history("second");
        editor.add_to_history("first"); // Not consecutive, should be added

        editor.handle_input("\x1b[A"); // "first"
        assert_eq!(editor.get_text(), "first");

        editor.handle_input("\x1b[A"); // "second"
        assert_eq!(editor.get_text(), "second");

        editor.handle_input("\x1b[A"); // "first" (older one)
        assert_eq!(editor.get_text(), "first");
    }

    #[test]
    fn uses_cursor_movement_instead_of_history_when_editor_has_content() {
        let mut editor = editor();

        editor.add_to_history("history item");
        editor.set_text("line1\nline2");

        // Cursor is at end of line2, Up should move to line1.
        editor.handle_input("\x1b[A"); // Up - cursor movement

        // Insert character to verify cursor position.
        editor.handle_input("X");

        // X should be inserted in line1, not replace with history.
        assert_eq!(editor.get_text(), "line1X\nline2");
    }

    #[test]
    fn limits_history_to_100_entries() {
        let mut editor = editor();

        // Add 105 entries.
        for i in 0..105 {
            editor.add_to_history(&format!("prompt {i}"));
        }

        // Navigate to oldest.
        for _ in 0..100 {
            editor.handle_input("\x1b[A");
        }

        // Should be at entry 5 (oldest kept), not entry 0.
        assert_eq!(editor.get_text(), "prompt 5");

        // One more Up should not change anything.
        editor.handle_input("\x1b[A");
        assert_eq!(editor.get_text(), "prompt 5");
    }

    #[test]
    fn places_cursor_at_start_after_browsing_history_upward() {
        let mut editor = editor();

        editor.add_to_history("older entry");
        editor.add_to_history("line1\nline2\nline3");

        editor.handle_input("\x1b[A"); // Up - shows multi-line entry at start
        assert_eq!(editor.get_text(), "line1\nline2\nline3");
        assert_cursor(&editor, 0, 0);

        editor.handle_input("\x1b[A"); // Up again - immediately navigates to older entry
        assert_eq!(editor.get_text(), "older entry");
        assert_cursor(&editor, 0, 0);
    }

    #[test]
    fn places_cursor_at_end_after_browsing_history_downward() {
        let mut editor = editor();

        editor.add_to_history("older entry");
        editor.add_to_history("line1\nline2\nline3");
        editor.add_to_history("newer entry");

        editor.handle_input("\x1b[A"); // newer entry
        editor.handle_input("\x1b[A"); // multi-line entry
        editor.handle_input("\x1b[A"); // older entry

        editor.handle_input("\x1b[B"); // Down - shows multi-line entry at end
        assert_eq!(editor.get_text(), "line1\nline2\nline3");
        assert_cursor(&editor, 2, 5);

        editor.handle_input("\x1b[B"); // Down again - immediately navigates to newer entry
        assert_eq!(editor.get_text(), "newer entry");
    }

    #[test]
    fn allows_opposite_direction_cursor_movement_within_multi_line_history_entry() {
        let mut editor = editor();

        editor.add_to_history("line1\nline2\nline3");

        editor.handle_input("\x1b[A"); // Up - shows entry at start
        assert_cursor(&editor, 0, 0);

        editor.handle_input("\x1b[B"); // Down - cursor moves to line2
        assert_eq!(editor.get_text(), "line1\nline2\nline3");
        assert_cursor(&editor, 1, 0);

        editor.handle_input("\x1b[A"); // Up - cursor moves back to line1
        assert_eq!(editor.get_text(), "line1\nline2\nline3");
        assert_cursor(&editor, 0, 0);
    }

    // --- public state accessors (editor.test.ts:287-313) ------------------

    #[test]
    fn returns_cursor_position() {
        let mut editor = editor();

        assert_cursor(&editor, 0, 0);

        editor.handle_input("a");
        editor.handle_input("b");
        editor.handle_input("c");

        assert_cursor(&editor, 0, 3);

        editor.handle_input("\x1b[D"); // Left
        assert_cursor(&editor, 0, 2);
    }

    #[test]
    fn returns_lines_as_a_defensive_copy() {
        let mut editor = editor();
        editor.set_text("a\nb");

        let lines = editor.get_lines();
        assert_eq!(lines, vec!["a".to_string(), "b".to_string()]);

        // Mutating the returned copy must not affect the editor.
        let mut lines = lines;
        lines[0] = "mutated".to_string();
        assert_eq!(editor.get_lines(), vec!["a".to_string(), "b".to_string()]);
    }

    // --- Backslash+Enter newline workaround (editor.test.ts:315-371) ------

    #[test]
    fn inserts_backslash_immediately_no_buffering() {
        let mut editor = editor();

        editor.handle_input("\\");

        assert_eq!(editor.get_text(), "\\");
    }

    #[test]
    fn converts_standalone_backslash_to_newline_on_enter() {
        let mut editor = editor();

        editor.handle_input("\\");
        editor.handle_input("\r");

        assert_eq!(editor.get_text(), "\n");
    }

    #[test]
    fn inserts_backslash_normally_when_followed_by_other_characters() {
        let mut editor = editor();

        editor.handle_input("\\");
        editor.handle_input("x");

        assert_eq!(editor.get_text(), "\\x");
    }

    #[test]
    fn does_not_trigger_newline_when_backslash_is_not_immediately_before_cursor() {
        let mut editor = editor();
        let submitted = Arc::new(std::sync::Mutex::new(false));
        let captured = Arc::clone(&submitted);
        editor.on_submit = Some(Box::new(move |_| {
            *captured.lock().unwrap() = true;
        }));

        editor.handle_input("\\");
        editor.handle_input("x");
        editor.handle_input("\r");

        // Should submit, not insert newline (backslash not at cursor).
        assert!(*submitted.lock().unwrap());
    }

    #[test]
    fn only_removes_one_backslash_when_multiple_are_present() {
        let mut editor = editor();

        editor.handle_input("\\");
        editor.handle_input("\\");
        editor.handle_input("\\");
        assert_eq!(editor.get_text(), "\\\\\\");

        editor.handle_input("\r");
        // Only the last backslash is removed, newline inserted.
        assert_eq!(editor.get_text(), "\\\\\n");
    }

    // --- Kitty CSI-u handling (editor.test.ts:373-397) --------------------

    #[test]
    fn ignores_printable_csi_u_sequences_with_unsupported_modifiers() {
        let mut editor = editor();

        editor.handle_input("\x1b[99;9u");

        assert_eq!(editor.get_text(), "");
    }

    #[test]
    fn inserts_shifted_csi_u_letters_as_text() {
        let mut editor = editor();

        editor.handle_input("\x1b[69;2u");

        assert_eq!(editor.get_text(), "E");
    }

    #[test]
    fn inserts_shifted_xterm_modify_other_keys_letters_as_text() {
        let mut editor = editor();

        editor.handle_input("\x1b[27;2;69~");

        assert_eq!(editor.get_text(), "E");
    }

    // --- Unicode text editing behavior (editor.test.ts:399-700) ------------

    #[test]
    fn inserts_mixed_ascii_umlauts_and_emojis_as_literal_text() {
        let mut editor = editor();

        for ch in "Hello äöü 😀".chars() {
            editor.handle_input(&ch.to_string());
        }

        assert_eq!(editor.get_text(), "Hello äöü 😀");
    }

    #[test]
    fn deletes_single_code_unit_unicode_characters_with_backspace() {
        let mut editor = editor();
        type_text(&mut editor, "äöü");

        // Delete the last character (ü).
        editor.handle_input("\x7f"); // Backspace

        assert_eq!(editor.get_text(), "äö");
    }

    #[test]
    fn deletes_multi_code_unit_emojis_with_single_backspace() {
        let mut editor = editor();
        type_text(&mut editor, "😀👍");

        // Delete the last emoji (👍) — single backspace deletes whole
        // grapheme cluster.
        editor.handle_input("\x7f"); // Backspace

        assert_eq!(editor.get_text(), "😀");
    }

    #[test]
    fn inserts_characters_at_the_correct_position_after_cursor_movement_over_umlauts() {
        let mut editor = editor();
        type_text(&mut editor, "äöü");

        // Move cursor left twice.
        editor.handle_input("\x1b[D"); // Left arrow
        editor.handle_input("\x1b[D"); // Left arrow

        // Insert 'x' in the middle.
        editor.handle_input("x");

        assert_eq!(editor.get_text(), "äxöü");
    }

    #[test]
    fn moves_cursor_across_multi_code_unit_emojis_with_single_arrow_key() {
        let mut editor = editor();
        type_text(&mut editor, "😀👍🎉");

        // Move cursor left over last emoji (🎉) — single arrow moves over
        // the whole grapheme.
        editor.handle_input("\x1b[D"); // Left arrow

        // Move cursor left over second emoji (👍).
        editor.handle_input("\x1b[D");

        // Insert 'x' between first and second emoji.
        editor.handle_input("x");

        assert_eq!(editor.get_text(), "😀x👍🎉");
    }

    #[test]
    fn preserves_umlauts_across_line_breaks() {
        let mut editor = editor();
        type_text(&mut editor, "äöü");
        editor.handle_input("\n"); // new line
        type_text(&mut editor, "ÄÖÜ");

        assert_eq!(editor.get_text(), "äöü\nÄÖÜ");
    }

    #[test]
    fn replaces_the_entire_document_with_unicode_text_via_set_text() {
        let mut editor = editor();

        // Simulate bracketed paste / programmatic replacement.
        editor.set_text("Hällö Wörld! 😀 äöüÄÖÜß");

        assert_eq!(editor.get_text(), "Hällö Wörld! 😀 äöüÄÖÜß");
    }

    #[test]
    fn moves_cursor_to_document_start_on_ctrl_a_and_inserts_at_the_beginning() {
        let mut editor = editor();

        editor.handle_input("a");
        editor.handle_input("b");
        editor.handle_input("\x01"); // Ctrl+A (move to start)
        editor.handle_input("x"); // Insert at start

        assert_eq!(editor.get_text(), "xab");
    }

    #[test]
    fn deletes_words_correctly_with_ctrl_w_and_alt_backspace() {
        let mut editor = editor();

        // Basic word deletion.
        editor.set_text("foo bar baz");
        editor.handle_input("\x17"); // Ctrl+W
        assert_eq!(editor.get_text(), "foo bar ");

        // Trailing whitespace.
        editor.set_text("foo bar   ");
        editor.handle_input("\x17");
        assert_eq!(editor.get_text(), "foo ");

        // Punctuation run.
        editor.set_text("foo bar...");
        editor.handle_input("\x17");
        assert_eq!(editor.get_text(), "foo bar");

        // ASCII punctuation inside Intl word-like segments preserves old
        // boundaries.
        editor.set_text("foo.bar");
        editor.handle_input("\x17");
        assert_eq!(editor.get_text(), "foo.");

        editor.set_text("foo:bar");
        editor.handle_input("\x17");
        assert_eq!(editor.get_text(), "foo:");

        // Delete across multiple lines.
        editor.set_text("line one\nline two");
        editor.handle_input("\x17");
        assert_eq!(editor.get_text(), "line one\nline ");

        // Delete empty line (merge).
        editor.set_text("line one\n");
        editor.handle_input("\x17");
        assert_eq!(editor.get_text(), "line one");

        // Grapheme safety (emoji as a word).
        editor.set_text("foo 😀😀 bar");
        editor.handle_input("\x17");
        assert_eq!(editor.get_text(), "foo 😀😀 ");
        editor.handle_input("\x17");
        assert_eq!(editor.get_text(), "foo ");

        // Alt+Backspace.
        editor.set_text("foo bar");
        editor.handle_input("\x1b\x7f"); // Alt+Backspace (legacy)
        assert_eq!(editor.get_text(), "foo ");
    }

    #[test]
    fn navigates_words_correctly_with_ctrl_left_right() {
        let mut editor = editor();

        editor.set_text("foo bar... baz");
        // Cursor at end.

        // Move left over baz.
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left
        assert_cursor(&editor, 0, 11); // after '...'

        // Move left over punctuation.
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left
        assert_cursor(&editor, 0, 7); // after 'bar'

        // Move left over bar.
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left
        assert_cursor(&editor, 0, 4); // after 'foo '

        // Move right over bar.
        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 0, 7); // at end of 'bar'

        // Move right over punctuation run.
        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 0, 10); // after '...'

        // Move right skips space and lands after baz.
        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 0, 14); // end of line

        // Test forward from start with leading whitespace.
        editor.set_text("   foo bar");
        editor.handle_input("\x01"); // Ctrl+A to go to start
        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 0, 6); // after 'foo'

        // ASCII punctuation inside Intl word-like segments preserves old
        // boundaries.
        editor.set_text("foo.bar baz");
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left over baz
        assert_cursor(&editor, 0, 8);
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left over bar
        assert_cursor(&editor, 0, 4);
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left over .
        assert_cursor(&editor, 0, 3);

        editor.handle_input("\x01"); // Ctrl+A
        editor.handle_input("\x1b[1;5C"); // Ctrl+Right over foo
        assert_cursor(&editor, 0, 3);
        editor.handle_input("\x1b[1;5C"); // Ctrl+Right over .
        assert_cursor(&editor, 0, 4);
        editor.handle_input("\x1b[1;5C"); // Ctrl+Right over bar
        assert_cursor(&editor, 0, 7);
    }

    #[test]
    fn stops_at_fullwidth_chinese_punctuation() {
        let mut editor = editor();

        // 你好，世界 = 你好(0-2) ，(2-3) 世界(3-5)
        editor.set_text("你好，世界");
        // Cursor at end (col 5).

        // Move left over 世界.
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left
        assert_cursor(&editor, 0, 3); // after ，

        // Move left over ，
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left
        assert_cursor(&editor, 0, 2); // after 你好

        // Move left over 你好.
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left
        assert_cursor(&editor, 0, 0); // start

        // Move right over 你好.
        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 0, 2); // after 你好

        // Move right over ，
        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 0, 3); // after ，

        // Move right over 世界.
        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 0, 5); // end
    }

    #[test]
    fn handles_mixed_cjk_and_ascii_word_movement() {
        let mut editor = editor();

        // "hello你好，world世界" = hello(0-5) 你好(5-7) ，(7-8) world(8-13) 世界(13-15)
        editor.set_text("hello你好，world世界");
        // Cursor at end (col 15).

        // Move left over 世界.
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left
        assert_cursor(&editor, 0, 13); // after 'world'

        // Move left over world.
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left
        assert_cursor(&editor, 0, 8); // after ，

        // Move left over ，
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left
        assert_cursor(&editor, 0, 7); // after 你好

        // Move left over 你好.
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left
        assert_cursor(&editor, 0, 5); // after 'hello'

        // Move left over hello.
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left
        assert_cursor(&editor, 0, 0); // start

        // Forward from start.
        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 0, 5); // after 'hello'

        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 0, 7); // after 你好

        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 0, 8); // after ，

        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 0, 13); // after 'world'

        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 0, 15); // end
    }

    // --- Scroll indicators (editor.test.ts:702-725) ------------------------

    #[test]
    fn keeps_truncated_scroll_indicators_within_width_and_preserves_their_color() {
        let width = 10;
        let border_color = |text: &str| format!("\x1b[35m{text}\x1b[39m");
        let mut editor = Editor::new(
            test_tui(width, 24),
            EditorTheme {
                border_color: Box::new(border_color),
                select_list: Arc::new(SelectListTheme::identity()),
            },
            EditorOptions::default(),
        );
        editor.set_text(
            &(0..20)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        // Render once to initialize wrapping, then move the cursor so
        // content remains above and below the viewport.
        editor.render(width);
        for _ in 0..10 {
            editor.handle_input("\x1b[A");
        }

        let lines = editor.render(width);
        let top_border = &lines[0];
        let bottom_border = lines.last().expect("at least one line");

        assert!(strip_ansi(top_border).starts_with("─── ↑"));
        assert!(strip_ansi(bottom_border).starts_with("─── ↓"));
        assert_eq!(*top_border, border_color(&strip_ansi(top_border)));
        assert_eq!(*bottom_border, border_color(&strip_ansi(bottom_border)));
        for line in &lines {
            assert_eq!(
                visible_width(line),
                width,
                "line exceeds width {width}: {}",
                strip_ansi(line)
            );
        }
    }

    // --- Grapheme-aware text wrapping (editor.test.ts:727-858) ------------

    #[test]
    fn wraps_lines_correctly_when_text_contains_wide_emojis() {
        let mut editor = editor();
        let width = 20;

        // ✅ is 2 columns wide, so "Hello ✅ World" is 14 columns.
        editor.set_text("Hello ✅ World");
        let lines = editor.render(width);

        // All content lines (between borders) should fit within width.
        for (i, line) in lines
            .iter()
            .enumerate()
            .skip(1)
            .take(lines.len().saturating_sub(2))
        {
            let line_width = visible_width(line);
            assert_eq!(
                line_width, width,
                "Line {i} has width {line_width}, expected {width}"
            );
        }
    }

    #[test]
    fn wraps_long_text_with_emojis_at_correct_positions() {
        let mut editor = editor();
        let width = 10;

        // Each ✅ is 2 columns. "✅✅✅✅✅" = 10 columns, fits exactly;
        // "✅✅✅✅✅✅" = 12 columns, needs wrap.
        editor.set_text("✅✅✅✅✅✅");
        let lines = editor.render(width);

        // Should have 2 content lines (plus 2 border lines).
        for (i, line) in lines
            .iter()
            .enumerate()
            .skip(1)
            .take(lines.len().saturating_sub(2))
        {
            let line_width = visible_width(line);
            assert_eq!(
                line_width, width,
                "Line {i} has width {line_width}, expected {width}"
            );
        }
    }

    #[test]
    fn renders_isolated_thai_and_lao_am_clusters_without_width_drift() {
        for text in ["ำabc", "ຳabc"] {
            let mut editor = editor();
            let width = 8;
            editor.set_text(text);

            for line in editor.render(width) {
                assert_eq!(visible_width(&line), width, "line width drift for {text:?}");
            }
        }
    }

    #[test]
    fn wraps_cjk_characters_correctly_each_is_2_columns_wide() {
        let mut editor = editor();
        let width = 10 + 1; // +1 col reserved for cursor

        // Each CJK char is 2 columns. "日本語テスト" = 6 chars = 12 columns.
        editor.set_text("日本語テスト");
        let lines = editor.render(width);

        for (i, line) in lines
            .iter()
            .enumerate()
            .skip(1)
            .take(lines.len().saturating_sub(2))
        {
            let line_width = visible_width(line);
            assert_eq!(
                line_width, width,
                "Line {i} has width {line_width}, expected {width}"
            );
        }

        // Verify content split correctly.
        let content_lines: Vec<String> = lines[1..lines.len() - 1]
            .iter()
            .map(|line| strip_ansi(line).trim().to_string())
            .collect();
        assert_eq!(content_lines.len(), 2);
        assert_eq!(content_lines[0], "日本語テス"); // 5 chars = 10 columns
        assert_eq!(content_lines[1], "ト"); // 1 char = 2 columns (+ padding)
    }

    #[test]
    fn handles_mixed_ascii_and_wide_characters_in_wrapping() {
        let mut editor = editor();
        let width = 15 + 1; // +1 col reserved for cursor

        // "Test ✅ OK 日本" = 4 + 1 + 2 + 1 + 2 + 1 + 4 = 15 columns (fits in width-1=15).
        editor.set_text("Test ✅ OK 日本");
        let lines = editor.render(width);

        // Should fit in one content line.
        let content_lines = &lines[1..lines.len() - 1];
        assert_eq!(content_lines.len(), 1);

        let line_width = visible_width(&content_lines[0]);
        assert_eq!(line_width, width);
    }

    #[test]
    fn renders_cursor_correctly_on_wide_characters() {
        let mut editor = editor();
        let width = 20;

        editor.set_text("A✅B");
        // Cursor should be at end (after B).
        let lines = editor.render(width);

        // The cursor (reverse video space) should be visible.
        let content_line = &lines[1];
        assert!(
            content_line.contains("\x1b[7m"),
            "Should have reverse video cursor"
        );

        // Line should still be correct width.
        assert_eq!(visible_width(content_line), width);
    }

    #[test]
    fn does_not_exceed_terminal_width_with_emoji_at_wrap_boundary() {
        let mut editor = editor();
        let width = 11;

        // "0123456789✅" = 10 ASCII + 2-wide emoji = 12 columns.
        // Should wrap before the emoji since it would exceed width.
        editor.set_text("0123456789✅");
        let lines = editor.render(width);

        for (i, line) in lines
            .iter()
            .enumerate()
            .skip(1)
            .take(lines.len().saturating_sub(2))
        {
            let line_width = visible_width(line);
            assert!(
                line_width <= width,
                "Line {i} has width {line_width}, exceeds max {width}"
            );
        }
    }

    #[test]
    fn shows_cursor_at_end_of_line_before_wrap_wraps_on_next_char() {
        let width = 10;
        for padding_x in [0usize, 1] {
            let mut editor = Editor::new(
                test_tui(width + padding_x, 24),
                test_theme(),
                EditorOptions {
                    padding_x: Some(padding_x),
                    autocomplete_max_visible: None,
                },
            );

            // Type 9 chars → fills layoutWidth exactly, cursor at end on
            // same line.
            type_text(&mut editor, "aaaaaaaaa");
            let lines = editor.render(width + padding_x);
            let content_lines = &lines[1..lines.len() - 1];
            assert_eq!(
                content_lines.len(),
                1,
                "Should be 1 content line before wrap"
            );
            assert!(
                content_lines[0].ends_with("\x1b[7m \x1b[0m"),
                "Cursor should be at end of line"
            );

            // Type 1 more → text wraps to second line.
            editor.handle_input("a");
            let lines = editor.render(width + padding_x);
            let content_lines = &lines[1..lines.len() - 1];
            assert_eq!(content_lines.len(), 2, "Should wrap to 2 content lines");
        }
    }

    // --- Word wrapping (editor.test.ts:860-1181) ---------------------------

    #[test]
    fn wraps_at_word_boundaries_instead_of_mid_word() {
        let mut editor = editor();
        let width = 40;

        editor.set_text("Hello world this is a test of word wrapping functionality");
        let lines = editor.render(width);

        // Get content lines (between borders).
        let content_lines: Vec<String> = lines[1..lines.len() - 1]
            .iter()
            .map(|line| strip_ansi(line).trim().to_string())
            .collect();

        // Should NOT break mid-word.
        assert!(
            !content_lines[0].ends_with('-'),
            "Line should not end with hyphen (mid-word break)"
        );

        // Each content line should be complete words.
        for line in &content_lines {
            let last_char = line.trim_end().chars().last().unwrap_or('\0');
            assert!(
                last_char == '\0'
                    || last_char.is_ascii_alphanumeric()
                    || matches!(last_char, '.' | ',' | '!' | '?' | ';' | ':' | '_'),
                "Line ends unexpectedly with: \"{last_char}\""
            );
        }
    }

    #[test]
    fn does_not_start_lines_with_leading_whitespace_after_word_wrap() {
        let mut editor = editor();
        let width = 20;

        editor.set_text("Word1 Word2 Word3 Word4 Word5 Word6");
        let lines = editor.render(width);

        // Get content lines (between borders).
        let content_lines = &lines[1..lines.len() - 1];

        // No line should start with whitespace (except for padding at the end).
        for (i, line) in content_lines.iter().enumerate() {
            let line = strip_ansi(line);
            let trimmed_start = line.trim_start();
            // The line should either be all padding or start with a word
            // character.
            if !trimmed_start.is_empty() {
                let trimmed_end = line.trim_end();
                assert!(
                    !trimmed_end
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_whitespace()),
                    "Line {i} starts with unexpected whitespace before content"
                );
            }
        }
    }

    #[test]
    fn breaks_long_words_urls_at_character_level() {
        let mut editor = editor();
        let width = 30;

        editor.set_text("Check https://example.com/very/long/path/that/exceeds/width here");
        let lines = editor.render(width);

        // All lines should fit within width.
        for (i, line) in lines
            .iter()
            .enumerate()
            .skip(1)
            .take(lines.len().saturating_sub(2))
        {
            let line_width = visible_width(line);
            assert_eq!(
                line_width, width,
                "Line {i} has width {line_width}, expected {width}"
            );
        }
    }

    #[test]
    fn preserves_multiple_spaces_within_words_on_same_line() {
        let mut editor = editor();
        let width = 50;

        editor.set_text("Word1   Word2    Word3");
        let lines = editor.render(width);

        let content_line = strip_ansi(&lines[1]);
        let content_line = content_line.trim();
        // Multiple spaces should be preserved.
        assert!(
            content_line.contains("Word1   Word2"),
            "Multiple spaces should be preserved"
        );
    }

    #[test]
    fn handles_empty_string_render() {
        let mut editor = editor();
        let width = 40;

        editor.set_text("");
        let lines = editor.render(width);

        // Should have border + empty content + border.
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn handles_single_word_that_fits_exactly() {
        let mut editor = editor();
        let width = 10 + 1; // +1 col reserved for cursor

        editor.set_text("1234567890");
        let lines = editor.render(width);

        // Should have exactly 3 lines (top border, content, bottom border).
        assert_eq!(lines.len(), 3);
        let content_line = strip_ansi(&lines[1]);
        assert!(
            content_line.contains("1234567890"),
            "Content should contain the word"
        );
    }

    fn wrap(line: &str, max_width: usize) -> Vec<TextChunk> {
        word_wrap_line(line, max_width, None)
    }

    #[test]
    fn wraps_word_to_next_line_when_it_ends_exactly_at_terminal_width() {
        let chunks = wrap("hello world test", 11);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "hello ");
        assert_eq!(chunks[1].text, "world test");
    }

    #[test]
    fn keeps_whitespace_at_terminal_width_boundary_on_same_line() {
        let chunks = wrap("hello world test", 12);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "hello world ");
        assert_eq!(chunks[1].text, "test");
    }

    #[test]
    fn handles_unbreakable_word_filling_width_exactly_followed_by_space() {
        let chunks = wrap("aaaaaaaaaaaa aaaa", 12);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "aaaaaaaaaaaa");
        assert_eq!(chunks[1].text, " aaaa");
    }

    #[test]
    fn wraps_word_to_next_line_when_it_fits_width_but_not_remaining_space() {
        let chunks = wrap("      aaaaaaaaaaaa", 12);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "      ");
        assert_eq!(chunks[1].text, "aaaaaaaaaaaa");
    }

    #[test]
    fn keeps_word_with_multi_space_and_following_word_together_when_they_fit() {
        let chunks = wrap("Lorem ipsum dolor sit amet,    consectetur", 30);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
        assert_eq!(chunks[1].text, "amet,    consectetur");
    }

    #[test]
    fn keeps_word_with_multi_space_and_following_word_when_they_fill_width_exactly() {
        let chunks = wrap("Lorem ipsum dolor sit amet,              consectetur", 30);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
        assert_eq!(chunks[1].text, "amet,              consectetur");
    }

    #[test]
    fn splits_when_word_plus_multi_space_plus_word_exceeds_width() {
        let chunks = wrap("Lorem ipsum dolor sit amet,               consectetur", 30);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
        assert_eq!(chunks[1].text, "amet,               ");
        assert_eq!(chunks[2].text, "consectetur");
    }

    #[test]
    fn breaks_long_whitespace_at_line_boundary() {
        let chunks = wrap(
            "Lorem ipsum dolor sit amet,                         consectetur",
            30,
        );
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
        assert_eq!(chunks[1].text, "amet,                         ");
        assert_eq!(chunks[2].text, "consectetur");
    }

    #[test]
    fn breaks_long_whitespace_at_line_boundary_2() {
        let chunks = wrap(
            "Lorem ipsum dolor sit amet,                          consectetur",
            30,
        );
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
        assert_eq!(chunks[1].text, "amet,                         ");
        assert_eq!(chunks[2].text, " consectetur");
    }

    #[test]
    fn breaks_whitespace_spanning_full_lines() {
        let chunks = wrap(
            "Lorem ipsum dolor sit amet,                                     consectetur",
            30,
        );
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
        assert_eq!(chunks[1].text, "amet,                         ");
        assert_eq!(chunks[2].text, "            consectetur");
    }

    #[test]
    fn force_breaks_when_wide_char_after_word_boundary_wrap_still_overflows() {
        // " " (1) + "a"*186 (186) + "你" (2) = 189 visible width; maxWidth =
        // 187: backtracking to the space would leave 186 + 2 = 188 > 187, so
        // the algorithm must force-break before the wide char instead.
        let line = format!(" {}{}", "a".repeat(186), "你");
        let chunks = wrap(&line, 187);

        for chunk in &chunks {
            assert!(
                visible_width(&chunk.text) <= 187,
                "chunk \"{}\"... has visible width {}, expected <= 187",
                chunk.text.chars().take(20).collect::<String>(),
                visible_width(&chunk.text)
            );
        }
        // Verify no content is lost (char-index reconstruction).
        let reconstructed: String = chunks
            .iter()
            .map(|c| {
                line.chars()
                    .skip(c.start_index)
                    .take(c.end_index - c.start_index)
                    .collect::<String>()
            })
            .collect();
        assert_eq!(reconstructed, line);
    }

    /// Build pre-segmented `EditorSegment` data (upstream `Intl.SegmentData[]`).
    /// The `line` argument only constrains the borrow lifetime.
    fn segments<'a>(_line: &'a str, pairs: &[(&'a str, usize)]) -> Vec<EditorSegment<'a>> {
        pairs
            .iter()
            .map(|(segment, index)| EditorSegment {
                segment,
                index: *index,
            })
            .collect()
    }

    #[test]
    fn splits_oversized_atomic_segment_across_multiple_chunks() {
        // Simulate a paste marker wider than maxWidth by passing
        // pre-segmented data.
        let marker = "[paste #1 +20 lines]"; // 21 chars
        let line = format!("A{marker}B");
        let pre = segments(
            &line,
            &[("A", 0), (marker, 1), ("B", 1 + marker.chars().count())],
        );

        let chunks = word_wrap_line(&line, 10, Some(&pre));

        // Every chunk must fit within maxWidth.
        for chunk in &chunks {
            assert!(
                visible_width(&chunk.text) <= 10,
                "chunk \"{}\" has visible width {}, expected <= 10",
                chunk.text,
                visible_width(&chunk.text)
            );
        }

        // Verify no content is lost.
        let reconstructed: String = chunks
            .iter()
            .map(|c| {
                line.chars()
                    .skip(c.start_index)
                    .take(c.end_index - c.start_index)
                    .collect::<String>()
            })
            .collect();
        assert_eq!(reconstructed, line);
    }

    #[test]
    fn splits_oversized_atomic_segment_at_start_of_line() {
        let marker = "[paste #1 +20 lines]"; // 21 chars
        let line = format!("{marker}B");
        let pre = segments(&line, &[(marker, 0), ("B", marker.chars().count())]);

        let chunks = word_wrap_line(&line, 10, Some(&pre));

        for chunk in &chunks {
            assert!(visible_width(&chunk.text) <= 10);
        }
        // "B" ends up on the last line (either alone or with the marker tail).
        assert!(chunks.last().expect("chunks non-empty").text.contains('B'));

        let reconstructed: String = chunks
            .iter()
            .map(|c| {
                line.chars()
                    .skip(c.start_index)
                    .take(c.end_index - c.start_index)
                    .collect::<String>()
            })
            .collect();
        assert_eq!(reconstructed, line);
    }

    #[test]
    fn splits_oversized_atomic_segment_at_end_of_line() {
        let marker = "[paste #1 +20 lines]"; // 21 chars
        let line = format!("A{marker}");
        let pre = segments(&line, &[("A", 0), (marker, 1)]);

        let chunks = word_wrap_line(&line, 10, Some(&pre));

        for chunk in &chunks {
            assert!(visible_width(&chunk.text) <= 10);
        }
        assert_eq!(chunks[0].text, "A");

        let reconstructed: String = chunks
            .iter()
            .map(|c| {
                line.chars()
                    .skip(c.start_index)
                    .take(c.end_index - c.start_index)
                    .collect::<String>()
            })
            .collect();
        assert_eq!(reconstructed, line);
    }

    #[test]
    fn splits_consecutive_oversized_atomic_segments() {
        let m1 = "[paste #1 +20 lines]"; // 21 chars
        let m2 = "[paste #2 +30 lines]"; // 21 chars
        let line = format!("{m1}{m2}");
        let pre = segments(&line, &[(m1, 0), (m2, m1.chars().count())]);

        let chunks = word_wrap_line(&line, 10, Some(&pre));

        for chunk in &chunks {
            assert!(
                visible_width(&chunk.text) <= 10,
                "chunk \"{}\" has visible width {}, expected <= 10",
                chunk.text,
                visible_width(&chunk.text)
            );
        }

        let reconstructed: String = chunks
            .iter()
            .map(|c| {
                line.chars()
                    .skip(c.start_index)
                    .take(c.end_index - c.start_index)
                    .collect::<String>()
            })
            .collect();
        assert_eq!(reconstructed, line);
    }

    #[test]
    fn wraps_normally_after_oversized_atomic_segment() {
        let marker = "[paste #1 +20 lines]"; // 21 chars
        let line = format!("{marker} hello world");
        let mut pre = vec![(marker, 0), (" ", marker.chars().count())];
        for (offset, _ch) in "hello world".chars().enumerate() {
            pre.push((
                &line[marker.chars().count() + 1 + offset..marker.chars().count() + 1 + offset + 1],
                marker.chars().count() + 1 + offset,
            ));
        }
        let pre_segments: Vec<EditorSegment<'_>> = pre
            .iter()
            .map(|(segment, index)| EditorSegment {
                segment,
                index: *index,
            })
            .collect();

        let chunks = word_wrap_line(&line, 10, Some(&pre_segments));

        // All chunks must fit.
        for chunk in &chunks {
            assert!(
                visible_width(&chunk.text) <= 10,
                "chunk \"{}\" has visible width {}, expected <= 10",
                chunk.text,
                visible_width(&chunk.text)
            );
        }

        // Last chunk should contain "world" (normal wrapping resumes).
        assert_eq!(chunks.last().expect("chunks non-empty").text, "world");

        let reconstructed: String = chunks
            .iter()
            .map(|c| {
                line.chars()
                    .skip(c.start_index)
                    .take(c.end_index - c.start_index)
                    .collect::<String>()
            })
            .collect();
        assert_eq!(reconstructed, line);
    }

    // --- Kill ring (editor.test.ts:1183-1578) ------------------------------

    #[test]
    fn ctrl_w_saves_deleted_text_to_kill_ring_and_ctrl_y_yanks_it() {
        let mut editor = editor();

        editor.set_text("foo bar baz");
        editor.handle_input("\x17"); // Ctrl+W - deletes "baz"
        assert_eq!(editor.get_text(), "foo bar ");

        // Move to beginning and yank.
        editor.handle_input("\x01"); // Ctrl+A
        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "bazfoo bar ");
    }

    #[test]
    fn ctrl_u_saves_deleted_text_to_kill_ring() {
        let mut editor = editor();

        editor.set_text("hello world");
        // Move cursor to middle.
        editor.handle_input("\x01"); // Ctrl+A (start)
        for _ in 0..6 {
            editor.handle_input("\x1b[C");
        }

        editor.handle_input("\x15"); // Ctrl+U - deletes "hello "
        assert_eq!(editor.get_text(), "world");

        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "hello world");
    }

    #[test]
    fn ctrl_k_saves_deleted_text_to_kill_ring() {
        let mut editor = editor();

        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A (start)
        editor.handle_input("\x0b"); // Ctrl+K - deletes "hello world"

        assert_eq!(editor.get_text(), "");

        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "hello world");
    }

    #[test]
    fn ctrl_y_does_nothing_when_kill_ring_is_empty() {
        let mut editor = editor();

        editor.set_text("test");
        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "test");
    }

    #[test]
    fn alt_y_cycles_through_kill_ring_after_ctrl_y() {
        let mut editor = editor();

        // Create kill ring with multiple entries.
        editor.set_text("first");
        editor.handle_input("\x17"); // Ctrl+W - deletes "first"
        editor.set_text("second");
        editor.handle_input("\x17"); // Ctrl+W - deletes "second"
        editor.set_text("third");
        editor.handle_input("\x17"); // Ctrl+W - deletes "third"

        // Kill ring now has: [first, second, third].
        assert_eq!(editor.get_text(), "");

        editor.handle_input("\x19"); // Ctrl+Y - yanks "third" (most recent)
        assert_eq!(editor.get_text(), "third");

        editor.handle_input("\x1by"); // Alt+Y - cycles to "second"
        assert_eq!(editor.get_text(), "second");

        editor.handle_input("\x1by"); // Alt+Y - cycles to "first"
        assert_eq!(editor.get_text(), "first");

        editor.handle_input("\x1by"); // Alt+Y - cycles back to "third"
        assert_eq!(editor.get_text(), "third");
    }

    #[test]
    fn alt_y_does_nothing_if_not_preceded_by_yank() {
        let mut editor = editor();

        editor.set_text("test");
        editor.handle_input("\x17"); // Ctrl+W - deletes "test"
        editor.set_text("other");

        // Type something to break the yank chain.
        editor.handle_input("x");
        assert_eq!(editor.get_text(), "otherx");

        // Alt+Y should do nothing.
        editor.handle_input("\x1by"); // Alt+Y
        assert_eq!(editor.get_text(), "otherx");
    }

    #[test]
    fn alt_y_does_nothing_if_kill_ring_has_one_entry() {
        let mut editor = editor();

        editor.set_text("only");
        editor.handle_input("\x17"); // Ctrl+W - deletes "only"

        editor.handle_input("\x19"); // Ctrl+Y - yanks "only"
        assert_eq!(editor.get_text(), "only");

        editor.handle_input("\x1by"); // Alt+Y - should do nothing (only 1 entry)
        assert_eq!(editor.get_text(), "only");
    }

    #[test]
    fn consecutive_ctrl_w_accumulates_into_one_kill_ring_entry() {
        let mut editor = editor();

        editor.set_text("one two three");
        editor.handle_input("\x17"); // Ctrl+W - deletes "three"
        editor.handle_input("\x17"); // Ctrl+W - deletes "two " (prepended)
        editor.handle_input("\x17"); // Ctrl+W - deletes "one " (prepended)

        assert_eq!(editor.get_text(), "");

        // Should be one combined entry.
        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "one two three");
    }

    #[test]
    fn ctrl_u_accumulates_multiline_deletes_including_newlines() {
        let mut editor = editor();

        // Start with multiline text, cursor at end.
        editor.set_text("line1\nline2\nline3");
        // Cursor is at end of line3 (line 2, col 5).

        // Delete "line3".
        editor.handle_input("\x15"); // Ctrl+U
        assert_eq!(editor.get_text(), "line1\nline2\n");

        // Delete newline (at start of empty line 2, merges with line1).
        editor.handle_input("\x15"); // Ctrl+U
        assert_eq!(editor.get_text(), "line1\nline2");

        // Delete "line2".
        editor.handle_input("\x15"); // Ctrl+U
        assert_eq!(editor.get_text(), "line1\n");

        // Delete newline.
        editor.handle_input("\x15"); // Ctrl+U
        assert_eq!(editor.get_text(), "line1");

        // Delete "line1".
        editor.handle_input("\x15"); // Ctrl+U
        assert_eq!(editor.get_text(), "");

        // All deletions accumulated into one entry.
        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "line1\nline2\nline3");
    }

    #[test]
    fn backward_deletions_prepend_forward_deletions_append_during_accumulation() {
        let mut editor = editor();

        editor.set_text("prefix|suffix");
        // Position cursor at |.
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..6 {
            editor.handle_input("\x1b[C"); // Move right 6
        }

        editor.handle_input("\x0b"); // Ctrl+K - deletes "suffix" (forward)
        editor.handle_input("\x0b"); // Ctrl+K - deletes "|" (forward, appended)
        assert_eq!(editor.get_text(), "prefix");

        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "prefix|suffix");
    }

    #[test]
    fn non_delete_actions_break_kill_accumulation() {
        let mut editor = editor();

        // Delete "baz", then type "x" to break accumulation, then delete "x".
        editor.set_text("foo bar baz");
        editor.handle_input("\x17"); // Ctrl+W - deletes "baz"
        assert_eq!(editor.get_text(), "foo bar ");

        editor.handle_input("x"); // Typing breaks accumulation
        assert_eq!(editor.get_text(), "foo bar x");

        editor.handle_input("\x17"); // Ctrl+W - deletes "x" (separate entry, not accumulated)
        assert_eq!(editor.get_text(), "foo bar ");

        // Yank most recent - should be "x", not "xbaz".
        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "foo bar x");

        // Cycle to previous - should be "baz" (separate entry).
        editor.handle_input("\x1by"); // Alt+Y
        assert_eq!(editor.get_text(), "foo bar baz");
    }

    #[test]
    fn non_yank_actions_break_alt_y_chain() {
        let mut editor = editor();

        editor.set_text("first");
        editor.handle_input("\x17"); // Ctrl+W
        editor.set_text("second");
        editor.handle_input("\x17"); // Ctrl+W
        editor.set_text("");

        editor.handle_input("\x19"); // Ctrl+Y - yanks "second"
        assert_eq!(editor.get_text(), "second");

        editor.handle_input("x"); // Type breaks yank chain
        assert_eq!(editor.get_text(), "secondx");

        editor.handle_input("\x1by"); // Alt+Y - should do nothing
        assert_eq!(editor.get_text(), "secondx");
    }

    #[test]
    fn kill_ring_rotation_persists_after_cycling() {
        let mut editor = editor();

        editor.set_text("first");
        editor.handle_input("\x17"); // deletes "first"
        editor.set_text("second");
        editor.handle_input("\x17"); // deletes "second"
        editor.set_text("third");
        editor.handle_input("\x17"); // deletes "third"
        editor.set_text("");

        // Ring: [first, second, third].

        editor.handle_input("\x19"); // Ctrl+Y - yanks "third"
        editor.handle_input("\x1by"); // Alt+Y - cycles to "second", ring rotates

        // Now ring is: [third, first, second].
        assert_eq!(editor.get_text(), "second");

        // Do something else.
        editor.handle_input("x");
        editor.set_text("");

        // New yank should get "second" (now at end after rotation).
        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "second");
    }

    #[test]
    fn consecutive_deletions_across_lines_coalesce_into_one_entry() {
        let mut editor = editor();

        // "1\n2\n3" with cursor at end, delete everything with Ctrl+W.
        editor.set_text("1\n2\n3");
        editor.handle_input("\x17"); // Ctrl+W - deletes "3"
        assert_eq!(editor.get_text(), "1\n2\n");

        editor.handle_input("\x17"); // Ctrl+W - deletes newline (merge with prev line)
        assert_eq!(editor.get_text(), "1\n2");

        editor.handle_input("\x17"); // Ctrl+W - deletes "2"
        assert_eq!(editor.get_text(), "1\n");

        editor.handle_input("\x17"); // Ctrl+W - deletes newline
        assert_eq!(editor.get_text(), "1");

        editor.handle_input("\x17"); // Ctrl+W - deletes "1"
        assert_eq!(editor.get_text(), "");

        // All deletions should have accumulated into one entry.
        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "1\n2\n3");
    }

    #[test]
    fn ctrl_k_at_line_end_deletes_newline_and_coalesces() {
        let mut editor = editor();

        // "ab" on line 1, "cd" on line 2, cursor at end of line 1.
        editor.set_text("");
        editor.handle_input("a");
        editor.handle_input("b");
        editor.handle_input("\n");
        editor.handle_input("c");
        editor.handle_input("d");
        // Move to end of first line.
        editor.handle_input("\x1b[A"); // Up arrow
        editor.handle_input("\x05"); // Ctrl+E - end of line

        // Now at end of "ab", Ctrl+K should delete newline (merge with "cd").
        editor.handle_input("\x0b"); // Ctrl+K - deletes newline
        assert_eq!(editor.get_text(), "abcd");

        // Continue deleting.
        editor.handle_input("\x0b"); // Ctrl+K - deletes "cd"
        assert_eq!(editor.get_text(), "ab");

        // Both deletions should accumulate.
        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "ab\ncd");
    }

    #[test]
    fn handles_yank_in_middle_of_text() {
        let mut editor = editor();

        editor.set_text("word");
        editor.handle_input("\x17"); // Ctrl+W - deletes "word"
        editor.set_text("hello world");

        // Move to middle (after "hello ").
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..6 {
            editor.handle_input("\x1b[C");
        }

        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "hello wordworld");
    }

    #[test]
    fn handles_yank_pop_in_middle_of_text() {
        let mut editor = editor();

        // Create two kill ring entries.
        editor.set_text("FIRST");
        editor.handle_input("\x17"); // Ctrl+W - deletes "FIRST"
        editor.set_text("SECOND");
        editor.handle_input("\x17"); // Ctrl+W - deletes "SECOND"

        // Ring: ["FIRST", "SECOND"].

        // Set up "hello world" and position cursor after "hello ".
        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start of line
        for _ in 0..6 {
            editor.handle_input("\x1b[C"); // Move right 6
        }

        // Yank "SECOND" in the middle.
        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "hello SECONDworld");

        // Yank-pop replaces "SECOND" with "FIRST".
        editor.handle_input("\x1by"); // Alt+Y
        assert_eq!(editor.get_text(), "hello FIRSTworld");
    }

    #[test]
    fn multiline_yank_and_yank_pop_in_middle_of_text() {
        let mut editor = editor();

        // Create single-line entry.
        editor.set_text("SINGLE");
        editor.handle_input("\x17"); // Ctrl+W - deletes "SINGLE"

        // Create multiline entry via consecutive Ctrl+U.
        editor.set_text("A\nB");
        editor.handle_input("\x15"); // Ctrl+U - deletes "B"
        editor.handle_input("\x15"); // Ctrl+U - deletes newline
        editor.handle_input("\x15"); // Ctrl+U - deletes "A"
                                     // Ring: ["SINGLE", "A\nB"].

        // Insert in middle of "hello world".
        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..6 {
            editor.handle_input("\x1b[C");
        }

        // Yank multiline "A\nB".
        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "hello A\nBworld");

        // Yank-pop replaces with "SINGLE".
        editor.handle_input("\x1by"); // Alt+Y
        assert_eq!(editor.get_text(), "hello SINGLEworld");
    }

    #[test]
    fn alt_d_deletes_word_forward_and_saves_to_kill_ring() {
        let mut editor = editor();

        editor.set_text("hello world test");
        editor.handle_input("\x01"); // Ctrl+A - go to start

        editor.handle_input("\x1bd"); // Alt+D - deletes "hello"
        assert_eq!(editor.get_text(), " world test");

        editor.handle_input("\x1bd"); // Alt+D - deletes " world" (skips whitespace, then word)
        assert_eq!(editor.get_text(), " test");

        // Yank should get accumulated text.
        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "hello world test");
    }

    #[test]
    fn alt_d_at_end_of_line_deletes_newline() {
        let mut editor = editor();

        editor.set_text("line1\nline2");
        // Move to start of document, then to end of first line.
        editor.handle_input("\x1b[A"); // Up arrow - go to first line
        editor.handle_input("\x05"); // Ctrl+E - end of line

        editor.handle_input("\x1bd"); // Alt+D - deletes newline (merges lines)
        assert_eq!(editor.get_text(), "line1line2");

        editor.handle_input("\x19"); // Ctrl+Y
        assert_eq!(editor.get_text(), "line1\nline2");
    }

    // --- Undo (editor.test.ts:1580-2115) -----------------------------------

    #[test]
    fn undo_does_nothing_when_undo_stack_is_empty() {
        let mut editor = editor();

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "");
    }

    #[test]
    fn undo_coalesces_consecutive_word_characters_into_one_undo_unit() {
        let mut editor = editor();

        type_text(&mut editor, "hello world");
        assert_eq!(editor.get_text(), "hello world");

        // Undo removes " world" (space captured state before it, so we
        // restore to "hello").
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello");

        // Undo removes "hello".
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "");
    }

    #[test]
    fn undo_undoes_spaces_one_at_a_time() {
        let mut editor = editor();

        type_text(&mut editor, "hello  ");
        assert_eq!(editor.get_text(), "hello  ");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo) - removes second " "
        assert_eq!(editor.get_text(), "hello ");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo) - removes first " "
        assert_eq!(editor.get_text(), "hello");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo) - removes "hello"
        assert_eq!(editor.get_text(), "");
    }

    #[test]
    fn undo_undoes_newlines_and_signals_next_word_to_capture_state() {
        let mut editor = editor();

        type_text(&mut editor, "hello\nworld");
        assert_eq!(editor.get_text(), "hello\nworld");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello\n");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "");
    }

    #[test]
    fn undo_undoes_backspace() {
        let mut editor = editor();

        type_text(&mut editor, "hello");
        editor.handle_input("\x7f"); // Backspace
        assert_eq!(editor.get_text(), "hell");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello");
    }

    #[test]
    fn undo_undoes_forward_delete() {
        let mut editor = editor();

        type_text(&mut editor, "hello");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        editor.handle_input("\x1b[C"); // Right arrow
        editor.handle_input("\x1b[3~"); // Delete key
        assert_eq!(editor.get_text(), "hllo");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello");
    }

    #[test]
    fn undo_undoes_ctrl_w() {
        let mut editor = editor();

        type_text(&mut editor, "hello world");
        assert_eq!(editor.get_text(), "hello world");

        editor.handle_input("\x17"); // Ctrl+W
        assert_eq!(editor.get_text(), "hello ");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello world");
    }

    #[test]
    fn undo_undoes_ctrl_k() {
        let mut editor = editor();

        type_text(&mut editor, "hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        for _ in 0..6 {
            editor.handle_input("\x1b[C"); // Move right 6
        }

        editor.handle_input("\x0b"); // Ctrl+K
        assert_eq!(editor.get_text(), "hello ");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello world");

        editor.handle_input("|");
        assert_eq!(editor.get_text(), "hello |world");
    }

    #[test]
    fn undo_undoes_ctrl_u() {
        let mut editor = editor();

        type_text(&mut editor, "hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        for _ in 0..6 {
            editor.handle_input("\x1b[C"); // Move right 6
        }

        editor.handle_input("\x15"); // Ctrl+U
        assert_eq!(editor.get_text(), "world");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello world");
    }

    #[test]
    fn undo_undoes_yank() {
        let mut editor = editor();

        type_text(&mut editor, "hello ");
        editor.handle_input("\x17"); // Ctrl+W - delete "hello "
        editor.handle_input("\x19"); // Ctrl+Y - yank
        assert_eq!(editor.get_text(), "hello ");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "");
    }

    #[test]
    fn undo_undoes_single_line_paste_atomically() {
        let mut editor = editor();

        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        for _ in 0..5 {
            editor.handle_input("\x1b[C"); // Move right 5 (after "hello", before space)
        }

        // Simulate bracketed paste of "beep boop".
        editor.handle_input("\x1b[200~beep boop\x1b[201~");
        assert_eq!(editor.get_text(), "hellobeep boop world");

        // Single undo should restore entire pre-paste state.
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello world");

        editor.handle_input("|");
        assert_eq!(editor.get_text(), "hello| world");
    }

    #[test]
    fn does_not_trigger_autocomplete_during_single_line_paste() {
        let mut editor = editor();
        let suggestion_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&suggestion_calls);

        let provider = MockProvider::new(move |_lines, _line, _col, _force| {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        });
        editor.set_autocomplete_provider(Arc::new(provider));
        editor.handle_input("\x1b[200~look at @node_modules/react/index.js please\x1b[201~");

        assert_eq!(
            editor.get_text(),
            "look at @node_modules/react/index.js please"
        );
        assert_eq!(suggestion_calls.load(Ordering::SeqCst), 0);
        assert!(!editor.is_showing_autocomplete());
    }

    #[test]
    fn decodes_csi_u_ctrl_letter_sequences_inside_bracketed_paste() {
        let mut editor = editor();

        // tmux popups with extended-keys-format=csi-u re-encode \n in pastes
        // as \x1b[106;5u (Ctrl+J). Without decoding, the per-char filter
        // strips ESC and leaks "[106;5u" between lines (issue #3599).
        editor.handle_input("\x1b[200~line1\x1b[106;5uline2\x1b[106;5uline3\x1b[201~");
        assert_eq!(editor.get_text(), "line1\nline2\nline3");
    }

    #[test]
    fn undo_undoes_multi_line_paste_atomically() {
        let mut editor = editor();

        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        for _ in 0..5 {
            editor.handle_input("\x1b[C"); // Move right 5
        }

        // Simulate bracketed paste of multi-line text.
        editor.handle_input("\x1b[200~line1\nline2\nline3\x1b[201~");
        assert_eq!(editor.get_text(), "helloline1\nline2\nline3 world");

        // Single undo should restore entire pre-paste state.
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello world");

        editor.handle_input("|");
        assert_eq!(editor.get_text(), "hello| world");
    }

    #[test]
    fn undo_undoes_insert_text_at_cursor_atomically() {
        let mut editor = editor();

        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        for _ in 0..5 {
            editor.handle_input("\x1b[C"); // Move right 5
        }

        // Programmatic insertion (e.g., clipboard image path).
        editor.insert_text_at_cursor("/tmp/image.png");
        assert_eq!(editor.get_text(), "hello/tmp/image.png world");

        // Single undo should restore entire pre-insert state.
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello world");

        editor.handle_input("|");
        assert_eq!(editor.get_text(), "hello| world");
    }

    #[test]
    fn insert_text_at_cursor_handles_multiline_text() {
        let mut editor = editor();

        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        for _ in 0..5 {
            editor.handle_input("\x1b[C"); // Move right 5
        }

        // Insert multiline text.
        editor.insert_text_at_cursor("line1\nline2\nline3");
        assert_eq!(editor.get_text(), "helloline1\nline2\nline3 world");

        // Cursor should be at end of inserted text (after "line3", before
        // " world").
        assert_cursor(&editor, 2, 5); // "line3".length

        // Single undo should restore entire pre-insert state.
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello world");
    }

    #[test]
    fn insert_text_at_cursor_normalizes_crlf_and_cr_line_endings() {
        let mut editor = editor();

        editor.set_text("");

        // Insert text with CRLF.
        editor.insert_text_at_cursor("a\r\nb\r\nc");
        assert_eq!(editor.get_text(), "a\nb\nc");

        editor.handle_input("\x1b[45;5u"); // Undo
        assert_eq!(editor.get_text(), "");

        // Insert text with CR only.
        editor.insert_text_at_cursor("x\ry\rz");
        assert_eq!(editor.get_text(), "x\ny\nz");
    }

    #[test]
    fn undo_undoes_set_text_to_empty_string() {
        let mut editor = editor();

        type_text(&mut editor, "hello world");
        assert_eq!(editor.get_text(), "hello world");

        editor.set_text("");
        assert_eq!(editor.get_text(), "");

        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello world");
    }

    #[test]
    fn clears_undo_stack_on_submit() {
        let mut editor = editor();
        let submitted = Arc::new(std::sync::Mutex::new(String::new()));
        let captured = Arc::clone(&submitted);
        editor.on_submit = Some(Box::new(move |text| {
            *captured.lock().unwrap() = text.to_string();
        }));

        type_text(&mut editor, "hello");
        editor.handle_input("\r"); // Enter - submit

        assert_eq!(*submitted.lock().unwrap(), "hello");
        assert_eq!(editor.get_text(), "");

        // Undo should do nothing - stack was cleared.
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "");
    }

    #[test]
    fn exits_history_browsing_mode_on_undo() {
        let mut editor = editor();

        // Add "hello" to history.
        editor.add_to_history("hello");
        assert_eq!(editor.get_text(), "");

        // Type "world".
        type_text(&mut editor, "world");
        assert_eq!(editor.get_text(), "world");

        // Ctrl+W - delete word.
        editor.handle_input("\x17"); // Ctrl+W
        assert_eq!(editor.get_text(), "");

        // Press Up - enter history browsing, shows "hello".
        editor.handle_input("\x1b[A"); // Up arrow
        assert_eq!(editor.get_text(), "hello");

        // Undo should restore to "" (state before entering history browsing).
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "");

        // Undo again should restore to "world" (state before Ctrl+W).
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "world");
    }

    #[test]
    fn undo_restores_to_pre_history_state_even_after_multiple_history_navigations() {
        let mut editor = editor();

        // Add history entries.
        editor.add_to_history("first");
        editor.add_to_history("second");
        editor.add_to_history("third");

        // Type something.
        type_text(&mut editor, "current");
        assert_eq!(editor.get_text(), "current");

        // Clear editor.
        editor.handle_input("\x17"); // Ctrl+W
        assert_eq!(editor.get_text(), "");

        // Navigate through history multiple times.
        editor.handle_input("\x1b[A"); // Up - "third"
        assert_eq!(editor.get_text(), "third");
        editor.handle_input("\x1b[A"); // Up - "second"
        assert_eq!(editor.get_text(), "second");
        editor.handle_input("\x1b[A"); // Up - "first"
        assert_eq!(editor.get_text(), "first");

        // Undo should go back to "" (state before we started browsing), not
        // intermediate states.
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "");

        // Another undo goes back to "current".
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "current");
    }

    #[test]
    fn cursor_movement_starts_new_undo_unit() {
        let mut editor = editor();

        type_text(&mut editor, "hello world");
        assert_eq!(editor.get_text(), "hello world");

        // Move cursor left 5 (to after "hello ").
        for _ in 0..5 {
            editor.handle_input("\x1b[D");
        }

        // Type "lol" in the middle.
        type_text(&mut editor, "lol");
        assert_eq!(editor.get_text(), "hello lolworld");

        // Undo should restore to "hello world" (before inserting "lol").
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello world");

        editor.handle_input("|");
        assert_eq!(editor.get_text(), "hello |world");
    }

    #[test]
    fn no_op_delete_operations_do_not_push_undo_snapshots() {
        let mut editor = editor();

        type_text(&mut editor, "hello");
        assert_eq!(editor.get_text(), "hello");

        // Delete word on empty - multiple times (should be no-ops).
        editor.handle_input("\x17"); // Ctrl+W - deletes "hello"
        assert_eq!(editor.get_text(), "");
        editor.handle_input("\x17"); // Ctrl+W - no-op (nothing to delete)
        editor.handle_input("\x17"); // Ctrl+W - no-op

        // Single undo should restore "hello".
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "hello");
    }

    #[test]
    fn undo_undoes_autocomplete() {
        let mut editor = editor();

        let provider = MockProvider::new(|lines, _cursor_line, cursor_col, _force| {
            let text = lines.first().cloned().unwrap_or_default();
            let prefix = text[..char_to_byte(&text, cursor_col)].to_string();
            if prefix == "di" {
                return Some(AutocompleteSuggestions {
                    items: vec![AutocompleteItem {
                        value: "dist/".to_string(),
                        label: "dist/".to_string(),
                        description: None,
                    }],
                    prefix: "di".to_string(),
                });
            }
            None
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        // Type "di".
        editor.handle_input("d");
        editor.handle_input("i");
        assert_eq!(editor.get_text(), "di");

        // Press Tab to trigger autocomplete.
        editor.handle_input("\t");
        flush_autocomplete(&mut editor);
        assert_eq!(editor.get_text(), "dist/");
        assert!(!editor.is_showing_autocomplete());

        // Undo should restore to "di".
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "di");
    }

    // --- Autocomplete (editor.test.ts:2117-2847) ---------------------------

    #[test]
    fn auto_applies_single_force_file_suggestion_without_showing_menu() {
        let mut editor = editor();

        let provider = MockProvider::new(|lines, _cursor_line, cursor_col, force| {
            if !force {
                return None;
            }
            let text = lines.first().cloned().unwrap_or_default();
            let prefix = text[..char_to_byte(&text, cursor_col)].to_string();
            if prefix == "Work" {
                return Some(AutocompleteSuggestions {
                    items: vec![AutocompleteItem {
                        value: "Workspace/".to_string(),
                        label: "Workspace/".to_string(),
                        description: None,
                    }],
                    prefix: "Work".to_string(),
                });
            }
            None
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        // Type "Work".
        type_text(&mut editor, "Work");
        assert_eq!(editor.get_text(), "Work");

        // Press Tab - should auto-apply without showing menu.
        editor.handle_input("\t");
        flush_autocomplete(&mut editor);
        assert_eq!(editor.get_text(), "Workspace/");
        assert!(!editor.is_showing_autocomplete());

        // Undo should restore to "Work".
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "Work");
    }

    #[test]
    fn shows_menu_when_force_file_has_multiple_suggestions() {
        let mut editor = editor();

        let provider = MockProvider::new(|lines, _cursor_line, cursor_col, force| {
            if !force {
                return None;
            }
            let text = lines.first().cloned().unwrap_or_default();
            let prefix = text[..char_to_byte(&text, cursor_col)].to_string();
            if prefix == "src" {
                return Some(AutocompleteSuggestions {
                    items: vec![
                        AutocompleteItem {
                            value: "src/".to_string(),
                            label: "src/".to_string(),
                            description: None,
                        },
                        AutocompleteItem {
                            value: "src.txt".to_string(),
                            label: "src.txt".to_string(),
                            description: None,
                        },
                    ],
                    prefix: "src".to_string(),
                });
            }
            None
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        // Type "src".
        type_text(&mut editor, "src");
        assert_eq!(editor.get_text(), "src");

        // Press Tab - should show menu because there are multiple suggestions.
        editor.handle_input("\t");
        flush_autocomplete(&mut editor);
        assert_eq!(editor.get_text(), "src");
        assert!(editor.is_showing_autocomplete());

        // Press Tab again to accept first suggestion.
        editor.handle_input("\t");
        assert_eq!(editor.get_text(), "src/");
        assert!(!editor.is_showing_autocomplete());
    }

    #[test]
    fn keeps_suggestions_open_when_typing_in_force_mode() {
        let mut editor = editor();

        let all_files = [
            ("readme.md", "readme.md"),
            ("package.json", "package.json"),
            ("src/", "src/"),
            ("dist/", "dist/"),
        ];
        let provider = MockProvider::new(move |lines, _cursor_line, cursor_col, force| {
            let text = lines.first().cloned().unwrap_or_default();
            let prefix = text[..char_to_byte(&text, cursor_col)].to_string();
            let should_match = force || prefix.contains('/') || prefix.starts_with('.');
            if !should_match {
                return None;
            }
            let filtered: Vec<AutocompleteItem> = all_files
                .iter()
                .filter(|(value, _)| value.to_lowercase().starts_with(&prefix.to_lowercase()))
                .map(|(value, label)| AutocompleteItem {
                    value: value.to_string(),
                    label: label.to_string(),
                    description: None,
                })
                .collect();
            if filtered.is_empty() {
                return None;
            }
            Some(AutocompleteSuggestions {
                items: filtered,
                prefix,
            })
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        // Press Tab on empty prompt - should show all files (force mode).
        editor.handle_input("\t");
        flush_autocomplete(&mut editor);
        assert!(editor.is_showing_autocomplete());

        // Type "r" - should narrow to "readme.md" (force mode keeps
        // suggestions open).
        editor.handle_input("r");
        flush_autocomplete(&mut editor);
        assert_eq!(editor.get_text(), "r");
        assert!(editor.is_showing_autocomplete());

        // Type "e" - should still show "readme.md".
        editor.handle_input("e");
        flush_autocomplete(&mut editor);
        assert_eq!(editor.get_text(), "re");
        assert!(editor.is_showing_autocomplete());

        // Accept with Tab.
        editor.handle_input("\t");
        assert_eq!(editor.get_text(), "readme.md");
        assert!(!editor.is_showing_autocomplete());
    }

    #[test]
    fn debounces_at_autocomplete_while_typing() {
        let mut editor = editor();
        let suggestion_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&suggestion_calls);

        let provider = MockProvider::new(move |lines, _cursor_line, cursor_col, _force| {
            calls.fetch_add(1, Ordering::SeqCst);
            let text = lines.first().cloned().unwrap_or_default();
            let prefix = text[..char_to_byte(&text, cursor_col)].to_string();
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "@main.ts".to_string(),
                    label: "main.ts".to_string(),
                    description: None,
                }],
                prefix,
            })
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        type_text(&mut editor, "@mai");

        assert_eq!(suggestion_calls.load(Ordering::SeqCst), 0);
        assert!(!editor.is_showing_autocomplete());

        std::thread::sleep(Duration::from_millis(50));
        flush_autocomplete(&mut editor);

        assert_eq!(suggestion_calls.load(Ordering::SeqCst), 1);
        assert!(editor.is_showing_autocomplete());
    }

    #[test]
    fn re_queries_the_autocomplete_picker_when_the_cursor_moves_back_into_the_command_name() {
        // Regression for earendil-works/pi#5496: arrowing left out of a
        // slash command's argument region must re-query the picker, not
        // leave the stale argument list showing.
        let mut editor = editor();

        let provider = MockProvider::new(|lines, _cursor_line, cursor_col, _force| {
            let before = {
                let text = lines.first().cloned().unwrap_or_default();
                text[..char_to_byte(&text, cursor_col)].to_string()
            };
            if !before.starts_with('/') {
                return None;
            }
            // Past the command name (a space before the cursor): offer
            // arguments.
            if before.contains(' ') {
                return Some(AutocompleteSuggestions {
                    items: vec![
                        AutocompleteItem {
                            value: "repo".to_string(),
                            label: "repo".to_string(),
                            description: None,
                        },
                        AutocompleteItem {
                            value: "message".to_string(),
                            label: "message".to_string(),
                            description: None,
                        },
                        AutocompleteItem {
                            value: "help".to_string(),
                            label: "help".to_string(),
                            description: None,
                        },
                    ],
                    prefix: before[before.find(' ').expect("space checked") + 1..].to_string(),
                });
            }
            // Inside the command name: offer the command name only.
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "cmd".to_string(),
                    label: "cmd".to_string(),
                    description: None,
                }],
                prefix: before,
            })
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        // Type `/cmd ` so the picker ends up showing the argument list.
        for ch in "/cmd ".chars() {
            editor.handle_input(&ch.to_string());
            flush_autocomplete(&mut editor);
        }
        assert_eq!(editor.get_text(), "/cmd ");
        assert!(editor.is_showing_autocomplete());
        let at_arg = editor
            .render(80)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            at_arg.contains("repo"),
            "argument menu should be visible at `/cmd `"
        );

        // Arrow Left back into the command name (`/cmd`).
        editor.handle_input("\x1b[D");
        flush_autocomplete(&mut editor);

        // The picker must have re-queried: the stale argument items are gone
        // (replaced by the command-name suggestion, or the picker closed).
        let after_move = editor
            .render(80)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !after_move.contains("repo"),
            "stale argument menu must not survive the cursor move"
        );
        assert!(
            !after_move.contains("message"),
            "stale argument menu must not survive the cursor move"
        );
    }

    #[test]
    fn debounces_hash_autocomplete_while_typing() {
        let mut editor = editor();
        let suggestion_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&suggestion_calls);

        let provider = MockProvider::new(move |lines, _cursor_line, cursor_col, _force| {
            calls.fetch_add(1, Ordering::SeqCst);
            let text = lines.first().cloned().unwrap_or_default();
            let prefix = text[..char_to_byte(&text, cursor_col)].to_string();
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "#2983".to_string(),
                    label: "#2983".to_string(),
                    description: None,
                }],
                prefix,
            })
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        type_text(&mut editor, "#298");

        assert_eq!(suggestion_calls.load(Ordering::SeqCst), 0);
        assert!(!editor.is_showing_autocomplete());

        std::thread::sleep(Duration::from_millis(50));
        flush_autocomplete(&mut editor);

        assert_eq!(suggestion_calls.load(Ordering::SeqCst), 1);
        assert!(editor.is_showing_autocomplete());
    }

    #[test]
    fn debounces_custom_trigger_characters_autocomplete_while_typing() {
        let mut editor = editor();
        let suggestion_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&suggestion_calls);

        let provider = MockProvider::new(move |lines, _cursor_line, cursor_col, _force| {
            calls.fetch_add(1, Ordering::SeqCst);
            let prefix = {
                let text = lines.first().cloned().unwrap_or_default();
                text[..char_to_byte(&text, cursor_col)].to_string()
            };
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "$skill-name".to_string(),
                    label: "skill-name".to_string(),
                    description: None,
                }],
                prefix,
            })
        })
        .with_trigger(&['$']);
        editor.set_autocomplete_provider(Arc::new(provider));

        type_text(&mut editor, "$sk");

        assert_eq!(suggestion_calls.load(Ordering::SeqCst), 0);
        std::thread::sleep(Duration::from_millis(50));
        flush_autocomplete(&mut editor);

        assert_eq!(suggestion_calls.load(Ordering::SeqCst), 1);
        assert!(editor.is_showing_autocomplete());
    }

    #[test]
    fn resets_custom_trigger_characters_when_provider_changes() {
        let mut editor = editor();
        let suggestion_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&suggestion_calls);

        let first = MockProvider::new(|_lines, _line, _col, _force| {
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "$skill-name".to_string(),
                    label: "skill-name".to_string(),
                    description: None,
                }],
                prefix: "$".to_string(),
            })
        })
        .with_trigger(&['$']);
        editor.set_autocomplete_provider(Arc::new(first));
        let second = MockProvider::new(move |_lines, _line, _col, _force| {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "$skill-name".to_string(),
                    label: "skill-name".to_string(),
                    description: None,
                }],
                prefix: "$".to_string(),
            })
        });
        editor.set_autocomplete_provider(Arc::new(second));

        type_text(&mut editor, "$s");
        std::thread::sleep(Duration::from_millis(50));
        flush_autocomplete(&mut editor);

        assert_eq!(suggestion_calls.load(Ordering::SeqCst), 0);
        assert!(!editor.is_showing_autocomplete());
    }

    /// Provider that resolves after ~500ms unless its abort flag is set —
    /// the sync-port equivalent of the upstream abort-listening mock
    /// (editor.test.ts:2424-2438).
    struct AbortObservingProvider {
        aborts: Arc<AtomicUsize>,
    }

    impl AutocompleteProvider for AbortObservingProvider {
        fn trigger_characters(&self) -> &[char] {
            &[]
        }

        fn get_suggestions(
            &self,
            _lines: &[String],
            _cursor_line: usize,
            _cursor_col: usize,
            options: &GetSuggestionsOptions,
        ) -> Option<AutocompleteSuggestions> {
            for _ in 0..100 {
                if options.abort.load(Ordering::SeqCst) {
                    self.aborts.fetch_add(1, Ordering::SeqCst);
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Some(AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "@main.ts".to_string(),
                    label: "main.ts".to_string(),
                    description: None,
                }],
                prefix: "@main".to_string(),
            })
        }

        fn apply_completion(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> CompletionResult {
            mock_apply_completion(lines, cursor_line, cursor_col, item, prefix)
        }
    }

    #[test]
    fn aborts_active_at_autocomplete_when_typing_continues() {
        let mut editor = editor();
        let aborts = Arc::new(AtomicUsize::new(0));

        editor.set_autocomplete_provider(Arc::new(AbortObservingProvider {
            aborts: Arc::clone(&aborts),
        }));

        type_text(&mut editor, "@mai");
        std::thread::sleep(Duration::from_millis(250));
        editor.handle_input("n");
        std::thread::sleep(Duration::from_millis(50));

        assert_eq!(aborts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn hides_autocomplete_when_backspacing_slash_command_to_empty() {
        let mut editor = editor();

        let provider = MockProvider::new(|lines, _cursor_line, cursor_col, _force| {
            let text = lines.first().cloned().unwrap_or_default();
            let prefix = text[..char_to_byte(&text, cursor_col)].to_string();
            // Only return slash command suggestions when line starts with /.
            if prefix.starts_with('/') {
                let commands = [
                    ("/model", "model", "Change model"),
                    ("/help", "help", "Show help"),
                ];
                let query = prefix.strip_prefix('/').expect("prefix starts with /");
                let filtered: Vec<AutocompleteItem> = commands
                    .iter()
                    .filter(|(value, _, _)| value.starts_with(query))
                    .map(|(value, label, description)| AutocompleteItem {
                        value: value.to_string(),
                        label: label.to_string(),
                        description: Some(description.to_string()),
                    })
                    .collect();
                if !filtered.is_empty() {
                    return Some(AutocompleteSuggestions {
                        items: filtered,
                        prefix,
                    });
                }
            }
            None
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        // Type "/" - should show slash command suggestions.
        editor.handle_input("/");
        flush_autocomplete(&mut editor);
        assert_eq!(editor.get_text(), "/");
        assert!(editor.is_showing_autocomplete());

        // Backspace to delete "/" - should hide autocomplete completely.
        editor.handle_input("\x7f"); // Backspace
        flush_autocomplete(&mut editor);
        assert_eq!(editor.get_text(), "");
        assert!(!editor.is_showing_autocomplete());
    }

    #[test]
    fn applies_exact_typed_slash_argument_value_on_enter_even_when_first_item_is_highlighted() {
        let mut editor = editor();

        let provider = MockProvider::new(|lines, _cursor_line, cursor_col, _force| {
            let text = lines.first().cloned().unwrap_or_default();
            let before_cursor = text[..char_to_byte(&text, cursor_col)].to_string();

            // Check if we're in argument completion context: "/argtest <prefix>".
            let rest = before_cursor.strip_prefix("/argtest ");
            if let Some(argument_text) = rest {
                if argument_text.contains(' ') {
                    return None;
                }
                let all_arguments = ["one", "two", "three"];
                let filtered: Vec<AutocompleteItem> = all_arguments
                    .iter()
                    .filter(|arg| arg.starts_with(argument_text))
                    .map(|arg| AutocompleteItem {
                        value: arg.to_string(),
                        label: arg.to_string(),
                        description: None,
                    })
                    .collect();
                if !filtered.is_empty() {
                    return Some(AutocompleteSuggestions {
                        items: filtered,
                        prefix: argument_text.to_string(),
                    });
                }
            }
            None
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        // Type "/argtest two".
        type_text(&mut editor, "/argtest two");

        assert_eq!(editor.get_text(), "/argtest two");
        flush_autocomplete(&mut editor);
        assert!(editor.is_showing_autocomplete());

        // Press Enter - should apply the exact typed value "two", not the
        // first item.
        editor.handle_input("\r");

        // The exact typed value "two" should be retained.
        assert_eq!(editor.get_text(), "/argtest two");
    }

    #[test]
    fn selects_first_prefix_match_on_enter_when_typed_arg_is_not_exact_match() {
        let mut editor = editor();

        let provider = MockProvider::new(|lines, _cursor_line, cursor_col, _force| {
            let text = lines.first().cloned().unwrap_or_default();
            let before_cursor = text[..char_to_byte(&text, cursor_col)].to_string();

            let rest = before_cursor.strip_prefix("/argtest ");
            if let Some(argument_text) = rest {
                if argument_text.contains(' ') {
                    return None;
                }
                let all_arguments = ["two", "three", "twelve"];
                let filtered: Vec<AutocompleteItem> = all_arguments
                    .iter()
                    .filter(|arg| arg.starts_with(argument_text))
                    .map(|arg| AutocompleteItem {
                        value: arg.to_string(),
                        label: arg.to_string(),
                        description: None,
                    })
                    .collect();
                if !filtered.is_empty() {
                    return Some(AutocompleteSuggestions {
                        items: filtered,
                        prefix: argument_text.to_string(),
                    });
                }
            }
            None
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        // Type "/argtest t" - filtered to [two, three, twelve], prefix "t"
        // matches "two" first.
        type_text(&mut editor, "/argtest t");

        flush_autocomplete(&mut editor);
        assert!(editor.is_showing_autocomplete());

        // Press Enter - "t" prefix matches "two" (first in list), so "two"
        // is applied.
        editor.handle_input("\r");
        assert_eq!(editor.get_text(), "/argtest two");
    }

    #[test]
    fn highlights_unique_prefix_match_as_user_types_before_full_exact_match() {
        let mut editor = editor();

        let provider = MockProvider::new(|lines, _cursor_line, cursor_col, _force| {
            let text = lines.first().cloned().unwrap_or_default();
            let before_cursor = text[..char_to_byte(&text, cursor_col)].to_string();

            let rest = before_cursor.strip_prefix("/argtest ");
            if let Some(argument_text) = rest {
                if argument_text.contains(' ') {
                    return None;
                }
                // Return all items - provider does not filter.
                let all_arguments = ["one", "two", "three"];
                return Some(AutocompleteSuggestions {
                    items: all_arguments
                        .iter()
                        .map(|arg| AutocompleteItem {
                            value: arg.to_string(),
                            label: arg.to_string(),
                            description: None,
                        })
                        .collect(),
                    prefix: argument_text.to_string(),
                });
            }
            None
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        // Type "/argtest tw" - "tw" is a prefix of only "two".
        type_text(&mut editor, "/argtest tw");

        assert_eq!(editor.get_text(), "/argtest tw");
        flush_autocomplete(&mut editor);
        assert!(editor.is_showing_autocomplete());

        // Press Enter - "tw" uniquely matches "two", so "two" should be
        // applied.
        editor.handle_input("\r");
        assert_eq!(editor.get_text(), "/argtest two");
    }

    #[test]
    fn selects_first_prefix_match_when_multiple_items_match() {
        let mut editor = editor();

        let provider = MockProvider::new(|lines, _cursor_line, cursor_col, _force| {
            let text = lines.first().cloned().unwrap_or_default();
            let before_cursor = text[..char_to_byte(&text, cursor_col)].to_string();

            let rest = before_cursor.strip_prefix("/argtest ");
            if let Some(argument_text) = rest {
                if argument_text.contains(' ') {
                    return None;
                }
                let all_arguments = ["one", "two", "three"];
                return Some(AutocompleteSuggestions {
                    items: all_arguments
                        .iter()
                        .map(|arg| AutocompleteItem {
                            value: arg.to_string(),
                            label: arg.to_string(),
                            description: None,
                        })
                        .collect(),
                    prefix: argument_text.to_string(),
                });
            }
            None
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        // Type "/argtest t" - "t" is a prefix of both "two" and "three".
        type_text(&mut editor, "/argtest t");

        flush_autocomplete(&mut editor);
        assert!(editor.is_showing_autocomplete());

        // Press Enter - "t" matches "two" first, so "two" is selected.
        editor.handle_input("\r");
        assert_eq!(editor.get_text(), "/argtest two");
    }

    #[test]
    fn works_for_built_in_style_command_argument_completion_path_model_like() {
        let mut editor = editor();

        let provider = MockProvider::new(|lines, _cursor_line, cursor_col, _force| {
            let text = lines.first().cloned().unwrap_or_default();
            let before_cursor = text[..char_to_byte(&text, cursor_col)].to_string();

            // Check if we're in /model argument completion context.
            let rest = before_cursor.strip_prefix("/model ");
            if let Some(model_text) = rest {
                if model_text.contains(' ') {
                    return None;
                }
                let all_models = ["gpt-4o", "gpt-4o-mini", "claude-sonnet"];
                let filtered: Vec<AutocompleteItem> = all_models
                    .iter()
                    .filter(|model| model.starts_with(model_text))
                    .map(|model| AutocompleteItem {
                        value: model.to_string(),
                        label: model.to_string(),
                        description: None,
                    })
                    .collect();
                if !filtered.is_empty() {
                    return Some(AutocompleteSuggestions {
                        items: filtered,
                        prefix: model_text.to_string(),
                    });
                }
            }
            None
        });
        editor.set_autocomplete_provider(Arc::new(provider));

        // Type "/model gpt-4o-mini" - exact match for second item in list.
        type_text(&mut editor, "/model gpt-4o-mini");

        assert_eq!(editor.get_text(), "/model gpt-4o-mini");
        flush_autocomplete(&mut editor);
        assert!(editor.is_showing_autocomplete());

        // Press Enter - should retain exact typed value, not apply first
        // highlighted item.
        editor.handle_input("\r");

        // The exact typed value should be retained.
        assert_eq!(editor.get_text(), "/model gpt-4o-mini");
    }

    #[test]
    fn awaits_slash_command_argument_completions() {
        let mut editor = editor();
        let provider = crate::autocomplete::CombinedAutocompleteProvider::new(
            vec![SlashCommandOrItem::Command(SlashCommand {
                name: "load-skills".to_string(),
                description: Some("Load skills".to_string()),
                argument_hint: None,
                get_argument_completions: Some(Box::new(|prefix: &str| {
                    if prefix.starts_with('s') {
                        Some(vec![AutocompleteItem {
                            value: "skill-a".to_string(),
                            label: "skill-a".to_string(),
                            description: None,
                        }])
                    } else {
                        None
                    }
                })),
            })],
            std::env::current_dir()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            None,
        );
        editor.set_autocomplete_provider(Arc::new(provider));
        editor.set_text("/load-skills ");

        editor.handle_input("s");
        flush_autocomplete(&mut editor);
        assert!(editor.is_showing_autocomplete());

        editor.handle_input("\t");
        assert_eq!(editor.get_text(), "/load-skills skill-a");
        assert!(!editor.is_showing_autocomplete());
    }

    #[test]
    fn ignores_invalid_slash_command_argument_completion_results() {
        // Upstream's non-array result (`"not-an-array"`) is unrepresentable
        // in Rust; the ported contract is "no suggestions" — covered by a
        // completer returning `None`.
        let mut editor = editor();
        let provider = crate::autocomplete::CombinedAutocompleteProvider::new(
            vec![SlashCommandOrItem::Command(SlashCommand {
                name: "load-skills".to_string(),
                description: Some("Load skills".to_string()),
                argument_hint: None,
                get_argument_completions: Some(Box::new(|_prefix: &str| None)),
            })],
            std::env::current_dir()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            None,
        );
        editor.set_autocomplete_provider(Arc::new(provider));
        editor.set_text("/load-skills ");

        editor.handle_input("s");
        flush_autocomplete(&mut editor);
        assert!(!editor.is_showing_autocomplete());
        assert_eq!(editor.get_text(), "/load-skills s");
    }

    #[test]
    fn does_not_show_argument_completions_when_command_has_no_argument_completer() {
        let mut editor = editor();
        let provider = crate::autocomplete::CombinedAutocompleteProvider::new(
            vec![
                SlashCommandOrItem::Command(SlashCommand {
                    name: "help".to_string(),
                    description: Some("Show help".to_string()),
                    argument_hint: None,
                    get_argument_completions: None,
                }),
                SlashCommandOrItem::Command(SlashCommand {
                    name: "model".to_string(),
                    description: Some("Switch model".to_string()),
                    argument_hint: None,
                    get_argument_completions: Some(Box::new(|_prefix: &str| {
                        Some(vec![AutocompleteItem {
                            value: "claude-opus".to_string(),
                            label: "claude-opus".to_string(),
                            description: None,
                        }])
                    })),
                }),
            ],
            std::env::current_dir()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            None,
        );
        editor.set_autocomplete_provider(Arc::new(provider));

        editor.handle_input("/");
        editor.handle_input("h");
        editor.handle_input("e");
        flush_autocomplete(&mut editor);
        assert!(editor.is_showing_autocomplete());

        editor.handle_input("\t");
        assert_eq!(editor.get_text(), "/help ");
        assert!(!editor.is_showing_autocomplete());
    }

    // --- Character jump Ctrl+] (editor.test.ts:2849-3068) ------------------

    #[test]
    fn jumps_forward_to_first_occurrence_of_character_on_same_line() {
        let mut editor = editor();

        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        assert_cursor(&editor, 0, 0);

        editor.handle_input("\x1d"); // Ctrl+] (legacy sequence for ctrl+])
        editor.handle_input("o"); // Jump to first 'o'

        assert_cursor(&editor, 0, 4); // 'o' in "hello"
    }

    #[test]
    fn jumps_forward_to_next_occurrence_after_cursor() {
        let mut editor = editor();

        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start
                                     // Move cursor to the 'o' in "hello" (col 4).
        for _ in 0..4 {
            editor.handle_input("\x1b[C");
        }
        assert_cursor(&editor, 0, 4);

        editor.handle_input("\x1d"); // Ctrl+]
        editor.handle_input("o"); // Jump to next 'o' (in "world")

        assert_cursor(&editor, 0, 7); // 'o' in "world"
    }

    #[test]
    fn jumps_forward_across_multiple_lines() {
        let mut editor = editor();

        editor.set_text("abc\ndef\nghi");
        // Cursor is at end (line 2, col 3). Move to line 0 via up arrows,
        // then Ctrl+A.
        editor.handle_input("\x1b[A"); // Up
        editor.handle_input("\x1b[A"); // Up - now on line 0
        editor.handle_input("\x01"); // Ctrl+A - go to start of line
        assert_cursor(&editor, 0, 0);

        editor.handle_input("\x1d"); // Ctrl+]
        editor.handle_input("g"); // Jump to 'g' on line 3

        assert_cursor(&editor, 2, 0);
    }

    #[test]
    fn jumps_backward_to_first_occurrence_before_cursor_on_same_line() {
        let mut editor = editor();

        editor.set_text("hello world");
        // Cursor at end (col 11).
        assert_cursor(&editor, 0, 11);

        editor.handle_input("\x1b\x1d"); // Ctrl+Alt+] (ESC followed by Ctrl+])
        editor.handle_input("o"); // Jump to last 'o' before cursor

        assert_cursor(&editor, 0, 7); // 'o' in "world"
    }

    #[test]
    fn jumps_backward_across_multiple_lines() {
        let mut editor = editor();

        editor.set_text("abc\ndef\nghi");
        // Cursor at end of line 3.
        assert_cursor(&editor, 2, 3);

        editor.handle_input("\x1b\x1d"); // Ctrl+Alt+]
        editor.handle_input("a"); // Jump to 'a' on line 1

        assert_cursor(&editor, 0, 0);
    }

    #[test]
    fn does_nothing_when_character_is_not_found_forward() {
        let mut editor = editor();

        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        assert_cursor(&editor, 0, 0);

        editor.handle_input("\x1d"); // Ctrl+]
        editor.handle_input("z"); // 'z' doesn't exist

        assert_cursor(&editor, 0, 0); // Cursor unchanged
    }

    #[test]
    fn does_nothing_when_character_is_not_found_backward() {
        let mut editor = editor();

        editor.set_text("hello world");
        // Cursor at end.
        assert_cursor(&editor, 0, 11);

        editor.handle_input("\x1b\x1d"); // Ctrl+Alt+]
        editor.handle_input("z"); // 'z' doesn't exist

        assert_cursor(&editor, 0, 11); // Cursor unchanged
    }

    #[test]
    fn jump_is_case_sensitive() {
        let mut editor = editor();

        editor.set_text("Hello World");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        assert_cursor(&editor, 0, 0);

        // Search for lowercase 'h' - should not find it (only 'H' exists).
        editor.handle_input("\x1d"); // Ctrl+]
        editor.handle_input("h");

        assert_cursor(&editor, 0, 0); // Cursor unchanged

        // Search for uppercase 'W' - should find it.
        editor.handle_input("\x1d"); // Ctrl+]
        editor.handle_input("W");

        assert_cursor(&editor, 0, 6); // 'W' in "World"
    }

    #[test]
    fn cancels_jump_mode_when_ctrl_bracket_is_pressed_again() {
        let mut editor = editor();

        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        assert_cursor(&editor, 0, 0);

        editor.handle_input("\x1d"); // Ctrl+] - enter jump mode
        editor.handle_input("\x1d"); // Ctrl+] again - cancel

        // Type 'o' normally - should insert, not jump.
        editor.handle_input("o");
        assert_eq!(editor.get_text(), "ohello world");
    }

    #[test]
    fn cancels_jump_mode_on_escape_and_processes_the_escape() {
        let mut editor = editor();

        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        assert_cursor(&editor, 0, 0);

        editor.handle_input("\x1d"); // Ctrl+] - enter jump mode
        editor.handle_input("\x1b"); // Escape - cancel jump mode

        // Cursor should be unchanged (Escape itself doesn't move cursor in
        // editor).
        assert_cursor(&editor, 0, 0);

        // Type 'o' normally - should insert, not jump.
        editor.handle_input("o");
        assert_eq!(editor.get_text(), "ohello world");
    }

    #[test]
    fn cancels_backward_jump_mode_when_ctrl_alt_bracket_is_pressed_again() {
        let mut editor = editor();

        editor.set_text("hello world");
        // Cursor at end.
        assert_cursor(&editor, 0, 11);

        editor.handle_input("\x1b\x1d"); // Ctrl+Alt+] - enter backward jump mode
        editor.handle_input("\x1b\x1d"); // Ctrl+Alt+] again - cancel

        // Type 'o' normally - should insert, not jump.
        editor.handle_input("o");
        assert_eq!(editor.get_text(), "hello worldo");
    }

    #[test]
    fn searches_for_special_characters() {
        let mut editor = editor();

        editor.set_text("foo(bar) = baz;");
        editor.handle_input("\x01"); // Ctrl+A - go to start
        assert_cursor(&editor, 0, 0);

        // Jump to '('.
        editor.handle_input("\x1d"); // Ctrl+]
        editor.handle_input("(");

        assert_cursor(&editor, 0, 3);

        // Jump to '='.
        editor.handle_input("\x1d"); // Ctrl+]
        editor.handle_input("=");

        assert_cursor(&editor, 0, 9);
    }

    #[test]
    fn handles_empty_text_gracefully() {
        let mut editor = editor();

        editor.set_text("");
        assert_cursor(&editor, 0, 0);

        editor.handle_input("\x1d"); // Ctrl+]
        editor.handle_input("x");

        assert_cursor(&editor, 0, 0); // Cursor unchanged
    }

    #[test]
    fn resets_last_action_when_jumping() {
        let mut editor = editor();

        editor.set_text("hello world");
        editor.handle_input("\x01"); // Ctrl+A - go to start

        // Type to set lastAction to "type-word".
        editor.handle_input("x");
        assert_eq!(editor.get_text(), "xhello world");

        // Jump forward.
        editor.handle_input("\x1d"); // Ctrl+]
        editor.handle_input("o");

        // Type more - should start a new undo unit (lastAction was reset).
        editor.handle_input("Y");
        assert_eq!(editor.get_text(), "xhellYo world");

        // Undo should only undo "Y", not "x" as well.
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "xhello world");
    }

    // --- Sticky column (editor.test.ts:3070-3570) --------------------------

    #[test]
    fn preserves_target_column_when_moving_up_through_a_shorter_line() {
        let mut editor = editor();

        // Line 0: "2222222222x222" (x at col 10)
        // Line 1: "" (empty)
        // Line 2: "1111111111_111111111111" (_ at col 10)
        editor.set_text("2222222222x222\n\n1111111111_111111111111");

        // Position cursor on _ (line 2, col 10).
        assert_cursor(&editor, 2, 23); // At end
        editor.handle_input("\x01"); // Ctrl+A - go to start of line
        for _ in 0..10 {
            editor.handle_input("\x1b[C"); // Move right to col 10
        }
        assert_cursor(&editor, 2, 10);

        // Press Up - should move to empty line (col clamped to 0).
        editor.handle_input("\x1b[A"); // Up arrow
        assert_cursor(&editor, 1, 0);

        // Press Up again - should move to line 0 at col 10 (on 'x').
        editor.handle_input("\x1b[A"); // Up arrow
        assert_cursor(&editor, 0, 10);
    }

    #[test]
    fn preserves_target_column_when_moving_down_through_a_shorter_line() {
        let mut editor = editor();

        editor.set_text("1111111111_111\n\n2222222222x222222222222");

        // Position cursor on _ (line 0, col 10).
        editor.handle_input("\x1b[A"); // Up to line 1
        editor.handle_input("\x1b[A"); // Up to line 0
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..10 {
            editor.handle_input("\x1b[C");
        }
        assert_cursor(&editor, 0, 10);

        // Press Down - should move to empty line (col clamped to 0).
        editor.handle_input("\x1b[B"); // Down arrow
        assert_cursor(&editor, 1, 0);

        // Press Down again - should move to line 2 at col 10 (on 'x').
        editor.handle_input("\x1b[B"); // Down arrow
        assert_cursor(&editor, 2, 10);
    }

    #[test]
    fn resets_sticky_column_on_horizontal_movement_left_arrow() {
        let mut editor = editor();

        editor.set_text("1234567890\n\n1234567890");

        // Start at line 2, col 5.
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..5 {
            editor.handle_input("\x1b[C");
        }
        assert_cursor(&editor, 2, 5);

        // Move up through empty line.
        editor.handle_input("\x1b[A"); // Up - line 1, col 0
        editor.handle_input("\x1b[A"); // Up - line 0, col 5 (sticky)
        assert_cursor(&editor, 0, 5);

        // Move left - resets sticky column.
        editor.handle_input("\x1b[D"); // Left
        assert_cursor(&editor, 0, 4);

        // Move down twice.
        editor.handle_input("\x1b[B"); // Down - line 1, col 0
        editor.handle_input("\x1b[B"); // Down - line 2, col 4 (new sticky from col 4)
        assert_cursor(&editor, 2, 4);
    }

    #[test]
    fn resets_sticky_column_on_horizontal_movement_right_arrow() {
        let mut editor = editor();

        editor.set_text("1234567890\n\n1234567890");

        // Start at line 0, col 5.
        editor.handle_input("\x1b[A"); // Up to line 1
        editor.handle_input("\x1b[A"); // Up to line 0
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..5 {
            editor.handle_input("\x1b[C");
        }
        assert_cursor(&editor, 0, 5);

        // Move down through empty line.
        editor.handle_input("\x1b[B"); // Down - line 1, col 0
        editor.handle_input("\x1b[B"); // Down - line 2, col 5 (sticky)
        assert_cursor(&editor, 2, 5);

        // Move right - resets sticky column.
        editor.handle_input("\x1b[C"); // Right
        assert_cursor(&editor, 2, 6);

        // Move up twice.
        editor.handle_input("\x1b[A"); // Up - line 1, col 0
        editor.handle_input("\x1b[A"); // Up - line 0, col 6 (new sticky from col 6)
        assert_cursor(&editor, 0, 6);
    }

    #[test]
    fn resets_sticky_column_on_typing() {
        let mut editor = editor();

        editor.set_text("1234567890\n\n1234567890");

        // Start at line 2, col 8.
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..8 {
            editor.handle_input("\x1b[C");
        }

        // Move up through empty line.
        editor.handle_input("\x1b[A"); // Up
        editor.handle_input("\x1b[A"); // Up - line 0, col 8
        assert_cursor(&editor, 0, 8);

        // Type a character - resets sticky column.
        editor.handle_input("X");
        assert_cursor(&editor, 0, 9);

        // Move down twice.
        editor.handle_input("\x1b[B"); // Down - line 1, col 0
        editor.handle_input("\x1b[B"); // Down - line 2, col 9 (new sticky from col 9)
        assert_cursor(&editor, 2, 9);
    }

    #[test]
    fn resets_sticky_column_on_backspace() {
        let mut editor = editor();

        editor.set_text("1234567890\n\n1234567890");

        // Start at line 2, col 8.
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..8 {
            editor.handle_input("\x1b[C");
        }

        // Move up through empty line.
        editor.handle_input("\x1b[A"); // Up
        editor.handle_input("\x1b[A"); // Up - line 0, col 8
        assert_cursor(&editor, 0, 8);

        // Backspace - resets sticky column.
        editor.handle_input("\x7f"); // Backspace
        assert_cursor(&editor, 0, 7);

        // Move down twice.
        editor.handle_input("\x1b[B"); // Down - line 1, col 0
        editor.handle_input("\x1b[B"); // Down - line 2, col 7 (new sticky from col 7)
        assert_cursor(&editor, 2, 7);
    }

    #[test]
    fn resets_sticky_column_on_ctrl_a_move_to_line_start() {
        let mut editor = editor();

        editor.set_text("1234567890\n\n1234567890");

        // Start at line 2, col 8.
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..8 {
            editor.handle_input("\x1b[C");
        }

        // Move up - establishes sticky col 8.
        editor.handle_input("\x1b[A"); // Up - line 1, col 0

        // Ctrl+A - resets sticky column to 0.
        editor.handle_input("\x01"); // Ctrl+A
        assert_cursor(&editor, 1, 0);

        // Move up.
        editor.handle_input("\x1b[A"); // Up - line 0, col 0 (new sticky from col 0)
        assert_cursor(&editor, 0, 0);
    }

    #[test]
    fn resets_sticky_column_on_ctrl_e_move_to_line_end() {
        let mut editor = editor();

        editor.set_text("12345\n\n1234567890");

        // Start at line 2, col 3.
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..3 {
            editor.handle_input("\x1b[C");
        }

        // Move up through empty line - establishes sticky col 3.
        editor.handle_input("\x1b[A"); // Up - line 1, col 0
        editor.handle_input("\x1b[A"); // Up - line 0, col 3
        assert_cursor(&editor, 0, 3);

        // Ctrl+E - resets sticky column to end.
        editor.handle_input("\x05"); // Ctrl+E
        assert_cursor(&editor, 0, 5);

        // Move down twice.
        editor.handle_input("\x1b[B"); // Down - line 1, col 0
        editor.handle_input("\x1b[B"); // Down - line 2, col 5 (new sticky from col 5)
        assert_cursor(&editor, 2, 5);
    }

    #[test]
    fn resets_sticky_column_on_word_movement_ctrl_left() {
        let mut editor = editor();

        editor.set_text("hello world\n\nhello world");

        // Start at end of line 2 (col 11).
        assert_cursor(&editor, 2, 11);

        // Move up through empty line - establishes sticky col 11.
        editor.handle_input("\x1b[A"); // Up - line 1, col 0
        editor.handle_input("\x1b[A"); // Up - line 0, col 11
        assert_cursor(&editor, 0, 11);

        // Ctrl+Left - word movement resets sticky column.
        editor.handle_input("\x1b[1;5D"); // Ctrl+Left
        assert_cursor(&editor, 0, 6); // Before "world"

        // Move down twice.
        editor.handle_input("\x1b[B"); // Down - line 1, col 0
        editor.handle_input("\x1b[B"); // Down - line 2, col 6 (new sticky from col 6)
        assert_cursor(&editor, 2, 6);
    }

    #[test]
    fn resets_sticky_column_on_word_movement_ctrl_right() {
        let mut editor = editor();

        editor.set_text("hello world\n\nhello world");

        // Start at line 0, col 0.
        editor.handle_input("\x1b[A"); // Up
        editor.handle_input("\x1b[A"); // Up
        editor.handle_input("\x01"); // Ctrl+A
        assert_cursor(&editor, 0, 0);

        // Move down through empty line - establishes sticky col 0.
        editor.handle_input("\x1b[B"); // Down - line 1, col 0
        editor.handle_input("\x1b[B"); // Down - line 2, col 0
        assert_cursor(&editor, 2, 0);

        // Ctrl+Right - word movement resets sticky column.
        editor.handle_input("\x1b[1;5C"); // Ctrl+Right
        assert_cursor(&editor, 2, 5); // After "hello"

        // Move up twice.
        editor.handle_input("\x1b[A"); // Up - line 1, col 0
        editor.handle_input("\x1b[A"); // Up - line 0, col 5 (new sticky from col 5)
        assert_cursor(&editor, 0, 5);
    }

    #[test]
    fn resets_sticky_column_on_undo() {
        let mut editor = editor();

        editor.set_text("1234567890\n\n1234567890");

        // Go to line 0, col 8.
        editor.handle_input("\x1b[A"); // Up to line 1
        editor.handle_input("\x1b[A"); // Up to line 0
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..8 {
            editor.handle_input("\x1b[C");
        }
        assert_cursor(&editor, 0, 8);

        // Move down through empty line - establishes sticky col 8.
        editor.handle_input("\x1b[B"); // Down - line 1, col 0
        editor.handle_input("\x1b[B"); // Down - line 2, col 8 (sticky)
        assert_cursor(&editor, 2, 8);

        // Type something to create undo state - this clears sticky and sets
        // col to 9.
        editor.handle_input("X");
        assert_eq!(editor.get_text(), "1234567890\n\n12345678X90");
        assert_cursor(&editor, 2, 9);

        // Move up - establishes new sticky col 9.
        editor.handle_input("\x1b[A"); // Up - line 1, col 0
        editor.handle_input("\x1b[A"); // Up - line 0, col 9
        assert_cursor(&editor, 0, 9);

        // Undo - resets sticky column and restores cursor to line 2, col 8.
        editor.handle_input("\x1b[45;5u"); // Ctrl+- (undo)
        assert_eq!(editor.get_text(), "1234567890\n\n1234567890");
        assert_cursor(&editor, 2, 8);

        // Move up - should capture new sticky from restored col 8, not old
        // col 9.
        editor.handle_input("\x1b[A"); // Up - line 1, col 0
        editor.handle_input("\x1b[A"); // Up - line 0, col 8 (new sticky from restored position)
        assert_cursor(&editor, 0, 8);
    }

    #[test]
    fn handles_multiple_consecutive_up_down_movements() {
        let mut editor = editor();

        editor.set_text("1234567890\nab\ncd\nef\n1234567890");

        // Start at line 4, col 7.
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..7 {
            editor.handle_input("\x1b[C");
        }
        assert_cursor(&editor, 4, 7);

        // Move up multiple times through short lines.
        editor.handle_input("\x1b[A"); // Up - line 3, col 2 (clamped)
        editor.handle_input("\x1b[A"); // Up - line 2, col 2 (clamped)
        editor.handle_input("\x1b[A"); // Up - line 1, col 2 (clamped)
        editor.handle_input("\x1b[A"); // Up - line 0, col 7 (restored)
        assert_cursor(&editor, 0, 7);

        // Move down multiple times - sticky should still be 7.
        editor.handle_input("\x1b[B"); // Down - line 1, col 2
        editor.handle_input("\x1b[B"); // Down - line 2, col 2
        editor.handle_input("\x1b[B"); // Down - line 3, col 2
        editor.handle_input("\x1b[B"); // Down - line 4, col 7 (restored)
        assert_cursor(&editor, 4, 7);
    }

    #[test]
    fn moves_correctly_through_wrapped_visual_lines_without_getting_stuck() {
        let mut editor = editor_at(15, 24); // Narrow terminal

        // Line 0: short; Line 1: 30 chars = wraps to 3 visual lines at
        // width 10 (after padding).
        editor.set_text("short\n123456789012345678901234567890");
        editor.render(15); // This gives 14 layout width

        // Position at end of line 1 (col 30).
        assert_cursor(&editor, 1, 30);

        // Move up repeatedly - should traverse all visual lines of the
        // wrapped text and eventually reach line 0.
        editor.handle_input("\x1b[A"); // Up - to previous visual line within line 1
        assert_eq!(editor.get_cursor().0, 1);

        editor.handle_input("\x1b[A"); // Up - another visual line
        assert_eq!(editor.get_cursor().0, 1);

        editor.handle_input("\x1b[A"); // Up - should reach line 0
        assert_eq!(editor.get_cursor().0, 0);
    }

    #[test]
    fn handles_set_text_resetting_sticky_column() {
        let mut editor = editor();

        editor.set_text("1234567890\n\n1234567890");

        // Establish sticky column.
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..8 {
            editor.handle_input("\x1b[C");
        }
        editor.handle_input("\x1b[A"); // Up

        // setText should reset sticky column.
        editor.set_text("abcdefghij\n\nabcdefghij");
        assert_cursor(&editor, 2, 10); // At end

        // Move up - should capture new sticky from current position (10).
        editor.handle_input("\x1b[A"); // Up - line 1, col 0
        editor.handle_input("\x1b[A"); // Up - line 0, col 10
        assert_cursor(&editor, 0, 10);
    }

    #[test]
    fn sets_preferred_visual_col_when_pressing_right_at_end_of_prompt() {
        let mut editor = editor();

        // Line 0: 20 chars with 'x' at col 10; Line 1: empty; Line 2: 10
        // chars ending with '_'.
        editor.set_text("111111111x1111111111\n\n333333333_");

        // Go to line 0, press Ctrl+E (end of line) - col 20.
        editor.handle_input("\x1b[A"); // Up to line 1
        editor.handle_input("\x1b[A"); // Up to line 0
        editor.handle_input("\x05"); // Ctrl+E - move to end of line
        assert_cursor(&editor, 0, 20);

        // Move down to line 2 - cursor clamped to col 10 (end of line).
        editor.handle_input("\x1b[B"); // Down to line 1, col 0
        editor.handle_input("\x1b[B"); // Down to line 2, col 10 (clamped)
        assert_cursor(&editor, 2, 10);

        // Press Right at end of prompt - nothing visible happens, but sets
        // preferredVisualCol to 10.
        editor.handle_input("\x1b[C"); // Right - can't move, but sets preferredVisualCol
        assert_cursor(&editor, 2, 10); // Still at same position

        // Move up twice to line 0 - should use preferredVisualCol (10) to
        // land on 'x'.
        editor.handle_input("\x1b[A"); // Up to line 1, col 0
        editor.handle_input("\x1b[A"); // Up to line 0, col 10 (on 'x')
        assert_cursor(&editor, 0, 10);
    }

    #[test]
    fn handles_editor_resizes_when_preferred_visual_col_is_on_the_same_line() {
        let mut editor = editor_at(80, 24);

        editor.set_text("12345678901234567890\n\n12345678901234567890");

        // Start at line 2, col 15.
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..15 {
            editor.handle_input("\x1b[C");
        }

        // Move up through empty line - establishes sticky col 15.
        editor.handle_input("\x1b[A"); // Up
        editor.handle_input("\x1b[A"); // Up - line 0, col 15
        assert_cursor(&editor, 0, 15);

        // Render with narrower width to simulate resize.
        editor.render(12); // Width 12

        // Move down - sticky should be clamped to new width.
        editor.handle_input("\x1b[B"); // Down - line 1
        editor.handle_input("\x1b[B"); // Down - line 2, col should be clamped
        assert_eq!(editor.get_cursor().1, 4);
    }

    #[test]
    fn handles_editor_resizes_when_preferred_visual_col_is_on_a_different_line() {
        let mut editor = editor_at(80, 24);

        // Create a line that wraps into multiple visual lines at width 10.
        editor.set_text("short\n12345678901234567890");

        // Go to line 1, col 15.
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..15 {
            editor.handle_input("\x1b[C");
        }
        assert_cursor(&editor, 1, 15);

        // Move up to establish sticky col 15.
        editor.handle_input("\x1b[A"); // Up to line 0
                                       // Line 0 has only 5 chars, so cursor at col 5.
        assert_cursor(&editor, 0, 5);

        // Narrow the editor.
        editor.render(10);

        // Move down - preferredVisualCol was 15, but width is 10.
        editor.handle_input("\x1b[B"); // Down to line 1
        assert_cursor(&editor, 1, 8);

        // Move up.
        editor.handle_input("\x1b[A"); // Up - should go to line 0
        assert_cursor(&editor, 0, 5); // Line 0 only has 5 chars

        // Restore the original width.
        editor.render(80);

        // Move down - preferredVisualCol was kept at 15.
        editor.handle_input("\x1b[B"); // Down to line 1
        assert_cursor(&editor, 1, 15);
    }

    #[test]
    fn rewrapped_lines_target_fits_current_visual_column() {
        let mut editor = editor_at(80, 24);
        editor.set_text("abcdefghijklmnopqr\n123456789012345678");

        position_cursor(&mut editor, 0, 18);
        assert_cursor(&editor, 0, 18);

        // Narrow to width 10 (layoutWidth = 9). Line 0 last segment has
        // visual col max 9, line 1 first segment max 8.
        editor.render(10);

        // Move down: cursor clamps to 8.
        editor.handle_input("\x1b[B");
        assert_cursor(&editor, 1, 8);

        // Widen back. Move up, the current visual col wins.
        editor.render(80);
        editor.handle_input("\x1b[A");
        assert_cursor(&editor, 0, 8);
    }

    #[test]
    fn rewrapped_lines_target_shorter_than_current_visual_column() {
        let mut editor = editor_at(80, 24);
        editor.set_text("abcdefghijklmnopqr\n123456789012345678\nab");

        position_cursor(&mut editor, 0, 18);
        assert_cursor(&editor, 0, 18);

        // Narrow to width 10 (layoutWidth = 9). Moving down clamps to col 8.
        editor.render(10);
        editor.handle_input("\x1b[B");
        assert_cursor(&editor, 1, 8);

        // Widen the editor.
        editor.render(80);

        // Move down to short line "ab". preferredVisualCol is replaced with
        // current visual col (8), cursor clamps to 2.
        editor.handle_input("\x1b[B");
        assert_cursor(&editor, 2, 2);

        // Moving up restores to preferred col 8.
        editor.handle_input("\x1b[A");
        assert_cursor(&editor, 1, 8);
    }

    // --- Paste marker atomic behavior (editor.test.ts:3572-4150) -----------

    #[test]
    fn creates_a_paste_marker_for_large_pastes() {
        let mut editor = editor();
        let text = paste_with_marker(&mut editor);
        assert!(
            paste_marker_regex().is_match(&text),
            "expected a paste marker, got {text:?}"
        );
    }

    #[test]
    fn treats_paste_marker_as_single_unit_for_right_arrow() {
        let mut editor = editor();
        editor.handle_input("A");
        let _ = paste_with_marker(&mut editor);
        editor.handle_input("B");
        // Text: "A[paste #1 +20 lines]B", cursor at end.

        // Go to start.
        editor.handle_input("\x01"); // Ctrl+A
        assert_cursor(&editor, 0, 0);

        // Right arrow: should move past "A".
        editor.handle_input("\x1b[C");
        assert_cursor(&editor, 0, 1);

        // Right arrow: should skip the entire marker.
        editor.handle_input("\x1b[C");
        let marker = paste_marker_regex()
            .find(&editor.get_text())
            .expect("marker present")
            .as_str()
            .chars()
            .count();
        assert_cursor(&editor, 0, 1 + marker);

        // Right arrow: should move past "B".
        editor.handle_input("\x1b[C");
        assert_cursor(&editor, 0, 1 + marker + 1);
    }

    #[test]
    fn treats_paste_marker_as_single_unit_for_left_arrow() {
        let mut editor = editor();
        editor.handle_input("A");
        let _ = paste_with_marker(&mut editor);
        editor.handle_input("B");
        // Cursor at end.

        // Left arrow: past "B".
        editor.handle_input("\x1b[D");
        let text = editor.get_text();
        let marker = paste_marker_regex()
            .find(&text)
            .expect("marker present")
            .as_str()
            .chars()
            .count();
        assert_cursor(&editor, 0, 1 + marker);

        // Left arrow: skip the entire marker.
        editor.handle_input("\x1b[D");
        assert_cursor(&editor, 0, 1);

        // Left arrow: past "A".
        editor.handle_input("\x1b[D");
        assert_cursor(&editor, 0, 0);
    }

    #[test]
    fn treats_paste_marker_as_single_unit_for_backspace() {
        let mut editor = editor();
        editor.handle_input("A");
        let _ = paste_with_marker(&mut editor);
        editor.handle_input("B");

        let marker = paste_marker_regex()
            .find(&editor.get_text())
            .expect("marker present")
            .as_str()
            .chars()
            .count();

        // Position cursor right after the marker (before "B").
        editor.handle_input("\x01"); // Ctrl+A
                                     // Move past "A" and the marker.
        editor.handle_input("\x1b[C"); // past "A"
        editor.handle_input("\x1b[C"); // past marker
        assert_cursor(&editor, 0, 1 + marker);

        // Backspace: should delete the entire marker at once.
        editor.handle_input("\x7f");
        assert_eq!(editor.get_text(), "AB");
        assert_cursor(&editor, 0, 1);
    }

    #[test]
    fn treats_paste_marker_as_single_unit_for_forward_delete() {
        let mut editor = editor();
        editor.handle_input("A");
        let _ = paste_with_marker(&mut editor);
        editor.handle_input("B");

        // Position cursor on "A" (col 0) then move right once to be just
        // before marker.
        editor.handle_input("\x01"); // Ctrl+A
        editor.handle_input("\x1b[C"); // past "A", now at col 1 (start of marker)

        // Forward delete: should delete the entire marker at once.
        editor.handle_input("\x1b[3~"); // Delete key
        assert_eq!(editor.get_text(), "AB");
        assert_cursor(&editor, 0, 1);
    }

    #[test]
    fn treats_paste_marker_as_single_unit_for_word_movement() {
        let mut editor = editor();
        editor.handle_input("X");
        editor.handle_input(" ");
        let _ = paste_with_marker(&mut editor);
        editor.handle_input(" ");
        editor.handle_input("Y");
        // Text: "X [paste #1 +20 lines] Y".

        let marker = paste_marker_regex()
            .find(&editor.get_text())
            .expect("marker present")
            .as_str()
            .chars()
            .count();

        // Go to start.
        editor.handle_input("\x01"); // Ctrl+A

        // Ctrl+Right: skip "X".
        editor.handle_input("\x1b[1;5C");
        assert_cursor(&editor, 0, 1);

        // Ctrl+Right: skip whitespace + marker (marker treated as single
        // non-ws, non-punct unit).
        editor.handle_input("\x1b[1;5C");
        assert_cursor(&editor, 0, 2 + marker);
    }

    #[test]
    fn undo_restores_marker_after_backspace_deletion() {
        let mut editor = editor();
        editor.handle_input("A");
        let _ = paste_with_marker(&mut editor);
        editor.handle_input("B");

        let text_before = editor.get_text();

        // Position after marker.
        editor.handle_input("\x01");
        editor.handle_input("\x1b[C"); // past A
        editor.handle_input("\x1b[C"); // past marker

        // Delete marker.
        editor.handle_input("\x7f");
        assert_eq!(editor.get_text(), "AB");

        // Undo.
        editor.handle_input("\x1b[45;5u");
        assert_eq!(editor.get_text(), text_before);
    }

    #[test]
    fn undo_after_paste_marker_deletion_restores_the_paste_registry() {
        let mut editor = editor();
        let submitted = Arc::new(std::sync::Mutex::new(String::new()));
        let captured = Arc::clone(&submitted);
        editor.on_submit = Some(Box::new(move |text| {
            *captured.lock().unwrap() = text.to_string();
        }));

        let paste = big_paste("alpha");
        editor.handle_input(&format!("\x1b[200~{paste}\x1b[201~"));
        editor.handle_input("\x7f"); // delete the marker
        editor.handle_input("\x1b[45;5u"); // undo: restores marker text and registry
        editor.handle_input("\r");
        assert_eq!(*submitted.lock().unwrap(), paste);
    }

    #[test]
    fn undo_after_deleting_the_first_of_two_paste_markers_restores_both_registry_entries() {
        let mut editor = editor();
        let submitted = Arc::new(std::sync::Mutex::new(String::new()));
        let captured = Arc::clone(&submitted);
        editor.on_submit = Some(Box::new(move |text| {
            *captured.lock().unwrap() = text.to_string();
        }));

        let paste_a = big_paste("alpha");
        let paste_b = big_paste("beta");
        editor.handle_input(&format!("\x1b[200~{paste_a}\x1b[201~")); // #1 = A
        editor.handle_input(&format!("\x1b[200~{paste_b}\x1b[201~")); // #2 = B, cursor at end
        editor.handle_input("\x01"); // Ctrl+A
        editor.handle_input("\x1b[C"); // right over marker #1
        editor.handle_input("\x7f"); // delete marker #1, renumbers #2 -> #1
        editor.handle_input("\x1b[45;5u"); // undo
        editor.handle_input("\r");
        assert_eq!(*submitted.lock().unwrap(), format!("{paste_a}{paste_b}"));
    }

    #[test]
    fn renumbers_the_paste_registry_in_ascending_id_order_when_markers_are_out_of_order_in_text() {
        let mut editor = editor();
        let submitted = Arc::new(std::sync::Mutex::new(String::new()));
        let captured = Arc::clone(&submitted);
        editor.on_submit = Some(Box::new(move |text| {
            *captured.lock().unwrap() = text.to_string();
        }));

        let paste_a = big_paste("alpha");
        let paste_b = big_paste("beta");
        let paste_c = big_paste("gamma");
        editor.handle_input(&format!("\x1b[200~{paste_a}\x1b[201~")); // #1 = A
        editor.handle_input("\x01"); // Ctrl+A
        editor.handle_input(&format!("\x1b[200~{paste_b}\x1b[201~")); // #2 = B, text: [#2][#1]
        editor.handle_input("\x01"); // Ctrl+A
        editor.handle_input(&format!("\x1b[200~{paste_c}\x1b[201~")); // #3 = C, text: [#3][#2][#1]
        editor.handle_input("\x05"); // Ctrl+E
        editor.handle_input("\x7f"); // delete marker #1, renumber #3 -> #2 and #2 -> #1
        editor.handle_input("\r");
        assert_eq!(*submitted.lock().unwrap(), format!("{paste_c}{paste_b}"));
    }

    #[test]
    fn undo_after_set_text_restores_paste_markers_and_registry() {
        let mut editor = editor();
        let submitted = Arc::new(std::sync::Mutex::new(String::new()));
        let captured = Arc::clone(&submitted);
        editor.on_submit = Some(Box::new(move |text| {
            *captured.lock().unwrap() = text.to_string();
        }));

        let paste = big_paste("alpha");
        editor.handle_input(&format!("\x1b[200~{paste}\x1b[201~"));
        editor.set_text("replacement");
        editor.handle_input("\x1b[45;5u"); // undo
        editor.handle_input("\r");
        assert_eq!(*submitted.lock().unwrap(), paste);
    }

    #[test]
    fn handles_multiple_paste_markers_in_same_line() {
        let mut editor = editor();
        let _ = paste_with_marker(&mut editor);
        editor.handle_input(" ");
        let _ = paste_with_marker(&mut editor);

        let text = editor.get_text();
        let markers: Vec<&str> = paste_marker_regex()
            .find_iter(&text)
            .map(|m| m.as_str())
            .collect();
        assert_eq!(markers.len(), 2);
        let m1 = markers[0].chars().count();
        let m2 = markers[1].chars().count();

        // Go to start.
        editor.handle_input("\x01");

        // Right arrow: should skip first marker atomically.
        editor.handle_input("\x1b[C");
        assert_cursor(&editor, 0, m1);

        // Right arrow: past space.
        editor.handle_input("\x1b[C");
        assert_cursor(&editor, 0, m1 + 1);

        // Right arrow: should skip second marker atomically.
        editor.handle_input("\x1b[C");
        assert_cursor(&editor, 0, m1 + 1 + m2);
    }

    #[test]
    fn does_not_treat_manually_typed_marker_like_text_as_atomic() {
        let mut editor = editor();
        // Type text that matches the pattern but was typed manually (no
        // paste entry).
        let fake_marker = "[paste #99 +5 lines]";
        type_text(&mut editor, fake_marker);

        assert_eq!(editor.get_text(), fake_marker);

        // No paste with ID 99 exists, so the marker is NOT treated
        // atomically. Right arrow should move one grapheme at a time.
        editor.handle_input("\x01"); // Ctrl+A
        editor.handle_input("\x1b[C"); // Right
        assert_cursor(&editor, 0, 1); // Just past "["
    }

    #[test]
    fn does_not_crash_when_paste_marker_is_wider_than_terminal_width() {
        // Reproduce: terminal width 8, paste marker "[paste #1 +47 lines]"
        // (21 chars).
        let mut editor = editor_at(80, 24);
        let big_content = "line\n".repeat(47);
        let big_content = big_content.trim_end().to_string();
        editor.handle_input(&format!("\x1b[200~{big_content}\x1b[201~"));

        let text = editor.get_text();
        let marker = paste_marker_regex().find(&text).expect("marker created");
        assert!(
            visible_width(marker.as_str()) > 8,
            "marker should be wider than render width"
        );

        // Render at very narrow width - should not throw.
        let lines = editor.render(8);
        // Every rendered line must fit within the width (marker is split).
        for line in &lines {
            assert!(
                visible_width(line) <= 8,
                "line exceeds width 8: visible={} text={}",
                visible_width(line),
                strip_ansi(line)
            );
        }
    }

    #[test]
    fn does_not_crash_when_text_plus_paste_marker_exceeds_terminal_width_with_cursor_on_marker() {
        // Reproduce: terminal width 54, text "b"*35 + "[paste #1 +27 lines]"
        // + "bbbb". Cursor lands on the paste marker after word-wrap.
        let mut editor = editor_at(80, 24);

        // Type 35 'b' characters.
        for _ in 0..35 {
            editor.handle_input("b");
        }

        // Paste 27 lines.
        let big_content = "line\n".repeat(27);
        let big_content = big_content.trim_end().to_string();
        editor.handle_input(&format!("\x1b[200~{big_content}\x1b[201~"));

        // Type a few more characters.
        for _ in 0..4 {
            editor.handle_input("b");
        }

        // Move cursor left to land on the paste marker.
        for _ in 0..5 {
            editor.handle_input("\x1b[D");
        }

        // Render at width 54 - should not throw.
        let render_width = 54;
        let lines = editor.render(render_width);
        for line in &lines {
            assert!(
                visible_width(line) <= render_width,
                "line exceeds width {render_width}: visible={} text={}",
                visible_width(line),
                strip_ansi(line)
            );
        }
    }

    #[test]
    fn word_wrap_line_re_checks_overflow_after_backtracking_to_wrap_opportunity() {
        // Reproduce crash #2: " " + "b"*35 + atomic_marker(20 chars) +
        // "bbbb", layoutWidth=53. After wrapping at the space, the remaining
        // 35 b's + marker = 55 must trigger a second force-break instead of
        // silently overflowing.
        let mut editor = editor_at(80, 24);

        // Type a space, then 35 b's.
        editor.handle_input(" ");
        for _ in 0..35 {
            editor.handle_input("b");
        }

        // Paste 27 lines to create marker.
        let big_content = "line\n".repeat(27);
        let big_content = big_content.trim_end().to_string();
        editor.handle_input(&format!("\x1b[200~{big_content}\x1b[201~"));

        // Type trailing chars.
        for _ in 0..4 {
            editor.handle_input("b");
        }

        // Render at width 54 (contentWidth=54, layoutWidth=53 with
        // paddingX=0).
        let render_width = 54;
        let lines = editor.render(render_width);
        for line in &lines {
            assert!(
                visible_width(line) <= render_width,
                "line exceeds width {render_width}: visible={} text={}",
                visible_width(line),
                strip_ansi(line)
            );
        }
    }

    #[test]
    fn expands_large_pasted_content_literally_in_get_expanded_text() {
        let mut editor = editor();
        let pasted_text = [
            "line 1",
            "line 2",
            "line 3",
            "line 4",
            "line 5",
            "line 6",
            "line 7",
            "line 8",
            "line 9",
            "line 10",
            "tokens $1 $2 $& $$ $` $' end",
        ]
        .join("\n");

        editor.handle_input(&format!("\x1b[200~{pasted_text}\x1b[201~"));

        assert!(paste_marker_regex().is_match(&editor.get_text()));
        assert_eq!(editor.get_expanded_text(), pasted_text);
    }

    #[test]
    fn snaps_to_the_paste_marker_start_when_navigating_down_into_it() {
        let mut editor = editor();

        // Line 0: long enough text to establish a sticky column.
        editor.set_text("12345678901234567890\n\nhello ");

        // Create a large paste to get a marker.
        let big_content = "x".repeat(2000);
        editor.handle_input(&format!("\x1b[200~{big_content}\x1b[201~"));
        editor.render(80);

        let text = editor.get_text();
        let _marker = paste_marker_regex().find(&text).expect("marker present");
        // Line 0: "12345678901234567890"; Line 1: ""; Line 2:
        // "hello [paste #1 2000 chars]" — marker starts at col 6.

        // Navigate to line 0, col 10.
        editor.handle_input("\x1b[A"); // Up to line 1
        editor.handle_input("\x1b[A"); // Up to line 0
        editor.handle_input("\x01"); // Ctrl+A (start of line)
        for _ in 0..10 {
            editor.handle_input("\x1b[C"); // Right 10
        }
        assert_cursor(&editor, 0, 10);

        // Down to empty line.
        editor.handle_input("\x1b[B");
        assert_cursor(&editor, 1, 0);

        // Down to paste marker line - sticky col 10 falls inside marker
        // (starts at col 6). Cursor should snap to start of marker (col 6),
        // not end (col 6 + marker.length).
        editor.handle_input("\x1b[B");
        assert_cursor(&editor, 2, 6);
    }

    #[test]
    fn preserves_sticky_column_when_navigating_through_paste_marker_line() {
        let mut editor = editor_at(30, 24);

        // Build:
        // Line 0: "1234567890123456" (16 chars)
        // Line 1: "" (empty)
        // Line 2: "[paste #1 2000 chars]" (22 chars, paste marker)
        // Line 3: "" (empty)
        // Line 4: "abcdefghijklmnop" (16 chars)
        type_text(&mut editor, "1234567890123456");
        editor.handle_input("\n");
        editor.handle_input("\n");
        editor.handle_input(&format!("\x1b[200~{}\x1b[201~", "x".repeat(2000)));
        editor.handle_input("\n");
        editor.handle_input("\n");
        type_text(&mut editor, "abcdefghijklmnop");
        editor.render(30);

        // Navigate to line 0, col 10.
        for _ in 0..4 {
            editor.handle_input("\x1b[A"); // Up to line 0
        }
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..10 {
            editor.handle_input("\x1b[C");
        }
        assert_cursor(&editor, 0, 10);

        // Down to empty line - sticky col 10 established.
        editor.handle_input("\x1b[B");
        assert_cursor(&editor, 1, 0);

        // Down to paste marker - cursor snapped to col 0 (start of marker).
        editor.handle_input("\x1b[B");
        assert_cursor(&editor, 2, 0);

        // Down to empty line.
        editor.handle_input("\x1b[B");
        assert_cursor(&editor, 3, 0);

        // Down to last line - should restore sticky col 10.
        editor.handle_input("\x1b[B");
        assert_cursor(&editor, 4, 10);
    }

    #[test]
    fn does_not_get_stuck_moving_down_from_a_multi_visual_line_paste_marker() {
        let mut editor = editor_at(20, 24);

        // Build:
        // Logical line 0: "abcdefgh" + marker(21 chars) + "ijklmnopqr"
        // Logical line 1: "123456789012345678"
        //
        // Marker "[paste #1 +100 lines]" (21 chars) is wider than the
        // terminal (20). Word-wrap splits at the space before "lines":
        //   VL1: abcdefgh              (startCol 0,  len 8)
        //   VL2: [paste #1 +100        (startCol 8,  len 15) <- marker head
        //   VL3: lines]ijklmnopqr      (startCol 23, len 16) <- marker tail + content
        //   VL4: 123456789012345678    (line 1)
        type_text(&mut editor, "abcdefgh");
        let big_content = "line\n".repeat(100);
        let big_content = big_content.trim_end().to_string();
        editor.handle_input(&format!("\x1b[200~{big_content}\x1b[201~"));
        type_text(&mut editor, "ijklmnopqr");
        editor.handle_input("\n");
        type_text(&mut editor, "123456789012345678");
        editor.render(20);

        let text = editor.get_text();
        let marker_match = paste_marker_regex().find(&text).expect("marker created");
        let marker_len = marker_match.as_str().chars().count(); // 21
        assert!(marker_len > 20, "marker should be wider than terminal");
        let marker_start = 8;
        let marker_end = marker_start + marker_len; // 29

        // Navigate to line 0, col 6 (on "g"). Preferred col 6 is past the
        // marker tail on VL3, so the cursor should land on content ("i" at
        // col 29) without snapping back.
        editor.handle_input("\x1b[A"); // Up to line 0
        editor.handle_input("\x01"); // Ctrl+A (start of line)
        for _ in 0..6 {
            editor.handle_input("\x1b[C"); // Right to col 6
        }
        assert_cursor(&editor, 0, 6);

        // Down: cursor lands on paste marker start.
        editor.handle_input("\x1b[B");
        assert_cursor(&editor, 0, marker_start);

        // Down again: preferred col 6 lands at VL3 col 29 ("i"), which is
        // past the marker. Cursor stays on line 0.
        editor.handle_input("\x1b[B");
        assert_eq!(editor.get_cursor().0, 0);
        assert_eq!(editor.get_cursor().1, marker_end); // col 29 = "i"

        // Up: back to paste marker.
        editor.handle_input("\x1b[A");
        assert_cursor(&editor, 0, marker_start);

        // Up again: back to col 6 ("g").
        editor.handle_input("\x1b[A");
        assert_cursor(&editor, 0, 6);
    }

    #[test]
    fn skips_marker_continuation_vls_when_preferred_col_falls_in_marker_tail() {
        let mut editor = editor_at(20, 24);

        // Same layout as above. Start at col 3 ("d"). Preferred col 3 maps
        // to VL3 visual col 3 which is inside the "lines]" marker tail.
        // moveToVisualLine detects the continuation VL and skips to VL4
        // (line 1).
        type_text(&mut editor, "abcdefgh");
        let big_content = "line\n".repeat(100);
        let big_content = big_content.trim_end().to_string();
        editor.handle_input(&format!("\x1b[200~{big_content}\x1b[201~"));
        type_text(&mut editor, "ijklmnopqr");
        editor.handle_input("\n");
        type_text(&mut editor, "123456789012345678");
        editor.render(20);

        // Navigate to line 0, col 3 (on "d").
        editor.handle_input("\x1b[A"); // Up to line 0
        editor.handle_input("\x01"); // Ctrl+A
        for _ in 0..3 {
            editor.handle_input("\x1b[C");
        }
        assert_cursor(&editor, 0, 3);

        // Down: marker.
        editor.handle_input("\x1b[B");
        assert_eq!(editor.get_cursor().1, 8);

        // Down: skips VL3 (col 3 in marker tail) and lands on line 1.
        editor.handle_input("\x1b[B");
        assert_cursor(&editor, 1, 3);

        // Round-trip back.
        editor.handle_input("\x1b[A");
        assert_eq!(editor.get_cursor().1, 8); // marker
        editor.handle_input("\x1b[A");
        assert_cursor(&editor, 0, 3);
    }

    #[test]
    fn submits_large_pasted_content_literally() {
        let mut editor = editor();
        let pasted_text = [
            "line 1",
            "line 2",
            "line 3",
            "line 4",
            "line 5",
            "line 6",
            "line 7",
            "line 8",
            "line 9",
            "line 10",
            "tokens $1 $2 $& $$ $` $' end",
        ]
        .join("\n");
        let submitted = Arc::new(std::sync::Mutex::new(String::new()));
        let captured = Arc::clone(&submitted);
        editor.on_submit = Some(Box::new(move |text| {
            *captured.lock().unwrap() = text.to_string();
        }));

        editor.handle_input(&format!("\x1b[200~{pasted_text}\x1b[201~"));
        editor.handle_input("\r");

        assert_eq!(*submitted.lock().unwrap(), pasted_text);
    }
}
