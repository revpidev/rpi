//! Port of `packages/tui/src/tui-alt-screen.ts` @ pi 0.84.1+ (4181f66):
//! `TuiAltScreen` — the alternate-screen TUI renderer with a scrollable,
//! application-owned viewport: terminal control (alt-screen enter/exit,
//! autowrap toggling, mouse modes), wheel routing with overscroll chaining,
//! scrollbar hover/drag, application-owned text selection with OSC 52
//! clipboard copy, OSC 8 hyperlink activation, flash notifications, Kitty
//! image management, and the exit-and-redraw-the-document stop sequence.
//!
//! [`TuiAltScreen`] is a clonable handle around `Arc<Mutex<..>>` shared
//! state, composed over [`TuiBase`] exactly like [`TuiMainScreen`] — see the
//! `tui.rs` header notes for the composition-over-inheritance mapping, the
//! re-entrancy queue and the explicit-deadline timer model.
//!
//! Intentional differences (in addition to the `tui.rs` header notes):
//! - The viewport input listener (registered first in the upstream
//!   constructor, tui-alt-screen.ts:182) is emulated as the first dispatch
//!   step of `TuiAltScreenInner::handle_input`: upstream's listener only ever
//!   returns `{ consume: true }` or `undefined` (never replacement `data`),
//!   so running it before the base listener chain is observably identical.
//! - Timers: the selection auto-scroll `setInterval(50)` (tui-alt-screen.ts:
//!   737), flash expiries and scrollbar hide delays are explicit deadlines
//!   fired from [`TuiAltScreen::tick`] (same convention as `scroll_view.rs` /
//!   `alt_screen_flash.rs`, deviation D-082). `next_deadline` /
//!   `has_pending_work` report them like the base's query timeouts.
//! - `process.platform === "win32"` (tui-alt-screen.ts:515) is `cfg!(windows)`
//!   with a test injection point ([`TuiAltScreenOptions::win32_override`]);
//!   upstream's test stubs `process.platform` at runtime.
//! - Mouse parsing, multiplexer detection and the mouse enable/disable
//!   sequences live in `mouse.rs`; the uploaded-Kitty-image cache and
//!   `prepareKittyScreen` live in `kitty_registry.rs` as a process-global
//!   registry. Because the registry is global (upstream: per-instance `Map`
//!   field), the start/stop cache clears only run when the renderer's image
//!   protocol is Kitty — a non-Kitty instance never populates the cache, so
//!   this preserves upstream's per-instance semantics while keeping unrelated
//!   renderers (and parallel tests) from evicting entries.
//! - ScrollView/component identity (`===` upstream) is `Arc::ptr_eq` over
//!   [`SharedComponent`], per the `tui.rs` ownership notes. Mouse coordinates
//!   are `u32` (upstream negative-coordinate edge saturates to 0 — see
//!   `mouse.rs`).
//! - `TuiAltScreenOptions` is not serialized (it holds callbacks), so the
//!   camelCase wire-format rule (coding-standards §4.4) does not apply.
//! - `lastDocument` (tui-alt-screen.ts:133) is kept for parity although
//!   upstream never reads it back.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use base64::Engine;
use tokio::sync::oneshot;

use crate::alt_screen_search::{
    get_alt_screen_search_match_key, AltScreenSearchComponent, AltScreenSearchIndex,
    AltScreenSearchMatch, NavigationButtonStyleFn,
};
use crate::components::alt_screen_flash::{AltScreenFlashContainer, DEFAULT_DURATION_MS};
use crate::components::scroll_view::{
    Follow, Overscroll, ScrollView, ScrollViewOptions, ScrollViewScrollToOptions, ScrollbarMode,
};
use crate::keybindings::{get_keybindings, Keybinding};
use crate::keys::is_key_release;
use crate::kitty_registry::{
    clear_kitty_image_cache, kitty_image_cache_has_entries, prepare_kitty_screen,
};
use crate::layout::{
    get_layout_boxes_at, get_scroll_view_box, get_scroll_views_at, render_layout_frame, LayoutBox,
    LayoutFrame, ScrollbarGeometry,
};
use crate::mouse::{
    is_mouse_sequence, is_multiplexer_env, parse_sgr_mouse_event, parse_wheel_event, SgrMouseEvent,
    WheelEvent, DISABLE_MOUSE, ENABLE_ALL_MOTION_MOUSE, ENABLE_BUTTON_MOTION_MOUSE, FOCUS_IN,
    FOCUS_OUT,
};
use crate::terminal::{InputHandler, ResizeHandler, Terminal};
use crate::terminal_colors::{RgbColor, TerminalColorScheme};
use crate::terminal_image::{
    delete_all_kitty_images, delete_all_kitty_placements, get_capabilities, is_image_line,
    set_capabilities, ImageProtocol, TerminalCapabilities,
};
use crate::tui::{
    composite_tui_line, dispatch_mouse_event, lock_component, lock_shared, retarget_mouse_event,
    same_component, shared_component, Component, OverlayAnchor, OverlayBounds, OverlayHandle,
    OverlayHandleOps, OverlayMarginSpec, OverlayOptions, OverlayUnfocusOptions, RenderHandle,
    SharedComponent, SharedTerminal, SizeValue, TerminalColorSchemeListener, Tui, TuiInputListener,
    TuiInputListenerResult, TuiMode, TuiMouseButton, TuiMouseDispatchResult,
    TuiMouseDispatchTarget, TuiMouseEvent, TuiMouseEventType, TuiMouseHandlerResult,
    TuiStopOptions, ViewportTui, CURSOR_MARKER,
};
use crate::tui_base::{
    schedule_render, PendingOsc11BackgroundQuery, PendingTerminalColorSchemeQuery, RenderSchedule,
    TerminalSizeCache, TuiBase,
};
use crate::utils::{
    extract_ansi_code, get_grapheme_cell_range, get_osc8_link_at_column, get_word_segmenter,
    slice_by_column, strip_terminal_sequences, truncate_to_width, visible_width,
};

// =============================================================================
// Constants (tui-alt-screen.ts:44-61)
// =============================================================================

/// `ENTER_ALT_SCREEN` (tui-alt-screen.ts:44).
const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
/// `EXIT_ALT_SCREEN` (tui-alt-screen.ts:45).
const EXIT_ALT_SCREEN: &str = "\x1b[?1049l";
/// `DISABLE_AUTOWRAP` (tui-alt-screen.ts:46).
const DISABLE_AUTOWRAP: &str = "\x1b[?7l";
/// `ENABLE_AUTOWRAP` (tui-alt-screen.ts:47).
const ENABLE_AUTOWRAP: &str = "\x1b[?7h";
/// `BEGIN_SYNCHRONIZED_OUTPUT` (tui-alt-screen.ts:53).
const BEGIN_SYNCHRONIZED_OUTPUT: &str = "\x1b[?2026h";
/// `END_SYNCHRONIZED_OUTPUT` (tui-alt-screen.ts:54).
const END_SYNCHRONIZED_OUTPUT: &str = "\x1b[?2026l";
/// `PAGE_SCROLL_OVERLAP` (tui-alt-screen.ts:57).
const PAGE_SCROLL_OVERLAP: usize = 4;
/// `DOUBLE_CLICK_INTERVAL_MS` (tui-alt-screen.ts:61).
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// `TERMINAL_WORD_SELECTION_JOINERS` (tui-alt-screen.ts:81 @ 9841914,
/// `1ac6128e6` #7746): regular mode delegates double-click selection to the
/// terminal emulator. Fullscreen owns mouse selection, so mirror common
/// terminal word-selection behavior by keeping paths and kebab-case tokens
/// whole.
const TERMINAL_WORD_SELECTION_JOINERS: [&str; 2] = ["/", "-"];
/// The selection auto-scroll `setInterval` period (tui-alt-screen.ts:737).
const AUTO_SCROLL_INTERVAL: Duration = Duration::from_millis(50);

/// Strip a leading run of OSC 133 prompt-zone markers (`OSC133_ZONE_PREFIX`,
/// tui-alt-screen.ts:55): `^(?:\x1b\]133;[ABC](?:\x07|\x1b\\))+`. Same
/// hand-rolled loop as `layout.rs`.
fn strip_osc133_zone_prefix(line: &str) -> &str {
    let mut rest = line;
    while let Some(after_prefix) = rest.strip_prefix("\x1b]133;") {
        let Some(&zone) = after_prefix.as_bytes().first() else {
            break;
        };
        if !matches!(zone, b'A' | b'B' | b'C') {
            break;
        }
        let after_zone = &after_prefix[1..];
        if let Some(stripped) = after_zone.strip_prefix('\x07') {
            rest = stripped;
        } else if let Some(stripped) = after_zone.strip_prefix("\x1b\\") {
            rest = stripped;
        } else {
            break;
        }
    }
    rest
}

/// `OSC133_PROMPT_START` (tui-alt-screen.ts:56): `/^\x1b\]133;A(?:\x07|\x1b\\)/`.
fn is_osc133_prompt_start(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("\x1b]133;A") else {
        return false;
    };
    rest.starts_with('\x07') || rest.starts_with("\x1b\\")
}

// =============================================================================
// Types (tui-alt-screen.ts:70-126)
// =============================================================================

/// `SelectionPoint` (tui-alt-screen.ts:70-76). `scrollView` is the
/// [`SharedComponent`] of the scroll view the point belongs to (upstream
/// holds the live object).
#[derive(Clone)]
struct SelectionPoint {
    row: usize,
    col: usize,
    scroll_view: Option<SharedComponent>,
    /// Whether this point lies between terminal cells rather than on a cell.
    boundary: bool,
}

impl SelectionPoint {
    /// `{ ...point, col }` / `{ ...point, col, boundary }` spreads.
    fn with(&self, col: usize, boundary: bool) -> SelectionPoint {
        SelectionPoint {
            row: self.row,
            col,
            scroll_view: self.scroll_view.clone(),
            boundary,
        }
    }
}

/// `SelectionRange` (tui-alt-screen.ts:78-81).
#[derive(Clone)]
struct SelectionRange {
    start: SelectionPoint,
    end: SelectionPoint,
}

/// `SelectionGranularity` (tui-alt-screen.ts:83).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionGranularity {
    Character,
    Word,
    Line,
}

/// `ClickTarget` (tui-alt-screen.ts:85-92).
#[derive(Clone)]
struct ClickTarget {
    timestamp: Instant,
    count: u32,
    row: usize,
    scroll_view: Option<SharedComponent>,
    word_start: usize,
    word_end: usize,
}

/// Result of [`TuiAltScreenInner::dispatch_mouse_to_overlay`] (upstream
/// `{ hit: boolean; result?: TuiMouseDispatchResult }`, tui.ts:824).
struct OverlayMouseDispatch {
    hit: bool,
    result: Option<TuiMouseDispatchResult>,
}

/// `ScrollbarDrag` (tui-alt-screen.ts:107-110).
#[derive(Clone)]
struct ScrollbarDrag {
    scroll_view: SharedComponent,
    grab_offset: isize,
}

/// `lastComponentClick` state (tui-alt-screen.ts:102-104 @ 9841914,
/// 71026970a): component-click counting for the synthesized-click
/// `clickCount` (identity = component + cell; window =
/// [`DOUBLE_CLICK_INTERVAL`]).
#[derive(Clone)]
struct LastComponentClick {
    timestamp: Instant,
    count: u32,
    component: SharedComponent,
    x: u32,
    y: u32,
}

/// `ScrollbarTarget` (tui-alt-screen.ts:112-115).
#[derive(Clone)]
struct ScrollbarTarget {
    scroll_view: SharedComponent,
    geometry: ScrollbarGeometry,
}

/// `ScrollToEndIndicatorRect` (tui-alt-screen.ts:138-141 @ 9841914,
/// 79680533c): the composited label's hit rectangle.
#[derive(Debug, Clone, Copy)]
struct ScrollToEndIndicatorRect {
    row: u32,
    column: u32,
    width: u32,
}

/// `SearchSelectionMode` (tui-alt-screen.ts:144 @ 9841914, 00121ed99).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchSelectionMode {
    Query,
    Retain,
    Next,
    Previous,
}

/// `ActiveSearch` (tui-alt-screen.ts:146-156 @ 9841914). The upstream
/// `overlay?: OverlayHandle` is replaced by the overlay stack `entry_id`
/// (hide / focus checks / bounds all resolve against the inner overlay
/// state without an outer handle).
struct ActiveSearch {
    component: SharedComponent,
    index: AltScreenSearchIndex,
    overlay_entry_id: u64,
    query: String,
    matches: Arc<Vec<AltScreenSearchMatch>>,
    selected_index: i64,
    selected_key: Option<String>,
    anchor_row: usize,
    selection_mode: SearchSelectionMode,
}

/// `SearchHighlightRange` (tui-alt-screen.ts:158-162).
struct SearchHighlightRange {
    start_col: usize,
    end_col: usize,
    current: bool,
}

/// Merge adjacent word segments that consist solely of Hiragana into one
/// (see the D-093 note in `get_word_selection`; upstream `Intl.Segmenter`
/// yields the whole run as one word-like segment).
fn merge_hiragana_runs(
    segments: Vec<(&str, usize, usize, bool, bool)>,
) -> Vec<(&str, usize, usize, bool, bool)> {
    fn all_hiragana(segment: &str) -> bool {
        !segment.is_empty()
            && segment
                .chars()
                .all(|c| ('\u{3040}'..='\u{309F}').contains(&c))
    }
    let mut merged: Vec<(&str, usize, usize, bool, bool)> = Vec::with_capacity(segments.len());
    for (text, start, end, selectable, joiner) in segments {
        // All-Hiragana segments are selectable and never joiners; merging
        // only affects the column range (the merged text is never re-read).
        let is_hiragana_run = !joiner && selectable && all_hiragana(text);
        match merged.last_mut() {
            Some(last)
                if is_hiragana_run
                    && !last.4
                    && last.3
                    && last
                        .0
                        .chars()
                        .all(|c| ('\u{3040}'..='\u{309F}').contains(&c)) =>
            {
                last.2 = end;
            }
            _ => merged.push((text, start, end, selectable, joiner)),
        }
        let _ = is_hiragana_run;
    }
    merged
}

/// Upstream `scrollViewA === scrollViewB` for optional scroll views
/// (`undefined === undefined` is true).
fn same_optional_scroll_view(a: &Option<SharedComponent>, b: &Option<SharedComponent>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => same_component(a, b),
        _ => false,
    }
}

/// Lock `shared` and run `f` against its [`ScrollView`]
/// (`Component::as_scroll_view`, the T30 downcast-style accessor).
fn with_scroll_view<R>(shared: &SharedComponent, f: impl FnOnce(&ScrollView) -> R) -> Option<R> {
    let guard = lock_component(shared);
    guard.as_scroll_view().map(f)
}

/// Drain the shared pending-op queue into a locked inner guard (callback
/// variant of [`TuiAltScreen::drain_pending_ops`]).
fn drain_pending_into(inner: &Arc<Mutex<TuiAltScreenInner>>, pending: &Arc<Mutex<Vec<PendingOp>>>) {
    loop {
        let ops: Vec<PendingOp> = std::mem::take(&mut *lock_shared(pending));
        if ops.is_empty() {
            return;
        }
        let mut guard = lock_shared(inner);
        for op in ops {
            op(&mut guard);
        }
    }
}

/// `openUrl` callback (tui-alt-screen.ts:123).
pub type OpenUrlCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// `onRightClickPaste` callback (tui-alt-screen.ts:125).
pub type RightClickPasteCallback = Arc<dyn Fn() + Send + Sync>;

/// `searchMatchStyle` / `searchCurrentMatchStyle` callbacks
/// (tui-alt-screen.ts:170-172 @ 9841914, 00121ed99).
pub type SearchTextStyleFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// `scrollToEndIndicator` callback (tui-alt-screen.ts:177-180 @ 9841914,
/// 79680533c): the label text composited on the last row.
pub type ScrollToEndIndicatorFn = Arc<dyn Fn() -> String + Send + Sync>;

/// `copySelection?: (text: string) => Promise<boolean>`
/// (tui-alt-screen.ts:190 @ 9841914, 4caa3c440): copy selected text to the
/// system clipboard, returning success. Upstream's hook is async; the rpi
/// render loop is synchronous, so the seam runs the clipboard write inline
/// and returns `bool` — same terminal bytes, same flash timing (established
/// async-flattening convention, cf. the V13 host_call synchronization).
pub type CopySelectionFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// `TuiAltScreenOptions` (tui-alt-screen.ts:117-126). Not serialized (holds
/// callbacks); see the header note.
#[derive(Default)]
pub struct TuiAltScreenOptions {
    /// `wheelScrollLines`: logical lines per wheel event. Normalized with
    /// `Math.max(1, Math.floor(_ ?? 1))` (tui-alt-screen.ts:178).
    pub wheel_scroll_lines: Option<u64>,
    /// `mouse` (default true): capture mouse events for viewport scrolling
    /// and application-owned text selection.
    pub mouse: Option<bool>,
    /// `searchMatchStyle`: style a non-current transcript search match
    /// (default underline, tui-alt-screen.ts:265).
    pub search_match_style: Option<SearchTextStyleFn>,
    /// `searchCurrentMatchStyle`: style the current transcript search match
    /// (default bold + inverse, tui-alt-screen.ts:266).
    pub search_current_match_style: Option<SearchTextStyleFn>,
    /// `searchNavigationButtonStyle`: style a transcript search navigation
    /// button (tui-alt-screen.ts:267).
    pub search_navigation_button_style: Option<NavigationButtonStyleFn>,
    /// `scrollToEndIndicator`: render a clickable jump-to-end label, centered
    /// on the last row of a follow-end primary scroll view while that view is
    /// scrolled away from its end (79680533c #9080).
    pub scroll_to_end_indicator: Option<ScrollToEndIndicatorFn>,
    /// `openUrl`: open an OSC 8 hyperlink activated with a primary click.
    pub open_url: Option<OpenUrlCallback>,
    /// `onRightClickPaste`: handle an unmodified secondary-button press for
    /// clipboard paste. Enabled on Windows only upstream.
    pub on_right_click_paste: Option<RightClickPasteCallback>,
    /// `copyOnSelect` (default true): automatically copy selected text to
    /// the clipboard on mouse release (tui-alt-screen.ts:185 @ 9841914,
    /// 4e4949299).
    pub copy_on_select: Option<bool>,
    /// `copySelection`: verified clipboard write hook
    /// (4caa3c440 #8110). When omitted, selections copy via a bare OSC 52
    /// write that is assumed successful (upstream default).
    pub copy_selection: Option<CopySelectionFn>,
    /// Test injection for upstream's `process.platform === "win32"` check
    /// (tui-alt-screen.test.ts stubs `process.platform`); production uses
    /// `cfg!(windows)`.
    #[doc(hidden)]
    pub win32_override: Option<bool>,
    /// Test injection for upstream's `process.env.TERM_PROGRAM` read in the
    /// right-click-paste exclusion (`374e56e55`): `None` reads the real
    /// environment; `Some(None)` models an unset variable and
    /// `Some(Some(value))` a fixed value.
    #[doc(hidden)]
    pub term_program_override: Option<Option<String>>,
}

// =============================================================================
// Implicit document (tui-alt-screen.ts:170-175)
// =============================================================================

/// `implicitDocument` (tui-alt-screen.ts:170-175): renders the TUI's child
/// list (`super.render(width)`) and invalidates all children. The child list
/// is a mirror of `TuiBase::children` (the mirror is what this component can
/// lock while the renderer's inner lock is held mid-render).
struct ImplicitDocument {
    children: Arc<Mutex<Vec<SharedComponent>>>,
    /// `Container.prototype` `mouseLayout` (tui.ts:321 @ 9841914): per-child
    /// rendered heights at the width of the last render, for the
    /// `handle_mouse` hit-test (the upstream implicit document is a plain
    /// `Container`, tui-alt-screen.ts:170).
    mouse_layout: RefCell<Option<(usize, Vec<usize>)>>,
}

impl Component for ImplicitDocument {
    fn render(&self, width: usize) -> Vec<String> {
        let children = lock_shared(&self.children).clone();
        let mut lines = Vec::new();
        let mut heights = Vec::with_capacity(children.len());
        for child in &children {
            let child_lines = lock_component(child).render(width);
            heights.push(child_lines.len());
            lines.extend(child_lines);
        }
        *self.mouse_layout.borrow_mut() = Some((width, heights));
        lines
    }

    fn invalidate(&mut self) {
        let children = lock_shared(&self.children).clone();
        for child in &children {
            lock_component(child).invalidate();
        }
    }

    /// The implicit document is a plain `Container` upstream
    /// (tui-alt-screen.ts:170); its `Container.prototype.handleMouse`
    /// (tui.ts:344-365 @ 9841914) hit-tests the shared children and
    /// dispatches with child-local coordinates — this is how layout-box
    /// dispatch reaches the TUI children (the layout engine does not
    /// descend into the implicit document).
    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseHandlerResult> {
        if event.y < 0 || event.y >= event.height {
            return None;
        }
        let width = event.width.max(1) as usize;
        let children = lock_shared(&self.children).clone();
        let cached_heights = self
            .mouse_layout
            .borrow()
            .as_ref()
            .filter(|(cached_width, _)| *cached_width == width)
            .map(|(_, heights)| heights.clone());
        let heights = match cached_heights {
            Some(heights) => heights,
            None => children
                .iter()
                .map(|child| lock_component(child).render(width).len())
                .collect::<Vec<_>>(),
        };
        let mut child_y: isize = 0;
        for (child, child_height) in children.iter().zip(heights) {
            let child_height = child_height as isize;
            if event.y >= child_y && event.y < child_y + child_height {
                let child_event =
                    event.with_local(event.x, event.y - child_y, event.width, child_height);
                // The child IS the dispatch target (upstream: the implicit
                // document is a plain Container returning the child's
                // dispatchMouseEvent result verbatim).
                return dispatch_mouse_event(child, &child_event)
                    .map(TuiMouseHandlerResult::Forwarded);
            }
            child_y += child_height;
        }
        None
    }
}

// =============================================================================
// TuiAltScreen (upstream `TuiAltScreen`, tui-alt-screen.ts:129)
// =============================================================================

/// Raw terminal events queued by the `Terminal::start` callbacks and drained
/// by [`TuiAltScreen::tick`] (same input-delivery model as `TuiMainScreen`).
enum InboxEvent {
    Input(String),
    Resize,
}

/// Mutation queued while the inner lock is held (see the `tui.rs` header
/// note on re-entrancy).
type PendingOp = Box<dyn FnOnce(&mut TuiAltScreenInner) + Send>;

/// Alternate-screen TUI with a scrollable, application-owned viewport
/// (upstream `TuiAltScreen`, tui-alt-screen.ts:129 @ 4181f66).
///
/// Clonable handle around shared state; all clones refer to the same TUI.
/// Drive rendering with [`TuiAltScreen::tick`] / [`TuiAltScreen::pump`] from
/// the loop thread, like [`TuiMainScreen`].
#[derive(Clone)]
pub struct TuiAltScreen {
    inner: Arc<Mutex<TuiAltScreenInner>>,
    /// Shared with `TuiBase::terminal`; see [`SharedTerminal`] for the
    /// lock-ordering rules.
    terminal: SharedTerminal,
    schedule: Arc<Mutex<RenderSchedule>>,
    pending: Arc<Mutex<Vec<PendingOp>>>,
    inbox: Arc<Mutex<VecDeque<InboxEvent>>>,
    /// Mirror of `TuiAltScreenInner::implicit_scroll_view` for lock-free
    /// fallback reads (e.g. `get_primary_scroll_view` on lock contention).
    implicit_scroll_view: SharedComponent,
    next_listener_id: Arc<AtomicU64>,
    next_overlay_id: Arc<AtomicU64>,
    /// Cached terminal dimensions for lock-free reads (refreshed per frame).
    size_cache: TerminalSizeCache,
}

/// Alternate-screen renderer state: the upstream `TuiAltScreen` private
/// fields (tui-alt-screen.ts:132-161) plus the composed [`TuiBase`].
pub(crate) struct TuiAltScreenInner {
    base: TuiBase,
    previous_screen: Vec<String>,
    last_document: Vec<String>,
    previous_screen_width: usize,
    previous_screen_height: usize,
    layout_root: Option<SharedComponent>,
    current_layout: Option<LayoutFrame>,
    /// Mirror of `TuiBase::children` feeding the implicit document
    /// (tui-alt-screen.ts:170-172).
    implicit_children: Arc<Mutex<Vec<SharedComponent>>>,
    implicit_scroll_view: SharedComponent,
    flashes: AltScreenFlashContainer,
    alt_screen_active: bool,
    image_protocol: Option<ImageProtocol>,
    saved_capabilities: Option<TerminalCapabilities>,
    selection_anchor: Option<SelectionPoint>,
    selection_focus: Option<SelectionPoint>,
    selection_granularity: SelectionGranularity,
    selection_initial_range: Option<SelectionRange>,
    last_click: Option<ClickTarget>,
    selection_drag_pointer: Option<(u32, u32)>,
    selection_auto_scroll_direction: i32,
    /// Replaces upstream's `selectionAutoScrollTimer` (`setInterval`,
    /// tui-alt-screen.ts:737): the next auto-scroll fire instant.
    selection_auto_scroll_next: Option<Instant>,
    selection_press_active: bool,
    scrollbar_drag: Option<ScrollbarDrag>,
    scrollbar_hover: Option<SharedComponent>,
    /// `scrollToEndIndicatorRect` (tui-alt-screen.ts:221 @ 9841914,
    /// 79680533c).
    scroll_to_end_indicator_rect: Option<ScrollToEndIndicatorRect>,
    /// `activeSearch` (tui-alt-screen.ts:222 @ 9841914, 00121ed99).
    active_search: Option<ActiveSearch>,
    pressed_url: Option<String>,
    selection_dragged: bool,
    wheel_scroll_lines: u64,
    mouse_enabled: bool,
    /// `mouseCapture` (tui-alt-screen.ts:96 @ 9841914, 71026970a): the
    /// component that requested drag/release routing via a `capture` result.
    mouse_capture: Option<TuiMouseDispatchTarget>,
    /// `mousePressTarget` (:97): the component that handled the press, for
    /// gesture re-dispatch and synthesized-click routing.
    mouse_press_target: Option<TuiMouseDispatchTarget>,
    /// `mousePressPoint` (:98): the raw cell of the press.
    mouse_press_point: Option<(u32, u32)>,
    /// `mousePressMoved` (:99): whether the pointer moved cells during the
    /// press (suppresses the synthesized click).
    mouse_press_moved: bool,
    /// `lastComponentClick` (:102-104): double/triple-click counting for
    /// component clicks (mirrors `ClickTarget` for the selection path).
    last_component_click: Option<LastComponentClick>,
    open_url: Option<OpenUrlCallback>,
    on_right_click_paste: Option<RightClickPasteCallback>,
    /// `copyOnSelect` (tui-alt-screen.ts:152 @ 9841914, 4e4949299);
    /// mutable via [`TuiAltScreen::set_copy_on_select`].
    copy_on_select: bool,
    /// `copySelection` injection (tui-alt-screen.ts:155 @ 9841914,
    /// 4caa3c440).
    copy_selection: Option<CopySelectionFn>,
    /// `searchMatchStyle` (default underline, tui-alt-screen.ts:265).
    search_match_style: SearchTextStyleFn,
    /// `searchCurrentMatchStyle` (default bold + inverse, :266).
    search_current_match_style: SearchTextStyleFn,
    /// `searchNavigationButtonStyle` (default identity, :267).
    search_navigation_button_style: NavigationButtonStyleFn,
    /// `scrollToEndIndicator` (:268).
    scroll_to_end_indicator: Option<ScrollToEndIndicatorFn>,
    /// `process.platform === "win32"` (see header note).
    win32: bool,
    /// `process.env.TERM_PROGRAM` for the VS Code right-click exclusion
    /// (tui-alt-screen.ts:996 @ 9841914, 374e56e55). `None` reads the real
    /// environment; `Some(None)` models an unset variable and
    /// `Some(Some(value))` a fixed value (test injection, same pattern as
    /// `win32_override`).
    term_program: Option<Option<String>>,
    /// Render handle handed to the layout engine and the flash container
    /// (upstream `() => this.requestRender()`).
    render_handle: RenderHandle,
    /// Weak self-reference for component callbacks that must mutate the
    /// inner state while the inner lock is held (search query changes;
    /// upstream closures capture the live TUI object, JS GC semantics).
    /// Set right after construction in [`TuiAltScreen::build`].
    self_handle: Weak<Mutex<TuiAltScreenInner>>,
    /// Shared with the outer handle's `pending` queue (the callback-side
    /// equivalent of [`TuiAltScreen::run_or_queue`]).
    pending_ops: Arc<Mutex<Vec<PendingOp>>>,
    /// Shared with the outer handle's overlay-id counter (search overlays
    /// are created from input handlers that already hold the inner lock).
    next_overlay_id: Arc<AtomicU64>,
}

impl Deref for TuiAltScreenInner {
    type Target = TuiBase;

    fn deref(&self) -> &TuiBase {
        &self.base
    }
}

impl DerefMut for TuiAltScreenInner {
    fn deref_mut(&mut self) -> &mut TuiBase {
        &mut self.base
    }
}

impl TuiAltScreen {
    /// Upstream `new TuiAltScreen(terminal)` (tui-alt-screen.ts:163).
    pub fn new(terminal: Box<dyn Terminal + Send>) -> TuiAltScreen {
        Self::with_options(terminal, None, None, TuiAltScreenOptions::default())
    }

    /// Upstream `new TuiAltScreen(terminal, showHardwareCursor?, logDirectory?, options?)`
    /// (tui-alt-screen.ts:163-183).
    pub fn with_options(
        terminal: Box<dyn Terminal + Send>,
        show_hardware_cursor: Option<bool>,
        log_directory: Option<PathBuf>,
        options: TuiAltScreenOptions,
    ) -> TuiAltScreen {
        let size_cache = TerminalSizeCache {
            rows: Arc::new(AtomicU16::new(terminal.rows())),
            columns: Arc::new(AtomicU16::new(terminal.columns())),
        };
        let terminal: SharedTerminal = Arc::new(Mutex::new(terminal));
        Self::build(
            terminal,
            show_hardware_cursor,
            log_directory,
            options,
            size_cache,
        )
    }

    /// T32 variant of [`TuiAltScreen::with_options`] that takes an existing
    /// [`SharedTerminal`] instead of `Box<dyn Terminal>`, so `switch_tui_mode`
    /// (interactive-mode.ts:808-814 @ b103937d3) can reuse the same terminal.
    pub fn with_shared_terminal(
        terminal: SharedTerminal,
        show_hardware_cursor: Option<bool>,
        log_directory: Option<PathBuf>,
        options: TuiAltScreenOptions,
    ) -> TuiAltScreen {
        let (rows, columns) = {
            let t = lock_shared(&terminal);
            (t.rows(), t.columns())
        };
        let size_cache = TerminalSizeCache {
            rows: Arc::new(AtomicU16::new(rows)),
            columns: Arc::new(AtomicU16::new(columns)),
        };
        Self::build(
            terminal,
            show_hardware_cursor,
            log_directory,
            options,
            size_cache,
        )
    }

    /// Shared body of [`with_options`] / [`with_shared_terminal`].
    fn build(
        terminal: SharedTerminal,
        show_hardware_cursor: Option<bool>,
        log_directory: Option<PathBuf>,
        options: TuiAltScreenOptions,
        size_cache: TerminalSizeCache,
    ) -> TuiAltScreen {
        let schedule = Arc::new(Mutex::new(RenderSchedule {
            requested: false,
            deadline: None,
            last_render_at: None,
        }));
        let base = TuiBase::new(
            Arc::clone(&terminal),
            show_hardware_cursor,
            log_directory,
            Arc::clone(&schedule),
            size_cache.clone(),
        );
        let render_handle = {
            let schedule = Arc::downgrade(&schedule);
            RenderHandle::new(move || {
                if let Some(schedule) = schedule.upgrade() {
                    schedule_render(&schedule, false, Instant::now());
                }
            })
        };
        let implicit_children = Arc::new(Mutex::new(Vec::new()));
        let pending: Arc<Mutex<Vec<PendingOp>>> = Arc::new(Mutex::new(Vec::new()));
        let next_overlay_id: Arc<AtomicU64> = Arc::new(AtomicU64::new(1));
        let implicit_document = shared_component(ImplicitDocument {
            children: Arc::clone(&implicit_children),
            mouse_layout: RefCell::new(None),
        });
        // `new ScrollView(this.implicitDocument, { follow: "end", primary: true })`
        // (tui-alt-screen.ts:176).
        let implicit_scroll_view = shared_component(ScrollView::new(
            implicit_document,
            ScrollViewOptions {
                follow: Follow::End,
                primary: true,
                ..ScrollViewOptions::default()
            },
        ));
        let inner = TuiAltScreenInner {
            base,
            previous_screen: Vec::new(),
            last_document: Vec::new(),
            previous_screen_width: 0,
            previous_screen_height: 0,
            layout_root: None,
            current_layout: None,
            implicit_children,
            implicit_scroll_view: implicit_scroll_view.clone(),
            flashes: AltScreenFlashContainer::new(render_handle.clone()),
            alt_screen_active: false,
            image_protocol: None,
            saved_capabilities: None,
            selection_anchor: None,
            selection_focus: None,
            selection_granularity: SelectionGranularity::Character,
            selection_initial_range: None,
            last_click: None,
            selection_drag_pointer: None,
            selection_auto_scroll_direction: 0,
            selection_auto_scroll_next: None,
            selection_press_active: false,
            scrollbar_drag: None,
            scrollbar_hover: None,
            scroll_to_end_indicator_rect: None,
            active_search: None,
            pressed_url: None,
            selection_dragged: false,
            // `Math.max(1, Math.floor(options.wheelScrollLines ?? 1))`
            // (tui-alt-screen.ts:178); `u64` is already floored.
            wheel_scroll_lines: options.wheel_scroll_lines.unwrap_or(1).max(1),
            mouse_enabled: options.mouse.unwrap_or(true),
            mouse_capture: None,
            mouse_press_target: None,
            mouse_press_point: None,
            mouse_press_moved: false,
            last_component_click: None,
            open_url: options.open_url,
            on_right_click_paste: options.on_right_click_paste,
            // `this.copyOnSelect = options.copyOnSelect ?? true`
            // (tui-alt-screen.ts:271).
            copy_on_select: options.copy_on_select.unwrap_or(true),
            copy_selection: options.copy_selection,
            search_match_style: options
                .search_match_style
                .unwrap_or_else(|| Arc::new(|text: &str| format!("\x1b[4m{text}\x1b[24m"))),
            search_current_match_style: options
                .search_current_match_style
                .unwrap_or_else(|| Arc::new(|text: &str| format!("\x1b[1;7m{text}\x1b[22;27m"))),
            search_navigation_button_style: options
                .search_navigation_button_style
                .unwrap_or_else(|| Arc::new(|text: &str, _hovered: bool| text.to_string())),
            scroll_to_end_indicator: options.scroll_to_end_indicator,
            win32: options.win32_override.unwrap_or(cfg!(windows)),
            term_program: options.term_program_override,
            render_handle,
            self_handle: Weak::new(),
            pending_ops: Arc::clone(&pending),
            next_overlay_id: Arc::clone(&next_overlay_id),
        };
        let inner = Arc::new(Mutex::new(inner));
        {
            let mut guard = lock_shared(&inner);
            guard.self_handle = Arc::downgrade(&inner);
        }
        TuiAltScreen {
            inner,
            terminal,
            schedule,
            pending,
            inbox: Arc::new(Mutex::new(VecDeque::new())),
            implicit_scroll_view,
            next_listener_id: Arc::new(AtomicU64::new(1)),
            next_overlay_id,
            size_cache,
        }
    }

    // --- lock helpers -----------------------------------------------------

    fn lock_inner(&self) -> MutexGuard<'_, TuiAltScreenInner> {
        lock_shared(&self.inner)
    }

    /// Run `op` against the inner state; queued when the lock is held
    /// (same re-entrancy contract as [`TuiMainScreen::run_or_queue`]).
    pub(crate) fn run_or_queue(&self, op: impl FnOnce(&mut TuiAltScreenInner) + Send + 'static) {
        match self.inner.try_lock() {
            Ok(mut inner) => {
                op(&mut inner);
                drop(inner);
                self.drain_pending_ops();
            }
            Err(_) => lock_shared(&self.pending).push(Box::new(op)),
        }
    }

    /// Read from the inner state; returns `None` on lock contention.
    pub(crate) fn try_read<R>(&self, read: impl FnOnce(&TuiAltScreenInner) -> R) -> Option<R> {
        self.inner.try_lock().ok().map(|inner| read(&inner))
    }

    fn drain_pending_ops(&self) {
        loop {
            let ops: Vec<PendingOp> = std::mem::take(&mut *lock_shared(&self.pending));
            if ops.is_empty() {
                return;
            }
            let mut inner = self.lock_inner();
            for op in ops {
                op(&mut inner);
            }
        }
    }

    // --- container API (upstream `TUI extends Container`) -----------------

    /// Upstream `addChild`.
    pub fn add_child(&self, component: SharedComponent) {
        self.run_or_queue(move |inner| {
            inner.children.push(component.clone());
            lock_shared(&inner.implicit_children).push(component);
        });
    }

    /// Upstream `removeChild` (identity comparison).
    pub fn remove_child(&self, component: &SharedComponent) {
        let component = Arc::clone(component);
        self.run_or_queue(move |inner| {
            if let Some(index) = inner
                .children
                .iter()
                .position(|child| same_component(child, &component))
            {
                inner.children.remove(index);
            }
            let mut mirror = lock_shared(&inner.implicit_children);
            if let Some(index) = mirror
                .iter()
                .position(|child| same_component(child, &component))
            {
                mirror.remove(index);
            }
        });
    }

    /// Upstream `clear`.
    pub fn clear(&self) {
        self.run_or_queue(|inner| {
            inner.children.clear();
            lock_shared(&inner.implicit_children).clear();
        });
    }

    // --- Rust additions: container list ops (mirror TuiMainScreen) ---------
    // These mirror the TuiMainScreen Rust-specific methods so a TuiHandle
    // proxy can forward them uniformly (T12-S5a showSelector region swap +
    // T32 switch_tui_mode child snapshot).

    /// Rust addition: insert a child at a specific position. Out-of-range
    /// indexes append. Mirrors [`TuiMainScreen::insert_child_at`].
    pub fn insert_child_at(&self, index: usize, component: SharedComponent) {
        self.run_or_queue(move |inner| {
            let index = index.min(inner.children.len());
            inner.children.insert(index, component.clone());
            lock_shared(&inner.implicit_children).insert(index, component);
        });
    }

    /// Rust addition: atomically swap `old` for `new`, preserving position.
    /// Mirrors [`TuiMainScreen::swap_child`].
    pub fn swap_child(&self, old: &SharedComponent, new: &SharedComponent) {
        let old = Arc::clone(old);
        let new = Arc::clone(new);
        self.run_or_queue(move |inner| {
            if let Some(index) = inner
                .children
                .iter()
                .position(|child| same_component(child, &old))
            {
                inner.children[index] = new.clone();
                lock_shared(&inner.implicit_children)[index] = new;
            } else if !inner
                .children
                .iter()
                .any(|child| same_component(child, &new))
            {
                inner.children.push(new.clone());
                lock_shared(&inner.implicit_children).push(new);
            }
        });
    }

    /// Rust addition: position of a child (identity comparison). `None` on
    /// lock contention or when not mounted.
    pub fn child_position(&self, component: &SharedComponent) -> Option<usize> {
        self.try_read(|inner| {
            inner
                .children
                .iter()
                .position(|child| same_component(child, component))
        })
        .flatten()
    }

    /// Rust addition: the number of top-level children. 0 on lock contention.
    pub fn children_len(&self) -> usize {
        self.try_read(|inner| inner.children.len()).unwrap_or(0)
    }

    /// Rust addition (T32 `switch_tui_mode`): snapshot the child list for
    /// re-mounting after a renderer swap. Empty on lock contention.
    pub fn children(&self) -> Vec<SharedComponent> {
        self.try_read(|inner| inner.children.clone())
            .unwrap_or_default()
    }

    // --- viewport API (tui-alt-screen.ts:185-210, 351-383) -----------------

    /// `get viewportTop` (tui-alt-screen.ts:185-187). 0 on lock contention.
    pub fn viewport_top(&self) -> usize {
        self.try_read(TuiAltScreenInner::viewport_top_inner)
            .unwrap_or(0)
    }

    /// `get isFollowingOutput` (tui-alt-screen.ts:189-191). `false` on lock
    /// contention.
    pub fn is_following_output(&self) -> bool {
        self.try_read(TuiAltScreenInner::is_following_output_inner)
            .unwrap_or(false)
    }

    /// `setLayoutRoot` (tui-alt-screen.ts:193-198).
    pub fn set_layout_root(&self, root: Option<SharedComponent>) {
        self.run_or_queue(move |inner| {
            let unchanged = match (&inner.layout_root, &root) {
                (None, None) => true,
                (Some(current), Some(next)) => same_component(current, next),
                _ => false,
            };
            if unchanged {
                return;
            }
            inner.layout_root = root;
            inner.current_layout = None;
            inner.request_render(false);
        });
    }

    /// `render` override (tui-alt-screen.ts:200-202): the layout root when
    /// set, else the child list. 0 lines on lock contention.
    pub fn render(&self, width: usize) -> Vec<String> {
        self.try_read(|inner| inner.render_document(width))
            .unwrap_or_default()
    }

    /// `getMountedRoots` (tui-alt-screen.ts:204-206).
    pub fn get_mounted_roots(&self) -> Vec<SharedComponent> {
        self.try_read(|inner| match &inner.layout_root {
            Some(root) => vec![root.clone()],
            None => inner.children.clone(),
        })
        .unwrap_or_default()
    }

    /// `getPrimaryScrollView` (tui-alt-screen.ts:208-210): the layout's
    /// primary scroll view, or the implicit one before the first frame (and
    /// on lock contention).
    pub fn get_primary_scroll_view(&self) -> SharedComponent {
        self.try_read(TuiAltScreenInner::get_primary_scroll_view)
            .unwrap_or_else(|| self.implicit_scroll_view.clone())
    }

    /// `scrollBy` (tui-alt-screen.ts:351-354); the unconsumed overscroll
    /// delta is dropped, like upstream.
    pub fn scroll_by(&self, lines: i64) {
        self.run_or_queue(move |inner| inner.scroll_by_inner(lines));
    }

    /// `scrollToTop` (tui-alt-screen.ts:356-359).
    pub fn scroll_to_top(&self) {
        self.run_or_queue(|inner| {
            let scroll_view = inner.get_primary_scroll_view();
            with_scroll_view(&scroll_view, ScrollView::scroll_to_start);
            inner.request_render(false);
        });
    }

    /// `scrollToBottom` (tui-alt-screen.ts:361-364).
    pub fn scroll_to_bottom(&self) {
        self.run_or_queue(|inner| {
            let scroll_view = inner.get_primary_scroll_view();
            with_scroll_view(&scroll_view, ScrollView::scroll_to_end);
            inner.request_render(false);
        });
    }

    /// `flash` (tui-alt-screen.ts:380-383): show a transient message in the
    /// alternate-screen flash stack. `None` uses the upstream default
    /// duration ([`DEFAULT_DURATION_MS`]).
    pub fn flash(&self, message: &str, duration_ms: Option<u64>) {
        let message = message.to_string();
        self.run_or_queue(move |inner| {
            inner
                .flashes
                .flash(message, duration_ms.unwrap_or(DEFAULT_DURATION_MS));
        });
    }

    // --- focus / overlay / listener API (same surface as TuiMainScreen) ----

    /// Upstream `setFocus` (tui.ts:368).
    pub fn set_focus(&self, component: Option<SharedComponent>) {
        self.run_or_queue(move |inner| inner.set_focus(component));
    }

    /// Never-blocking `setFocus` variant for callers that hold a
    /// component-container lock (see `TuiMainScreen::set_focus_nonblocking`
    /// for the ABBA rationale; same shape here for the fullscreen
    /// renderer).
    pub fn set_focus_nonblocking(&self, component: Option<SharedComponent>) {
        match self.inner.try_lock() {
            Ok(mut inner) => {
                inner.set_focus(component);
            }
            Err(_) => {
                lock_shared(&self.pending).push(Box::new(move |inner| inner.set_focus(component)))
            }
        }
    }

    /// Upstream `getFocusedComponent` (tui.ts:414-416). `None` on lock
    /// contention.
    pub fn get_focused_component(&self) -> Option<SharedComponent> {
        self.try_read(|inner| inner.focused_component.clone())
            .flatten()
    }

    /// Upstream `showOverlay` (tui.ts:495). Returns a handle to control the
    /// overlay's visibility and focus.
    pub fn show_overlay(
        &self,
        component: SharedComponent,
        options: Option<OverlayOptions>,
    ) -> OverlayHandle {
        let entry_id = self.next_overlay_id.fetch_add(1, Ordering::Relaxed);
        self.run_or_queue(move |inner| inner.show_overlay(entry_id, component, options));
        OverlayHandle::new(Arc::new(self.clone()), entry_id)
    }

    /// Upstream `hideOverlay` (tui.ts:591).
    pub fn hide_overlay(&self) {
        self.run_or_queue(|inner| inner.hide_overlay());
    }

    /// Upstream `hasOverlay` (tui.ts:607). `false` on lock contention.
    pub fn has_overlay(&self) -> bool {
        self.try_read(|inner| inner.has_overlay()).unwrap_or(false)
    }

    /// Upstream `get hasOverlayEntries` (tui.ts:358-360). `false` on lock
    /// contention.
    pub fn has_overlay_entries(&self) -> bool {
        self.try_read(|inner| inner.has_overlay_entries())
            .unwrap_or(false)
    }

    /// Upstream `addInputListener` (tui.ts:651). Returns an id for
    /// [`TuiAltScreen::remove_input_listener`].
    pub fn add_input_listener(&self, listener: TuiInputListener) -> u64 {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.run_or_queue(move |inner| inner.input_listeners.push((id, listener)));
        id
    }

    /// Upstream `removeInputListener` (tui.ts:658); by id.
    pub fn remove_input_listener(&self, id: u64) {
        self.run_or_queue(move |inner| inner.input_listeners.retain(|(lid, _)| *lid != id));
    }

    /// Global callback for the debug key (Shift+Ctrl+D) (upstream `onDebug`,
    /// tui.ts:305).
    pub fn set_on_debug(&self, on_debug: Option<Box<dyn FnMut() + Send>>) {
        self.run_or_queue(move |inner| inner.on_debug = on_debug);
    }

    /// Take the debug callback so `switch_tui_mode` can move it to the new
    /// renderer (interactive-mode.ts:798, 816). `None` on lock contention
    /// (same fallback discipline as [`TuiAltScreen::children`]).
    pub fn take_on_debug(&self) -> Option<Box<dyn FnMut() + Send>> {
        self.inner
            .try_lock()
            .ok()
            .and_then(|mut inner| inner.on_debug.take())
    }

    /// Upstream `onTerminalColorSchemeChange` (tui.ts:662).
    pub fn on_terminal_color_scheme_change(&self, listener: TerminalColorSchemeListener) -> u64 {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.run_or_queue(move |inner| inner.terminal_color_scheme_listeners.push((id, listener)));
        id
    }

    /// Remove a color scheme listener registered with
    /// [`TuiAltScreen::on_terminal_color_scheme_change`].
    pub fn remove_terminal_color_scheme_listener(&self, id: u64) {
        self.run_or_queue(move |inner| {
            inner
                .terminal_color_scheme_listeners
                .retain(|(lid, _)| *lid != id);
        });
    }

    /// Upstream `setTerminalColorSchemeNotifications` (tui.ts:669).
    pub fn set_terminal_color_scheme_notifications(&self, enabled: bool) {
        self.run_or_queue(move |inner| inner.set_terminal_color_scheme_notifications(enabled));
    }

    /// Number of registered terminal color-scheme listeners (observability
    /// for the `switch_tui_mode` theme-listener rebind,
    /// interactive-mode.ts:827).
    pub fn terminal_color_scheme_listener_count(&self) -> usize {
        self.try_read(|inner| inner.terminal_color_scheme_listeners.len())
            .unwrap_or(0)
    }

    /// Upstream `queryTerminalBackgroundColor` (tui.ts:1670); the timeout is
    /// fired by [`TuiAltScreen::tick`].
    pub fn query_terminal_background_color(
        &self,
        timeout: Duration,
    ) -> oneshot::Receiver<Option<RgbColor>> {
        let (sender, receiver) = oneshot::channel();
        let deadline = Instant::now() + timeout;
        self.run_or_queue(move |inner| {
            inner
                .pending_osc11_background_queries
                .push_back(PendingOsc11BackgroundQuery {
                    settled: false,
                    sender: Some(sender),
                    deadline: Some(deadline),
                });
            inner.pending_osc11_background_replies += 1;
            inner.terminal().write("\x1b]11;?\x07");
        });
        receiver
    }

    /// Upstream `queryTerminalColorScheme` (tui.ts:1698).
    pub fn query_terminal_color_scheme(
        &self,
        timeout: Duration,
    ) -> oneshot::Receiver<Option<TerminalColorScheme>> {
        let (sender, receiver) = oneshot::channel();
        let deadline = Instant::now() + timeout;
        self.run_or_queue(move |inner| {
            inner
                .pending_terminal_color_scheme_queries
                .push(PendingTerminalColorSchemeQuery {
                    settled: false,
                    sender: Some(sender),
                    deadline: Some(deadline),
                });
            inner.terminal().write("\x1b[?996n");
        });
        receiver
    }

    /// Upstream `get fullRedraws` (tui.ts:338). 0 on lock contention.
    pub fn full_redraws(&self) -> u64 {
        self.try_read(|inner| inner.full_redraw_count).unwrap_or(0)
    }

    /// Upstream `getShowHardwareCursor` (tui.ts:342).
    pub fn get_show_hardware_cursor(&self) -> bool {
        self.try_read(|inner| inner.show_hardware_cursor)
            .unwrap_or(false)
    }

    /// Upstream `setShowHardwareCursor` (tui.ts:346).
    pub fn set_show_hardware_cursor(&self, enabled: bool) {
        self.run_or_queue(move |inner| inner.set_show_hardware_cursor(enabled));
    }

    /// Upstream `getClearOnShrink` (tui.ts:355).
    pub fn get_clear_on_shrink(&self) -> bool {
        self.try_read(|inner| inner.clear_on_shrink)
            .unwrap_or(false)
    }

    /// Upstream `setClearOnShrink` (tui.ts:364).
    pub fn set_clear_on_shrink(&self, enabled: bool) {
        self.run_or_queue(move |inner| inner.clear_on_shrink = enabled);
    }

    /// Upstream `invalidate` override (tui.ts:686-689 via `getMountedRoots`):
    /// mounted roots plus overlays.
    pub fn invalidate(&self) {
        self.run_or_queue(TuiAltScreenInner::invalidate_mounted);
    }

    /// Apply the fullscreen scrollbar setting to the primary scroll view
    /// (interactive-mode.ts:1894-1896 @ 6129a353b). Rust addition for T32.
    pub fn set_fullscreen_scrollbar(&self, mode: ScrollbarMode) {
        let scroll_view = self.get_primary_scroll_view();
        with_scroll_view(&scroll_view, |sv| sv.set_scrollbar(mode));
    }

    /// `getCopyOnSelect` (tui-alt-screen.ts:284-286 @ 9841914, 4e4949299).
    /// `true` on lock contention (the default).
    pub fn get_copy_on_select(&self) -> bool {
        self.try_read(|inner| inner.copy_on_select).unwrap_or(true)
    }

    /// `setCopyOnSelect` (tui-alt-screen.ts:288-290): runtime toggle.
    pub fn set_copy_on_select(&self, enabled: bool) {
        self.run_or_queue(move |inner| inner.copy_on_select = enabled);
    }

    /// `hasActiveSelection` (tui-alt-screen.ts:293-295): whether the
    /// fullscreen viewport has a non-empty active text selection. `false`
    /// on lock contention.
    pub fn has_active_selection(&self) -> bool {
        self.try_read(|inner| inner.get_active_selection_text().is_some())
            .unwrap_or(false)
    }

    /// `copyActiveSelectionToClipboard` (tui-alt-screen.ts:298-302): copy
    /// the active selection through the configured clipboard path; `false`
    /// when there is nothing to copy. Uses `try_lock`: callers inside the
    /// inner-lock-held input dispatch must defer via the interactive-mode
    /// event drain (blocking here would self-deadlock the non-reentrant
    /// mutex; `false` models "nothing copied synchronously").
    pub fn copy_active_selection_to_clipboard(&self) -> bool {
        let Ok(mut inner) = self.inner.try_lock() else {
            return false;
        };
        inner.copy_active_selection_to_clipboard()
    }

    /// Access the terminal (upstream `public terminal`, tui.ts:296). Same
    /// lock-ordering rules as [`TuiMainScreen::with_terminal`].
    pub fn with_terminal<R>(&self, f: impl FnOnce(&mut dyn Terminal) -> R) -> R {
        f(&mut **lock_shared(&self.terminal))
    }

    /// Cloneable capability handle for timer-driven components (same as
    /// [`TuiMainScreen::render_handle`]).
    pub fn render_handle(&self) -> RenderHandle {
        let schedule = Arc::downgrade(&self.schedule);
        RenderHandle::new(move || {
            if let Some(schedule) = schedule.upgrade() {
                schedule_render(&schedule, false, Instant::now());
            }
        })
    }

    /// Terminal row count, cached for lock-free reads (see
    /// [`TuiMainScreen::terminal_rows`]).
    pub fn terminal_rows(&self) -> u16 {
        self.size_cache.rows.load(Ordering::Relaxed)
    }

    /// Runtime flip of the win32 flag for the right-click-paste test
    /// (upstream stubs `process.platform` mid-test,
    /// tui-alt-screen.test.ts:223-229).
    #[cfg(test)]
    pub(crate) fn set_win32_for_test(&self, win32: bool) {
        self.run_or_queue(move |inner| inner.win32 = win32);
    }

    // --- lifecycle and driving --------------------------------------------

    /// Upstream `start` (tui.ts:691-705 @ 4181f66) with the alt-screen
    /// `beforeTerminalStart` hook (tui-alt-screen.ts:212-250) orchestrated
    /// before `terminal.start` (the enter sequence must be written first —
    /// tui-alt-screen.test.ts:1051).
    pub fn start(&self) {
        let mut inner = self.lock_inner();
        inner.before_terminal_start();
        let input_inbox = Arc::clone(&self.inbox);
        let on_input: InputHandler = Box::new(move |data: &str| {
            lock_shared(&input_inbox).push_back(InboxEvent::Input(data.to_string()));
        });
        let resize_inbox = Arc::clone(&self.inbox);
        let on_resize: ResizeHandler = Box::new(move || {
            lock_shared(&resize_inbox).push_back(InboxEvent::Resize);
        });
        inner.start_common(on_input, on_resize);
    }

    /// Upstream `stop(options)` (tui.ts:745-755 @ 4181f66).
    pub fn stop(&self, options: TuiStopOptions) {
        self.lock_inner().stop_internal(options);
    }

    /// Upstream `requestRender(force)` (tui.ts:765-774 @ 4181f66); `force`
    /// resets the render state first (tui-alt-screen.ts:344-349).
    pub fn request_render(&self, force: bool) {
        if force {
            self.run_or_queue(TuiAltScreenInner::reset_render_state);
        }
        schedule_render(&self.schedule, force, Instant::now());
    }

    /// Upstream `renderNow` (tui.ts:757-763 @ 4181f66): render synchronously,
    /// bypassing the throttle.
    pub fn render_now(&self, force: bool) {
        let mut inner = self.lock_inner();
        if force {
            inner.reset_render_state();
        }
        {
            let mut schedule = lock_shared(&self.schedule);
            schedule.requested = false;
            schedule.deadline = None;
            schedule.last_render_at = Some(Instant::now());
        }
        inner.do_render();
        drop(inner);
        self.drain_pending_ops();
    }

    /// The next instant at which [`TuiAltScreen::tick`] has work to do: a
    /// pending render deadline, the earliest unsettled introspection query
    /// timeout, a terminal-side flush deadline, a flash expiry, the selection
    /// auto-scroll, or a scrollbar hide deadline. While stopped, a pending
    /// render is deferred until the restart and not reported here (same
    /// convention as [`TuiMainScreen::next_deadline`]).
    pub fn next_deadline(&self) -> Option<Instant> {
        let render_deadline = {
            let schedule = lock_shared(&self.schedule);
            if schedule.requested {
                schedule.deadline
            } else {
                None
            }
        };
        let inner = self.lock_inner();
        let query_deadline = inner.next_query_deadline();
        let terminal_deadline = inner.terminal().next_flush_deadline();
        let render_deadline = if inner.stopped { None } else { render_deadline };
        let flash_deadline = inner.flashes.next_deadline();
        let auto_scroll_deadline = inner.selection_auto_scroll_next;
        let scrollbar_deadline = inner.next_scrollbar_deadline();
        [
            render_deadline,
            query_deadline,
            terminal_deadline,
            flash_deadline,
            auto_scroll_deadline,
            scrollbar_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Whether unprocessed input events, queued mutations, a pending render
    /// or an expired deadline (query timeout / flash expiry / auto-scroll /
    /// scrollbar hide) exist. While stopped, a pending render is deferred and
    /// does not count (same convention as [`TuiMainScreen::has_pending_work`]).
    pub fn has_pending_work(&self) -> bool {
        if !lock_shared(&self.inbox).is_empty() || !lock_shared(&self.pending).is_empty() {
            return true;
        }
        let render_requested = lock_shared(&self.schedule).requested;
        let inner = self.lock_inner();
        if render_requested && !inner.stopped {
            return true;
        }
        let now = Instant::now();
        if inner.has_expired_query(now) {
            return true;
        }
        if inner
            .flashes
            .next_deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            return true;
        }
        if inner
            .selection_auto_scroll_next
            .is_some_and(|deadline| now >= deadline)
        {
            return true;
        }
        inner.has_expired_scrollbar_deadline(now)
    }

    /// Drive the TUI: drain queued input events through the `handleInput`
    /// flow (viewport listener first), run queued mutations, then fire the
    /// expired deadlines and the render deadline (see
    /// [`TuiAltScreenInner::tick`]).
    pub fn tick(&self, now: Instant) {
        loop {
            let event = lock_shared(&self.inbox).pop_front();
            match event {
                Some(InboxEvent::Input(data)) => {
                    self.lock_inner().handle_input(&data);
                    self.drain_pending_ops();
                }
                Some(InboxEvent::Resize) => self.request_render(false),
                None => break,
            }
        }
        self.drain_pending_ops();
        self.lock_inner().tick(now);
        self.drain_pending_ops();
    }

    /// Wait up to `timeout` (`None` = indefinitely) for a terminal event,
    /// then drive the TUI like [`TuiAltScreen::tick`]. Same wait strategy as
    /// [`TuiMainScreen::pump`].
    pub fn pump(&self, timeout: Option<Duration>) -> bool {
        let source = lock_shared(&self.terminal).event_source();
        let Some(source) = source else {
            let dispatched = lock_shared(&self.terminal).pump(timeout);
            self.tick(Instant::now());
            return dispatched;
        };
        let first = source.wait(timeout);
        let mut dispatched = false;
        {
            let mut terminal = lock_shared(&self.terminal);
            if let Some(event) = first {
                terminal.dispatch_terminal_event(event);
                dispatched = true;
            }
            while let Some(event) = source.try_recv() {
                terminal.dispatch_terminal_event(event);
                dispatched = true;
            }
            terminal.tick(Instant::now());
        }
        self.tick(Instant::now());
        dispatched
    }
}

impl TuiAltScreenInner {
    /// `resetRenderState` (tui-alt-screen.ts:344-349), called by
    /// `request_render(true)` / `render_now(force)` / `before_terminal_start`.
    fn reset_render_state(&mut self) {
        self.previous_screen = Vec::new();
        self.previous_screen_width = 0;
        self.previous_screen_height = 0;
        self.current_layout = None;
    }

    /// `stop` (tui.ts:745-755 @ 4181f66) with the alt-screen
    /// `beforeTerminalStop` / `afterTerminalStop` hooks orchestrated by hand.
    fn stop_internal(&mut self, options: TuiStopOptions) {
        self.begin_stop();
        self.before_terminal_stop(options);
        self.end_stop();
        self.after_terminal_stop(options);
    }

    /// `beforeTerminalStart` (tui-alt-screen.ts:212-250): reset all
    /// interaction state, snapshot the image protocol (iTerm2 is demoted to
    /// no-images for the alt screen), pick the mouse sequence for
    /// multiplexers, and write the one-shot enter sequence BEFORE
    /// `terminal.start`.
    fn before_terminal_start(&mut self) {
        self.stop_selection_auto_scroll();
        self.selection_press_active = false;
        self.stop_scrollbar_hover();
        self.stop_scrollbar_drag();
        self.flashes.dispose();
        self.alt_screen_active = true;
        let capabilities = get_capabilities();
        self.image_protocol = capabilities.images;
        // The registry is process-global here (upstream: per-instance field);
        // clearing it only for Kitty instances keeps non-Kitty renderers (and
        // tests) from evicting entries they never populate — per-instance
        // semantics are preserved for the only protocol that uses the cache.
        if capabilities.images == Some(ImageProtocol::Kitty) {
            clear_kitty_image_cache();
        }
        if capabilities.images == Some(ImageProtocol::ITerm2) {
            self.saved_capabilities = Some(capabilities);
            set_capabilities(TerminalCapabilities {
                images: None,
                ..capabilities
            });
            self.invalidate_mounted();
        }
        self.last_document = Vec::new();
        self.selection_anchor = None;
        self.selection_focus = None;
        self.selection_granularity = SelectionGranularity::Character;
        self.selection_initial_range = None;
        self.last_click = None;
        self.pressed_url = None;
        self.selection_dragged = false;
        self.reset_render_state();
        // Multiplexers can lag when every pointer movement is forwarded.
        // Button-motion tracking preserves clicks, wheel events, selections,
        // and scrollbar dragging (tui-alt-screen.ts:237-238).
        let mouse_sequence = if is_multiplexer_env() {
            ENABLE_BUTTON_MOTION_MOUSE
        } else {
            ENABLE_ALL_MOTION_MOUSE
        };
        let mouse = if self.mouse_enabled {
            mouse_sequence
        } else {
            ""
        };
        self.terminal().write(&format!(
            "{ENTER_ALT_SCREEN}{DISABLE_AUTOWRAP}{mouse}\x1b[2J\x1b[H\x1b[?25l"
        ));
    }

    /// `beforeTerminalStop` (tui-alt-screen.ts:252-263): write the
    /// mouse-disable + autowrap-restore sequence (wrapped in synchronized
    /// output) BEFORE `terminal.stop`; the alt screen is exited in
    /// [`TuiAltScreenInner::after_terminal_stop`].
    fn before_terminal_stop(&mut self, _options: TuiStopOptions) {
        // `this.closeSearch()` (tui-alt-screen.ts:366 @ 9841914, 00121ed99).
        self.close_search();
        self.stop_selection_auto_scroll();
        self.selection_press_active = false;
        self.stop_scrollbar_hover();
        self.stop_scrollbar_drag();
        self.flashes.dispose();
        if !self.alt_screen_active {
            return;
        }
        let delete_images = self.delete_kitty_images();
        let disable_mouse = if self.mouse_enabled {
            DISABLE_MOUSE
        } else {
            ""
        };
        self.terminal().write(&format!(
            "{BEGIN_SYNCHRONIZED_OUTPUT}{delete_images}{disable_mouse}{ENABLE_AUTOWRAP}{END_SYNCHRONIZED_OUTPUT}"
        ));
        // See the note in `before_terminal_start`.
        if self.image_protocol == Some(ImageProtocol::Kitty) {
            clear_kitty_image_cache();
        }
    }

    /// `afterTerminalStop` (tui-alt-screen.ts:265-288): with `preserve_screen`
    /// only exit the alt screen; otherwise reprint the whole document on the
    /// main screen (OSC 133 prefixes stripped, cursor markers removed, line
    /// resets applied, overlong lines truncated), then restore autowrap and
    /// the cursor. Restores the capabilities saved for iTerm2.
    fn after_terminal_stop(&mut self, options: TuiStopOptions) {
        if !self.alt_screen_active {
            return;
        }
        self.alt_screen_active = false;
        if options.preserve_screen {
            self.terminal().write(&format!(
                "{BEGIN_SYNCHRONIZED_OUTPUT}{EXIT_ALT_SCREEN}\x1b[?25h{END_SYNCHRONIZED_OUTPUT}"
            ));
        } else {
            let width = usize::from(self.terminal().columns()).max(1);
            let document_lines: Vec<String> = self
                .render_document(width)
                .iter()
                .map(|line| strip_osc133_zone_prefix(line).to_string())
                .collect();
            let mut processed: Vec<String> = document_lines
                .iter()
                .map(|line| line.replace(CURSOR_MARKER, ""))
                .collect();
            TuiBase::apply_line_resets(&mut processed);
            self.last_document = processed
                .into_iter()
                .map(|line| {
                    if is_image_line(&line) || visible_width(&line) <= width {
                        line
                    } else {
                        slice_by_column(&line, 0, width, true)
                    }
                })
                .collect();
            let mut buffer =
                format!("{BEGIN_SYNCHRONIZED_OUTPUT}{EXIT_ALT_SCREEN}{DISABLE_AUTOWRAP}");
            for (row, line) in self.last_document.iter().enumerate() {
                if row > 0 {
                    buffer.push_str("\r\n");
                }
                buffer.push_str("\r\x1b[2K");
                buffer.push_str(line);
            }
            buffer.push_str(&format!(
                "\x1b[0m{ENABLE_AUTOWRAP}\r\n\x1b[?25h{END_SYNCHRONIZED_OUTPUT}"
            ));
            self.terminal().write(&buffer);
        }
        if let Some(saved) = self.saved_capabilities.take() {
            set_capabilities(saved);
        }
    }

    /// `deleteKittyImages` (tui-alt-screen.ts:290-292).
    fn delete_kitty_images(&self) -> String {
        if self.image_protocol == Some(ImageProtocol::Kitty) {
            delete_all_kitty_images()
        } else {
            String::new()
        }
    }

    /// `render` override body (tui-alt-screen.ts:200-202): the layout root
    /// when set, else the child list (`Container.render`).
    fn render_document(&self, width: usize) -> Vec<String> {
        if let Some(root) = &self.layout_root {
            return lock_component(root).render(width);
        }
        let mut lines = Vec::new();
        for child in &self.children {
            lines.extend(lock_component(child).render(width));
        }
        lines
    }

    /// `invalidate` override body (tui.ts:686-689 via `getMountedRoots`,
    /// tui-alt-screen.ts:204-206): the layout root (or the children) plus the
    /// overlay stack.
    fn invalidate_mounted(&mut self) {
        match &self.layout_root {
            Some(root) => lock_component(root).invalidate(),
            None => {
                for child in &self.children {
                    lock_component(child).invalidate();
                }
            }
        }
        for overlay in &self.overlay_stack {
            lock_component(&overlay.component).invalidate();
        }
    }

    /// `getPrimaryScrollView` (tui-alt-screen.ts:208-210).
    fn get_primary_scroll_view(&self) -> SharedComponent {
        self.current_layout
            .as_ref()
            .and_then(|layout| layout.primary_scroll_view.clone())
            .unwrap_or_else(|| self.implicit_scroll_view.clone())
    }

    fn viewport_top_inner(&self) -> usize {
        with_scroll_view(&self.get_primary_scroll_view(), ScrollView::scroll_top).unwrap_or(0)
    }

    fn is_following_output_inner(&self) -> bool {
        with_scroll_view(
            &self.get_primary_scroll_view(),
            ScrollView::is_following_end,
        )
        .unwrap_or(false)
    }

    /// `scrollBy` body (tui-alt-screen.ts:351-354).
    fn scroll_by_inner(&mut self, lines: i64) {
        let scroll_view = self.get_primary_scroll_view();
        with_scroll_view(&scroll_view, |view| {
            view.scroll_by(lines);
        });
        self.request_render(false);
    }

    /// `scrollToPrompt` (tui-alt-screen.ts:366-378): scan the primary scroll
    /// view's content lines for the next/previous OSC 133 prompt-start zone.
    fn scroll_to_prompt(&mut self, direction: i64) {
        let Some(lines) = self.current_layout.as_ref().and_then(|layout| {
            let scroll_view = self.get_primary_scroll_view();
            get_scroll_view_box(layout, &scroll_view)
                .and_then(|layout_box| layout_box.scroll_content_lines.clone())
        }) else {
            return;
        };
        let scroll_view = self.get_primary_scroll_view();
        let Some(scroll_top) = with_scroll_view(&scroll_view, ScrollView::scroll_top) else {
            return;
        };
        let mut row = scroll_top as i64 + direction;
        while row >= 0 && (row as usize) < lines.len() {
            if is_osc133_prompt_start(&lines[row as usize]) {
                with_scroll_view(&scroll_view, |view| view.scroll_to(row));
                self.request_render(false);
                return;
            }
            row += direction;
        }
    }

    // --- transcript search (tui-alt-screen.ts:494-638 @ 9841914,
    //     00121ed99 / 7d399e7be / 2d4116333) ------------------------------

    /// Callback-side `run_or_queue`: search-component callbacks fire while
    /// the inner lock is held (input dispatch), so mutations queue into the
    /// shared pending list and drain at the end of the current input event
    /// ([`TuiAltScreen::tick`], same ordering as upstream's synchronous
    /// closure call — the op runs before the next render).
    fn queue_or_run(
        weak: &Weak<Mutex<TuiAltScreenInner>>,
        pending: &Arc<Mutex<Vec<PendingOp>>>,
        op: impl FnOnce(&mut TuiAltScreenInner) + Send + 'static,
    ) {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let mut op: Option<PendingOp> = Some(Box::new(op));
        let direct = match inner.try_lock() {
            Ok(mut guard) => {
                if let Some(op) = op.take() {
                    op(&mut guard);
                }
                drop(guard);
                op.is_none()
            }
            Err(_) => false,
        };
        if direct {
            drain_pending_into(&inner, pending);
        } else if let Some(op) = op.take() {
            lock_shared(pending).push(op);
        }
    }

    /// `toggleSearch` (tui-alt-screen.ts:495-520): open the search overlay
    /// anchored top-right at 40% width (min 32, margin 1), or close an
    /// active search.
    fn toggle_search(&mut self) {
        if self.active_search.is_some() {
            self.close_search();
            return;
        }
        let weak = self.self_handle.clone();
        let pending = Arc::clone(&self.pending_ops);
        let on_query_change: crate::alt_screen_search::SearchQueryChangeFn =
            Arc::new(move |query: &str| {
                let query = query.to_string();
                Self::queue_or_run(&weak, &pending, move |inner| {
                    inner.update_search_query(&query);
                });
            });
        let component = shared_component(AltScreenSearchComponent::new(
            on_query_change,
            Some(Arc::clone(&self.search_navigation_button_style)),
        ));
        let entry_id = self.next_overlay_id.fetch_add(1, Ordering::Relaxed);
        self.active_search = Some(ActiveSearch {
            component: component.clone(),
            index: AltScreenSearchIndex::new(),
            overlay_entry_id: entry_id,
            query: String::new(),
            matches: Arc::new(Vec::new()),
            selected_index: -1,
            selected_key: None,
            anchor_row: with_scroll_view(&self.get_primary_scroll_view(), ScrollView::scroll_top)
                .unwrap_or(0),
            selection_mode: SearchSelectionMode::Query,
        });
        self.show_overlay(
            entry_id,
            component,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::TopRight),
                width: Some(SizeValue::Percent(40.0)),
                min_width: Some(32),
                margin: Some(OverlayMarginSpec::Uniform(1)),
                ..OverlayOptions::default()
            }),
        );
    }

    /// `closeSearch` (tui-alt-screen.ts:522-528).
    fn close_search(&mut self) {
        let Some(search) = self.active_search.take() else {
            return;
        };
        self.overlay_hide(search.overlay_entry_id);
        self.request_render(false);
    }

    /// `updateSearchQuery` (tui-alt-screen.ts:530-539): re-anchor on the
    /// current selection (or the current scroll position), then re-run the
    /// query-mode selection on the next refresh.
    fn update_search_query(&mut self, query: &str) {
        let anchor_row = match self.active_search.as_ref() {
            None => return,
            Some(search) if query == search.query => return,
            Some(search) => search
                .matches
                .get(search.selected_index.max(0) as usize)
                .and_then(|selected| selected.segments.first())
                .map(|segment| segment.row)
                .or_else(|| {
                    with_scroll_view(&self.get_primary_scroll_view(), ScrollView::scroll_top)
                })
                .unwrap_or(0),
        };
        let Some(search) = self.active_search.as_mut() else {
            return;
        };
        search.anchor_row = anchor_row;
        search.query = query.to_string();
        search.selection_mode = SearchSelectionMode::Query;
        let component = Arc::clone(&search.component);
        let mut guard = lock_component(&component);
        if let Some(component) = guard.as_search_component_mut() {
            component.set_result(-1, 0);
        }
        drop(guard);
        self.request_render(false);
    }

    /// `navigateSearch` (tui-alt-screen.ts:541-546).
    fn navigate_search(&mut self, direction: i64) {
        let Some(search) = self.active_search.as_mut() else {
            return;
        };
        if search.query.is_empty() {
            return;
        }
        search.selection_mode = if direction < 0 {
            SearchSelectionMode::Previous
        } else {
            SearchSelectionMode::Next
        };
        self.request_render(false);
    }

    /// `getSearchNavigationDirectionAt` (tui-alt-screen.ts:548-555).
    fn get_search_navigation_direction_at(&self, x: u32, y: u32) -> Option<i32> {
        let search = self.active_search.as_ref()?;
        let bounds = self.overlay_get_bounds(search.overlay_entry_id)?;
        if (x as isize) < bounds.col as isize
            || (x as isize) >= bounds.col as isize + bounds.width as isize
            || (y as isize) < bounds.row as isize
            || (y as isize) >= bounds.row as isize + bounds.height as isize
        {
            return None;
        }
        let component = lock_component(&search.component);
        let search_component = component.as_search_component()?;
        search_component.get_navigation_direction_at(
            (y as isize - bounds.row as isize) as i32,
            (x as isize - bounds.col as isize) as i32,
        )
    }

    /// `handleSearchMouseEvent` (tui-alt-screen.ts:558-569): hover tracking
    /// for the ↑/↓ buttons and press-activated navigation.
    fn handle_search_mouse_event(&mut self, event: &SgrMouseEvent) -> bool {
        let Some(search) = self.active_search.as_ref() else {
            return false;
        };
        let component = Arc::clone(&search.component);
        let direction = self.get_search_navigation_direction_at(event.x, event.y);
        let changed = {
            let mut guard = lock_component(&component);
            match guard.as_search_component_mut() {
                Some(component) => component.set_hovered_navigation_direction(direction),
                None => false,
            }
        };
        if changed {
            self.request_render(false);
        }
        if direction.is_none()
            || event.release
            || (event.button & 32) != 0
            || (event.button & 3) != 0
        {
            return false;
        }
        self.navigate_search(direction.expect("checked above") as i64);
        true
    }

    /// `refreshSearch` (tui-alt-screen.ts:570-638): re-run the index, apply
    /// the selection-mode state machine, and reveal the selection. Returns
    /// whether the scroll position changed (the caller re-renders the
    /// layout).
    fn refresh_search(&mut self, layout: &LayoutFrame) -> bool {
        let Some(search) = self.active_search.as_mut() else {
            return false;
        };
        let scroll_view = layout
            .primary_scroll_view
            .clone()
            .unwrap_or_else(|| self.implicit_scroll_view.clone());
        let lines = get_scroll_view_box(layout, &scroll_view)
            .and_then(|layout_box| layout_box.scroll_content_lines.clone());
        // (upstream `!lines || !search.query.trim()` — both arms share the
        // same reset)
        let bail_no_query = |search: &mut ActiveSearch| {
            search.matches = Arc::new(Vec::new());
            search.selected_index = -1;
            search.selected_key = None;
            search.selection_mode = SearchSelectionMode::Retain;
            let component = Arc::clone(&search.component);
            let mut guard = lock_component(&component);
            if let Some(component) = guard.as_search_component_mut() {
                component.set_result(-1, 0);
            }
        };
        if lines.is_none() {
            bail_no_query(search);
            return false;
        }
        if search.query.trim().is_empty() {
            bail_no_query(search);
            return false;
        }
        let lines = lines.expect("checked above");

        let should_reveal_selection = search.selection_mode != SearchSelectionMode::Retain;
        let query = search.query.clone();
        // `Arc<[String]>` derefs to the slice — no per-frame copy.
        let result = search.index.search(&lines, &query);
        let matches = Arc::clone(&result.matches);
        search.matches = Arc::clone(&matches);
        if !result.changed && search.selection_mode == SearchSelectionMode::Retain {
            return false;
        }

        let exact_index: i64 = if result.changed {
            match &search.selected_key {
                Some(key) => matches
                    .iter()
                    .position(|search_match| &get_alt_screen_search_match_key(search_match) == key)
                    .map(|index| index as i64)
                    .unwrap_or(-1),
                None => -1,
            }
        } else {
            search.selected_index
        };
        let mut selected_index: i64 = -1;
        if !matches.is_empty() {
            let last = matches.len() as i64 - 1;
            match search.selection_mode {
                SearchSelectionMode::Query => {
                    // Binary search for the first match at or after the
                    // anchor row (tui-alt-screen.ts:598-603).
                    let mut low: i64 = 0;
                    let mut high: i64 = matches.len() as i64;
                    while low < high {
                        let middle = low + (high - low) / 2;
                        let row = matches[middle as usize]
                            .segments
                            .first()
                            .map(|segment| segment.row as i64)
                            .unwrap_or(0);
                        if row < search.anchor_row as i64 {
                            low = middle + 1;
                        } else {
                            high = middle;
                        }
                    }
                    selected_index = if low < matches.len() as i64 { low } else { 0 };
                }
                SearchSelectionMode::Next => {
                    let base = if exact_index >= 0 {
                        exact_index
                    } else {
                        search.selected_index.min(last)
                    };
                    selected_index = if base < 0 {
                        0
                    } else {
                        (base + 1) % matches.len() as i64
                    };
                }
                SearchSelectionMode::Previous => {
                    let base = if exact_index >= 0 {
                        exact_index
                    } else {
                        search.selected_index.min(last)
                    };
                    selected_index = if base < 0 {
                        matches.len() as i64 - 1
                    } else {
                        (base - 1 + matches.len() as i64) % matches.len() as i64
                    };
                }
                SearchSelectionMode::Retain => {
                    selected_index = if exact_index >= 0 {
                        exact_index
                    } else {
                        search.selected_index.max(0).min(last)
                    };
                }
            }
        }

        search.selected_index = selected_index;
        search.selected_key = if selected_index >= 0 {
            matches
                .get(selected_index as usize)
                .map(get_alt_screen_search_match_key)
        } else {
            None
        };
        search.selection_mode = SearchSelectionMode::Retain;
        {
            let component = Arc::clone(&search.component);
            let mut guard = lock_component(&component);
            if let Some(component) = guard.as_search_component_mut() {
                component.set_result(selected_index, matches.len());
            }
        }
        if !should_reveal_selection {
            return false;
        }

        let selected = matches.get(selected_index.max(0) as usize);
        let (first_segment, last_segment) =
            match selected.map(|selected| (selected.segments.first(), selected.segments.last())) {
                Some((Some(first), Some(last))) => (first, last),
                _ => return false,
            };
        let viewport_height =
            with_scroll_view(&scroll_view, ScrollView::viewport_height).unwrap_or(0);
        let box_ = get_scroll_view_box(layout, &scroll_view);
        if box_.is_none() || viewport_height == 0 {
            return false;
        }
        let before = with_scroll_view(&scroll_view, ScrollView::scroll_top).unwrap_or(0) as i64;
        let visible_bottom = before + viewport_height as i64 - 1;
        let mut target = before;
        if (first_segment.row as i64) < before || (last_segment.row as i64) > visible_bottom {
            target = first_segment.row as i64 - (viewport_height as i64 / 3);
        }
        with_scroll_view(&scroll_view, |view| {
            view.scroll_to_with_options(
                target,
                ScrollViewScrollToOptions {
                    disable_follow: true,
                },
            )
        });
        let after = with_scroll_view(&scroll_view, ScrollView::scroll_top).unwrap_or(0) as i64;
        after != before
    }

    // --- input routing (tui-alt-screen.ts:385-460) -------------------------

    /// `handleInput` with the upstream first-position viewport listener
    /// emulated as a pre-dispatch step (see the header note).
    fn handle_input(&mut self, data: &str) {
        if self.consume_osc11_background_response(data) {
            return;
        }
        if self.consume_terminal_color_scheme_report(data) {
            return;
        }
        if let Some(result) = self.handle_viewport_input(data) {
            if result.consume {
                return;
            }
        }
        self.handle_input_dispatch(data);
    }

    /// `handleViewportInput` (tui-alt-screen.ts:385-460 @ 9841914): the
    /// first input listener — focus events, mouse (component dispatch /
    /// paste / scrollbar / selection), the mouse-sequence catch-all, and the
    /// eight `tui.altScreen.*` keybinding actions. Returns `Some(consume)`
    /// when the input is swallowed. Mouse dispatch and the overlay-input
    /// deferral (`2e4d23959`) are part of the V14-14 port of
    /// `71026970a`.
    fn handle_viewport_input(&mut self, data: &str) -> Option<TuiInputListenerResult> {
        let consume = || TuiInputListenerResult {
            consume: true,
            data: None,
        };

        if data == FOCUS_OUT {
            // `4a879dd75` (#7892): a lost focus clears the selection/gesture
            // state as before, but a re-render is only requested when a
            // non-empty active selection was actually cleared — an empty
            // selection must not wipe the screen state on every focus loss.
            let had_active_selection = self.selection_press_active;
            let had_non_empty_active_selection =
                had_active_selection && self.get_selection_bounds().is_some();
            self.selection_press_active = false;
            self.stop_selection_auto_scroll();
            self.stop_scrollbar_hover();
            if self.active_search.as_ref().is_some_and(|search| {
                let component = Arc::clone(&search.component);
                let mut guard = lock_component(&component);
                guard
                    .as_search_component_mut()
                    .is_some_and(|component| component.set_hovered_navigation_direction(None))
            }) {
                self.request_render(false);
            }
            self.stop_scrollbar_drag();
            self.pressed_url = None;
            self.selection_dragged = false;
            self.clear_component_mouse_gesture();
            self.last_component_click = None;
            if had_active_selection {
                self.selection_anchor = None;
                self.selection_focus = None;
                self.selection_granularity = SelectionGranularity::Character;
                self.selection_initial_range = None;
                if had_non_empty_active_selection {
                    self.request_render(false);
                }
            }
            self.last_click = None;
            return Some(consume());
        }
        if data == FOCUS_IN {
            return Some(consume());
        }

        if let Some(wheel_event) = parse_wheel_event(data) {
            // Wheel events reach components first (`71026970a`); the
            // viewport scroll only runs when no component consumed them and
            // no overlay holds focus (`2e4d23959`, FR-G).
            let event = self.create_mouse_event(
                TuiMouseEventType::Wheel,
                wheel_event.button,
                wheel_event.x,
                wheel_event.y,
                Some(i64::from(wheel_event.direction) * self.wheel_scroll_lines as i64),
                None,
            );
            let overlay = self.dispatch_mouse_to_overlay(&event);
            let result = overlay.result.clone().or_else(|| {
                if overlay.hit {
                    None
                } else {
                    self.dispatch_mouse_to_layout(&event)
                }
            });
            if let Some(result) = result {
                if self.apply_mouse_dispatch_result(&event, result) {
                    self.request_render(false);
                }
                return Some(consume());
            }
            if self.should_defer_viewport_input_to_overlay() {
                return None;
            }
            self.route_wheel(wheel_event);
            return Some(consume());
        }
        if let Some(mouse_event) = parse_sgr_mouse_event(data) {
            self.handle_mouse_event(mouse_event);
            return Some(consume());
        }
        if is_mouse_sequence(data) {
            return Some(consume());
        }

        let is_release = is_key_release(data);
        let keybindings = get_keybindings()
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Search toggle runs before the overlay-input deferral
        // (tui-alt-screen.ts:704-707 @ 9841914, 00121ed99).
        if keybindings.matches(data, Keybinding::AltScreenSearch) {
            if !is_release {
                self.toggle_search();
            }
            return Some(consume());
        }
        // While the search overlay holds focus, its navigation/close keys are
        // consumed here and never reach the search input
        // (tui-alt-screen.ts:708-719).
        if self.search_overlay_is_focused() {
            if keybindings.matches(data, Keybinding::AltScreenSearchNext) {
                if !is_release {
                    self.navigate_search(1);
                }
                return Some(consume());
            }
            if keybindings.matches(data, Keybinding::AltScreenSearchPrevious) {
                if !is_release {
                    self.navigate_search(-1);
                }
                return Some(consume());
            }
            if keybindings.matches(data, Keybinding::AltScreenSearchClose) {
                if !is_release {
                    self.close_search();
                }
                return Some(consume());
            }
        }
        // The active-search overlay exception (tui-alt-screen.ts:644-645
        // second conjunct, V14-15): a focused search overlay still lets
        // wheel/page keys control the transcript.
        if self.should_defer_viewport_input_to_overlay() {
            return None;
        }
        let primary = self.get_primary_scroll_view();
        let viewport_height = with_scroll_view(&primary, ScrollView::viewport_height).unwrap_or(0);
        if keybindings.matches(data, Keybinding::AltScreenPageUp) {
            if !is_release {
                self.scroll_by_inner(
                    -(viewport_height.saturating_sub(PAGE_SCROLL_OVERLAP).max(1) as i64),
                );
            }
            return Some(consume());
        }
        if keybindings.matches(data, Keybinding::AltScreenPageDown) {
            if !is_release {
                self.scroll_by_inner(
                    viewport_height.saturating_sub(PAGE_SCROLL_OVERLAP).max(1) as i64
                );
            }
            return Some(consume());
        }
        if keybindings.matches(data, Keybinding::AltScreenHalfPageUp) {
            if !is_release {
                self.scroll_by_inner(-((viewport_height / 2).max(1) as i64));
            }
            return Some(consume());
        }
        if keybindings.matches(data, Keybinding::AltScreenHalfPageDown) {
            if !is_release {
                self.scroll_by_inner((viewport_height / 2).max(1) as i64);
            }
            return Some(consume());
        }
        if keybindings.matches(data, Keybinding::AltScreenLineUp) {
            if !is_release {
                self.scroll_by_inner(-1);
            }
            return Some(consume());
        }
        if keybindings.matches(data, Keybinding::AltScreenLineDown) {
            if !is_release {
                self.scroll_by_inner(1);
            }
            return Some(consume());
        }
        if keybindings.matches(data, Keybinding::AltScreenPreviousPrompt) {
            if !is_release {
                self.scroll_to_prompt(-1);
            }
            return Some(consume());
        }
        if keybindings.matches(data, Keybinding::AltScreenNextPrompt) {
            if !is_release {
                self.scroll_to_prompt(1);
            }
            return Some(consume());
        }
        if keybindings.matches(data, Keybinding::AltScreenTop) {
            if !is_release {
                let scroll_view = self.get_primary_scroll_view();
                with_scroll_view(&scroll_view, ScrollView::scroll_to_start);
                self.request_render(false);
            }
            return Some(consume());
        }
        if keybindings.matches(data, Keybinding::AltScreenBottom) {
            if !is_release {
                let scroll_view = self.get_primary_scroll_view();
                with_scroll_view(&scroll_view, ScrollView::scroll_to_end);
                self.request_render(false);
            }
            return Some(consume());
        }
        None
    }

    // --- component mouse dispatch (tui-alt-screen.ts:642-939 @ 9841914,
    //     introduced by 71026970a) -----------------------------------------

    /// `shouldDeferViewportInputToOverlay` (tui-alt-screen.ts:644-645 @
    /// 9841914, 2e4d23959 + 00121ed99): while an overlay holds keyboard
    /// focus, viewport scrolling keys and unconsumed wheel events let the
    /// overlay handle the input instead — except the active search overlay,
    /// which only takes the keys it consumes above and lets wheel/page keys
    /// keep controlling the transcript (second conjunct).
    fn should_defer_viewport_input_to_overlay(&self) -> bool {
        self.base.is_overlay_focused() && !self.search_overlay_is_focused()
    }

    /// `this.activeSearch?.overlay?.isFocused() === true`
    /// (tui-alt-screen.ts:708, 645): the search overlay's component holds
    /// keyboard focus.
    fn search_overlay_is_focused(&self) -> bool {
        self.active_search.as_ref().is_some_and(|search| {
            self.focused_component
                .as_ref()
                .is_some_and(|focused| same_component(focused, &search.component))
        })
    }

    /// `clearComponentMouseGesture` (tui-alt-screen.ts:647-653 @ 9841914):
    /// drop the component press/capture gesture state (focus loss, gesture
    /// end).
    fn clear_component_mouse_gesture(&mut self) {
        self.mouse_capture = None;
        self.mouse_press_target = None;
        self.mouse_press_point = None;
        self.mouse_press_moved = false;
    }

    /// `decodeMouseButton` (tui-alt-screen.ts:770-783 @ 9841914).
    fn decode_mouse_button(button: u32) -> TuiMouseButton {
        match button & 3 {
            0 => TuiMouseButton::Left,
            1 => TuiMouseButton::Middle,
            2 => TuiMouseButton::Right,
            _ => TuiMouseButton::None,
        }
    }

    /// `createMouseEvent` (tui-alt-screen.ts:785-806 @ 9841914): the raw SGR
    /// fields become a normalized event — screen == local coordinates at the
    /// top level, bounds clamped to at least one cell, modifiers from the
    /// SGR modifier bits, `button: "none"` for wheel events.
    fn create_mouse_event(
        &self,
        event_type: TuiMouseEventType,
        button: u32,
        x: u32,
        y: u32,
        wheel_delta: Option<i64>,
        click_count: Option<u32>,
    ) -> TuiMouseEvent {
        let rows = self.terminal().rows();
        let columns = self.terminal().columns();
        TuiMouseEvent {
            event_type,
            button: if event_type == TuiMouseEventType::Wheel {
                TuiMouseButton::None
            } else {
                Self::decode_mouse_button(button)
            },
            x: x as isize,
            y: y as isize,
            screen_x: x as isize,
            screen_y: y as isize,
            width: 1.max(usize::from(columns)) as isize,
            height: 1.max(usize::from(rows)) as isize,
            shift: button & 4 != 0,
            alt: button & 8 != 0,
            ctrl: button & 16 != 0,
            wheel_delta: wheel_delta.map(|delta| delta as isize),
            click_count,
        }
    }

    /// `dispatchMouseToOverlay` (tui.ts:824-848 @ 9841914): dispatch to the
    /// visually topmost rendered overlay under the pointer; a hit without a
    /// result suppresses the layout dispatch below it.
    fn dispatch_mouse_to_overlay(&self, event: &TuiMouseEvent) -> OverlayMouseDispatch {
        let layouts = self.base.rendered_overlay_layouts.borrow();
        for layout in layouts.iter().rev() {
            if event.screen_x < layout.col as isize
                || event.screen_x >= layout.col as isize + layout.width as isize
                || event.screen_y < layout.row as isize
                || event.screen_y >= layout.row as isize + layout.height as isize
            {
                continue;
            }
            let overlay_event = event.with_local(
                event.screen_x - layout.col as isize,
                event.screen_y - layout.row as isize,
                layout.width as isize,
                layout.height as isize,
            );
            let result =
                dispatch_mouse_event(&layout.component, &overlay_event).map(|mut result| {
                    // `result.focus ? { ...result, focusTarget:
                    // layout.entry.component } : result` — the overlay entry
                    // stays the keyboard focus owner for nested hits.
                    if result.focus {
                        result.focus_target = Some(Arc::clone(&layout.component));
                    }
                    result
                });
            return OverlayMouseDispatch { hit: true, result };
        }
        OverlayMouseDispatch {
            hit: false,
            result: None,
        }
    }

    /// `dispatchMouseToLayout` (tui-alt-screen.ts:808-827 @ 9841914):
    /// deepest-first walk over the layout boxes containing the point.
    /// Layout containers (stacks / scroll views) are skipped like upstream's
    /// `getLayoutNode && handleMouse === Container.prototype.handleMouse`
    /// check — rpi layout containers never override `handle_mouse`.
    fn dispatch_mouse_to_layout(&self, event: &TuiMouseEvent) -> Option<TuiMouseDispatchResult> {
        let layout = self.current_layout.as_ref()?;
        let mut visited: Vec<*const Mutex<Box<dyn Component>>> = Vec::new();
        let boxes = get_layout_boxes_at(layout, event.screen_x, event.screen_y);
        for layout_box in boxes {
            let pointer = Arc::as_ptr(&layout_box.component);
            if visited.contains(&pointer) {
                continue;
            }
            if lock_component(&layout_box.component)
                .layout_node()
                .is_some()
            {
                continue;
            }
            visited.push(pointer);
            let child_event = event.with_local(
                event.screen_x - layout_box.rect.x,
                event.screen_y - layout_box.rect.y,
                layout_box.rect.width as isize,
                layout_box.rect.height as isize,
            );
            if let Some(result) = dispatch_mouse_event(&layout_box.component, &child_event) {
                return Some(result);
            }
        }
        None
    }

    /// `applyMouseDispatchResult` (tui-alt-screen.ts:829-841 @ 9841914):
    /// move keyboard focus, record pointer capture, and decide whether a
    /// render is needed (`render ?? focusChanged || press/click/drag/wheel`).
    fn apply_mouse_dispatch_result(
        &mut self,
        event: &TuiMouseEvent,
        result: TuiMouseDispatchResult,
    ) -> bool {
        let focus_target = self.base.resolve_mouse_focus_target(
            result
                .focus_target
                .as_ref()
                .unwrap_or(&result.target.component),
        );
        let focus_changed = result.focus
            && !self
                .base
                .focused_component
                .as_ref()
                .is_some_and(|focused| same_component(focused, &focus_target));
        if result.focus {
            self.base.set_focus(Some(focus_target));
        }
        if result.capture {
            self.mouse_capture = Some(result.target.clone());
        }
        result.render.unwrap_or(
            focus_changed
                || event.event_type == TuiMouseEventType::Press
                || event.event_type == TuiMouseEventType::Click
                || event.event_type == TuiMouseEventType::Drag
                || event.event_type == TuiMouseEventType::Wheel,
        )
    }

    /// `dispatchMouseToTarget` (tui-alt-screen.ts:843-847 @ 9841914):
    /// re-dispatch to a saved press/capture target with rebuilt local
    /// coordinates.
    fn dispatch_mouse_to_target(
        &self,
        event: &TuiMouseEvent,
        target: &TuiMouseDispatchTarget,
    ) -> Option<TuiMouseDispatchResult> {
        let retargeted = retarget_mouse_event(event, target);
        dispatch_mouse_event(&target.component, &retargeted)
    }

    /// `getComponentClickCount` (tui-alt-screen.ts:855-868 @ 9841914):
    /// consecutive clicks on the same component cell count up within
    /// [`DOUBLE_CLICK_INTERVAL`], cycling 1 → 2 → 3 → 1.
    fn get_component_click_count(
        &mut self,
        target: &TuiMouseDispatchTarget,
        x: u32,
        y: u32,
    ) -> u32 {
        let now = Instant::now();
        let count = match &self.last_component_click {
            Some(previous)
                if now.saturating_duration_since(previous.timestamp) <= DOUBLE_CLICK_INTERVAL
                    && same_component(&previous.component, &target.component)
                    && previous.x == x
                    && previous.y == y =>
            {
                (previous.count % 3) + 1
            }
            _ => 1,
        };
        self.last_component_click = Some(LastComponentClick {
            timestamp: now,
            count,
            component: Arc::clone(&target.component),
            x,
            y,
        });
        count
    }

    /// `clearTextSelection` (tui-alt-screen.ts:869-880 @ 9841914): drop all
    /// selection state (press consumed by a component, URL activation, …).
    fn clear_text_selection(&mut self) {
        self.stop_selection_auto_scroll();
        self.selection_press_active = false;
        self.selection_anchor = None;
        self.selection_focus = None;
        self.selection_granularity = SelectionGranularity::Character;
        self.selection_initial_range = None;
        self.pressed_url = None;
        self.selection_dragged = false;
    }

    /// `handleMouseEvent` (tui-alt-screen.ts:870-909 @ 9841914): the SGR
    /// event state machine. Active component gestures (capture or press
    /// target) receive move/drag/release directly, with a click synthesized
    /// on a same-cell release; otherwise the event goes to the search
    /// overlay (V14-15), the topmost overlay, the layout, and finally the
    /// screen-level selection / right-click paste fallbacks.
    fn handle_mouse_event(&mut self, raw: SgrMouseEvent) {
        let is_motion = raw.button & 32 != 0;
        let event_type = if raw.release {
            TuiMouseEventType::Release
        } else if is_motion {
            if Self::decode_mouse_button(raw.button) == TuiMouseButton::None {
                TuiMouseEventType::Move
            } else {
                TuiMouseEventType::Drag
            }
        } else {
            TuiMouseEventType::Press
        };
        let event = self.create_mouse_event(event_type, raw.button, raw.x, raw.y, None, None);

        // `if (this.mouseCapture || this.mousePressTarget)` — an active
        // component gesture re-dispatches directly to its target.
        if let Some(target) = self
            .mouse_capture
            .clone()
            .or_else(|| self.mouse_press_target.clone())
        {
            if let Some((press_x, press_y)) = self.mouse_press_point {
                if raw.x != press_x || raw.y != press_y {
                    self.mouse_press_moved = true;
                    self.last_component_click = None;
                }
            }
            let mut render = false;
            if let Some(target_result) = self.dispatch_mouse_to_target(&event, &target) {
                render = self.apply_mouse_dispatch_result(&event, target_result);
            }
            if raw.release {
                let same_cell = !self.mouse_press_moved
                    && self
                        .mouse_press_point
                        .is_some_and(|(press_x, press_y)| press_x == raw.x && press_y == raw.y);
                if same_cell {
                    let click_count = self.get_component_click_count(&target, raw.x, raw.y);
                    let click_event = self.create_mouse_event(
                        TuiMouseEventType::Click,
                        raw.button,
                        raw.x,
                        raw.y,
                        None,
                        Some(click_count),
                    );
                    if let Some(click_result) = self.dispatch_mouse_to_target(&click_event, &target)
                    {
                        render =
                            self.apply_mouse_dispatch_result(&click_event, click_result) || render;
                    }
                }
                self.clear_component_mouse_gesture();
            }
            if render {
                self.request_render(false);
            }
            return;
        }

        // `handleSearchMouseEvent` (tui-alt-screen.ts:908 @ 9841914,
        // 00121ed99): hover tracking + button-press navigation.
        if self.handle_search_mouse_event(&raw) {
            return;
        }

        let overlay = self.dispatch_mouse_to_overlay(&event);
        if !overlay.hit {
            // `handleScrollToEndIndicatorMouseEvent` (tui-alt-screen.ts:913
            // @ 9841914, 79680533c).
            if self.handle_scroll_to_end_indicator_mouse_event(&raw) {
                return;
            }
            let handled = self.handle_scrollbar_mouse_event(raw);
            if self.scrollbar_drag.is_none() {
                self.update_scrollbar_hover(raw.x, raw.y);
            }
            if handled {
                return;
            }
        } else {
            self.stop_scrollbar_hover();
        }

        let result = overlay.result.or_else(|| {
            if overlay.hit {
                None
            } else {
                self.dispatch_mouse_to_layout(&event)
            }
        });
        eprintln!(
            "[DBG layout dispatch] x={} y={} consumed={}",
            event.x,
            event.y,
            result.is_some()
        );
        if let Some(result) = result {
            let render = self.apply_mouse_dispatch_result(&event, result.clone());
            if event_type == TuiMouseEventType::Press {
                self.clear_text_selection();
                self.mouse_press_target = Some(result.target);
                self.mouse_press_point = Some((raw.x, raw.y));
                self.mouse_press_moved = false;
            }
            if render {
                self.request_render(false);
            }
            return;
        }

        if self.handle_right_click_paste(raw) {
            return;
        }
        self.handle_selection_mouse_event(raw);
    }

    /// `routeWheel` (tui-alt-screen.ts:489-501): deepest scroll view under
    /// the pointer first, chaining the unconsumed delta; the primary scroll
    /// view is the fallback. `overscroll: "contain"` stops the chain.
    fn route_wheel(&mut self, event: WheelEvent) {
        let mut remaining = i64::from(event.direction) * self.wheel_scroll_lines as i64;
        let mut seen: Vec<*const Mutex<Box<dyn Component>>> = Vec::new();
        let scroll_views = self
            .current_layout
            .as_ref()
            .map(|layout| get_scroll_views_at(layout, event.x as isize, event.y as isize))
            .unwrap_or_default();
        for scroll_view in scroll_views {
            seen.push(Arc::as_ptr(&scroll_view));
            remaining = with_scroll_view(&scroll_view, |view| view.scroll_by(remaining))
                .unwrap_or(remaining);
            let overscroll = with_scroll_view(&scroll_view, ScrollView::overscroll);
            if remaining == 0 || overscroll == Some(Overscroll::Contain) {
                break;
            }
        }
        let primary = self.get_primary_scroll_view();
        if remaining != 0 && !seen.contains(&Arc::as_ptr(&primary)) {
            with_scroll_view(&primary, |view| {
                view.scroll_by(remaining);
            });
        }
        self.update_scrollbar_hover(event.x, event.y);
        self.request_render(false);
    }

    /// `handleRightClickPaste` (tui-alt-screen.ts:992-1010 @ 9841914):
    /// unmodified secondary-button press on Windows, except under VS Code's
    /// integrated terminal — `TERM_PROGRAM=vscode` terminals already paste
    /// on right click themselves (`374e56e55`, #8186). Clipboard paste is
    /// best-effort.
    /// `handleScrollToEndIndicatorMouseEvent` (tui-alt-screen.ts:1010-1017
    /// @ 9841914, 79680533c): a primary-button press (no motion bit) on the
    /// composited jump-to-end label scrolls to the bottom.
    fn handle_scroll_to_end_indicator_mouse_event(&mut self, event: &SgrMouseEvent) -> bool {
        let Some(rect) = self.scroll_to_end_indicator_rect else {
            return false;
        };
        if event.release || (event.button & 32) != 0 || (event.button & 3) != 0 {
            return false;
        }
        if event.y != rect.row || event.x < rect.column || event.x >= rect.column + rect.width {
            return false;
        }
        let scroll_view = self.get_primary_scroll_view();
        with_scroll_view(&scroll_view, ScrollView::scroll_to_end);
        self.request_render(false);
        true
    }

    fn handle_right_click_paste(&mut self, event: SgrMouseEvent) -> bool {
        if self.on_right_click_paste.is_none()
            || !self.win32
            || self.is_vscode_term_program()
            || event.release
            || event.button != 2
        {
            return false;
        }
        if let Some(callback) = &self.on_right_click_paste {
            callback();
        }
        true
    }

    /// `process.env.TERM_PROGRAM?.toLowerCase() === "vscode"`
    /// (tui-alt-screen.ts:996 @ 9841914): VS Code terminals (including
    /// `code-serve` variants reporting plain `vscode`) handle right-click
    /// paste natively. The environment read goes through
    /// [`TuiAltScreenOptions::term_program_override`] when injected
    /// (tests never touch the real environment, coding-standards §12.4).
    fn is_vscode_term_program(&self) -> bool {
        let term_program: Option<String> = match &self.term_program {
            Some(injected) => injected.clone(),
            None => std::env::var("TERM_PROGRAM").ok(),
        };
        term_program.is_some_and(|program| program.to_lowercase() == "vscode")
    }

    // --- scrollbar (tui-alt-screen.ts:1018-1108 @ 9841914) -------------

    /// `getScrollbarTargetAt` (tui-alt-screen.ts:1018-1037): the first
    /// (deepest) scroll view whose scrollbar TRACK covers the point.
    /// `include_hidden_auto` reveals a hidden `auto` track (the hover
    /// hit-test wakes it as the pointer enters, 457ae8c79).
    fn get_scrollbar_target_at(
        &self,
        x: isize,
        y: isize,
        include_hidden_auto: bool,
    ) -> Option<ScrollbarTarget> {
        if self.has_overlay() {
            return None;
        }
        let layout = self.current_layout.as_ref()?;
        for scroll_view in get_scroll_views_at(layout, x, y) {
            let geometry = get_scroll_view_box(layout, &scroll_view)
                .and_then(|box_| crate::layout::get_scrollbar_geometry(box_, include_hidden_auto));
            if let Some(geometry) = geometry {
                if x == geometry.column
                    && y >= geometry.track_top
                    && y < geometry.track_top + geometry.track_height as isize
                {
                    return Some(ScrollbarTarget {
                        scroll_view,
                        geometry,
                    });
                }
            }
        }
        None
    }

    /// `setScrollbarHover` (tui-alt-screen.ts:1039-1041).
    fn set_scrollbar_hover(&mut self, scroll_view: Option<SharedComponent>) {
        let unchanged = match (&self.scrollbar_hover, &scroll_view) {
            (None, None) => true,
            (Some(current), Some(next)) => same_component(current, next),
            _ => false,
        };
        if unchanged {
            return;
        }
        if let Some(previous) = self.scrollbar_hover.take() {
            with_scroll_view(&previous, |view| view.set_scrollbar_active(false));
        }
        self.scrollbar_hover = scroll_view;
        if let Some(current) = &self.scrollbar_hover {
            with_scroll_view(current, |view| view.set_scrollbar_active(true));
        }
    }

    /// `updateScrollbarHover` (tui-alt-screen.ts:1042-1048): hover hit-test
    /// with `includeHiddenAuto` so a pointer entering a hidden `auto` track
    /// reveals it.
    fn update_scrollbar_hover(&mut self, x: u32, y: u32) {
        let target = self
            .get_scrollbar_target_at(x as isize, y as isize, true)
            .map(|target| target.scroll_view);
        self.set_scrollbar_hover(target);
    }

    /// `stopScrollbarHover` (tui-alt-screen.ts:1050-1052).
    fn stop_scrollbar_hover(&mut self) {
        self.set_scrollbar_hover(None);
    }

    /// `scrollScrollbarToPointer` (tui-alt-screen.ts:1054-1061): map the
    /// pointer row back to a scroll offset, keeping the thumb's grab point
    /// under the pointer.
    fn scroll_scrollbar_to_pointer(
        scroll_view: &SharedComponent,
        geometry: &ScrollbarGeometry,
        pointer_y: u32,
        grab_offset: isize,
    ) {
        let max_thumb_offset = geometry.track_height - geometry.thumb_height;
        let thumb_offset = (pointer_y as isize - geometry.track_top - grab_offset)
            .clamp(0, max_thumb_offset as isize);
        let scroll_top = if max_thumb_offset == 0 {
            0
        } else {
            ((thumb_offset as f64 / max_thumb_offset as f64) * geometry.max_scroll_top as f64)
                .round() as i64
        };
        with_scroll_view(scroll_view, |view| view.scroll_to(scroll_top));
    }

    /// `handleScrollbarMouseEvent` (tui-alt-screen.ts:1062-1108 @ 9841914,
    /// 457ae8c79): thumb drag AND track clicks — a press off the thumb
    /// jumps the thumb center to the pointer (`grabOffset =
    /// floor(thumbHeight / 2)`) and continues dragging from there; a press
    /// on the thumb keeps the grab point under the pointer.
    fn handle_scrollbar_mouse_event(&mut self, event: SgrMouseEvent) -> bool {
        if let Some(drag) = self.scrollbar_drag.clone() {
            if event.release {
                self.stop_scrollbar_drag();
                return true;
            }
            let geometry = self.current_layout.as_ref().and_then(|layout| {
                get_scroll_view_box(layout, &drag.scroll_view)
                    .and_then(|box_| crate::layout::get_scrollbar_geometry(box_, false))
            });
            if let Some(geometry) = geometry {
                Self::scroll_scrollbar_to_pointer(
                    &drag.scroll_view,
                    &geometry,
                    event.y,
                    drag.grab_offset,
                );
            }
            return true;
        }

        if event.release || (event.button & 32) != 0 || (event.button & 3) != 0 {
            return false;
        }
        let Some(target) = self.get_scrollbar_target_at(event.x as isize, event.y as isize, false)
        else {
            return false;
        };
        self.stop_selection_auto_scroll();
        self.selection_press_active = false;
        self.selection_anchor = None;
        self.selection_focus = None;
        self.selection_granularity = SelectionGranularity::Character;
        self.selection_initial_range = None;
        self.last_click = None;
        self.pressed_url = None;
        self.selection_dragged = false;
        self.set_scrollbar_hover(Some(target.scroll_view.clone()));
        let pointer_y = event.y as isize;
        let on_thumb = pointer_y >= target.geometry.thumb_top
            && pointer_y < target.geometry.thumb_top + target.geometry.thumb_height as isize;
        let grab_offset = if on_thumb {
            event.y as isize - target.geometry.thumb_top
        } else {
            (target.geometry.thumb_height / 2) as isize
        };
        if !on_thumb {
            Self::scroll_scrollbar_to_pointer(
                &target.scroll_view,
                &target.geometry,
                event.y,
                grab_offset,
            );
        }
        self.scrollbar_drag = Some(ScrollbarDrag {
            scroll_view: target.scroll_view,
            grab_offset,
        });
        true
    }

    /// `stopScrollbarDrag` (tui-alt-screen.ts:601-603).
    fn stop_scrollbar_drag(&mut self) {
        self.scrollbar_drag = None;
    }

    // --- selection (tui-alt-screen.ts:605-963) -----------------------------

    /// `getScrollSelectionPoint` (tui-alt-screen.ts:605-623): a selection
    /// point in scroll-view content coordinates, clamped to the visible rows
    /// and the box's column range.
    fn get_scroll_selection_point(
        &self,
        scroll_view: &SharedComponent,
        x: u32,
        y: u32,
    ) -> Option<SelectionPoint> {
        let layout = self.current_layout.as_ref()?;
        let layout_box = get_scroll_view_box(layout, scroll_view)?;
        if layout_box.rect.height == 0 || layout_box.clip.height == 0 {
            return None;
        }
        let visible_top = 0.max(layout_box.rect.y).max(layout_box.clip.y);
        let visible_bottom = (self.terminal().rows() as isize - 1)
            .min(layout_box.rect.y + layout_box.rect.height as isize - 1)
            .min(layout_box.clip.y + layout_box.clip.height as isize - 1);
        if visible_bottom < visible_top {
            return None;
        }
        let pointer_row = (y as isize).clamp(visible_top, visible_bottom);
        let max_content_row = layout_box
            .scroll_content_lines
            .as_ref()
            .map_or(1, |lines| lines.len())
            .saturating_sub(1);
        let scroll_top = with_scroll_view(scroll_view, ScrollView::scroll_top).unwrap_or(0);
        let row = (scroll_top as isize + pointer_row - layout_box.rect.y)
            .clamp(0, max_content_row as isize) as usize;
        let col =
            (x as isize - layout_box.rect.x).clamp(0, layout_box.rect.width as isize - 1) as usize;
        Some(SelectionPoint {
            row,
            col,
            scroll_view: Some(scroll_view.clone()),
            boundary: false,
        })
    }

    /// `getSelectionPoint` (tui-alt-screen.ts:625-634): scroll-view content
    /// coordinates when a scroll view is hit, else screen coordinates clamped
    /// to the terminal.
    fn get_selection_point(
        &self,
        event: SgrMouseEvent,
        scroll_view: Option<&SharedComponent>,
    ) -> SelectionPoint {
        if let Some(scroll_view) = scroll_view {
            if let Some(point) = self.get_scroll_selection_point(scroll_view, event.x, event.y) {
                return point;
            }
        }
        // Separate statements: each `terminal()` guard must drop before the
        // next acquisition (std::sync::Mutex is not reentrant).
        let rows = self.terminal().rows();
        let columns = self.terminal().columns();
        SelectionPoint {
            row: (event.y as usize).min(usize::from(rows).saturating_sub(1)),
            col: (event.x as usize).min(usize::from(columns).saturating_sub(1)),
            scroll_view: None,
            boundary: false,
        }
    }

    /// `getSelectionSourceLine` (tui-alt-screen.ts:636-642): the raw content
    /// line a selection point refers to.
    fn get_selection_source_line(&self, point: &SelectionPoint) -> String {
        if let (Some(scroll_view), Some(layout)) = (&point.scroll_view, &self.current_layout) {
            if let Some(lines) = get_scroll_view_box(layout, scroll_view)
                .and_then(|layout_box| layout_box.scroll_content_lines.as_ref())
            {
                return lines.get(point.row).cloned().unwrap_or_default();
            }
        }
        self.previous_screen
            .get(point.row)
            .cloned()
            .unwrap_or_default()
    }

    /// `getWordSelection` (tui-alt-screen.ts:1150-1185 @ 9841914,
    /// `1ac6128e6` #7746): the word segment covering the point's column,
    /// with `"/"`/`"-"` joiner segments gluing adjacent word-like segments
    /// into one range (paths and kebab-case tokens stay whole — fullscreen
    /// owns mouse selection and mirrors common terminal word-selection
    /// behavior). Joining is bidirectional: a joiner extends through any
    /// neighboring selectable segment whose far side is also a joiner.
    fn get_word_selection(&self, point: &SelectionPoint) -> Option<SelectionRange> {
        let line = strip_terminal_sequences(&self.get_selection_source_line(point));
        let segmenter = get_word_segmenter();
        // `TERMINAL_WORD_SELECTION_JOINERS` (tui-alt-screen.ts:81).
        let is_joiner = |segment: &str| TERMINAL_WORD_SELECTION_JOINERS.contains(&segment);
        // Segments with their text, column ranges, and the
        // selectable/joiner flags (tui-alt-screen.ts:1152-1160 @ 9841914).
        let mut word_segments: Vec<(&str, usize, usize, bool, bool)> = segmenter
            .segment(&line)
            .fold((Vec::new(), 0usize), |(mut acc, start), segment: &str| {
                let end = start + visible_width(segment);
                let joiner = is_joiner(segment);
                let selectable = segmenter.is_word_like(segment) || joiner;
                acc.push((segment, start, end, selectable, joiner));
                (acc, end)
            })
            .0;
        // D-093 refinement (calibrated against Node 24 / ICU 78):
        // `Intl.Segmenter` keeps Hiragana runs as one word-like segment,
        // while plain UAX #29 (`split_word_bounds`) breaks between every
        // Hiragana character. Merge adjacent all-Hiragana segments so the
        // double-click word selection over Japanese text matches upstream
        // (Katakana runs are already joined by UAX #29 WB13; the
        // Katakana/Hiragana and Hiragana/Han boundaries stay breaks, like
        // ICU).
        word_segments = merge_hiragana_runs(word_segments);
        let segments: Vec<(usize, usize, bool, bool)> = word_segments
            .iter()
            .map(|&(_, start, end, selectable, joiner)| (start, end, selectable, joiner))
            .collect();
        let clicked_segment_index = segments
            .iter()
            .position(|(start, end, _, _)| point.col >= *start && point.col < *end)?;

        // `canJoin` (tui-alt-screen.ts:1167-1169): both sides selectable and
        // at least one a joiner.
        let can_join = |left: &(usize, usize, bool, bool), right: &(usize, usize, bool, bool)| {
            left.2 && right.2 && (left.3 || right.3)
        };
        let mut selection_start = segments[clicked_segment_index].0;
        let mut selection_end = segments[clicked_segment_index].1;
        let mut index = clicked_segment_index;
        while index > 0 && can_join(&segments[index - 1], &segments[index]) {
            selection_start = segments[index - 1].0;
            index -= 1;
        }
        let mut index = clicked_segment_index;
        while index + 1 < segments.len() && can_join(&segments[index], &segments[index + 1]) {
            selection_end = segments[index + 1].1;
            index += 1;
        }
        Some(SelectionRange {
            start: point.with(selection_start, false),
            end: point.with(selection_end, true),
        })
    }

    /// `getLineSelection` (tui-alt-screen.ts:660-665).
    fn get_line_selection(&self, point: &SelectionPoint) -> SelectionRange {
        SelectionRange {
            start: point.with(0, false),
            end: point.with(visible_width(&self.get_selection_source_line(point)), true),
        }
    }

    /// `updateSelectionFocus` (tui-alt-screen.ts:667-685): character
    /// granularity tracks the pointer directly; word/line granularity expands
    /// both ends to whole segments around the initial range.
    fn update_selection_focus(&mut self, point: SelectionPoint) {
        if self.selection_granularity == SelectionGranularity::Character
            || self.selection_initial_range.is_none()
        {
            self.selection_focus = Some(point);
            return;
        }
        let range = if self.selection_granularity == SelectionGranularity::Word {
            self.get_word_selection(&point)
        } else {
            Some(self.get_line_selection(&point))
        };
        let Some(range) = range else { return };
        let Some(initial) = self.selection_initial_range.clone() else {
            return;
        };
        let target_before_initial = range.start.row < initial.start.row
            || (range.start.row == initial.start.row && range.start.col < initial.start.col);
        if target_before_initial {
            self.selection_anchor = Some(initial.end);
            self.selection_focus = Some(range.start);
        } else {
            self.selection_anchor = Some(initial.start);
            self.selection_focus = Some(range.end);
        }
    }

    /// `getClickCount` (tui-alt-screen.ts:687-711): multi-click detection —
    /// same word, same row, same scroll view, within
    /// [`DOUBLE_CLICK_INTERVAL`]; the count cycles 1 → 2 → 3 → 1 (`% 3`).
    fn get_click_count(&mut self, point: &SelectionPoint, word: Option<&SelectionRange>) -> u32 {
        let now = Instant::now();
        let count = match (word, &self.last_click) {
            (Some(word), Some(previous))
                if now.saturating_duration_since(previous.timestamp) <= DOUBLE_CLICK_INTERVAL
                    && previous.row == point.row
                    && same_optional_scroll_view(&previous.scroll_view, &point.scroll_view)
                    && previous.word_start == word.start.col
                    && previous.word_end == word.end.col =>
            {
                (previous.count % 3) + 1
            }
            _ => 1,
        };
        self.last_click = word.map(|word| ClickTarget {
            timestamp: now,
            count,
            row: point.row,
            scroll_view: point.scroll_view.clone(),
            word_start: word.start.col,
            word_end: word.end.col,
        });
        count
    }

    /// `updateSelectionAutoScroll` (tui-alt-screen.ts:713-739): a drag held at
    /// the anchor scroll view's top/bottom edge starts the auto-scroll
    /// "interval" (an explicit deadline here; see the header note).
    fn update_selection_auto_scroll(&mut self, event: SgrMouseEvent) {
        let Some(scroll_view) = self
            .selection_anchor
            .as_ref()
            .and_then(|anchor| anchor.scroll_view.clone())
        else {
            self.stop_selection_auto_scroll();
            return;
        };
        let geometry = self.current_layout.as_ref().and_then(|layout| {
            let layout_box = get_scroll_view_box(layout, &scroll_view)?;
            if layout_box.rect.height == 0 || layout_box.clip.height == 0 {
                return None;
            }
            let visible_top = 0.max(layout_box.rect.y).max(layout_box.clip.y);
            let visible_bottom = (self.terminal().rows() as isize - 1)
                .min(layout_box.rect.y + layout_box.rect.height as isize - 1)
                .min(layout_box.clip.y + layout_box.clip.height as isize - 1);
            Some((visible_top, visible_bottom))
        });
        let Some((visible_top, visible_bottom)) = geometry else {
            self.stop_selection_auto_scroll();
            return;
        };
        self.selection_drag_pointer = Some((event.x, event.y));
        self.selection_auto_scroll_direction = if (event.y as isize) <= visible_top {
            -1
        } else if (event.y as isize) >= visible_bottom {
            1
        } else {
            0
        };
        if self.selection_auto_scroll_direction == 0 {
            self.stop_selection_auto_scroll();
            return;
        }
        if self.selection_auto_scroll_next.is_some() {
            return;
        }
        self.selection_auto_scroll_next = Some(Instant::now() + AUTO_SCROLL_INTERVAL);
    }

    /// `autoScrollSelection` (tui-alt-screen.ts:741-757): one interval tick —
    /// scroll the anchor's scroll view by one line and extend the selection
    /// to the pointer row. Stops when the scroll consumes nothing.
    fn auto_scroll_selection(&mut self) {
        let (Some(scroll_view), Some(pointer), direction) = (
            self.selection_anchor
                .as_ref()
                .and_then(|anchor| anchor.scroll_view.clone()),
            self.selection_drag_pointer,
            self.selection_auto_scroll_direction,
        ) else {
            self.stop_selection_auto_scroll();
            return;
        };
        if direction == 0 {
            self.stop_selection_auto_scroll();
            return;
        }
        let remaining = with_scroll_view(&scroll_view, |view| view.scroll_by(i64::from(direction)))
            .unwrap_or(i64::from(direction));
        if remaining == i64::from(direction) {
            self.stop_selection_auto_scroll();
            return;
        }
        if let Some(point) = self.get_scroll_selection_point(&scroll_view, pointer.0, pointer.1) {
            self.update_selection_focus(point);
        }
        self.request_render(false);
    }

    /// `stopSelectionAutoScroll` (tui-alt-screen.ts:759-766).
    fn stop_selection_auto_scroll(&mut self) {
        self.selection_auto_scroll_next = None;
        self.selection_auto_scroll_direction = 0;
        self.selection_drag_pointer = None;
    }

    /// `handleSelectionMouseEvent` (tui-alt-screen.ts:1292-1362 @ 9841914):
    /// the primary button's press / drag-motion / release state machine.
    /// Orphan events (release or motion without an active press) are
    /// swallowed. Release accepts the SGR button code 3 (the generic
    /// "no button" release many terminals send, `83aed2ba5` #7963), and a
    /// press+release on the same cell without movement synthesizes a
    /// component click before the selection/copy fallbacks run
    /// (`71026970a`).
    fn handle_selection_mouse_event(&mut self, event: SgrMouseEvent) {
        let button = event.button & 3;
        if button != 0 && !(event.release && button == 3) {
            return;
        }
        let anchor_scroll_view = self
            .selection_anchor
            .as_ref()
            .and_then(|anchor| anchor.scroll_view.clone());
        let point = self.get_selection_point(event, anchor_scroll_view.as_ref());
        if event.release {
            if !self.selection_press_active {
                return;
            }
            self.selection_press_active = false;
            self.stop_selection_auto_scroll();
            let Some(anchor) = self.selection_anchor.clone() else {
                return;
            };
            self.update_selection_focus(point.clone());
            let is_click = !self.selection_dragged
                && same_optional_scroll_view(&anchor.scroll_view, &point.scroll_view)
                && anchor.row == point.row
                && anchor.col == point.col;
            let clicked_url = if is_click {
                self.pressed_url.clone()
            } else {
                None
            };
            self.pressed_url = None;
            if let (Some(url), Some(open_url)) = (clicked_url, self.open_url.clone()) {
                self.selection_anchor = None;
                self.selection_focus = None;
                open_url(&url);
                self.request_render(false);
                return;
            }
            if is_click {
                // Synthesized click (tui-alt-screen.ts:1324-1336 @ 9841914,
                // 71026970a): offer the click to the overlay/layout
                // component tree before the selection fallbacks run.
                let click_count = self
                    .last_click
                    .as_ref()
                    .map(|click| click.count)
                    .unwrap_or(1);
                let click_event = self.create_mouse_event(
                    TuiMouseEventType::Click,
                    event.button,
                    event.x,
                    event.y,
                    None,
                    Some(click_count),
                );
                let overlay = self.dispatch_mouse_to_overlay(&click_event);
                let result = overlay.result.or_else(|| {
                    if overlay.hit {
                        None
                    } else {
                        self.dispatch_mouse_to_layout(&click_event)
                    }
                });
                if let Some(result) = result {
                    let render = self.apply_mouse_dispatch_result(&click_event, result);
                    self.clear_text_selection();
                    if render {
                        self.request_render(false);
                    }
                    return;
                }
            }
            // `if (this.copyOnSelect) void this.copySelectionToClipboard();`
            // (tui-alt-screen.ts:1337 @ 9841914, 4e4949299): selections
            // only copy on release while copyOnSelect is enabled; with it
            // off the selection stays visible for the Ctrl+X fork.
            if self.copy_on_select {
                self.copy_selection_to_clipboard();
            }
            self.request_render(false);
            return;
        }
        if (event.button & 32) != 0 {
            if !self.selection_press_active || self.selection_anchor.is_none() {
                return;
            }
            self.selection_dragged = true;
            self.last_click = None;
            self.pressed_url = None;
            self.update_selection_focus(point);
            self.update_selection_auto_scroll(event);
            self.request_render(false);
            return;
        }
        self.stop_selection_auto_scroll();
        self.selection_press_active = true;
        eprintln!(
            "[DBG selection press] x={} y={} anchor will be computed",
            event.x, event.y
        );
        let scroll_view = if !self.has_overlay() {
            self.current_layout.as_ref().and_then(|layout| {
                get_scroll_views_at(layout, event.x as isize, event.y as isize)
                    .into_iter()
                    .next()
            })
        } else {
            None
        };
        let anchor = self.get_selection_point(event, scroll_view.as_ref());
        let word = self.get_word_selection(&anchor);
        let click_count = self.get_click_count(&anchor, word.as_ref());
        let range = match click_count {
            2 => word,
            3 => Some(self.get_line_selection(&anchor)),
            _ => None,
        };
        self.selection_granularity = match (&range, click_count) {
            (Some(_), 2) => SelectionGranularity::Word,
            (Some(_), _) => SelectionGranularity::Line,
            (None, _) => SelectionGranularity::Character,
        };
        self.selection_initial_range = range.clone();
        self.selection_anchor = Some(
            range
                .as_ref()
                .map(|range| range.start.clone())
                .unwrap_or_else(|| anchor.clone()),
        );
        self.selection_focus = Some(
            range
                .map(|range| range.end)
                .unwrap_or_else(|| anchor.clone()),
        );
        self.selection_dragged = false;
        self.pressed_url = if self.selection_initial_range.is_some() {
            None
        } else {
            let row = (event.y as usize).min(usize::from(self.terminal().rows()).saturating_sub(1));
            let col =
                (event.x as usize).min(usize::from(self.terminal().columns()).saturating_sub(1));
            self.previous_screen
                .get(row)
                .and_then(|line| get_osc8_link_at_column(line, col))
        };
        self.request_render(false);
    }

    /// `getSelectionBounds` (tui-alt-screen.ts:835-850): the ordered
    /// (start, end) pair, or `None` for empty/cross-scroll-view selections.
    fn get_selection_bounds(&self) -> Option<SelectionRange> {
        let anchor = self.selection_anchor.as_ref()?;
        let focus = self.selection_focus.as_ref()?;
        if !same_optional_scroll_view(&anchor.scroll_view, &focus.scroll_view) {
            return None;
        }
        if anchor.row == focus.row && anchor.col == focus.col {
            return None;
        }
        let anchor_before_focus =
            anchor.row < focus.row || (anchor.row == focus.row && anchor.col < focus.col);
        if anchor_before_focus {
            Some(SelectionRange {
                start: anchor.clone(),
                end: focus.clone(),
            })
        } else {
            Some(SelectionRange {
                start: focus.clone(),
                end: anchor.clone(),
            })
        }
    }

    /// Screen-space selection used by the highlight/copy paths: rows and
    /// columns are `i64` because the scroll-view coordinate transform
    /// (tui-alt-screen.ts:932-943) can produce off-screen (negative) rows,
    /// which then simply match no screen row.
    fn screen_selection(
        &self,
        selection: &SelectionRange,
        layout: Option<&LayoutFrame>,
    ) -> Option<ScreenSelection> {
        let mut screen_selection = ScreenSelection {
            start_row: selection.start.row as i64,
            start_col: selection.start.col as i64,
            end_row: selection.end.row as i64,
            end_col: selection.end.col as i64,
            end_boundary: selection.end.boundary,
            min_row: 0,
            max_row: i64::MAX,
            min_column: 0,
            max_column: i64::MAX,
        };
        if let Some(scroll_view) = &selection.start.scroll_view {
            let layout_box = layout.and_then(|layout| get_scroll_view_box(layout, scroll_view))?;
            screen_selection.min_row = 0
                .max(layout_box.rect.y as i64)
                .max(layout_box.clip.y as i64);
            screen_selection.max_row = (layout_box.rect.y as i64 + layout_box.rect.height as i64
                - 1)
            .min(layout_box.clip.y as i64 + layout_box.clip.height as i64 - 1);
            screen_selection.min_column = 0
                .max(layout_box.rect.x as i64)
                .max(layout_box.clip.x as i64);
            screen_selection.max_column = (layout_box.rect.x as i64 + layout_box.rect.width as i64)
                .min(layout_box.clip.x as i64 + layout_box.clip.width as i64);
            let scroll_top = with_scroll_view(scroll_view, ScrollView::scroll_top).unwrap_or(0);
            screen_selection.start_row =
                layout_box.rect.y as i64 + selection.start.row as i64 - scroll_top as i64;
            screen_selection.start_col = layout_box.rect.x as i64 + selection.start.col as i64;
            screen_selection.end_row =
                layout_box.rect.y as i64 + selection.end.row as i64 - scroll_top as i64;
            screen_selection.end_col = layout_box.rect.x as i64 + selection.end.col as i64;
        }
        Some(screen_selection)
    }

    /// `getActiveSelectionText` (tui-alt-screen.ts:1413-1435 @ 9841914):
    /// the selected text, or `None` for an empty/absent selection. The
    /// highlight transform does not apply to the copy path (upstream reads
    /// rows/cols in source coordinates here).
    fn get_active_selection_text(&self) -> Option<String> {
        let selection = self.get_selection_bounds()?;
        // The scroll-content-lines source (tui-alt-screen.ts:1417-1422).
        let source_lines: Option<Arc<[String]>> = match &selection.start.scroll_view {
            Some(scroll_view) => {
                let layout = self.current_layout.as_ref()?;
                let lines = get_scroll_view_box(layout, scroll_view)
                    .and_then(|layout_box| layout_box.scroll_content_lines.clone())?;
                Some(lines)
            }
            None => None,
        };
        let mut lines: Vec<String> = Vec::new();
        for row in selection.start.row..=selection.end.row {
            let line = match &source_lines {
                Some(source) => source.get(row).cloned().unwrap_or_default(),
                None => self.previous_screen.get(row).cloned().unwrap_or_default(),
            };
            let columns = selection_columns(
                &line,
                row as i64,
                &ScreenSelection {
                    start_row: selection.start.row as i64,
                    start_col: selection.start.col as i64,
                    end_row: selection.end.row as i64,
                    end_col: selection.end.col as i64,
                    end_boundary: selection.end.boundary,
                    min_row: 0,
                    max_row: i64::MAX,
                    min_column: 0,
                    max_column: visible_width(&line) as i64,
                },
            );
            lines.push(
                strip_terminal_sequences(&slice_by_column(
                    &line,
                    columns.0,
                    columns.1.saturating_sub(columns.0),
                    true,
                ))
                .trim_end()
                .to_string(),
            );
        }
        let text = lines.join("\n");
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// `copySelectionToClipboard` (tui-alt-screen.ts:1437-1440): the
    /// release-path copy (flash reflects the outcome).
    fn copy_selection_to_clipboard(&mut self) -> bool {
        let Some(text) = self.get_active_selection_text() else {
            return false;
        };
        self.copy_text_to_clipboard(&text)
    }

    /// `copyActiveSelectionToClipboard` (tui-alt-screen.ts:298-302).
    fn copy_active_selection_to_clipboard(&mut self) -> bool {
        let Some(text) = self.get_active_selection_text() else {
            return false;
        };
        self.copy_text_to_clipboard(&text)
    }

    /// `copyTextToClipboard` (tui-alt-screen.ts:1443-1456 @ 9841914,
    /// 4caa3c440): prefer an injected clipboard implementation (native
    /// clipboard + platform tools with a verified success path) when the
    /// host app provides one. A bare OSC 52 write can show "Copied!" while
    /// leaving the system clipboard untouched (e.g. macOS Terminal.app,
    /// tmux without OSC 52 clipboard passthrough), so the flash reflects
    /// the verified outcome of the injected hook; the default OSC 52 path
    /// keeps the historical unconditional success.
    fn copy_text_to_clipboard(&mut self, text: &str) -> bool {
        if let Some(copy_selection) = &self.copy_selection {
            let ok = copy_selection(text);
            self.flashes.flash(
                if ok { "Copied!" } else { "Copy failed" },
                DEFAULT_DURATION_MS,
            );
            return ok;
        }
        let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        self.terminal().write(&format!("\x1b]52;c;{payload}\x07"));
        self.flashes.flash("Copied!", DEFAULT_DURATION_MS);
        true
    }

    /// `applySearchTextHighlight` (tui-alt-screen.ts:1458-1476 @ 9841914,
    /// 00121ed99): style every plain-text run of the slice, re-emitting
    /// inner SGR codes verbatim between runs.
    fn apply_search_text_highlight(&self, text: &str, current: bool) -> String {
        let style = if current {
            &self.search_current_match_style
        } else {
            &self.search_match_style
        };
        let mut result = String::new();
        let mut plain_start = 0;
        let mut index = 0;
        while index < text.len() {
            let Some(ansi) = extract_ansi_code(text, index) else {
                index += 1;
                continue;
            };
            if index > plain_start {
                result.push_str(&style(&text[plain_start..index]));
            }
            result.push_str(ansi.code);
            index += ansi.length;
            plain_start = index;
        }
        if plain_start < text.len() {
            result.push_str(&style(&text[plain_start..]));
        }
        result
    }

    /// `applySearchHighlights` (tui-alt-screen.ts:1478-1537 @ 9841914,
    /// 00121ed99 / 2d4116333): highlight only the matches intersecting the
    /// visible viewport — binary search to the first candidate, clamp each
    /// segment to the scroll-view box and scrollbar column, and restyle the
    /// plain runs of each hit slice (current match distinct from the rest).
    fn apply_search_highlights(&self, screen: Vec<String>, layout: &LayoutFrame) -> Vec<String> {
        let Some(search) = self.active_search.as_ref() else {
            return screen;
        };
        if search.selected_index < 0 || search.matches.is_empty() {
            return screen;
        }
        let scroll_view = layout
            .primary_scroll_view
            .clone()
            .unwrap_or_else(|| self.implicit_scroll_view.clone());
        let Some(box_) = get_scroll_view_box(layout, &scroll_view) else {
            return screen;
        };

        let scroll_top = with_scroll_view(&scroll_view, ScrollView::scroll_top).unwrap_or(0);
        let mut ranges_by_row: std::collections::HashMap<usize, Vec<SearchHighlightRange>> =
            std::collections::HashMap::new();
        let scrollbar_column = crate::layout::get_scrollbar_geometry(box_, false)
            .map(|geometry| geometry.column)
            .unwrap_or(isize::MAX);
        let min_row = box_.rect.y.max(box_.clip.y).max(0) as usize;
        let max_row = screen
            .len()
            .min((box_.rect.y + box_.rect.height as isize).max(0) as usize)
            .min((box_.clip.y + box_.clip.height as isize).max(0) as usize);
        let min_column = box_.rect.x.max(box_.clip.x).max(0) as usize;
        let terminal_columns = self.terminal().columns() as isize;
        let max_column = terminal_columns
            .min((box_.rect.x + box_.rect.width as isize).max(0))
            .min((box_.clip.x + box_.clip.width as isize).max(0))
            .min(scrollbar_column)
            .max(0) as usize;
        let min_content_row = scroll_top + min_row - box_.rect.y.max(0) as usize;
        let max_content_row = scroll_top + max_row - box_.rect.y.max(0) as usize - 1;

        let matches = &search.matches;
        let mut low = 0usize;
        let mut high = matches.len();
        while low < high {
            let middle = low + (high - low) / 2;
            let last_row = matches[middle]
                .segments
                .last()
                .map(|segment| segment.row)
                .unwrap_or(0);
            if last_row < min_content_row {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        for match_index in low..matches.len() {
            let search_match = &matches[match_index];
            let first_row = search_match
                .segments
                .first()
                .map(|segment| segment.row)
                .unwrap_or(0);
            if first_row > max_content_row {
                break;
            }
            for segment in &search_match.segments {
                let row = box_.rect.y + segment.row as isize - scroll_top as isize;
                if row < min_row as isize || row >= max_row as isize {
                    continue;
                }
                let start_col =
                    min_column.max((box_.rect.x + segment.start_col as isize).max(0) as usize);
                let end_col =
                    max_column.min((box_.rect.x + segment.end_col as isize).max(0) as usize);
                if end_col <= start_col {
                    continue;
                }
                ranges_by_row
                    .entry(row.max(0) as usize)
                    .or_default()
                    .push(SearchHighlightRange {
                        start_col,
                        end_col,
                        current: match_index as i64 == search.selected_index,
                    });
            }
        }

        let mut result = screen;
        for (row, mut ranges) in ranges_by_row {
            let Some(line) = result.get_mut(row) else {
                continue;
            };
            if is_image_line(line) {
                continue;
            }
            let line_width = visible_width(line);
            ranges.sort_by_key(|range| std::cmp::Reverse(range.start_col));
            for range in ranges {
                let start_col = range.start_col.min(line_width);
                let end_col = range.end_col.min(line_width);
                if end_col <= start_col {
                    continue;
                }
                let before = slice_by_column(line, 0, start_col, true);
                let highlighted = slice_by_column(line, start_col, end_col - start_col, true);
                let after = slice_by_column(line, end_col, line_width - end_col, true);
                *line = format!(
                    "{before}{}{after}",
                    self.apply_search_text_highlight(&highlighted, range.current)
                );
            }
        }
        result
    }

    /// `compositeScrollToEndIndicator` (tui-alt-screen.ts:1611-1628 @
    /// 9841914, 79680533c): center the jump-to-end label on the last row of
    /// a follow-end primary scroll view scrolled away from its end, and
    /// record its hit rectangle.
    fn composite_scroll_to_end_indicator(
        &mut self,
        screen: Vec<String>,
        layout: &LayoutFrame,
        width: usize,
    ) -> Vec<String> {
        self.scroll_to_end_indicator_rect = None;
        let Some(indicator) = self.scroll_to_end_indicator.clone() else {
            return screen;
        };
        let scroll_view = layout
            .primary_scroll_view
            .clone()
            .unwrap_or_else(|| self.implicit_scroll_view.clone());
        let follows_and_scrolled_away = with_scroll_view(&scroll_view, |view| {
            view.follows_end() && !view.is_following_end()
        });
        if !follows_and_scrolled_away.unwrap_or(false) {
            return screen;
        }
        let Some(box_) = get_scroll_view_box(layout, &scroll_view) else {
            return screen;
        };
        let clip = &box_.clip;
        if clip.width == 0 || clip.height == 0 {
            return screen;
        }
        let row = clip.y + clip.height as isize - 1;
        if row < 0 || row as usize >= screen.len() {
            return screen;
        }
        if is_image_line(&screen[row as usize]) {
            return screen;
        }
        let scrollbar_column = crate::layout::get_scrollbar_geometry(box_, false)
            .map(|geometry| geometry.column)
            .unwrap_or(clip.x + clip.width as isize);
        let available_width = (scrollbar_column - clip.x).max(0) as usize;
        let text = truncate_to_width(&indicator(), available_width, "", false);
        let text_width = visible_width(&text);
        if text_width == 0 {
            return screen;
        }
        let column = clip.x + ((available_width - text_width) / 2) as isize;
        let mut result = screen;
        let composited = composite_tui_line(
            &result[row as usize],
            &text,
            column as i32,
            text_width as i32,
            width as i32,
        );
        result[row as usize] = composited;
        self.scroll_to_end_indicator_rect = Some(ScrollToEndIndicatorRect {
            row: row.max(0) as u32,
            column: column.max(0) as u32,
            width: text_width as u32,
        });
        result
    }

    /// `applySelectionHighlight` (tui-alt-screen.ts:899-914): wrap the
    /// selected text in reverse video, re-applying `\x1b[7m` after every SGR
    /// sequence inside the selection (resets would otherwise cancel it).
    fn apply_selection_highlight(text: &str) -> String {
        let mut result = String::from("\x1b[7m");
        let mut index = 0;
        while index < text.len() {
            let Some(ansi) = extract_ansi_code(text, index) else {
                // Whole characters instead of UTF-16 code units (JS iterates
                // code units; astral chars come out whole either way).
                let ch = text[index..].chars().next().unwrap_or('\u{fffd}');
                result.push(ch);
                index += ch.len_utf8();
                continue;
            };
            result.push_str(ansi.code);
            if ansi.code.ends_with('m') {
                result.push_str("\x1b[7m");
            }
            index += ansi.length;
        }
        format!("{result}\x1b[27m")
    }

    /// `applySelection` (tui-alt-screen.ts:916-963): paint the active
    /// selection onto the rendered screen (scroll-view selections are
    /// transformed into screen coordinates first).
    fn apply_selection(&self, screen: Vec<String>, layout: Option<&LayoutFrame>) -> Vec<String> {
        let Some(selection) = self.get_selection_bounds() else {
            return screen;
        };
        let Some(screen_selection) = self.screen_selection(&selection, layout) else {
            return screen;
        };
        let max_row = (screen.len() as i64 - 1).min(screen_selection.max_row);
        let max_column = (i64::from(self.terminal().columns())).min(screen_selection.max_column);
        screen
            .into_iter()
            .enumerate()
            .map(|(row, line)| {
                let row = row as i64;
                if row < screen_selection.min_row
                    || row > max_row
                    || row < screen_selection.start_row
                    || row > screen_selection.end_row
                    || is_image_line(&line)
                {
                    return line;
                }
                let line_width = visible_width(&line);
                let columns = selection_columns(
                    &line,
                    row,
                    &ScreenSelection {
                        min_column: screen_selection.min_column,
                        max_column,
                        ..screen_selection
                    },
                );
                if columns.1 <= columns.0 {
                    return line;
                }
                let before = slice_by_column(&line, 0, columns.0, true);
                let selected = slice_by_column(&line, columns.0, columns.1 - columns.0, true);
                let after =
                    slice_by_column(&line, columns.1, line_width.saturating_sub(columns.1), true);
                format!(
                    "{before}{}{after}",
                    Self::apply_selection_highlight(&selected)
                )
            })
            .collect()
    }

    // --- flash compositing (tui-alt-screen.ts:969-981) ----------------------

    /// `compositeFlashes` (tui-alt-screen.ts:969-981): right-align the flash
    /// stack over the top rows of the screen.
    fn composite_flashes(&self, screen: Vec<String>, width: usize, height: usize) -> Vec<String> {
        let flash_lines = self.flashes.render(width);
        let flash_lines = if flash_lines.len() > height {
            flash_lines[flash_lines.len() - height..].to_vec()
        } else {
            flash_lines
        };
        if flash_lines.is_empty() {
            return screen;
        }
        let mut result = screen;
        while result.len() < height {
            result.push(String::new());
        }
        for (row, line) in flash_lines.iter().enumerate() {
            let flash_width = visible_width(line);
            if flash_width == 0 {
                continue;
            }
            result[row] = composite_tui_line(
                &result[row],
                line,
                (width - flash_width) as i32,
                flash_width as i32,
                width as i32,
            );
        }
        result
    }

    // --- scroll-view timer helpers (explicit-deadline model) ----------------

    /// All scroll views in the current layout (dedup by identity).
    fn layout_scroll_views(&self) -> Vec<SharedComponent> {
        fn collect(layout_box: &LayoutBox, out: &mut Vec<SharedComponent>) {
            if let Some(scroll_view) = &layout_box.scroll_view {
                if !out
                    .iter()
                    .any(|existing| same_component(existing, scroll_view))
                {
                    out.push(scroll_view.clone());
                }
            }
            for child in &layout_box.children {
                collect(child, out);
            }
        }
        let mut views = Vec::new();
        if let Some(layout) = &self.current_layout {
            collect(&layout.root, &mut views);
        }
        views
    }

    /// Earliest scrollbar hide deadline across the layout's scroll views.
    fn next_scrollbar_deadline(&self) -> Option<Instant> {
        self.layout_scroll_views()
            .iter()
            .filter_map(|view| with_scroll_view(view, ScrollView::scrollbar_hide_deadline))
            .flatten()
            .min()
    }

    /// Whether any scrollbar hide deadline has expired by `now`.
    fn has_expired_scrollbar_deadline(&self, now: Instant) -> bool {
        self.layout_scroll_views()
            .iter()
            .filter_map(|view| with_scroll_view(view, ScrollView::scrollbar_hide_deadline))
            .flatten()
            .any(|deadline| now >= deadline)
    }

    /// Drive the transient-scrollbar timers of every layout scroll view
    /// (upstream's per-view `scrollbarHideTimer`).
    fn tick_scroll_views(&self, now: Instant) {
        for view in self.layout_scroll_views() {
            with_scroll_view(&view, |scroll_view| scroll_view.tick(now));
        }
    }

    // --- render (tui-alt-screen.ts:983-1046) --------------------------------

    /// `doRender` (tui-alt-screen.ts:983-1046): layout-frame render → OSC 133
    /// strip → overlay composite → height clamp → selection highlight → flash
    /// composite → cursor extraction → line resets + width clamp →
    /// full-redraw / image-redraw decision (Kitty placement-only reuse) →
    /// per-row differential write wrapped in synchronized output.
    fn do_render(&mut self) {
        if self.stopped || !self.alt_screen_active {
            return;
        }
        let width = usize::from(self.terminal().columns()).max(1);
        let height = usize::from(self.terminal().rows()).max(1);
        // Refresh the lock-free size cache (same role as TuiMainScreen).
        self.size_cache
            .rows
            .store(self.terminal().rows(), Ordering::Relaxed);
        self.size_cache
            .columns
            .store(self.terminal().columns(), Ordering::Relaxed);
        let root = self
            .layout_root
            .clone()
            .unwrap_or_else(|| self.implicit_scroll_view.clone());
        let mut next_layout = render_layout_frame(&root, width, height, self.render_handle.clone());
        // `refreshSearch` (tui-alt-screen.ts:1652-1654 @ 9841914): a
        // selection reveal scrolls the view — re-render the layout so the
        // highlight and indicator composite against the scrolled frame.
        if self.refresh_search(&next_layout) {
            next_layout = render_layout_frame(&root, width, height, self.render_handle.clone());
        }
        let mut screen: Vec<String> = next_layout
            .lines
            .iter()
            .map(|line| strip_osc133_zone_prefix(line).to_string())
            .collect();
        screen = self.apply_search_highlights(screen, &next_layout);
        screen = self.composite_scroll_to_end_indicator(screen, &next_layout, width);
        screen = self.composite_overlays(screen, width as i32, height as i32);
        if screen.len() > height {
            screen.drain(..screen.len() - height);
        }
        screen = self.apply_selection(screen, Some(&next_layout));
        screen = self.composite_flashes(screen, width, height);

        let cursor_pos = TuiBase::extract_cursor_position(&mut screen, height as i32);
        TuiBase::apply_line_resets(&mut screen);
        for line in &mut screen {
            if !is_image_line(line) && visible_width(line) > width {
                *line = slice_by_column(line, 0, width, true);
            }
        }

        let full_redraw = self.previous_screen.is_empty()
            || self.previous_screen_width != width
            || self.previous_screen_height != height;
        let images_need_redraw = screen.iter().enumerate().any(|(row, line)| {
            let previous = self.previous_screen.get(row).map_or("", String::as_str);
            line != previous && (is_image_line(line) || is_image_line(previous))
        });
        let redraw_images = full_redraw || images_need_redraw;
        let had_uploaded_kitty_images = kitty_image_cache_has_entries();
        let prepared = if redraw_images && self.image_protocol == Some(ImageProtocol::Kitty) {
            Some(prepare_kitty_screen(&screen))
        } else {
            None
        };
        let out_lines: &[String] = match &prepared {
            Some(prepared) => &prepared.screen,
            None => &screen,
        };

        let mut buffer = String::from(BEGIN_SYNCHRONIZED_OUTPUT);
        if full_redraw {
            self.full_redraw_count += 1;
            if self.image_protocol == Some(ImageProtocol::Kitty) && had_uploaded_kitty_images {
                buffer.push_str(&delete_all_kitty_placements());
            } else {
                buffer.push_str(&self.delete_kitty_images());
            }
            buffer.push_str("\x1b[2J");
        } else if images_need_redraw {
            match self.image_protocol {
                Some(ImageProtocol::ITerm2) => buffer.push_str("\x1b[2J"),
                Some(ImageProtocol::Kitty) => buffer.push_str(&delete_all_kitty_placements()),
                None => {}
            }
        }
        if let Some(prepared) = &prepared {
            buffer.push_str(&prepared.evicted_image_deletion);
        }

        for row in 0..height {
            if !full_redraw
                && !images_need_redraw
                && screen.get(row) == self.previous_screen.get(row)
            {
                continue;
            }
            buffer.push_str(&format!("\x1b[{};1H\x1b[2K", row + 1));
            if let Some(line) = out_lines.get(row) {
                buffer.push_str(line);
            }
        }

        if let Some(cursor_pos) = cursor_pos {
            buffer.push_str(&format!(
                "\x1b[{};{}H",
                cursor_pos.row + 1,
                (width as i32).min(cursor_pos.col) + 1
            ));
            buffer.push_str(if self.show_hardware_cursor {
                "\x1b[?25h"
            } else {
                "\x1b[?25l"
            });
        } else {
            buffer.push_str("\x1b[?25l");
        }
        buffer.push_str(END_SYNCHRONIZED_OUTPUT);
        self.terminal().write(&buffer);

        self.previous_screen = screen;
        self.previous_screen_width = width;
        self.previous_screen_height = height;
        self.current_layout = Some(next_layout);
    }

    /// Fire the expired deadlines in upstream's timer order of effect:
    /// flash expiries, the selection auto-scroll interval, and scrollbar hide
    /// deadlines schedule renders, which the same tick then fires; finally
    /// the introspection query timeouts (same shape as
    /// [`TuiMainScreenInner::tick`]).
    fn tick(&mut self, now: Instant) {
        self.flashes.tick(now);
        if let Some(next) = self.selection_auto_scroll_next {
            if now >= next {
                self.auto_scroll_selection();
                // `setInterval` re-arm: fire again one period after the fired
                // instant, unless the scroll stopped (which clears it).
                if self.selection_auto_scroll_next.is_some() {
                    self.selection_auto_scroll_next = Some(next + AUTO_SCROLL_INTERVAL);
                }
            }
        }
        self.tick_scroll_views(now);
        if self.take_render_due(now) {
            self.do_render();
        }
        self.fire_expired_queries(now);
    }
}

/// `getSelectionColumns` (tui-alt-screen.ts:852-871) over a
/// [`ScreenSelection`]: snap the start/end columns to grapheme cell
/// boundaries (the end snaps outward unless it is a between-cells boundary
/// point from a word/line selection).
fn selection_columns(line: &str, row: i64, selection: &ScreenSelection) -> (usize, usize) {
    let line_width = visible_width(line);
    let mut start = 0.max(selection.min_column) as usize;
    let mut end = line_width.min(0.max(selection.max_column) as usize);
    if row == selection.start_row {
        start = get_grapheme_cell_range(line, selection.start_col.max(0) as usize)
            .map(|range| range.start)
            .unwrap_or((selection.start_col.max(0) as usize).min(line_width));
    }
    if row == selection.end_row {
        end = if selection.end_boundary {
            (selection.end_col.max(0) as usize).min(line_width)
        } else {
            get_grapheme_cell_range(line, selection.end_col.max(0) as usize)
                .map(|range| range.end)
                .unwrap_or(((selection.end_col.max(0) as usize) + 1).min(line_width))
        };
    }
    (
        start.max(0.max(selection.min_column) as usize),
        end.min(0.max(selection.max_column) as usize),
    )
}

/// Screen-space selection with box clamps (`i64` — see
/// [`TuiAltScreenInner::screen_selection`]).
#[derive(Debug, Clone, Copy)]
struct ScreenSelection {
    start_row: i64,
    start_col: i64,
    end_row: i64,
    end_col: i64,
    end_boundary: bool,
    min_row: i64,
    max_row: i64,
    min_column: i64,
    max_column: i64,
}

// =============================================================================
// Trait implementations
// =============================================================================

impl Tui for TuiAltScreen {
    fn mode(&self) -> TuiMode {
        TuiMode::Fullscreen
    }

    fn terminal(&self) -> SharedTerminal {
        Arc::clone(&self.terminal)
    }

    fn full_redraws(&self) -> u64 {
        TuiAltScreen::full_redraws(self)
    }

    fn add_child(&self, component: SharedComponent) {
        TuiAltScreen::add_child(self, component);
    }

    fn remove_child(&self, component: &SharedComponent) {
        TuiAltScreen::remove_child(self, component);
    }

    fn clear(&self) {
        TuiAltScreen::clear(self);
    }

    fn get_show_hardware_cursor(&self) -> bool {
        TuiAltScreen::get_show_hardware_cursor(self)
    }

    fn set_show_hardware_cursor(&self, enabled: bool) {
        TuiAltScreen::set_show_hardware_cursor(self, enabled);
    }

    fn get_clear_on_shrink(&self) -> bool {
        TuiAltScreen::get_clear_on_shrink(self)
    }

    fn set_clear_on_shrink(&self, enabled: bool) {
        TuiAltScreen::set_clear_on_shrink(self, enabled);
    }

    fn set_focus(&self, component: Option<SharedComponent>) {
        TuiAltScreen::set_focus(self, component);
    }

    fn get_focused_component(&self) -> Option<SharedComponent> {
        TuiAltScreen::get_focused_component(self)
    }

    fn show_overlay(
        &self,
        component: SharedComponent,
        options: Option<OverlayOptions>,
    ) -> OverlayHandle {
        TuiAltScreen::show_overlay(self, component, options)
    }

    fn hide_overlay(&self) {
        TuiAltScreen::hide_overlay(self);
    }

    fn has_overlay(&self) -> bool {
        TuiAltScreen::has_overlay(self)
    }

    fn has_overlay_entries(&self) -> bool {
        TuiAltScreen::has_overlay_entries(self)
    }

    fn start(&self) {
        TuiAltScreen::start(self);
    }

    fn stop(&self, options: TuiStopOptions) {
        TuiAltScreen::stop(self, options);
    }

    fn render_now(&self, force: bool) {
        TuiAltScreen::render_now(self, force);
    }

    fn request_render(&self, force: bool) {
        TuiAltScreen::request_render(self, force);
    }

    fn add_input_listener(&self, listener: TuiInputListener) -> u64 {
        TuiAltScreen::add_input_listener(self, listener)
    }

    fn remove_input_listener(&self, id: u64) {
        TuiAltScreen::remove_input_listener(self, id);
    }

    fn on_terminal_color_scheme_change(&self, listener: TerminalColorSchemeListener) -> u64 {
        TuiAltScreen::on_terminal_color_scheme_change(self, listener)
    }

    fn set_terminal_color_scheme_notifications(&self, enabled: bool) {
        TuiAltScreen::set_terminal_color_scheme_notifications(self, enabled);
    }

    fn query_terminal_background_color(
        &self,
        timeout: Duration,
    ) -> oneshot::Receiver<Option<RgbColor>> {
        TuiAltScreen::query_terminal_background_color(self, timeout)
    }

    fn query_terminal_color_scheme(
        &self,
        timeout: Duration,
    ) -> oneshot::Receiver<Option<TerminalColorScheme>> {
        TuiAltScreen::query_terminal_color_scheme(self, timeout)
    }

    fn invalidate(&self) {
        TuiAltScreen::invalidate(self);
    }
}

impl ViewportTui for TuiAltScreen {
    fn set_layout_root(&self, root: Option<SharedComponent>) {
        TuiAltScreen::set_layout_root(self, root);
    }

    fn viewport_top(&self) -> usize {
        TuiAltScreen::viewport_top(self)
    }

    fn is_following_output(&self) -> bool {
        TuiAltScreen::is_following_output(self)
    }

    fn scroll_by(&self, lines: i64) {
        TuiAltScreen::scroll_by(self, lines);
    }

    fn scroll_to_top(&self) {
        TuiAltScreen::scroll_to_top(self);
    }

    fn scroll_to_bottom(&self) {
        TuiAltScreen::scroll_to_bottom(self);
    }

    fn flash(&self, message: &str, duration_ms: Option<u64>) {
        TuiAltScreen::flash(self, message, duration_ms);
    }
}

impl OverlayHandleOps for TuiAltScreen {
    fn hide(&self, entry_id: u64) {
        self.run_or_queue(move |inner| inner.overlay_hide(entry_id));
    }

    fn set_hidden(&self, entry_id: u64, hidden: bool) {
        self.run_or_queue(move |inner| inner.overlay_set_hidden(entry_id, hidden));
    }

    fn is_hidden(&self, entry_id: u64) -> bool {
        self.try_read(move |inner| {
            inner
                .overlay_stack
                .iter()
                .find(|entry| entry.id == entry_id)
                .is_some_and(|entry| entry.hidden)
        })
        .unwrap_or(false)
    }

    fn focus(&self, entry_id: u64) {
        self.run_or_queue(move |inner| inner.overlay_focus(entry_id));
    }

    fn unfocus(&self, entry_id: u64, options: Option<OverlayUnfocusOptions>) {
        self.run_or_queue(move |inner| inner.overlay_unfocus(entry_id, options));
    }

    fn is_focused(&self, entry_id: u64) -> bool {
        self.try_read(move |inner| {
            let Some(entry) = inner
                .overlay_stack
                .iter()
                .find(|entry| entry.id == entry_id)
            else {
                return false;
            };
            inner
                .focused_component
                .as_ref()
                .is_some_and(|focused| same_component(focused, &entry.component))
        })
        .unwrap_or(false)
    }

    fn get_bounds(&self, entry_id: u64) -> Option<OverlayBounds> {
        self.try_read(move |inner| inner.overlay_get_bounds(entry_id))
            .flatten()
    }
}

// =============================================================================
// Tests: 1:1 intent ports of `test/tui-alt-screen.test.ts` @ 4181f66 (31 its).
// =============================================================================

#[cfg(test)]
mod tests {
    //! Every upstream `it` in `packages/tui/test/tui-alt-screen.test.ts`
    //! (4181f66) has one same-named (snake_case) Rust test here, in upstream
    //! order. Upstream `waitForRender` (nextTick + 20ms settle) is the
    //! `settle` helper; real-time waits (`setTimeout` 70/100/130ms) become
    //! explicit `tick(now)` drives at the relevant deadline (flash expiry,
    //! scrollbar hide, auto-scroll interval), per coding-standards §12.5.

    use super::*;
    use crate::components::h_stack::HStack;
    use crate::components::image::{Image, ImageOptions, ImageTheme};
    use crate::components::scroll_view::{Follow, ScrollView, ScrollViewOptions, ScrollbarMode};
    use crate::components::stack::{StackChild, StackEntryOptions, StackOptions};
    use crate::components::text::Text;
    use crate::components::v_stack::VStack;
    use crate::layout_node::Basis;
    use crate::terminal_image::{
        encode_kitty, hyperlink, register_kitty_image_metadata, reset_capabilities_cache,
        ImageDimensions, KittyEncodeOptions, KittyImageMetadata,
    };
    use crate::test_vt::{
        osc52_sequence, send_input, settle, state_lock, EnvGuard, RecordingTerminal, TestTui,
        VirtualTerminal, VtEvent,
    };
    use crate::tui::{shared_component, Focusable, SizeValue, TuiMouseEventResult};
    use std::sync::atomic::AtomicBool;
    use std::sync::MutexGuard;

    /// `TestTui` drive impl backing the shared settle/render helpers.
    impl TestTui for TuiAltScreen {
        fn tick(&self, now: Instant) {
            TuiAltScreen::tick(self, now);
        }

        fn has_pending_work(&self) -> bool {
            TuiAltScreen::has_pending_work(self)
        }
    }

    // ---------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------

    /// `TestText`: upstream `Text` without padding, with a shared line list
    /// the test can replace (upstream calls `text.setText(...)`; real `Text`
    /// needs `&mut self`, unreachable through `SharedComponent`).
    struct TestText {
        lines: Arc<Mutex<Vec<String>>>,
    }

    impl Component for TestText {
        fn render(&self, _width: usize) -> Vec<String> {
            lock_shared(&self.lines).clone()
        }
    }

    fn test_text(lines: &[String]) -> (SharedComponent, Arc<Mutex<Vec<String>>>) {
        let shared = Arc::new(Mutex::new(lines.to_vec()));
        (
            shared_component(TestText {
                lines: Arc::clone(&shared),
            }),
            shared,
        )
    }

    fn set_lines(handle: &Arc<Mutex<Vec<String>>>, lines: &[String]) {
        *lock_shared(handle) = lines.to_vec();
    }

    /// Upstream `new Text(text, 0, 0)`.
    fn text(text: &str) -> SharedComponent {
        shared_component(Text::new(text, 0, 0, None))
    }

    /// `Array.from({ length: n }, (_, i) => `line ${i + 1}`).join("\n")`.
    fn numbered_lines(n: usize) -> Vec<String> {
        (1..=n).map(|index| format!("line {index}")).collect()
    }

    fn numbered_text(n: usize) -> (SharedComponent, Arc<Mutex<Vec<String>>>) {
        test_text(&numbered_lines(n))
    }

    /// `terminal.getViewport().map((line) => line.trimEnd())`.
    fn trimmed_viewport(terminal: &VirtualTerminal) -> Vec<String> {
        terminal
            .get_viewport()
            .iter()
            .map(|line| line.trim_end().to_string())
            .collect()
    }

    /// Lock a shared scroll view for assertions (upstream reads the live
    /// `ScrollView` object: `scrollView.scrollTop`, `isScrollbarVisible`).
    fn with_sv<R>(shared: &SharedComponent, f: impl FnOnce(&ScrollView) -> R) -> R {
        let guard = lock_component(shared);
        let scroll_view = guard
            .as_scroll_view()
            .unwrap_or_else(|| unreachable!("test component is a ScrollView"));
        f(scroll_view)
    }

    fn stop(tui: &TuiAltScreen) {
        tui.stop(TuiStopOptions::default());
    }

    // ---------------------------------------------------------------------
    // it("renders a terminal-height viewport and preserves manual scroll position")
    // ---------------------------------------------------------------------

    #[test]
    fn renders_a_terminal_height_viewport_and_preserves_manual_scroll_position() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (text, handle) = numbered_text(10);
        tui.add_child(text);
        tui.start();
        settle(&tui);

        assert_eq!(
            trimmed_viewport(&terminal),
            vec!["line 7", "line 8", "line 9", "line 10"]
        );
        assert!(tui.is_following_output());

        send_input(&terminal, &tui, "\x1b[<64;1;1M");
        settle(&tui);
        assert_eq!(
            trimmed_viewport(&terminal),
            vec!["line 6", "line 7", "line 8", "line 9"]
        );
        assert_eq!(tui.viewport_top(), 5);
        assert!(!tui.is_following_output());

        set_lines(&handle, &numbered_lines(12));
        tui.request_render(false);
        settle(&tui);
        assert_eq!(
            trimmed_viewport(&terminal),
            vec!["line 6", "line 7", "line 8", "line 9"]
        );

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("keeps an explicit dock fixed while the transcript scrolls")
    // ---------------------------------------------------------------------

    #[test]
    fn keeps_an_explicit_dock_fixed_while_the_transcript_scrolls() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(20, 6);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (transcript_text, transcript_handle) = numbered_text(8);
        let transcript = shared_component(ScrollView::new(
            transcript_text,
            ScrollViewOptions {
                follow: Follow::End,
                primary: true,
                ..ScrollViewOptions::default()
            },
        ));
        let dock = shared_component(VStack::new(
            vec![
                StackChild::Component(text("editor")),
                StackChild::Component(text("footer")),
            ],
            StackOptions::default(),
        ));
        tui.set_layout_root(Some(shared_component(VStack::new(
            vec![
                StackChild::Entry(
                    transcript.clone(),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(0.0)),
                        grow: Some(1.0),
                        min_size: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
                StackChild::Entry(
                    dock,
                    StackEntryOptions {
                        basis: Some(Basis::Auto),
                        min_size: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        ))));
        tui.start();
        settle(&tui);

        assert_eq!(
            trimmed_viewport(&terminal),
            vec!["line 5", "line 6", "line 7", "line 8", "editor", "footer"]
        );

        // Wheel over the dock falls back to the primary transcript scroll view.
        send_input(&terminal, &tui, "\x1b[<64;1;6M");
        settle(&tui);
        assert_eq!(
            trimmed_viewport(&terminal),
            vec!["line 4", "line 5", "line 6", "line 7", "editor", "footer"]
        );
        assert!(!with_sv(&transcript, ScrollView::is_following_end));

        set_lines(&transcript_handle, &numbered_lines(10));
        tui.request_render(false);
        settle(&tui);
        assert_eq!(
            trimmed_viewport(&terminal),
            vec!["line 4", "line 5", "line 6", "line 7", "editor", "footer"]
        );

        tui.scroll_to_bottom();
        settle(&tui);
        assert_eq!(
            trimmed_viewport(&terminal),
            vec!["line 7", "line 8", "line 9", "line 10", "editor", "footer"]
        );
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("restores keyboard state before leaving alt mode and prints the full document")
    // ---------------------------------------------------------------------

    #[test]
    fn restores_keyboard_state_before_leaving_alt_mode_and_prints_the_full_document() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 3);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (content, _) = test_text(&[
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
            "fourth".to_string(),
            "fifth".to_string(),
            "sixth".to_string(),
        ]);
        tui.add_child(content);
        tui.start();
        settle(&tui);
        stop(&tui);

        let events = terminal.events();
        let write_index = |needle: &str| {
            events
                .iter()
                .position(|event| matches!(event, VtEvent::Write(data) if data.contains(needle)))
        };
        let start_index = events
            .iter()
            .position(|event| matches!(event, VtEvent::Start));
        let alt_screen_enter_index = write_index("\x1b[?1049h");
        let stop_index = events
            .iter()
            .position(|event| matches!(event, VtEvent::Stop));
        let mouse_disable_index = write_index("\x1b[?1006l");
        let main_screen_restore_index = write_index("\x1b[?1049l");

        assert!(
            alt_screen_enter_index.is_some() && alt_screen_enter_index < start_index,
            "1049h must be written before terminal start"
        );
        assert!(
            mouse_disable_index.is_some() && mouse_disable_index < stop_index,
            "1006l must be written before terminal stop"
        );
        assert!(
            main_screen_restore_index.is_some() && main_screen_restore_index > stop_index,
            "1049l must be written after terminal stop"
        );

        let restore_index =
            main_screen_restore_index.unwrap_or_else(|| unreachable!("checked above"));
        let VtEvent::Write(restore_data) = &events[restore_index] else {
            unreachable!("restore event is a write");
        };
        for line in ["first", "second", "third", "fourth", "fifth", "sixth"] {
            assert!(restore_data.contains(line), "restore must contain {line}");
        }
        let first_index = restore_data.find("first");
        let sixth_index = restore_data.find("sixth");
        assert!(first_index < sixth_index);
    }

    // ---------------------------------------------------------------------
    // Self-test gate: exit reprint strips OSC 133 zone prefixes (T31 自测清单)
    // ---------------------------------------------------------------------

    #[test]
    fn exit_reprint_strips_osc133_zone_prefixes() {
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 3);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (content, _) = test_text(&[
            "\x1b]133;A\x07prompt one".to_string(),
            "plain".to_string(),
            "\x1b]133;B\x1b\\\x1b]133;C\x07zoned".to_string(),
        ]);
        tui.add_child(content);
        tui.start();
        settle(&tui);
        stop(&tui);

        let restore = terminal
            .events()
            .iter()
            .rev()
            .find_map(|event| match event {
                VtEvent::Write(data) if data.contains("\x1b[?1049l") => Some(data.clone()),
                _ => None,
            })
            .unwrap_or_default();
        assert!(
            restore.contains("prompt one"),
            "reprint keeps text: {restore:?}"
        );
        assert!(restore.contains("zoned"));
        assert!(
            !restore.contains("\x1b]133;"),
            "reprint must strip OSC 133 prefixes: {restore:?}"
        );

        // Pure-function spot checks (BEL/ST terminators, repeated zones,
        // non-zone lines untouched).
        assert_eq!(strip_osc133_zone_prefix("\x1b]133;A\x07x"), "x");
        assert_eq!(strip_osc133_zone_prefix("\x1b]133;B\x1b\\x"), "x");
        assert_eq!(
            strip_osc133_zone_prefix("\x1b]133;B\x07\x1b]133;C\x1b\\x"),
            "x"
        );
        assert_eq!(strip_osc133_zone_prefix("plain"), "plain");
        assert_eq!(
            strip_osc133_zone_prefix("\x1b]133;D\x07x"),
            "\x1b]133;D\x07x"
        );
    }

    // ---------------------------------------------------------------------
    // it("invalidates overlays with an explicit layout root")
    // ---------------------------------------------------------------------

    /// Overlay component with an observable `invalidate` (upstream overrides
    /// `overlay.invalidate` on a `Text`).
    struct InvalidatableOverlay {
        lines: Vec<String>,
        invalidated: Arc<AtomicBool>,
    }

    impl Component for InvalidatableOverlay {
        fn render(&self, _width: usize) -> Vec<String> {
            self.lines.clone()
        }

        fn invalidate(&mut self) {
            self.invalidated.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn invalidates_overlays_with_an_explicit_layout_root() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let tui = TuiAltScreen::new(Box::new(VirtualTerminal::default()));
        let invalidated = Arc::new(AtomicBool::new(false));
        let overlay = shared_component(InvalidatableOverlay {
            lines: vec!["overlay".to_string()],
            invalidated: Arc::clone(&invalidated),
        });
        tui.set_layout_root(Some(text("root")));
        tui.show_overlay(overlay, None);

        tui.invalidate();

        assert!(invalidated.load(Ordering::Relaxed));
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("routes wheel input to the scroll view under the pointer")
    // ---------------------------------------------------------------------

    #[test]
    fn routes_wheel_input_to_the_scroll_view_under_the_pointer() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (left_text, _) = test_text(&[
            "a1".to_string(),
            "a2".to_string(),
            "a3".to_string(),
            "a4".to_string(),
            "a5".to_string(),
            "a6".to_string(),
            "a7".to_string(),
        ]);
        let left = shared_component(ScrollView::new(
            left_text,
            ScrollViewOptions {
                follow: Follow::End,
                primary: true,
                ..ScrollViewOptions::default()
            },
        ));
        let (right_text, _) = test_text(&[
            "b1".to_string(),
            "b2".to_string(),
            "b3".to_string(),
            "b4".to_string(),
            "b5".to_string(),
            "b6".to_string(),
            "b7".to_string(),
        ]);
        let right = shared_component(ScrollView::new(
            right_text,
            ScrollViewOptions {
                follow: Follow::End,
                ..ScrollViewOptions::default()
            },
        ));
        tui.set_layout_root(Some(shared_component(HStack::new(
            vec![
                StackChild::Entry(
                    left.clone(),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(10.0)),
                        shrink: Some(0.0),
                        ..StackEntryOptions::default()
                    },
                ),
                StackChild::Entry(
                    right.clone(),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(10.0)),
                        shrink: Some(0.0),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        ))));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<64;15;1M");
        settle(&tui);
        assert_eq!(with_sv(&left, ScrollView::scroll_top), 3);
        assert_eq!(with_sv(&right, ScrollView::scroll_top), 2);
        assert_eq!(
            trimmed_viewport(&terminal),
            vec![
                "a4        b3",
                "a5        b4",
                "a6        b5",
                "a7        b6"
            ]
        );
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("uses button-motion tracking inside terminal multiplexers")
    // ---------------------------------------------------------------------

    #[test]
    fn uses_button_motion_tracking_inside_terminal_multiplexers() {
        let _caps = CapsGuard::lock_only();
        let _tmux = EnvGuard::set("TMUX", None);
        let _zellij = EnvGuard::set("ZELLIJ", None);
        let _sty = EnvGuard::set("STY", None);
        let _term = EnvGuard::set("TERM", Some("xterm-256color"));

        let direct_terminal = RecordingTerminal::default();
        let direct_tui = TuiAltScreen::new(Box::new(direct_terminal.clone()));
        direct_tui.start();
        assert!(direct_terminal.writes().contains("\x1b[?1003h"));
        stop(&direct_tui);

        type Env = [(&'static str, Option<&'static str>); 4];
        let multiplexers: [(&str, Env); 5] = [
            (
                "tmux environment",
                [
                    ("TMUX", Some("/tmp/tmux/default,1,0")),
                    ("ZELLIJ", None),
                    ("STY", None),
                    ("TERM", Some("xterm-256color")),
                ],
            ),
            (
                "tmux TERM",
                [
                    ("TMUX", None),
                    ("ZELLIJ", None),
                    ("STY", None),
                    ("TERM", Some("tmux-256color")),
                ],
            ),
            (
                "Zellij environment",
                [
                    ("TMUX", None),
                    ("ZELLIJ", Some("0")),
                    ("STY", None),
                    ("TERM", Some("xterm-256color")),
                ],
            ),
            (
                "Screen environment",
                [
                    ("TMUX", None),
                    ("ZELLIJ", None),
                    ("STY", Some("123.session")),
                    ("TERM", Some("xterm-256color")),
                ],
            ),
            (
                "Screen TERM",
                [
                    ("TMUX", None),
                    ("ZELLIJ", None),
                    ("STY", None),
                    ("TERM", Some("screen-256color")),
                ],
            ),
        ];
        for (name, environment) in multiplexers {
            let _guards: Vec<EnvGuard> = environment
                .iter()
                .map(|(key, value)| EnvGuard::set(key, *value))
                .collect();
            let terminal = RecordingTerminal::default();
            let tui = TuiAltScreen::new(Box::new(terminal.clone()));
            tui.start();
            let writes = terminal.writes();
            assert!(
                writes.contains("\x1b[?1002h"),
                "{name} should enable button-motion tracking"
            );
            assert!(
                !writes.contains("\x1b[?1003h"),
                "{name} should not enable all-motion tracking"
            );
            assert!(
                writes.contains("\x1b[?1006h"),
                "{name} should enable SGR mouse encoding"
            );
            stop(&tui);
        }
    }

    // ---------------------------------------------------------------------
    // it("invokes the right-click paste handler only on Windows")
    // ---------------------------------------------------------------------

    #[test]
    fn invokes_the_right_click_paste_handler_only_on_windows() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::default();
        let paste_count = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&paste_count);
        let tui = TuiAltScreen::with_options(
            Box::new(terminal.clone()),
            None,
            None,
            TuiAltScreenOptions {
                on_right_click_paste: Some(Arc::new(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                })),
                win32_override: Some(true),
                ..TuiAltScreenOptions::default()
            },
        );
        tui.start();
        send_input(&terminal, &tui, "\x1b[<2;1;1M");
        send_input(&terminal, &tui, "\x1b[<2;1;1m");
        assert_eq!(paste_count.load(Ordering::Relaxed), 1);

        tui.set_win32_for_test(false);
        send_input(&terminal, &tui, "\x1b[<2;1;1M");
        assert_eq!(paste_count.load(Ordering::Relaxed), 1);
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("chains unused wheel delta to an outer scroll view")
    // ---------------------------------------------------------------------

    #[test]
    fn chains_unused_wheel_delta_to_an_outer_scroll_view() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(20, 4);
        let tui = TuiAltScreen::with_options(
            Box::new(terminal.clone()),
            None,
            None,
            TuiAltScreenOptions {
                wheel_scroll_lines: Some(3),
                ..TuiAltScreenOptions::default()
            },
        );
        let (inner_text, _) = test_text(&[
            "i1".to_string(),
            "i2".to_string(),
            "i3".to_string(),
            "i4".to_string(),
            "i5".to_string(),
            "i6".to_string(),
        ]);
        let inner = shared_component(ScrollView::new(inner_text, ScrollViewOptions::default()));
        let (tail_text, _) = test_text(&[
            "tail1".to_string(),
            "tail2".to_string(),
            "tail3".to_string(),
            "tail4".to_string(),
            "tail5".to_string(),
        ]);
        let outer = shared_component(ScrollView::new(
            shared_component(VStack::new(
                vec![
                    StackChild::Entry(
                        inner.clone(),
                        StackEntryOptions {
                            basis: Some(Basis::Fixed(2.0)),
                            ..StackEntryOptions::default()
                        },
                    ),
                    StackChild::Component(tail_text),
                ],
                StackOptions::default(),
            )),
            ScrollViewOptions {
                primary: true,
                ..ScrollViewOptions::default()
            },
        ));
        tui.set_layout_root(Some(outer.clone()));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<65;1;1M");
        settle(&tui);
        assert_eq!(with_sv(&inner, ScrollView::scroll_top), 3);
        assert_eq!(with_sv(&outer, ScrollView::scroll_top), 0);

        send_input(&terminal, &tui, "\x1b[<65;1;1M");
        settle(&tui);
        assert_eq!(with_sv(&inner, ScrollView::scroll_top), 4);
        assert_eq!(with_sv(&outer, ScrollView::scroll_top), 2);
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("supports configurable keyboard viewport navigation with four rows of page overlap")
    // ---------------------------------------------------------------------

    #[test]
    fn supports_configurable_keyboard_viewport_navigation_with_four_rows_of_page_overlap() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(20, 8);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (content, _) = numbered_text(12);
        tui.add_child(content);
        tui.start();
        settle(&tui);

        let lines_1_to_8: Vec<String> = numbered_lines(8);
        let lines_5_to_12: Vec<String> = (5..=12).map(|index| format!("line {index}")).collect();

        send_input(&terminal, &tui, "\x1b[57421u");
        send_input(&terminal, &tui, "\x1b[57421;1:3u");
        settle(&tui);
        assert_eq!(trimmed_viewport(&terminal), lines_1_to_8);

        send_input(&terminal, &tui, "\x1b[57422u");
        send_input(&terminal, &tui, "\x1b[57422;1:3u");
        settle(&tui);
        assert_eq!(trimmed_viewport(&terminal), lines_5_to_12);

        send_input(&terminal, &tui, "\x1bOH");
        settle(&tui);
        assert_eq!(trimmed_viewport(&terminal), lines_1_to_8);

        send_input(&terminal, &tui, "\x1bOF");
        settle(&tui);
        assert_eq!(trimmed_viewport(&terminal), lines_5_to_12);

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("scrolls the transcript by half a page with custom bindings")
    // ---------------------------------------------------------------------

    #[test]
    fn scrolls_the_transcript_by_half_a_page_with_custom_bindings() {
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(20, 10);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let mut user_bindings = crate::keybindings::KeybindingsConfig::new();
        user_bindings.insert(
            "tui.altScreen.halfPageUp".to_string(),
            crate::keybindings::KeyBindingValue::Single("ctrl+u".to_string()),
        );
        user_bindings.insert(
            "tui.altScreen.halfPageDown".to_string(),
            crate::keybindings::KeyBindingValue::Single("ctrl+d".to_string()),
        );
        crate::keybindings::set_keybindings(crate::keybindings::KeybindingsManager::new(
            crate::keybindings::tui_keybindings().to_vec(),
            user_bindings,
        ));
        // Restore the default manager afterwards (upstream restores the
        // previous instance; a fresh default manager is equivalent).
        let restore = scopeguard_defaults();
        let (content, _) = numbered_text(30);
        tui.add_child(content);
        tui.start();
        settle(&tui);
        assert_eq!(tui.viewport_top(), 20);

        send_input(&terminal, &tui, "\x15");
        settle(&tui);
        assert_eq!(tui.viewport_top(), 15);

        send_input(&terminal, &tui, "\x04");
        settle(&tui);
        assert_eq!(tui.viewport_top(), 20);

        stop(&tui);
        drop(restore);
    }

    /// Reinstall the default keybinding manager on drop (upstream
    /// `setKeybindings(originalKeybindings)` in `finally`).
    fn scopeguard_defaults() -> impl Drop {
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                crate::keybindings::set_keybindings(
                    crate::keybindings::KeybindingsManager::with_defaults(),
                );
            }
        }
        Restore
    }

    // ---------------------------------------------------------------------
    // it("routes Ctrl-modified viewport navigation to the focused component")
    // ---------------------------------------------------------------------

    /// Editor component recording its inputs (upstream's inline `{ focused,
    /// render, invalidate, handleInput }` object).
    struct RecordingEditor {
        inputs: Arc<Mutex<Vec<String>>>,
        focused: Arc<Mutex<bool>>,
    }

    impl Component for RecordingEditor {
        fn render(&self, _width: usize) -> Vec<String> {
            vec!["editor".to_string()]
        }

        fn handle_input(&mut self, data: &str) {
            lock_shared(&self.inputs).push(data.to_string());
        }

        fn as_focusable(&self) -> Option<&dyn Focusable> {
            Some(self)
        }

        fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
            Some(self)
        }
    }

    impl Focusable for RecordingEditor {
        fn focused(&self) -> bool {
            *lock_shared(&self.focused)
        }

        fn set_focused(&mut self, focused: bool) {
            *lock_shared(&self.focused) = focused;
        }
    }

    #[test]
    fn routes_ctrl_modified_viewport_navigation_to_the_focused_component() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(20, 6);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (transcript_text, _) = numbered_text(12);
        let transcript = shared_component(ScrollView::new(
            transcript_text,
            ScrollViewOptions {
                follow: Follow::End,
                primary: true,
                ..ScrollViewOptions::default()
            },
        ));
        let editor_inputs = Arc::new(Mutex::new(Vec::new()));
        let editor = shared_component(RecordingEditor {
            inputs: Arc::clone(&editor_inputs),
            focused: Arc::new(Mutex::new(false)),
        });
        tui.set_layout_root(Some(shared_component(VStack::new(
            vec![
                StackChild::Entry(
                    transcript.clone(),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(0.0)),
                        grow: Some(1.0),
                        min_size: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
                StackChild::Entry(
                    editor.clone(),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(1.0)),
                        shrink: Some(0.0),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        ))));
        tui.set_focus(Some(editor.clone()));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1bOH");
        settle(&tui);
        assert_eq!(with_sv(&transcript, ScrollView::scroll_top), 0);
        assert!(lock_shared(&editor_inputs).is_empty());

        let modified_inputs = [
            "\x1b[1;5H",
            "\x1b[1;5F",
            "\x1b[5;5~",
            "\x1b[6;5~",
            "\x1b[57423;5u",
        ];
        for input in modified_inputs {
            send_input(&terminal, &tui, input);
        }
        send_input(&terminal, &tui, "\x1b[57423;5:3u");
        settle(&tui);
        assert_eq!(with_sv(&transcript, ScrollView::scroll_top), 0);
        assert_eq!(lock_shared(&editor_inputs).as_slice(), modified_inputs);

        send_input(&terminal, &tui, "\x1b[6~");
        settle(&tui);
        assert_eq!(with_sv(&transcript, ScrollView::scroll_top), 1);
        assert_eq!(lock_shared(&editor_inputs).as_slice(), modified_inputs);

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("jumps between OSC 133 semantic prompt markers")
    // ---------------------------------------------------------------------

    #[test]
    fn jumps_between_osc_133_semantic_prompt_markers() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        const OSC133_ZONE_START: &str = "\x1b]133;A\x07";
        let terminal = VirtualTerminal::new(20, 3);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let mut lines = Vec::new();
        for message in 1..=4 {
            lines.push(format!("{OSC133_ZONE_START}message {message}"));
            lines.push("detail".to_string());
        }
        let (content, _) = test_text(&lines);
        tui.add_child(content);
        tui.start();
        settle(&tui);
        assert_eq!(tui.viewport_top(), 5);

        send_input(&terminal, &tui, "\x1b[57419;6u");
        send_input(&terminal, &tui, "\x1b[57419;6:3u");
        settle(&tui);
        assert_eq!(tui.viewport_top(), 4);
        assert_eq!(terminal.get_viewport()[0].trim_end(), "message 3");

        send_input(&terminal, &tui, "\x1b[1;6A");
        settle(&tui);
        assert_eq!(tui.viewport_top(), 2);
        assert_eq!(terminal.get_viewport()[0].trim_end(), "message 2");

        send_input(&terminal, &tui, "\x1b[57420;6u");
        send_input(&terminal, &tui, "\x1b[57420;6:3u");
        settle(&tui);
        assert_eq!(tui.viewport_top(), 4);
        assert_eq!(terminal.get_viewport()[0].trim_end(), "message 3");

        send_input(&terminal, &tui, "\x1b[1;6B");
        settle(&tui);
        assert_eq!(tui.viewport_top(), 5);
        assert_eq!(terminal.get_viewport()[1].trim_end(), "message 4");
        assert!(tui.is_following_output());

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("ignores horizontal trackpad wheel events")
    // ---------------------------------------------------------------------

    #[test]
    fn ignores_horizontal_trackpad_wheel_events() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (content, _) = numbered_text(8);
        tui.add_child(content);
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<66;1;1M");
        send_input(&terminal, &tui, "\x1b[<67;1;1M");
        settle(&tui);
        assert_eq!(tui.viewport_top(), 4);
        assert_eq!(
            trimmed_viewport(&terminal),
            vec!["line 5", "line 6", "line 7", "line 8"]
        );

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("drags a visible scrollbar thumb and keeps it visible until release")
    // ---------------------------------------------------------------------

    #[test]
    fn drags_a_visible_scrollbar_thumb_and_keeps_it_visible_until_release() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(10, 5);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (content, _) = numbered_text(20);
        let scroll_view = shared_component(ScrollView::new(
            content,
            ScrollViewOptions {
                primary: true,
                scrollbar: ScrollbarMode::Auto,
                scrollbar_hide_delay: Duration::from_millis(50),
                ..ScrollViewOptions::default()
            },
        ));
        tui.set_layout_root(Some(scroll_view.clone()));
        tui.start();
        settle(&tui);
        assert!(!with_sv(&scroll_view, ScrollView::is_scrollbar_visible));

        send_input(&terminal, &tui, "\x1b[<65;10;1M");
        settle(&tui);
        assert_eq!(with_sv(&scroll_view, ScrollView::scroll_top), 1);
        assert!(with_sv(&scroll_view, ScrollView::is_scrollbar_visible));

        // Primary-button press on the thumb starts the drag; while the
        // scrollbar is active no hide deadline is armed (upstream: 70ms
        // real-time wait).
        send_input(&terminal, &tui, "\x1b[<0;10;1M");
        let now = settle(&tui);
        tui.tick(now + Duration::from_millis(70));
        settle(&tui);
        assert!(with_sv(&scroll_view, ScrollView::is_scrollbar_visible));

        send_input(&terminal, &tui, "\x1b[<32;10;4M");
        settle(&tui);
        assert_eq!(with_sv(&scroll_view, ScrollView::scroll_top), 15);
        // V14-16 (457ae8c79): the viewport now overlays the full track —
        // `│` on non-thumb rows, `█` on the thumb while active (scrollTop
        // 15/15 → thumb rows 3-4 of 5).
        assert_eq!(
            trimmed_viewport(&terminal),
            vec![
                "line 16  │",
                "line 17  │",
                "line 18  │",
                "line 19  █",
                "line 20  █"
            ]
        );

        // Release over the thumb keeps it hovered: still visible after 70ms.
        send_input(&terminal, &tui, "\x1b[<0;10;4m");
        let now = settle(&tui);
        assert!(with_sv(&scroll_view, ScrollView::is_scrollbar_visible));
        tui.tick(now + Duration::from_millis(70));
        settle(&tui);
        assert!(with_sv(&scroll_view, ScrollView::is_scrollbar_visible));

        // Moving off the thumb clears the hover; the hide deadline fires.
        send_input(&terminal, &tui, "\x1b[<35;9;4M");
        let now = settle(&tui);
        tui.tick(now + Duration::from_millis(70));
        settle(&tui);
        assert!(!with_sv(&scroll_view, ScrollView::is_scrollbar_visible));

        // Wheeling over the thumb shows and hovers it again.
        send_input(&terminal, &tui, "\x1b[<64;10;5M");
        settle(&tui);
        assert_eq!(with_sv(&scroll_view, ScrollView::scroll_top), 14);
        let now = settle(&tui);
        tui.tick(now + Duration::from_millis(70));
        settle(&tui);
        assert!(with_sv(&scroll_view, ScrollView::is_scrollbar_visible));

        send_input(&terminal, &tui, "\x1b[<35;9;5M");
        let now = settle(&tui);
        tui.tick(now + Duration::from_millis(70));
        settle(&tui);
        assert!(!with_sv(&scroll_view, ScrollView::is_scrollbar_visible));

        assert!(terminal
            .events()
            .iter()
            .all(|event| !matches!(event, VtEvent::Write(data) if data.contains("\x1b]52;c;"))));
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("keeps the scrollbar column selectable while the thumb is hidden")
    // ---------------------------------------------------------------------

    #[test]
    fn keeps_the_scrollbar_column_selectable_while_the_thumb_is_hidden() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(10, 2);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (content, _) = test_text(&[
            "123456789A".to_string(),
            "abcdefghij".to_string(),
            "more".to_string(),
            "lines".to_string(),
        ]);
        let scroll_view = shared_component(ScrollView::new(
            content,
            ScrollViewOptions {
                scrollbar: ScrollbarMode::Auto,
                ..ScrollViewOptions::default()
            },
        ));
        tui.set_layout_root(Some(scroll_view.clone()));
        tui.start();
        settle(&tui);
        assert!(!with_sv(&scroll_view, ScrollView::is_scrollbar_visible));

        send_input(&terminal, &tui, "\x1b[<0;10;1M");
        send_input(&terminal, &tui, "\x1b[<32;10;2M");
        send_input(&terminal, &tui, "\x1b[<0;10;2m");
        settle(&tui);

        let expected = osc52_sequence("A\nabcdefghij");
        assert!(terminal
            .events()
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains(&expected))),);
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("reveals an auto scrollbar when the pointer enters its hidden track")
    // ---------------------------------------------------------------------

    #[test]
    fn reveals_an_auto_scrollbar_when_the_pointer_enters_its_hidden_track() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(10, 5);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (content, _) = numbered_text(20);
        let scroll_view = shared_component(ScrollView::new(
            content,
            ScrollViewOptions {
                primary: true,
                scrollbar: ScrollbarMode::Auto,
                scrollbar_hide_delay: Duration::from_millis(20),
                ..ScrollViewOptions::default()
            },
        ));
        tui.set_layout_root(Some(scroll_view.clone()));
        tui.start();
        settle(&tui);
        assert!(!with_sv(&scroll_view, ScrollView::is_scrollbar_visible));

        // Pointer motion (button 35 = no-button move) onto the hidden track
        // column wakes it (hover hit-test with includeHiddenAuto).
        send_input(&terminal, &tui, "\x1b[<35;10;3M");
        settle(&tui);
        assert!(with_sv(&scroll_view, ScrollView::is_scrollbar_visible));
        assert!(with_sv(&scroll_view, ScrollView::is_scrollbar_active));
        assert!(terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains(['│', '█'])));

        // Leaving the track arms the hide deadline; after it fires the
        // scrollbar is hidden again.
        send_input(&terminal, &tui, "\x1b[<35;9;3M");
        let now = settle(&tui);
        tui.tick(now + Duration::from_millis(40));
        settle(&tui);
        assert!(!with_sv(&scroll_view, ScrollView::is_scrollbar_visible));
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("jumps to a scrollbar track position and continues dragging from there")
    // ---------------------------------------------------------------------

    #[test]
    fn jumps_to_a_scrollbar_track_position_and_continues_dragging_from_there() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(10, 10);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (content, _) = numbered_text(50);
        let scroll_view = shared_component(ScrollView::new(
            content,
            ScrollViewOptions {
                primary: true,
                scrollbar: ScrollbarMode::Always,
                ..ScrollViewOptions::default()
            },
        ));
        tui.set_layout_root(Some(scroll_view.clone()));
        tui.start();
        settle(&tui);
        assert_eq!(with_sv(&scroll_view, ScrollView::scroll_top), 0);

        // Press on the track ABOVE the thumb: the thumb center jumps to the
        // pointer row (content 50 / track 10 → thumb 2 rows; press row 6 →
        // thumb top 5, center 6 → scrollTop 20).
        send_input(&terminal, &tui, "\x1b[<0;10;6M");
        settle(&tui);
        assert_eq!(with_sv(&scroll_view, ScrollView::scroll_top), 20);

        // Drag to the bottom track row: thumb center follows the pointer.
        send_input(&terminal, &tui, "\x1b[<32;10;10M");
        settle(&tui);
        assert_eq!(with_sv(&scroll_view, ScrollView::scroll_top), 40);

        // Track drags/presses never touch the clipboard.
        send_input(&terminal, &tui, "\x1b[<0;10;10m");
        settle(&tui);
        assert!(terminal
            .events()
            .iter()
            .all(|event| !matches!(event, VtEvent::Write(data) if data.contains("\x1b]52;c;"))));
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("opens an OSC 8 hyperlink on click but not on drag")
    // ---------------------------------------------------------------------

    #[test]
    fn opens_an_osc_8_hyperlink_on_click_but_not_on_drag() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 3);
        let opened_urls = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::clone(&opened_urls);
        let tui = TuiAltScreen::with_options(
            Box::new(terminal.clone()),
            None,
            None,
            TuiAltScreenOptions {
                open_url: Some(Arc::new(move |url: &str| {
                    lock_shared(&opened).push(url.to_string());
                })),
                ..TuiAltScreenOptions::default()
            },
        );
        let url = "https://example.com/path?q=1";
        let bel_url = "https://example.com/bel";
        let emoji_url = "https://example.com/emoji";
        tui.add_child(text(&format!(
            "{}\n\x1b]8;;{bel_url}\x07link\x1b]8;;\x07\n{}",
            hyperlink("link", url),
            hyperlink("\u{1F642}", emoji_url),
        )));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<0;2;1M");
        send_input(&terminal, &tui, "\x1b[<0;2;1m");
        settle(&tui);
        assert_eq!(lock_shared(&opened_urls).as_slice(), [url]);

        send_input(&terminal, &tui, "\x1b[<0;2;2M");
        send_input(&terminal, &tui, "\x1b[<0;2;2m");
        settle(&tui);
        assert_eq!(lock_shared(&opened_urls).as_slice(), [url, bel_url]);

        send_input(&terminal, &tui, "\x1b[<0;2;3M");
        send_input(&terminal, &tui, "\x1b[<0;2;3m");
        settle(&tui);
        assert_eq!(
            lock_shared(&opened_urls).as_slice(),
            [url, bel_url, emoji_url]
        );

        send_input(&terminal, &tui, "\x1b[<0;2;1M");
        send_input(&terminal, &tui, "\x1b[<32;4;1M");
        send_input(&terminal, &tui, "\x1b[<0;4;1m");
        settle(&tui);
        assert_eq!(
            lock_shared(&opened_urls).as_slice(),
            [url, bel_url, emoji_url]
        );

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("selects visible text with the mouse and copies it with OSC 52")
    // ---------------------------------------------------------------------

    #[test]
    fn selects_visible_text_with_the_mouse_and_copies_it_with_osc_52() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("\x1b[1mal\x1b[0mpha\nbeta\ngamma\ndelta"));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<0;1;1M");
        send_input(&terminal, &tui, "\x1b[<32;4;2M");
        send_input(&terminal, &tui, "\x1b[<0;4;2m");
        settle(&tui);

        let expected_clipboard_sequence = osc52_sequence("alpha\nbeta");
        assert!(
            terminal
                .events()
                .iter()
                .any(|event| matches!(event, VtEvent::Write(data) if data.contains(&expected_clipboard_sequence))),
        );
        assert!(terminal
            .events()
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains("\x1b[7m"))));
        assert!(
            terminal
                .events()
                .iter()
                .any(|event| matches!(event, VtEvent::Write(data) if data.contains("al\x1b[0m\x1b[7mpha"))),
            "selection inverse must be reapplied after a reset inside the selection"
        );
        assert!(terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("Copied!")));

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("uses an injected copySelection handler instead of OSC 52 and reports success")
    // ---------------------------------------------------------------------

    #[test]
    fn uses_an_injected_copy_selection_handler_instead_of_osc52_and_reports_success() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let copied: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let copied_handle = Arc::clone(&copied);
        let tui = TuiAltScreen::with_options(
            Box::new(terminal.clone()),
            None,
            None,
            TuiAltScreenOptions {
                copy_selection: Some(Arc::new(move |text: &str| {
                    lock_shared(&copied_handle).push(text.to_string());
                    true
                })),
                ..TuiAltScreenOptions::default()
            },
        );
        tui.add_child(text("alpha\nbeta\ngamma\ndelta"));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<0;1;1M");
        send_input(&terminal, &tui, "\x1b[<32;4;2M");
        send_input(&terminal, &tui, "\x1b[<0;4;2m");
        settle(&tui);

        assert_eq!(lock_shared(&copied).as_slice(), ["alpha\nbeta".to_string()]);
        assert!(
            terminal
                .events()
                .iter()
                .all(|event| !matches!(event, VtEvent::Write(data) if data.contains("\x1b]52;c;"))),
            "must not emit OSC 52 when a copySelection handler is provided"
        );
        assert!(terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("Copied!")));

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("leaves selections visible without copying when copyOnSelect is disabled")
    // ---------------------------------------------------------------------

    #[test]
    fn leaves_selections_visible_without_copying_when_copy_on_select_is_disabled() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let copied: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let copied_handle = Arc::clone(&copied);
        let tui = TuiAltScreen::with_options(
            Box::new(terminal.clone()),
            None,
            None,
            TuiAltScreenOptions {
                copy_on_select: Some(false),
                copy_selection: Some(Arc::new(move |text: &str| {
                    lock_shared(&copied_handle).push(text.to_string());
                    true
                })),
                ..TuiAltScreenOptions::default()
            },
        );
        tui.add_child(text("alpha\nbeta\ngamma\ndelta"));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<0;1;1M");
        send_input(&terminal, &tui, "\x1b[<32;4;2M");
        send_input(&terminal, &tui, "\x1b[<0;4;2m");
        settle(&tui);

        assert!(lock_shared(&copied).is_empty());
        assert!(tui.has_active_selection());
        assert!(terminal
            .events()
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains("\x1b[7m"))));
        assert!(terminal
            .get_viewport()
            .iter()
            .all(|line| !line.contains("Copied!")));

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("copies an active selection programmatically")
    // ---------------------------------------------------------------------

    #[test]
    fn copies_an_active_selection_programmatically() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let copied: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let copied_handle = Arc::clone(&copied);
        let tui = TuiAltScreen::with_options(
            Box::new(terminal.clone()),
            None,
            None,
            TuiAltScreenOptions {
                copy_selection: Some(Arc::new(move |text: &str| {
                    lock_shared(&copied_handle).push(text.to_string());
                    true
                })),
                ..TuiAltScreenOptions::default()
            },
        );
        tui.add_child(text("alpha\nbeta\ngamma\ndelta"));
        tui.start();
        settle(&tui);

        assert!(!tui.has_active_selection());
        assert!(!tui.copy_active_selection_to_clipboard());

        send_input(&terminal, &tui, "\x1b[<0;1;1M");
        send_input(&terminal, &tui, "\x1b[<32;4;2M");
        send_input(&terminal, &tui, "\x1b[<0;4;2m");
        settle(&tui);

        assert_eq!(lock_shared(&copied).as_slice(), ["alpha\nbeta".to_string()]);
        assert!(tui.has_active_selection());

        lock_shared(&copied).clear();
        assert!(tui.copy_active_selection_to_clipboard());
        settle(&tui);

        assert_eq!(lock_shared(&copied).as_slice(), ["alpha\nbeta".to_string()]);
        assert!(terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("Copied!")));

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("flashes an error when the injected copySelection handler fails")
    // ---------------------------------------------------------------------

    #[test]
    fn flashes_an_error_when_the_injected_copy_selection_handler_fails() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::with_options(
            Box::new(terminal.clone()),
            None,
            None,
            TuiAltScreenOptions {
                copy_selection: Some(Arc::new(|_text: &str| false)),
                ..TuiAltScreenOptions::default()
            },
        );
        tui.add_child(text("alpha\nbeta\ngamma\ndelta"));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<0;1;1M");
        send_input(&terminal, &tui, "\x1b[<32;4;2M");
        send_input(&terminal, &tui, "\x1b[<0;4;2m");
        settle(&tui);

        assert!(terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("Copy failed")));
        assert!(
            terminal
                .events()
                .iter()
                .all(|event| !matches!(event, VtEvent::Write(data) if data.contains("\x1b]52;c;"))),
            "must not emit OSC 52 when a copySelection handler is provided"
        );

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("does not append whitespace to double-click word highlighting")
    // ---------------------------------------------------------------------

    #[test]
    fn does_not_append_whitespace_to_double_click_word_highlighting() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 1);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("foo  bar"));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<0;1;1M");
        send_input(&terminal, &tui, "\x1b[<0;1;1m");
        send_input(&terminal, &tui, "\x1b[<0;3;1M");
        settle(&tui);

        assert!(terminal
            .events()
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains("foo\x1b[27m"))));
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("highlights a complete whitespace segment during a word drag")
    // ---------------------------------------------------------------------

    #[test]
    fn highlights_a_complete_whitespace_segment_during_a_word_drag() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 1);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("foo  bar"));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<0;1;1M");
        send_input(&terminal, &tui, "\x1b[<0;1;1m");
        send_input(&terminal, &tui, "\x1b[<0;2;1M");
        send_input(&terminal, &tui, "\x1b[<32;4;1M");
        settle(&tui);

        assert!(terminal
            .events()
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains("foo  \x1b[27m"))));
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("selects whole words on double click, extends word drags, and selects lines on triple click")
    // ---------------------------------------------------------------------

    #[test]
    fn selects_whole_words_on_double_click_extends_word_drags_and_selects_lines_on_triple_click() {
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 2);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("zero alpha beta\ngamma delta"));
        tui.start();
        settle(&tui);

        // The second click lands on a different character in alpha.
        send_input(&terminal, &tui, "\x1b[<0;6;1M");
        send_input(&terminal, &tui, "\x1b[<0;6;1m");
        send_input(&terminal, &tui, "\x1b[<0;10;1M");
        send_input(&terminal, &tui, "\x1b[<0;10;1m");
        settle(&tui);
        let alpha = osc52_sequence("alpha");
        assert!(terminal
            .events()
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains(&alpha))));

        // A double-click drag includes each word touched, rather than partial words.
        send_input(&terminal, &tui, "\x1b[<0;12;1M");
        send_input(&terminal, &tui, "\x1b[<0;12;1m");
        send_input(&terminal, &tui, "\x1b[<0;14;1M");
        send_input(&terminal, &tui, "\x1b[<32;3;2M");
        send_input(&terminal, &tui, "\x1b[<0;3;2m");
        settle(&tui);
        let words = osc52_sequence("beta\ngamma");
        assert!(terminal
            .events()
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains(&words))));

        send_input(&terminal, &tui, "\x1b[<0;7;2M");
        send_input(&terminal, &tui, "\x1b[<0;7;2m");
        send_input(&terminal, &tui, "\x1b[<0;9;2M");
        send_input(&terminal, &tui, "\x1b[<0;9;2m");
        send_input(&terminal, &tui, "\x1b[<0;11;2M");
        send_input(&terminal, &tui, "\x1b[<0;11;2m");
        settle(&tui);
        let line = osc52_sequence("gamma delta");
        assert!(terminal
            .events()
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains(&line))));

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("ignores orphan selection events and cancels an active selection on focus loss")
    // ---------------------------------------------------------------------

    #[test]
    fn ignores_orphan_selection_events_and_cancels_an_active_selection_on_focus_loss() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("alpha\nbeta\ngamma\ndelta"));
        tui.start();
        settle(&tui);

        let clipboard_write_count = || {
            terminal
                .events()
                .iter()
                .filter(
                    |event| matches!(event, VtEvent::Write(data) if data.contains("\x1b]52;c;")),
                )
                .count()
        };

        // A completed click leaves a zero-width anchor, but later orphaned
        // drag/release events must not extend it.
        send_input(&terminal, &tui, "\x1b[<0;1;1M");
        send_input(&terminal, &tui, "\x1b[<0;1;1m");
        send_input(&terminal, &tui, "\x1b[<32;4;2M");
        send_input(&terminal, &tui, "\x1b[<0;4;2m");
        settle(&tui);
        assert_eq!(clipboard_write_count(), 0);

        // Losing focus also cancels a press whose matching release never arrived.
        send_input(&terminal, &tui, "\x1b[<0;1;1M");
        send_input(&terminal, &tui, "\x1b[O");
        send_input(&terminal, &tui, "\x1b[I");
        send_input(&terminal, &tui, "\x1b[<32;4;2M");
        send_input(&terminal, &tui, "\x1b[<0;4;2m");
        settle(&tui);
        assert_eq!(clipboard_write_count(), 0);
        assert!(terminal
            .events()
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains("\x1b[?1004h"))));

        stop(&tui);
        assert!(terminal
            .events()
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains("\x1b[?1004l"))));
    }

    // ---------------------------------------------------------------------
    // it("auto-scrolls and extends a drag selection held at the viewport edge")
    // ---------------------------------------------------------------------

    #[test]
    fn auto_scrolls_and_extends_a_drag_selection_held_at_the_viewport_edge() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (content, _) = numbered_text(10);
        tui.add_child(content);
        tui.start();
        settle(&tui);
        assert_eq!(tui.viewport_top(), 6);

        send_input(&terminal, &tui, "\x1b[<0;1;3M");
        send_input(&terminal, &tui, "\x1b[<32;1;1M");
        // The auto-scroll "interval" (50ms upstream) is driven by explicit
        // ticks: two periods elapse (upstream: 130ms real time).
        let base = Instant::now();
        tui.tick(base + Duration::from_millis(60));
        tui.tick(base + Duration::from_millis(120));
        settle(&tui);

        let selection_top = tui.viewport_top();
        assert!(
            selection_top < 6,
            "expected auto-scroll above row 6, got {selection_top}"
        );
        send_input(&terminal, &tui, "\x1b[<0;1;1m");
        settle(&tui);

        let mut selected_lines: Vec<String> = (0..(8 - selection_top))
            .map(|index| format!("line {}", selection_top + index + 1))
            .collect();
        selected_lines.push("l".to_string());
        let expected_clipboard_sequence = osc52_sequence(&selected_lines.join("\n"));
        assert!(
            terminal
                .events()
                .iter()
                .any(|event| matches!(event, VtEvent::Write(data) if data.contains(&expected_clipboard_sequence))),
        );
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("snaps mouse selection to CJK, emoji, and combining grapheme boundaries")
    // ---------------------------------------------------------------------

    #[test]
    fn snaps_mouse_selection_to_cjk_emoji_and_combining_grapheme_boundaries() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 2);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("A\u{754C}\u{1F642}e\u{0301}Z"));
        tui.start();
        settle(&tui);

        let wide_selection = osc52_sequence("\u{754C}\u{1F642}");
        send_input(&terminal, &tui, "\x1b[<0;3;1M");
        send_input(&terminal, &tui, "\x1b[<32;4;1M");
        send_input(&terminal, &tui, "\x1b[<0;4;1m");
        settle(&tui);
        assert_eq!(
            terminal
                .events()
                .iter()
                .filter(
                    |event| matches!(event, VtEvent::Write(data) if data.contains(&wide_selection))
                )
                .count(),
            1
        );

        send_input(&terminal, &tui, "\x1b[<0;5;1M");
        send_input(&terminal, &tui, "\x1b[<32;2;1M");
        send_input(&terminal, &tui, "\x1b[<0;2;1m");
        settle(&tui);
        assert_eq!(
            terminal
                .events()
                .iter()
                .filter(
                    |event| matches!(event, VtEvent::Write(data) if data.contains(&wide_selection))
                )
                .count(),
            2
        );

        let combining_selection = osc52_sequence("e\u{0301}Z");
        send_input(&terminal, &tui, "\x1b[<0;6;1M");
        send_input(&terminal, &tui, "\x1b[<32;7;1M");
        send_input(&terminal, &tui, "\x1b[<0;7;1m");
        settle(&tui);
        assert!(terminal.events().iter().any(
            |event| matches!(event, VtEvent::Write(data) if data.contains(&combining_selection))
        ));

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("does not emit Kitty graphics commands or OSC 133 zones in iTerm2")
    // ---------------------------------------------------------------------

    /// Sets terminal capabilities and restores the cache on drop (upstream
    /// `setCapabilities` / `resetCapabilitiesCache` try/finally); holds both
    /// global-state locks: capabilities, the Kitty metadata registry and the
    /// Kitty image cache are process globals mutated by this module, the
    /// `terminal_image`/`kitty_registry` suites and the main-screen suite.
    struct CapsGuard {
        _state: MutexGuard<'static, ()>,
        _image_state: MutexGuard<'static, ()>,
    }

    impl CapsGuard {
        fn lock_only() -> CapsGuard {
            let state = state_lock();
            let image_state = crate::terminal_image::TEST_STATE_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            CapsGuard {
                _state: state,
                _image_state: image_state,
            }
        }

        fn set(caps: TerminalCapabilities) -> CapsGuard {
            let guard = CapsGuard::lock_only();
            set_capabilities(caps);
            guard
        }

        fn iterm2() -> CapsGuard {
            CapsGuard::set(TerminalCapabilities {
                images: Some(ImageProtocol::ITerm2),
                true_color: true,
                hyperlinks: true,
            })
        }

        fn kitty() -> CapsGuard {
            CapsGuard::set(TerminalCapabilities {
                images: Some(ImageProtocol::Kitty),
                true_color: true,
                hyperlinks: true,
            })
        }
    }

    impl Drop for CapsGuard {
        fn drop(&mut self) {
            reset_capabilities_cache();
        }
    }

    #[test]
    fn does_not_emit_kitty_graphics_commands_or_osc_133_zones_in_iterm2() {
        let _caps = CapsGuard::iterm2();
        let terminal = RecordingTerminal::new(20, 3);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (zones, _) =
            test_text(&["\x1b]133;B\x07\x1b]133;C\x07\x1b]133;A\x07content".to_string()]);
        tui.add_child(zones);
        tui.add_child(shared_component(Image::new(
            "AAAA",
            "image/png",
            ImageTheme {
                fallback_color: Box::new(|value| value.to_string()),
            },
            Some(ImageOptions {
                filename: Some("example.png".to_string()),
                ..ImageOptions::default()
            }),
            Some(ImageDimensions {
                width_px: 10.0,
                height_px: 10.0,
            }),
        )));
        tui.start();
        settle(&tui);
        stop(&tui);

        let events = terminal.events();
        assert!(events
            .iter()
            .all(|event| !matches!(event, VtEvent::Write(data) if data.contains("\x1b_G"))));
        assert!(events
            .iter()
            .all(|event| !matches!(event, VtEvent::Write(data) if data.contains("\x1b]133;"))));
        assert!(events.iter().all(
            |event| !matches!(event, VtEvent::Write(data) if data.contains("\x1b]1337;File="))
        ));
        assert!(events
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains("[Image:"))));
    }

    // ---------------------------------------------------------------------
    // it("clears stale iTerm2 image placements when they leave the viewport")
    // ---------------------------------------------------------------------

    #[test]
    fn clears_stale_iterm2_image_placements_when_they_leave_the_viewport() {
        let _caps = CapsGuard::iterm2();
        let terminal = RecordingTerminal::new(20, 3);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let image_line = "\x1b]1337;File=inline=1;width=2;height=auto:AAAA\x07";
        let (content, _) = test_text(&[
            image_line.to_string(),
            String::new(),
            String::new(),
            "after".to_string(),
            "more".to_string(),
            "end".to_string(),
        ]);
        tui.add_child(content);
        tui.start();
        settle(&tui);
        tui.scroll_to_top();
        settle(&tui);
        let event_count = terminal.events().len();

        tui.scroll_by(1);
        settle(&tui);
        assert!(terminal.events()[event_count..]
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains("\x1b[2J"))));
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("crops a Kitty image whose first line is above the viewport")
    // ---------------------------------------------------------------------

    #[test]
    fn crops_a_kitty_image_whose_first_line_is_above_the_viewport() {
        // No capabilities override (like upstream); the guard serializes the
        // global Kitty metadata registry / image cache with the other suites.
        let _guard = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 3);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let image_id = 123u32;
        let image_line = encode_kitty(
            "AAAA",
            &KittyEncodeOptions {
                columns: Some(2),
                rows: Some(3),
                image_id: Some(image_id),
                move_cursor: Some(false),
            },
        );
        register_kitty_image_metadata(KittyImageMetadata {
            image_id,
            columns: 2,
            rows: 3,
            width_px: 100.0,
            height_px: 100.0,
        });
        let (content, _) = test_text(&[
            "before".to_string(),
            image_line,
            String::new(),
            String::new(),
            "after".to_string(),
            "end".to_string(),
        ]);
        tui.add_child(content);
        tui.start();
        settle(&tui);

        assert_eq!(tui.viewport_top(), 3);
        assert!(terminal.events().iter().any(|event| matches!(
            event,
            VtEvent::Write(data) if data.contains("i=123") && data.contains("y=66,h=34,r=1")
        )));

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("reuses moved Kitty images without dropping HStack siblings")
    // ---------------------------------------------------------------------

    #[test]
    fn reuses_moved_kitty_images_without_dropping_h_stack_siblings() {
        let _caps = CapsGuard::kitty();
        let terminal = RecordingTerminal::new(20, 6);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (label, label_handle) = test_text(&["left".to_string()]);
        let image = shared_component(Image::new(
            "A".repeat(8192),
            "image/png",
            ImageTheme {
                fallback_color: Box::new(|value| value.to_string()),
            },
            None,
            Some(ImageDimensions {
                width_px: 100.0,
                height_px: 100.0,
            }),
        ));
        let (header, header_handle) = test_text(&["header".to_string()]);
        let row = shared_component(HStack::new(
            vec![
                StackChild::Entry(
                    label,
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(10.0)),
                        ..StackEntryOptions::default()
                    },
                ),
                StackChild::Entry(
                    image,
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(10.0)),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        ));
        tui.set_layout_root(Some(shared_component(VStack::new(
            vec![
                StackChild::Entry(
                    header,
                    StackEntryOptions {
                        basis: Some(Basis::Auto),
                        ..StackEntryOptions::default()
                    },
                ),
                StackChild::Entry(
                    row,
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(4.0)),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        ))));
        tui.start();
        settle(&tui);
        assert!(terminal
            .events()
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains("\x1b_Ga=T"))));

        let event_count = terminal.events().len();
        set_lines(&label_handle, &["changed".to_string()]);
        set_lines(
            &header_handle,
            &["header".to_string(), "second".to_string()],
        );
        tui.request_render(false);
        settle(&tui);
        let redraw_writes: String = terminal.events()[event_count..]
            .iter()
            .filter_map(|event| match event {
                VtEvent::Write(data) => Some(data.as_str()),
                _ => None,
            })
            .collect();
        let placement_index = redraw_writes.find("\x1b_Ga=p,q=2").unwrap_or_else(|| {
            panic!(
                "placement-only redraw expected; a=T retransmit: {}, probe: {:?}",
                redraw_writes.contains("\x1b_Ga=T"),
                crate::kitty_registry::retransmit_probe_log(),
            )
        });
        assert!(redraw_writes.contains("\x1b_Ga=d,d=a,q=2\x1b\\"));
        let changed_index = redraw_writes
            .find("changed")
            .unwrap_or_else(|| unreachable!("sibling text expected"));
        assert!(placement_index > changed_index);
        assert!(!redraw_writes.contains("\x1b_Ga=T"));
        assert!(
            redraw_writes.len() < 2000,
            "expected placement-only redraw, got {} bytes",
            redraw_writes.len()
        );
        assert!(terminal
            .get_viewport()
            .iter()
            .any(|line| line.trim_end() == "changed"));
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("retains recently offscreen Kitty images for placement-only reuse")
    // ---------------------------------------------------------------------

    #[test]
    fn retains_recently_offscreen_kitty_images_for_placement_only_reuse() {
        let _caps = CapsGuard::kitty();
        let terminal = RecordingTerminal::new(20, 1);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let image_id = 321u32;
        let image_line = encode_kitty(
            "AAAA",
            &KittyEncodeOptions {
                columns: Some(2),
                rows: Some(1),
                image_id: Some(image_id),
                move_cursor: Some(false),
            },
        );
        register_kitty_image_metadata(KittyImageMetadata {
            image_id,
            columns: 2,
            rows: 1,
            width_px: 100.0,
            height_px: 50.0,
        });
        let (content, _) = test_text(&[image_line, "after".to_string()]);
        tui.set_layout_root(Some(shared_component(ScrollView::new(
            content,
            ScrollViewOptions {
                primary: true,
                ..ScrollViewOptions::default()
            },
        ))));
        tui.start();
        settle(&tui);
        assert!(terminal
            .events()
            .iter()
            .any(|event| matches!(event, VtEvent::Write(data) if data.contains("\x1b_Ga=T"))));

        let event_count = terminal.events().len();
        tui.scroll_by(1);
        settle(&tui);
        tui.scroll_by(-1);
        settle(&tui);
        let reentry_writes: String = terminal.events()[event_count..]
            .iter()
            .filter_map(|event| match event {
                VtEvent::Write(data) => Some(data.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            reentry_writes.contains("\x1b_Ga=p,q=2"),
            "expected placement-only reentry; retransmits a=T: {}; probe: {:?}",
            reentry_writes.contains("\x1b_Ga=T"),
            crate::kitty_registry::retransmit_probe_log(),
        );
        assert!(!reentry_writes.contains("\x1b_Ga=T"));
        assert!(!reentry_writes.contains(&format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")));
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("evicts the least recently visible Kitty image when the cache is full")
    // ---------------------------------------------------------------------

    #[test]
    fn evicts_the_least_recently_visible_kitty_image_when_the_cache_is_full() {
        let _caps = CapsGuard::kitty();
        let terminal = RecordingTerminal::new(20, 1);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let first_image_id = 500u32;
        let image_lines: Vec<String> = (0..18)
            .map(|index| {
                let image_id = first_image_id + index;
                register_kitty_image_metadata(KittyImageMetadata {
                    image_id,
                    columns: 2,
                    rows: 1,
                    width_px: 100.0,
                    height_px: 50.0,
                });
                encode_kitty(
                    "AAAA",
                    &KittyEncodeOptions {
                        columns: Some(2),
                        rows: Some(1),
                        image_id: Some(image_id),
                        move_cursor: Some(false),
                    },
                )
            })
            .collect();
        let (content, _) = test_text(&image_lines);
        tui.set_layout_root(Some(shared_component(ScrollView::new(
            content,
            ScrollViewOptions {
                primary: true,
                ..ScrollViewOptions::default()
            },
        ))));
        tui.start();
        settle(&tui);
        for _ in 1..image_lines.len() {
            tui.scroll_by(1);
            settle(&tui);
        }
        assert!(terminal.events().iter().any(|event| matches!(
            event,
            VtEvent::Write(data) if data.contains(&format!("\x1b_Ga=d,d=I,i={first_image_id},q=2\x1b\\"))
        )));

        let event_count = terminal.events().len();
        tui.scroll_to_top();
        settle(&tui);
        let reentry_writes: String = terminal.events()[event_count..]
            .iter()
            .filter_map(|event| match event {
                VtEvent::Write(data) => Some(data.as_str()),
                _ => None,
            })
            .collect();
        assert!(reentry_writes.contains("\x1b_Ga=T"));
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("evicts offscreen Kitty images when decoded raster memory exceeds the cache quota")
    // ---------------------------------------------------------------------

    #[test]
    fn evicts_offscreen_kitty_images_when_decoded_raster_memory_exceeds_the_cache_quota() {
        let _caps = CapsGuard::kitty();
        let terminal = RecordingTerminal::new(20, 1);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let first_image_id = 600u32;
        let image_lines: Vec<String> = (0..4)
            .map(|index| {
                let image_id = first_image_id + index;
                register_kitty_image_metadata(KittyImageMetadata {
                    image_id,
                    columns: 2,
                    rows: 1,
                    width_px: 3840.0,
                    height_px: 2160.0,
                });
                encode_kitty(
                    "AAAA",
                    &KittyEncodeOptions {
                        columns: Some(2),
                        rows: Some(1),
                        image_id: Some(image_id),
                        move_cursor: Some(false),
                    },
                )
            })
            .collect();
        let (content, _) = test_text(&image_lines);
        tui.set_layout_root(Some(shared_component(ScrollView::new(
            content,
            ScrollViewOptions {
                primary: true,
                ..ScrollViewOptions::default()
            },
        ))));
        tui.start();
        settle(&tui);
        for _ in 1..image_lines.len() {
            tui.scroll_by(1);
            settle(&tui);
        }
        assert!(terminal.events().iter().any(|event| matches!(
            event,
            VtEvent::Write(data) if data.contains(&format!("\x1b_Ga=d,d=I,i={first_image_id},q=2\x1b\\"))
        )));
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("stacks flash messages and collapses them as they expire")
    // ---------------------------------------------------------------------

    #[test]
    fn stacks_flash_messages_and_collapses_them_as_they_expire() {
        // Serialized with the other global-state tests (capabilities,
        // kitty metadata/image caches are process globals).
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("one\ntwo\nthree\nfour"));
        tui.start();
        settle(&tui);

        tui.flash("First", Some(80));
        tui.flash("Second", Some(500));
        settle(&tui);
        let viewport = terminal.get_viewport();
        assert!(viewport[0].ends_with(" First "));
        assert!(viewport[1].ends_with(" Second "));

        // Upstream waits 100ms real time; the flash expiry deadline is
        // driven by an explicit tick.
        let now = settle(&tui);
        tui.tick(now + Duration::from_millis(100));
        settle(&tui);
        let viewport = terminal.get_viewport();
        assert!(viewport[0].ends_with(" Second "));
        assert!(!viewport.iter().any(|line| line.contains("First")));

        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // V14-14: component mouse dispatch + click synthesis + fix family
    // (`71026970a`, `2470ea440`, `83aed2ba5`, `1ac6128e6`, `9841914c7`,
    // `374e56e55`, `2e4d23959`, `4a879dd75`)
    // ---------------------------------------------------------------------

    /// (label, event type, x, click count) entries recorded by `MouseSpy`.
    type MouseSpyLog = Arc<Mutex<Vec<(&'static str, TuiMouseEventType, u32, Option<u32>)>>>;

    /// Component recording mouse events into a shared log; answers with a
    /// fixed result table keyed by event type.
    struct MouseSpy {
        label: &'static str,
        results: Vec<(TuiMouseEventType, TuiMouseEventResult)>,
        log: MouseSpyLog,
    }

    impl Component for MouseSpy {
        fn render(&self, _width: usize) -> Vec<String> {
            vec![format!("{} row", self.label)]
        }

        fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseHandlerResult> {
            lock_shared(&self.log).push((
                self.label,
                event.event_type,
                event.x as u32,
                event.click_count,
            ));
            self.results
                .iter()
                .find(|(event_type, _)| *event_type == event.event_type)
                .map(|(_, result)| TuiMouseHandlerResult::Event(*result))
        }
    }

    fn press_result() -> TuiMouseEventResult {
        TuiMouseEventResult {
            handled: true,
            focus: true,
            ..Default::default()
        }
    }

    fn click_result() -> TuiMouseEventResult {
        TuiMouseEventResult {
            handled: true,
            ..Default::default()
        }
    }

    /// Decode the last OSC 52 clipboard write from the terminal output.
    fn last_osc52_clipboard(terminal: &RecordingTerminal) -> Option<String> {
        let mut last = None;
        for event in terminal.events() {
            if let VtEvent::Write(data) = event {
                let data: String = data.clone();
                if let Some(start) = data.find("\x1b]52;c;") {
                    let payload = &data[start + 7..];
                    if let Some(end) = payload.find('\x07') {
                        if let Ok(bytes) =
                            base64::engine::general_purpose::STANDARD.decode(&payload[..end])
                        {
                            last = Some(String::from_utf8(bytes).unwrap_or_default());
                        }
                    }
                }
            }
        }
        last
    }

    #[test]
    fn gesture_press_release_same_cell_synthesizes_click_with_count() {
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let log: MouseSpyLog = Arc::new(Mutex::new(Vec::new()));
        // A component that handles presses (e.g. a list row) and clicks.
        tui.add_child(shared_component(MouseSpy {
            label: "spy",
            results: vec![
                (TuiMouseEventType::Press, press_result()),
                (TuiMouseEventType::Click, click_result()),
            ],
            log: Arc::clone(&log),
        }));
        tui.start();
        settle(&tui);

        // Single press+release on the same cell: press handled → gesture
        // path; release synthesizes click 1 (tui-alt-screen.ts:895-905).
        send_input(&terminal, &tui, "\x1b[<0;2;1M");
        send_input(&terminal, &tui, "\x1b[<0;2;1m");
        settle(&tui);
        let events = lock_shared(&log).clone();
        assert!(
            events
                .iter()
                .any(|(label, event_type, x, count)| *label == "spy"
                    && *event_type == TuiMouseEventType::Click
                    && *x == 1
                    && *count == Some(1)),
            "events: {events:?}"
        );

        // Immediate second press+release on the same cell: click 2.
        send_input(&terminal, &tui, "\x1b[<0;2;1M");
        send_input(&terminal, &tui, "\x1b[<0;2;1m");
        settle(&tui);
        let events = lock_shared(&log).clone();
        let clicks: Vec<_> = events
            .iter()
            .filter(|(label, event_type, _, _)| {
                *label == "spy" && *event_type == TuiMouseEventType::Click
            })
            .collect();
        assert_eq!(clicks.len(), 2, "events: {events:?}");
        assert_eq!(clicks[0].3, Some(1));
        assert_eq!(clicks[1].3, Some(2), "double click counts up");

        stop(&tui);
    }

    #[test]
    fn gesture_release_after_movement_does_not_synthesize_click() {
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let log: MouseSpyLog = Arc::new(Mutex::new(Vec::new()));
        tui.add_child(shared_component(MouseSpy {
            label: "spy",
            results: vec![(TuiMouseEventType::Press, press_result())],
            log: Arc::clone(&log),
        }));
        tui.start();
        settle(&tui);

        // Press at (2,1), drag to (5,1), release there: moved → no click.
        send_input(&terminal, &tui, "\x1b[<0;2;1M");
        send_input(&terminal, &tui, "\x1b[<32;5;1M");
        send_input(&terminal, &tui, "\x1b[<0;5;1m");
        settle(&tui);
        let events = lock_shared(&log).clone();
        assert!(
            !events
                .iter()
                .any(|(_, event_type, _, _)| *event_type == TuiMouseEventType::Click),
            "no click after movement: {events:?}"
        );
        // The drag and release were still routed to the press target.
        assert!(
            events
                .iter()
                .any(|(_, event_type, x, _)| *event_type == TuiMouseEventType::Drag && *x == 4),
            "drag re-dispatched to press target: {events:?}"
        );

        stop(&tui);
    }

    #[test]
    fn selection_release_same_cell_synthesizes_click_for_components() {
        // The second click-synthesis point (tui-alt-screen.ts:1324-1336):
        // press fell through to screen selection (component ignores press),
        // and the release synthesizes a click for the MouseRegion-style
        // handler.
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let log: MouseSpyLog = Arc::new(Mutex::new(Vec::new()));
        tui.add_child(shared_component(MouseSpy {
            label: "region",
            results: vec![(TuiMouseEventType::Click, click_result())],
            log: Arc::clone(&log),
        }));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<0;2;1M");
        send_input(&terminal, &tui, "\x1b[<0;2;1m");
        settle(&tui);
        let events = lock_shared(&log).clone();
        assert!(
            events
                .iter()
                .any(|(label, event_type, _, _)| *label == "region"
                    && *event_type == TuiMouseEventType::Click),
            "selection release synthesizes click: {events:?}"
        );
        // The click result clears the selection instead of copying it (no
        // OSC 52 write for an empty selection anyway — assert no clipboard).
        assert_eq!(last_osc52_clipboard(&terminal), None);

        stop(&tui);
    }

    #[test]
    fn captured_component_receives_subsequent_events_without_hit_test() {
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let log: MouseSpyLog = Arc::new(Mutex::new(Vec::new()));
        // Captures on press (tui.ts:50, tui-alt-screen.ts:836-837).
        tui.add_child(shared_component(MouseSpy {
            label: "capturer",
            results: vec![
                (
                    TuiMouseEventType::Press,
                    TuiMouseEventResult {
                        handled: true,
                        capture: true,
                        ..Default::default()
                    },
                ),
                (TuiMouseEventType::Click, click_result()),
            ],
            log: Arc::clone(&log),
        }));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<0;2;1M");
        // Move off the component: capture routing still delivers the move.
        send_input(&terminal, &tui, "\x1b[<35;10;3M");
        settle(&tui);
        let events = lock_shared(&log).clone();
        assert!(
            events
                .iter()
                .any(|(label, event_type, _, _)| *label == "capturer"
                    && *event_type == TuiMouseEventType::Move),
            "capture routes off-target moves: {events:?}"
        );

        stop(&tui);
    }

    #[test]
    fn overlay_hit_consumes_and_hit_without_result_blocks_layout() {
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let log: MouseSpyLog = Arc::new(Mutex::new(Vec::new()));
        // Layout child: claims presses (must NOT receive overlay-area hits).
        tui.add_child(shared_component(MouseSpy {
            label: "layout",
            results: vec![
                (TuiMouseEventType::Press, press_result()),
                (TuiMouseEventType::Click, click_result()),
            ],
            log: Arc::clone(&log),
        }));
        tui.start();
        settle(&tui);

        // Overlay WITHOUT a handler (hit-but-no-result must still block the
        // layout dispatch, tui.ts:846-847).
        tui.show_overlay(
            text("overlay content"),
            Some(OverlayOptions {
                width: Some(SizeValue::Absolute(16)),
                row: Some(SizeValue::Absolute(0)),
                col: Some(SizeValue::Absolute(2)),
                ..Default::default()
            }),
        );
        settle(&tui);

        // Press inside the overlay: the layout spy must not see it.
        send_input(&terminal, &tui, "\x1b[<0;4;1M");
        send_input(&terminal, &tui, "\x1b[<0;4;1m");
        settle(&tui);
        let events = lock_shared(&log).clone();
        assert!(
            !events.iter().any(|(label, _, _, _)| *label == "layout"),
            "overlay hit blocks layout dispatch: {events:?}"
        );

        stop(&tui);
    }

    #[test]
    fn overlay_handler_receives_clicks_and_focused_overlay_defers_viewport_keys() {
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let (transcript, handle) = numbered_text(20);
        tui.add_child(transcript);
        let log: MouseSpyLog = Arc::new(Mutex::new(Vec::new()));
        let overlay = shared_component(MouseSpy {
            label: "overlay",
            results: vec![
                (TuiMouseEventType::Press, press_result()),
                (TuiMouseEventType::Click, click_result()),
            ],
            log: Arc::clone(&log),
        });
        tui.start();
        settle(&tui);
        let before_top = tui.viewport_top();
        assert_eq!(before_top, 16, "follows the end of 20 lines");

        let _overlay_handle = tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                width: Some(SizeValue::Absolute(16)),
                row: Some(SizeValue::Absolute(0)),
                col: Some(SizeValue::Absolute(2)),
                ..Default::default()
            }),
        );
        settle(&tui);
        // showOverlay focuses the overlay → isOverlayFocused (FR-G).
        assert!(tui.lock_inner().base.is_overlay_focused());

        // Click inside the overlay reaches its handler (dispatch order:
        // overlay before layout).
        send_input(&terminal, &tui, "\x1b[<0;4;1M");
        send_input(&terminal, &tui, "\x1b[<0;4;1m");
        settle(&tui);
        let events = lock_shared(&log).clone();
        assert!(
            events
                .iter()
                .any(|(label, event_type, _, _)| *label == "overlay"
                    && *event_type == TuiMouseEventType::Click),
            "overlay handler receives click: {events:?}"
        );

        // FR-G (2e4d23959): wheel and PageUp defer to the focused overlay —
        // the viewport must not scroll.
        send_input(&terminal, &tui, "\x1b[<64;1;1M");
        send_input(&terminal, &tui, "\x1b[<64;1;1M");
        send_input(&terminal, &tui, "\x1b[5~");
        settle(&tui);
        assert_eq!(
            tui.viewport_top(),
            before_top,
            "focused overlay defers viewport scrolling"
        );

        // Closing the overlay restores viewport scrolling.
        _overlay_handle.hide();
        settle(&tui);
        assert!(!tui.lock_inner().base.is_overlay_focused());
        send_input(&terminal, &tui, "\x1b[<64;1;1M");
        settle(&tui);
        assert_eq!(tui.viewport_top(), before_top - 1);
        let _ = handle;

        stop(&tui);
    }

    #[test]
    fn sgr_release_button_three_completes_selection() {
        // FR-C (83aed2ba5 #7963): terminals reporting the generic release
        // code (button bits 3) must complete the selection. Old behavior
        // rejected the release, leaving the press active.
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("alpha\nbeta\ngamma\ndelta"));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<0;1;1M");
        send_input(&terminal, &tui, "\x1b[<32;4;2M");
        // Release with button code 3 (the "no button" release).
        send_input(&terminal, &tui, "\x1b[<3;4;2m");
        settle(&tui);

        assert_eq!(
            last_osc52_clipboard(&terminal),
            Some("alpha\nbeta".to_string())
        );
        // The gesture fully ended: a following motion is ignored.
        send_input(&terminal, &tui, "\x1b[<32;4;2M");
        settle(&tui);
        assert_eq!(
            last_osc52_clipboard(&terminal),
            Some("alpha\nbeta".to_string()),
            "no second copy for orphan motion"
        );

        stop(&tui);
    }

    #[test]
    fn double_click_word_selection_joins_slash_and_dash() {
        // FR-D (1ac6128e6 #7746): "foo/bar-baz" double-click selects the
        // whole path+kebab token via the joiner segments.
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(30, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("foo/bar-baz qux"));
        tui.start();
        settle(&tui);

        // Double-click the "bar" part: both clicks land inside the joined
        // token; the selection covers "foo/bar-baz" (col 0..11).
        send_input(&terminal, &tui, "\x1b[<0;7;1M");
        send_input(&terminal, &tui, "\x1b[<0;7;1m");
        send_input(&terminal, &tui, "\x1b[<0;7;1M");
        send_input(&terminal, &tui, "\x1b[<0;7;1m");
        settle(&tui);
        assert_eq!(
            last_osc52_clipboard(&terminal),
            Some("foo/bar-baz".to_string()),
            "joiners glue the word-like segments"
        );

        stop(&tui);
    }

    #[test]
    fn double_click_word_selection_matrix() {
        // FR-D edge matrix: plain words, mixed "a/b-c", punctuation
        // separators (not joiners), and leading/trailing joiners.
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(40, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("a/b-c foo,bar -lead tail- 你好"));
        tui.start();
        settle(&tui);

        let click_at = |col: usize| {
            send_input(&terminal, &tui, &format!("\x1b[<0;{c};1M", c = col + 1));
            send_input(&terminal, &tui, &format!("\x1b[<0;{c};1m", c = col + 1));
            send_input(&terminal, &tui, &format!("\x1b[<0;{c};1M", c = col + 1));
            send_input(&terminal, &tui, &format!("\x1b[<0;{c};1m", c = col + 1));
        };

        // Layout: "a/b-c foo,bar -lead tail- 你好"
        //           01234 5 6..12 13 14..18 19 20..24 25 26 27

        // "a/b-c" (cols 0..4): whole token.
        click_at(2);
        assert_eq!(last_osc52_clipboard(&terminal), Some("a/b-c".to_string()));

        // "foo,bar": comma is not a joiner — only "foo".
        click_at(7);
        assert_eq!(last_osc52_clipboard(&terminal), Some("foo".to_string()));

        // "-lead" (cols 14..18): the space before it is not selectable, so
        // the token does not glue to "foo,bar" — but its own leading "-"
        // joiner glues to "lead" → "-lead" whole.
        click_at(15);
        assert_eq!(last_osc52_clipboard(&terminal), Some("-lead".to_string()));

        // "tail-" (cols 20..24): trailing joiner stays attached.
        click_at(22);
        assert_eq!(last_osc52_clipboard(&terminal), Some("tail-".to_string()));

        // CJK (cols 26..27): each Han char is its own word-like segment
        // (no joiners).
        click_at(26);
        assert_eq!(last_osc52_clipboard(&terminal), Some("你".to_string()));

        stop(&tui);
    }

    #[test]
    fn double_click_hiragana_run_selects_whole_run() {
        // D-093 refinement: Intl.Segmenter keeps Hiragana runs whole; the
        // selection path merges the per-character UAX #29 segments
        // (`merge_hiragana_runs`), so a double-click over hiragana selects
        // the run like the upstream terminal.
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(30, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("こんにちは世界"));
        tui.start();
        settle(&tui);

        let click_at = |col: usize| {
            send_input(&terminal, &tui, &format!("\x1b[<0;{c};1M", c = col + 1));
            send_input(&terminal, &tui, &format!("\x1b[<0;{c};1m", c = col + 1));
            send_input(&terminal, &tui, &format!("\x1b[<0;{c};1M", c = col + 1));
            send_input(&terminal, &tui, &format!("\x1b[<0;{c};1m", c = col + 1));
        };
        // The hiragana run こんにちは (columns 0..9, two columns per char)
        // selects whole; the Han part (columns 10..13) stays per-character.
        click_at(2);
        assert_eq!(
            last_osc52_clipboard(&terminal),
            Some("こんにちは".to_string())
        );
        click_at(10);
        assert_eq!(last_osc52_clipboard(&terminal), Some("世".to_string()));

        stop(&tui);
    }

    #[test]
    fn vscode_term_program_suppresses_right_click_paste() {
        // FR-F (374e56e55 #8186): VS Code's terminal handles right-click
        // paste itself; rpi must not double-paste under TERM_PROGRAM=vscode.
        let _caps = CapsGuard::lock_only();
        for (term_program, expect_paste) in [
            (Some(Some("vscode".to_string())), false),
            (Some(Some("VSCode".to_string())), false),
            (Some(Some("code.cmd".to_string())), true),
            (Some(None), true),
            (None, true),
        ] {
            let pasted = Arc::new(AtomicBool::new(false));
            let pasted_flag = Arc::clone(&pasted);
            let terminal = RecordingTerminal::new(20, 4);
            let tui = TuiAltScreen::with_options(
                Box::new(terminal.clone()),
                None,
                None,
                TuiAltScreenOptions {
                    win32_override: Some(true),
                    on_right_click_paste: Some(Arc::new(move || {
                        pasted_flag.store(true, Ordering::SeqCst);
                    })),
                    term_program_override: term_program.clone(),
                    ..Default::default()
                },
            );
            tui.add_child(text("content"));
            tui.start();
            settle(&tui);
            send_input(&terminal, &tui, "\x1b[<2;3;2M");
            settle(&tui);
            assert_eq!(
                pasted.load(Ordering::SeqCst),
                expect_paste,
                "TERM_PROGRAM={term_program:?}"
            );
            stop(&tui);
        }
    }

    #[test]
    fn focus_out_without_selection_does_not_render() {
        // FR-H (4a879dd75 #7892): clearing empty selection state on focus
        // loss must not request a render.
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("alpha\nbeta\ngamma\ndelta"));
        tui.start();
        settle(&tui);
        let writes_before = terminal.events().len();

        send_input(&terminal, &tui, "\x1b[O");
        settle(&tui);
        assert_eq!(
            terminal.events().len(),
            writes_before,
            "no render output for an empty selection"
        );

        stop(&tui);
    }

    #[test]
    fn focus_out_with_active_selection_renders_once() {
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("alpha\nbeta\ngamma\ndelta"));
        tui.start();
        settle(&tui);

        // Non-empty active selection: press + drag (selection visible).
        send_input(&terminal, &tui, "\x1b[<0;1;1M");
        send_input(&terminal, &tui, "\x1b[<32;4;2M");
        settle(&tui);
        let writes_before = terminal.events().len();
        // Still press-active (no release yet) → hadActiveSelection.
        send_input(&terminal, &tui, "\x1b[O");
        settle(&tui);
        assert!(
            terminal.events().len() > writes_before,
            "non-empty selection cleared on focus loss renders"
        );
        // The selection is gone: the next release-less motion copies
        // nothing new.
        assert_eq!(
            last_osc52_clipboard(&terminal),
            None,
            "selection dropped without a release copy"
        );

        stop(&tui);
    }

    #[test]
    fn wheel_events_reach_layout_components() {
        // SelectList-style wheel handling inside the layout
        // (71026970a): the wheel dispatches to components (implicit-document
        // hit-test) before the viewport fallback; a handled wheel stops the
        // viewport routing.
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(20, 4);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        let log: MouseSpyLog = Arc::new(Mutex::new(Vec::new()));
        // The spy is the whole document (1 row) — the wheel over it is
        // dispatched to it and handled, so routeWheel never runs.
        tui.add_child(shared_component(MouseSpy {
            label: "wheel",
            results: vec![(
                TuiMouseEventType::Wheel,
                TuiMouseEventResult {
                    handled: true,
                    ..Default::default()
                },
            )],
            log: Arc::clone(&log),
        }));
        tui.start();
        settle(&tui);
        assert_eq!(tui.viewport_top(), 0);

        send_input(&terminal, &tui, "\x1b[<64;1;1M");
        settle(&tui);
        let events = lock_shared(&log).clone();
        assert!(
            events
                .iter()
                .any(|(label, event_type, _, _)| *label == "wheel"
                    && *event_type == TuiMouseEventType::Wheel),
            "wheel dispatched to components: {events:?}"
        );
        assert_eq!(
            tui.viewport_top(),
            0,
            "no viewport scroll for a handled wheel"
        );

        stop(&tui);
    }
    /// Char (column) index of `needle` in `line` (JS `indexOf` returns a
    /// UTF-16 unit index; these tests need columns, not byte offsets).
    fn column_of(line: &str, needle: &str) -> i32 {
        let byte = line.find(needle).expect("needle in line");
        line[..byte].chars().count() as i32
    }

    fn column_of_last(line: &str, needle: &str) -> i32 {
        let byte = line.rfind(needle).expect("needle in line");
        line[..byte].chars().count() as i32
    }

    // ---------------------------------------------------------------------
    // V14-15: fullscreen transcript search (00121ed99 / 7d399e7be /
    // 2d4116333) and scrollToEndIndicator (79680533c)
    // ---------------------------------------------------------------------

    /// Upstream test layout: `ScrollView(follow: "end", primary)` over the
    /// transcript, docked above a fixed editor/footer VStack, on a TUI built
    /// with `options`.
    fn transcript_with_dock(
        lines: Vec<String>,
        options: TuiAltScreenOptions,
        columns: usize,
        rows: usize,
    ) -> (SharedComponent, TuiAltScreen, VirtualTerminal) {
        let terminal = VirtualTerminal::new(columns, rows);
        let tui = TuiAltScreen::with_options(Box::new(terminal.clone()), None, None, options);
        let transcript = shared_component(ScrollView::new(
            shared_component(TestText {
                lines: Arc::new(Mutex::new(lines)),
            }),
            ScrollViewOptions {
                follow: Follow::End,
                primary: true,
                ..ScrollViewOptions::default()
            },
        ));
        let dock = shared_component(VStack::new(
            vec![
                StackChild::Component(text("editor")),
                StackChild::Component(text("footer")),
            ],
            StackOptions::default(),
        ));
        tui.set_layout_root(Some(shared_component(VStack::new(
            vec![
                StackChild::Entry(
                    transcript.clone(),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(0.0)),
                        grow: Some(1.0),
                        min_size: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
                StackChild::Entry(
                    dock,
                    StackEntryOptions {
                        basis: Some(Basis::Auto),
                        min_size: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        ))));
        (transcript, tui, terminal)
    }

    // it("renders transcript search with a muted placeholder and right-aligned
    // controls") (tui-alt-screen.test.ts:553)
    #[test]
    fn search_component_renders_placeholder_and_right_aligned_controls() {
        let _caps = CapsGuard::lock_only();
        let query_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&query_log);
        let component = AltScreenSearchComponent::new(
            Arc::new(move |query: &str| {
                lock_shared(&log).push(query.to_string());
            }),
            None,
        );
        let rendered = Component::render(&component, 48);
        let lines: Vec<String> = rendered
            .iter()
            .map(|l| strip_terminal_sequences(l))
            .collect();

        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|line| visible_width(line) == 48));
        assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'));
        assert_eq!(
            lines[1],
            format!("│ {:<width$} │", "Find in transcript", width = 44)
        );
        assert!(rendered[1].contains("\x1b[2m"));
        assert_eq!(
            lines[2]
                .trim_start_matches('└')
                .trim_end_matches('┘')
                .trim_matches('─'),
            " ↑ Shift+Enter · ↓ Enter "
        );
        let controls = lines[2].clone();
        let arrow_col = column_of(&controls, "↑");
        assert_eq!(
            component.get_navigation_direction_at(2, arrow_col),
            Some(-1)
        );
        let shift_enter = column_of(&controls, "Shift+Enter");
        assert_eq!(
            component.get_navigation_direction_at(2, shift_enter + 5),
            Some(-1)
        );
        let separator = column_of(&controls, "·");
        assert_eq!(component.get_navigation_direction_at(2, separator), None);
        let next_arrow = column_of(&controls, "↓");
        assert_eq!(
            component.get_navigation_direction_at(2, next_arrow),
            Some(1)
        );
        let next_enter = column_of_last(&controls, "Enter");
        assert_eq!(
            component.get_navigation_direction_at(2, next_enter + 2),
            Some(1)
        );
        // Other rows never hit the buttons.
        assert_eq!(component.get_navigation_direction_at(0, arrow_col), None);

        // Typing reports the query and replaces the placeholder with n/m.
        let mut component = component;
        component.handle_input("n");
        assert_eq!(lock_shared(&query_log).as_slice(), ["n"]);
        component.set_result(0, 2);
        let populated = Component::render(&component, 48);
        let plain: Vec<String> = populated
            .iter()
            .map(|l| strip_terminal_sequences(l))
            .collect();
        assert!(plain[1].contains('n'));
        assert!(plain[1].contains("1/2"));
        assert!(populated[1].contains("\x1b[2m 1/2 \x1b[22m"));
        assert!(!plain.iter().any(|line| line.contains("Find in transcript")));
    }

    /// Narrow overlay: the button labels degrade (alt-screen-search.ts:297).
    #[test]
    fn search_component_degrades_on_narrow_width() {
        let component = AltScreenSearchComponent::new(Arc::new(|_: &str| {}), None);
        let rendered = Component::render(&component, 20);
        let lines: Vec<String> = rendered
            .iter()
            .map(|l| strip_terminal_sequences(l))
            .collect();
        // Width 20 → inner 18: " ↑ Shift+Enter · ↓ Enter " does not fit, the
        // labels collapse to bare arrows separated by one space.
        assert!(
            lines[2].contains("↑ ↓") || lines[2].contains("↑ ·↓"),
            "got {:?}",
            lines[2]
        );
    }

    /// macOS test injection: `alt` renders as `Option`
    /// (alt-screen-search.ts:268).
    #[test]
    fn search_button_key_renames_alt_to_option_on_darwin() {
        let _caps = CapsGuard::lock_only();
        {
            use crate::keybindings::{KeyBindingValue, KeybindingsConfig, KeybindingsManager};
            let mut config = KeybindingsConfig::new();
            config.insert(
                "tui.altScreen.searchPrevious".to_string(),
                KeyBindingValue::Single("alt+enter".to_string()),
            );
            crate::keybindings::set_keybindings(KeybindingsManager::new(
                crate::keybindings::tui_keybindings().to_vec(),
                config,
            ));
        }
        let _restore = scopeguard_defaults();
        let mut component = AltScreenSearchComponent::new(Arc::new(|_: &str| {}), None);
        component.set_darwin_override(Some(true));
        let rendered = Component::render(&component, 48);
        let plain = strip_terminal_sequences(&rendered[2]);
        assert!(plain.contains("Option+Enter"), "got: {plain:?}");
        component.set_darwin_override(Some(false));
        let rendered = Component::render(&component, 48);
        let plain = strip_terminal_sequences(&rendered[2]);
        assert!(plain.contains("Alt+Enter"), "got: {plain:?}");
    }

    // it("searches the transcript with Ctrl+Shift+F and restores editor focus
    // on close") (tui-alt-screen.test.ts:691)
    #[test]
    fn searches_transcript_with_ctrl_shift_f_and_restores_editor_focus_on_close() {
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(60, 8);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        struct EditorStub {
            inputs: Arc<Mutex<Vec<String>>>,
        }
        impl Component for EditorStub {
            fn render(&self, _width: usize) -> Vec<String> {
                vec!["editor".to_string()]
            }
            fn handle_input(&mut self, data: &str) {
                lock_shared(&self.inputs).push(data.to_string());
            }
            fn as_focusable(&self) -> Option<&dyn Focusable> {
                Some(self)
            }
            fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
                Some(self)
            }
        }
        impl Focusable for EditorStub {
            fn focused(&self) -> bool {
                false
            }
            fn set_focused(&mut self, _focused: bool) {}
        }
        let editor_inputs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let editor = shared_component(EditorStub {
            inputs: Arc::clone(&editor_inputs),
        });
        let transcript = shared_component(ScrollView::new(
            shared_component(TestText {
                lines: Arc::new(Mutex::new(
                    (0..12)
                        .map(|index| {
                            if index == 4 {
                                "line 5 needle one".to_string()
                            } else if index == 9 {
                                "line 10 needle two".to_string()
                            } else {
                                format!("line {}", index + 1)
                            }
                        })
                        .collect::<Vec<_>>(),
                )),
            }),
            ScrollViewOptions {
                follow: Follow::End,
                primary: true,
                ..ScrollViewOptions::default()
            },
        ));
        tui.set_layout_root(Some(shared_component(VStack::new(
            vec![
                StackChild::Entry(
                    transcript.clone(),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(0.0)),
                        grow: Some(1.0),
                        min_size: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
                StackChild::Entry(
                    editor.clone(),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(1.0)),
                        shrink: Some(0.0),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        ))));
        tui.set_focus(Some(editor.clone()));
        tui.start();
        settle(&tui);

        // ctrl+shift+f (kitty CSI-u) opens the search; typing fills it.
        send_input(&terminal, &tui, "\x1b[102;6u");
        send_input(&terminal, &tui, "needle");
        settle(&tui);
        assert!(!with_sv(&transcript, ScrollView::is_following_end));
        let viewport = terminal.get_viewport();
        assert!(viewport.iter().any(|line| line.contains("2/2")));
        assert!(viewport
            .iter()
            .any(|line| line.contains("↑ Shift+Enter · ↓ Enter")));
        assert!(viewport
            .iter()
            .any(|line| line.contains("line 10 needle two")));
        assert!(lock_shared(&editor_inputs).is_empty());
        // Default current-match style (tui-alt-screen.ts:266).
        assert!(terminal.writes().contains("\x1b[1;7mneedle\x1b[22;27m"));

        // Manual wheel scrolling does not snap back to the current match
        // (selectionMode stays "retain", tui-alt-screen.ts:580-589).
        for _ in 0..6 {
            send_input(&terminal, &tui, "\x1b[<64;1;4M");
        }
        settle(&tui);
        assert_eq!(with_sv(&transcript, ScrollView::scroll_top), 0);
        let viewport = terminal.get_viewport();
        assert!(viewport
            .iter()
            .any(|line| line.contains("needle") && line.contains("2/2")));

        // ctrl+g → next match.
        send_input(&terminal, &tui, "\x07");
        settle(&tui);
        let viewport = terminal.get_viewport();
        assert!(viewport.iter().any(|line| line.contains("1/2")));
        assert!(viewport
            .iter()
            .any(|line| line.contains("line 5 needle one")));

        // ctrl+shift+g → previous match.
        send_input(&terminal, &tui, "\x1b[103;6u");
        settle(&tui);
        let viewport = terminal.get_viewport();
        assert!(viewport.iter().any(|line| line.contains("2/2")));
        assert!(viewport
            .iter()
            .any(|line| line.contains("line 10 needle two")));

        // escape closes; plain keys flow back to the editor.
        send_input(&terminal, &tui, "\x1b");
        send_input(&terminal, &tui, "x");
        settle(&tui);
        assert!(!terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("↑ Shift+Enter · ↓ Enter")));
        assert_eq!(lock_shared(&editor_inputs).as_slice(), ["x"]);

        stop(&tui);
    }

    // it("navigates transcript search with hoverable arrow buttons and toggles
    // it with its shortcut") (tui-alt-screen.test.ts:581)
    #[test]
    fn navigates_search_with_hoverable_arrow_buttons_and_toggles_with_shortcut() {
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(120, 6);
        let tui = TuiAltScreen::with_options(
            Box::new(terminal.clone()),
            None,
            None,
            TuiAltScreenOptions {
                search_navigation_button_style: Some(Arc::new(|text: &str, hovered: bool| {
                    format!(
                        "{}{text}\x1b[49m",
                        if hovered { "\x1b[45m" } else { "\x1b[44m" }
                    )
                })),
                ..TuiAltScreenOptions::default()
            },
        );
        tui.add_child(text("needle one\nmiddle\nneedle two\nend"));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[102;6u");
        send_input(&terminal, &tui, "needle");
        settle(&tui);
        let viewport = terminal.get_viewport();
        assert!(viewport.iter().any(|line| line.contains("1/2")));
        assert!(viewport
            .iter()
            .any(|line| line.contains("↑ Shift+Enter · ↓ Enter")));

        let arrow_row = viewport
            .iter()
            .position(|line| line.contains('↑') && line.contains('↓'))
            .expect("button row");
        let arrow_column = column_of_last(&viewport[arrow_row], "Enter") as u32 + 1;
        // Motion (button 35) over the next button → hovered style emitted.
        send_input(
            &terminal,
            &tui,
            &format!("\x1b[<35;{arrow_column};{}M", arrow_row as u32 + 1),
        );
        settle(&tui);
        assert!(terminal.writes().contains("\x1b[45m↓ Enter\x1b[49m"));
        // Click the next button → 2/2.
        send_input(
            &terminal,
            &tui,
            &format!("\x1b[<0;{arrow_column};{}M", arrow_row as u32 + 1),
        );
        settle(&tui);
        let viewport = terminal.get_viewport();
        assert!(viewport.iter().any(|line| line.contains("2/2")));

        let viewport = terminal.get_viewport();
        let arrow_column = column_of(&viewport[arrow_row], "Shift+Enter") as u32 + 3;
        send_input(
            &terminal,
            &tui,
            &format!("\x1b[<0;{arrow_column};{}M", arrow_row as u32 + 1),
        );
        settle(&tui);
        assert!(terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("1/2")));

        // Toggle with ctrl+shift+f again closes the overlay.
        send_input(&terminal, &tui, "\x1b[102;6u");
        settle(&tui);
        assert!(!terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("↑ Shift+Enter · ↓ Enter")));
        stop(&tui);
    }

    // it("does not treat transcript box drawing as search navigation buttons")
    // (tui-alt-screen.test.ts:628)
    #[test]
    fn box_drawing_is_not_treated_as_search_navigation_buttons() {
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(80, 10);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text(
            &[
                "needle one",
                "middle",
                "needle two",
                "filler",
                "┌────────────────────────────────────────┐",
                "│ box                                    │",
                "└────────────────────────────────────────┘",
                "end",
            ]
            .join("\n"),
        ));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[102;6u");
        send_input(&terminal, &tui, "needle");
        settle(&tui);
        let viewport = terminal.get_viewport();
        assert!(viewport.iter().any(|line| line.contains("1/2")));

        let box_bottom_row = viewport
            .iter()
            .position(|line| line.starts_with('└'))
            .expect("box bottom");
        send_input(
            &terminal,
            &tui,
            &format!("\x1b[<0;24;{}M", box_bottom_row as u32 + 1),
        );
        settle(&tui);

        let viewport = terminal.get_viewport();
        assert!(viewport.iter().any(|line| line.contains("1/2")));
        assert!(!viewport.iter().any(|line| line.contains("2/2")));
        stop(&tui);
    }

    // it("uses configured styles for current and non-current search matches")
    // (tui-alt-screen.test.ts:668)
    #[test]
    fn uses_configured_styles_for_current_and_non_current_search_matches() {
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(60, 4);
        let tui = TuiAltScreen::with_options(
            Box::new(terminal.clone()),
            None,
            None,
            TuiAltScreenOptions {
                search_match_style: Some(Arc::new(|text: &str| format!("\x1b[41m{text}\x1b[49m"))),
                search_current_match_style: Some(Arc::new(|text: &str| {
                    format!("\x1b[42m{text}\x1b[49m")
                })),
                ..TuiAltScreenOptions::default()
            },
        );
        tui.add_child(text("needle first\nmiddle\nneedle second\nend"));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[102;6u");
        send_input(&terminal, &tui, "needle");
        settle(&tui);

        assert!(terminal.writes().contains("\x1b[42mneedle\x1b[49m"));
        assert!(terminal.writes().contains("\x1b[41mneedle\x1b[49m"));
        stop(&tui);
    }

    // it("keeps viewport scrolling while transcript search is focused")
    // (tui-alt-screen.test.ts:1873; the R5 search exception to the
    // overlay-input deferral)
    #[test]
    fn keeps_viewport_scrolling_while_transcript_search_is_focused() {
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(20, 6);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text(
            &(1..=12)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        tui.start();
        settle(&tui);
        let top_before = tui.viewport_top();

        send_input(&terminal, &tui, "\x1b[102;6u");
        settle(&tui);
        assert!(terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("↑ ↓")));

        // PageUp and wheel still scroll the transcript (the focused search
        // overlay only consumes its own keys).
        send_input(&terminal, &tui, "\x1b[5~");
        send_input(&terminal, &tui, "\x1b[<64;1;4M");
        settle(&tui);
        assert!(tui.viewport_top() < top_before);
        assert!(terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("↑ ↓")));
        stop(&tui);
    }

    /// R6: complete SGR mouse sequences arriving while the search input holds
    /// focus never reach the query (the viewport listener consumes all mouse
    /// sequences; fragmented sequences are reassembled by the stdin buffer
    /// before the TUI sees them — stdin_buffer.rs:273
    /// `extract_complete_sequences`, verified by its own test suite).
    #[test]
    fn sgr_mouse_sequences_do_not_pollute_the_search_query() {
        let _caps = CapsGuard::lock_only();
        let terminal = RecordingTerminal::new(60, 8);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text("needle one\nmiddle\nneedle two"));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[102;6u");
        send_input(&terminal, &tui, "need");
        settle(&tui);
        // Motion + press + release mouse sequences around the overlay.
        send_input(&terminal, &tui, "\x1b[<35;40;1M");
        send_input(&terminal, &tui, "\x1b[<0;40;1M");
        send_input(&terminal, &tui, "\x1b[<0;40;1m");
        // A wheel event likewise.
        send_input(&terminal, &tui, "\x1b[<64;1;3M");
        settle(&tui);
        let viewport = terminal.get_viewport();
        // Query still "need" → 1/2, no stray characters in the box.
        assert!(viewport.iter().any(|line| line.contains("1/2")));
        let input_row = viewport
            .iter()
            .find(|line| line.contains("need"))
            .expect("query row");
        let plain = strip_terminal_sequences(input_row);
        assert!(
            !plain.contains('[') && !plain.contains('M'),
            "no mouse bytes leaked into the query: {plain:?}"
        );
        stop(&tui);
    }

    // it("scrolls the transcript by one line with custom bindings")
    // (tui-alt-screen.test.ts:772; 1279952de FR-C R1)
    #[test]
    fn scrolls_the_transcript_by_one_line_with_custom_bindings() {
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(20, 10);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        {
            use crate::keybindings::{KeyBindingValue, KeybindingsConfig, KeybindingsManager};
            let mut config = KeybindingsConfig::new();
            config.insert(
                "tui.altScreen.lineUp".to_string(),
                KeyBindingValue::Single("ctrl+y".to_string()),
            );
            config.insert(
                "tui.altScreen.lineDown".to_string(),
                KeyBindingValue::Single("ctrl+e".to_string()),
            );
            crate::keybindings::set_keybindings(KeybindingsManager::new(
                crate::keybindings::tui_keybindings().to_vec(),
                config,
            ));
        }
        let _restore = scopeguard_defaults();
        tui.add_child(text(
            &(1..=30)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        tui.start();
        settle(&tui);
        assert_eq!(tui.viewport_top(), 20);

        send_input(&terminal, &tui, "\x19"); // ctrl+y → lineUp
        settle(&tui);
        assert_eq!(tui.viewport_top(), 19);

        send_input(&terminal, &tui, "\x05"); // ctrl+e → lineDown
        settle(&tui);
        assert_eq!(tui.viewport_top(), 20);
        stop(&tui);
    }

    /// Unbound by default: plain `ctrl+y` does nothing without a user
    /// binding (1279952de).
    #[test]
    fn line_scroll_actions_are_unbound_by_default() {
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(20, 10);
        let tui = TuiAltScreen::new(Box::new(terminal.clone()));
        tui.add_child(text(
            &(1..=30)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        tui.start();
        settle(&tui);
        assert_eq!(tui.viewport_top(), 20);

        send_input(&terminal, &tui, "\x19");
        settle(&tui);
        assert_eq!(tui.viewport_top(), 20);
        stop(&tui);
    }

    // it("shows a clickable jump-to-end indicator on the transcript's last row
    // while scrolled up") (tui-alt-screen.test.ts:97; 79680533c)
    #[test]
    fn shows_clickable_jump_to_end_indicator_while_scrolled_up() {
        let _caps = CapsGuard::lock_only();
        let (transcript, tui, terminal) = transcript_with_dock(
            (1..=8).map(|i| format!("line {i}")).collect::<Vec<_>>(),
            TuiAltScreenOptions {
                scroll_to_end_indicator: Some(Arc::new(|| {
                    "\x1b[7m ↓ Jump to end \x1b[27m".to_string()
                })),
                ..TuiAltScreenOptions::default()
            },
            30,
            6,
        );
        tui.start();
        settle(&tui);
        assert!(!terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("Jump to end")));

        send_input(&terminal, &tui, "\x1b[<64;1;1M");
        settle(&tui);
        assert!(!with_sv(&transcript, ScrollView::is_following_end));
        assert_eq!(terminal.get_viewport()[3], "line 7  ↓ Jump to end         ");
        assert_eq!(terminal.get_viewport()[4].trim_end(), "editor");

        // Pressing next to the label starts a selection instead of jumping.
        send_input(&terminal, &tui, "\x1b[<0;2;4M");
        send_input(&terminal, &tui, "\x1b[<0;2;4m");
        settle(&tui);
        assert!(!with_sv(&transcript, ScrollView::is_following_end));

        send_input(&terminal, &tui, "\x1b[<0;15;4M");
        send_input(&terminal, &tui, "\x1b[<0;15;4m");
        settle(&tui);
        assert!(with_sv(&transcript, ScrollView::is_following_end));
        assert_eq!(
            terminal
                .get_viewport()
                .iter()
                .map(|line| line.trim_end().to_string())
                .collect::<Vec<_>>(),
            vec!["line 5", "line 6", "line 7", "line 8", "editor", "footer"]
        );
        stop(&tui);
    }

    // ---------------------------------------------------------------------
    // it("leaves the scrollbar clickable when the jump-to-end indicator spans the transcript")
    // (tui-alt-screen.test.ts:141-169 @ 9841914; with 457ae8c79 the press
    // lands on the scrollbar TRACK and starts a drag instead of activating
    // the full-width indicator)
    // ---------------------------------------------------------------------

    #[test]
    fn leaves_the_scrollbar_clickable_when_the_jump_to_end_indicator_spans_the_transcript() {
        let _caps = CapsGuard::lock_only();
        // Upstream test layout: a follow-end transcript with `always`
        // scrollbar + a two-line dock (tui-alt-screen.test.ts:148-158).
        let terminal = VirtualTerminal::new(30, 6);
        let tui = TuiAltScreen::with_options(
            Box::new(terminal.clone()),
            None,
            None,
            TuiAltScreenOptions {
                scroll_to_end_indicator: Some(Arc::new(|| "↓".repeat(30))),
                ..TuiAltScreenOptions::default()
            },
        );
        let transcript = shared_component(ScrollView::new(
            shared_component(TestText {
                lines: Arc::new(Mutex::new(
                    (1..=12).map(|i| format!("line {i}")).collect::<Vec<_>>(),
                )),
            }),
            ScrollViewOptions {
                follow: Follow::End,
                primary: true,
                scrollbar: ScrollbarMode::Always,
                ..ScrollViewOptions::default()
            },
        ));
        let dock = shared_component(VStack::new(
            vec![
                StackChild::Component(text("editor")),
                StackChild::Component(text("footer")),
            ],
            StackOptions::default(),
        ));
        tui.set_layout_root(Some(shared_component(VStack::new(
            vec![
                StackChild::Entry(
                    transcript.clone(),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(0.0)),
                        grow: Some(1.0),
                        min_size: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
                StackChild::Entry(
                    dock,
                    StackEntryOptions {
                        basis: Some(Basis::Auto),
                        min_size: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        ))));
        tui.start();
        settle(&tui);

        send_input(&terminal, &tui, "\x1b[<64;1;1M");
        settle(&tui);
        assert!(!with_sv(&transcript, ScrollView::is_following_end));

        // The indicator must not intercept a press on the scrollbar's last
        // column: the track press starts a drag (jump-to-page clamps at the
        // pointer row, which is above the end).
        send_input(&terminal, &tui, "\x1b[<0;30;4M");
        send_input(&terminal, &tui, "\x1b[<0;30;4m");
        settle(&tui);
        assert!(!with_sv(&transcript, ScrollView::is_following_end));
        stop(&tui);
    }

    // it("never shows the jump-to-end indicator for a primary scroll view
    // without follow-end") (tui-alt-screen.test.ts:174)
    #[test]
    fn never_shows_jump_to_end_indicator_without_follow_end() {
        let _caps = CapsGuard::lock_only();
        let terminal = VirtualTerminal::new(30, 3);
        let tui = TuiAltScreen::with_options(
            Box::new(terminal.clone()),
            None,
            None,
            TuiAltScreenOptions {
                scroll_to_end_indicator: Some(Arc::new(|| " ↓ Jump to end ".to_string())),
                ..TuiAltScreenOptions::default()
            },
        );
        let transcript = shared_component(ScrollView::new(
            text("one\ntwo\nthree\nfour\nfive"),
            ScrollViewOptions {
                primary: true,
                ..ScrollViewOptions::default()
            },
        ));
        tui.set_layout_root(Some(transcript.clone()));
        tui.start();
        settle(&tui);

        assert!(!with_sv(&transcript, ScrollView::is_following_end));
        assert!(!terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("Jump to end")));
        stop(&tui);
    }

    /// FR-B R3 wiring: the indicator label carries the current
    /// `tui.altScreen.bottom` key display and `selectedBg`/`text` styling
    /// (tui-renderer.ts:29-34) — exercised through the same options the app
    /// passes (`fullscreen_alt_screen_options` lives in the rpi crate; this
    /// checks the contract from the tui side with an equivalent closure).
    #[test]
    fn jump_to_end_indicator_label_reflects_bottom_shortcut() {
        let _caps = CapsGuard::lock_only();
        let (transcript, tui, terminal) = transcript_with_dock(
            (1..=8).map(|i| format!("line {i}")).collect::<Vec<_>>(),
            TuiAltScreenOptions {
                scroll_to_end_indicator: Some(Arc::new(move || {
                    // keyDisplayText("tui.altScreen.bottom") with the app
                    // default table → "End".
                    let shortcut = {
                        let manager = crate::keybindings::get_keybindings()
                            .read()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let keys = manager.get_keys_by_id("tui.altScreen.bottom");
                        if keys.is_empty() {
                            String::new()
                        } else {
                            // capitalize parts like formatKeys
                            keys[0]
                                .split('+')
                                .map(|part| {
                                    let mut chars = part.chars();
                                    match chars.next() {
                                        Some(first) => {
                                            first.to_uppercase().collect::<String>()
                                                + chars.as_str()
                                        }
                                        None => String::new(),
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("+")
                        }
                    };
                    if shortcut.is_empty() {
                        " ↓ Jump to latest message ".to_string()
                    } else {
                        format!(" ↓ Jump to latest message · {shortcut} ")
                    }
                })),
                ..TuiAltScreenOptions::default()
            },
            40,
            6,
        );
        let _ = &transcript;
        tui.start();
        settle(&tui);
        send_input(&terminal, &tui, "\x1b[<64;1;1M");
        settle(&tui);
        let row = terminal.get_viewport()[3].clone();
        assert!(
            row.contains("↓ Jump to latest message · End"),
            "label with bottom shortcut: {row:?}"
        );
        stop(&tui);
    }
}
