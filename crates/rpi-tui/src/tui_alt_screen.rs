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

use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::Engine;
use tokio::sync::oneshot;

use crate::components::alt_screen_flash::{AltScreenFlashContainer, DEFAULT_DURATION_MS};
use crate::components::scroll_view::{
    Follow, Overscroll, ScrollView, ScrollViewOptions, ScrollbarMode,
};
use crate::keybindings::{get_keybindings, Keybinding};
use crate::keys::is_key_release;
use crate::kitty_registry::{
    clear_kitty_image_cache, kitty_image_cache_has_entries, prepare_kitty_screen,
};
use crate::layout::{
    get_scroll_view_box, get_scroll_views_at, render_layout_frame, LayoutBox, LayoutFrame,
    ScrollbarGeometry,
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
    composite_tui_line, lock_component, lock_shared, same_component, shared_component, Component,
    OverlayHandle, OverlayHandleOps, OverlayOptions, OverlayUnfocusOptions, RenderHandle,
    SharedComponent, SharedTerminal, TerminalColorSchemeListener, Tui, TuiInputListener,
    TuiInputListenerResult, TuiMode, TuiStopOptions, ViewportTui, CURSOR_MARKER,
};
use crate::tui_base::{
    schedule_render, PendingOsc11BackgroundQuery, PendingTerminalColorSchemeQuery, RenderSchedule,
    TerminalSizeCache, TuiBase,
};
use crate::utils::{
    extract_ansi_code, get_grapheme_cell_range, get_osc8_link_at_column, get_word_segmenter,
    slice_by_column, strip_terminal_sequences, visible_width,
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

/// `ScrollbarDrag` (tui-alt-screen.ts:107-110).
#[derive(Clone)]
struct ScrollbarDrag {
    scroll_view: SharedComponent,
    grab_offset: isize,
}

/// `ScrollbarTarget` (tui-alt-screen.ts:112-115).
#[derive(Clone)]
struct ScrollbarTarget {
    scroll_view: SharedComponent,
    geometry: ScrollbarGeometry,
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

/// `openUrl` callback (tui-alt-screen.ts:123).
pub type OpenUrlCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// `onRightClickPaste` callback (tui-alt-screen.ts:125).
pub type RightClickPasteCallback = Arc<dyn Fn() + Send + Sync>;

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
    /// `openUrl`: open an OSC 8 hyperlink activated with a primary click.
    pub open_url: Option<OpenUrlCallback>,
    /// `onRightClickPaste`: handle an unmodified secondary-button press for
    /// clipboard paste. Enabled on Windows only upstream.
    pub on_right_click_paste: Option<RightClickPasteCallback>,
    /// Test injection for upstream's `process.platform === "win32"` check
    /// (tui-alt-screen.test.ts stubs `process.platform`); production uses
    /// `cfg!(windows)`.
    #[doc(hidden)]
    pub win32_override: Option<bool>,
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
}

impl Component for ImplicitDocument {
    fn render(&self, width: usize) -> Vec<String> {
        let children = lock_shared(&self.children).clone();
        let mut lines = Vec::new();
        for child in &children {
            lines.extend(lock_component(child).render(width));
        }
        lines
    }

    fn invalidate(&mut self) {
        let children = lock_shared(&self.children).clone();
        for child in &children {
            lock_component(child).invalidate();
        }
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
    pressed_url: Option<String>,
    selection_dragged: bool,
    wheel_scroll_lines: u64,
    mouse_enabled: bool,
    open_url: Option<OpenUrlCallback>,
    on_right_click_paste: Option<RightClickPasteCallback>,
    /// `process.platform === "win32"` (see header note).
    win32: bool,
    /// Render handle handed to the layout engine and the flash container
    /// (upstream `() => this.requestRender()`).
    render_handle: RenderHandle,
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
        let implicit_document = shared_component(ImplicitDocument {
            children: Arc::clone(&implicit_children),
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
            pressed_url: None,
            selection_dragged: false,
            // `Math.max(1, Math.floor(options.wheelScrollLines ?? 1))`
            // (tui-alt-screen.ts:178); `u64` is already floored.
            wheel_scroll_lines: options.wheel_scroll_lines.unwrap_or(1).max(1),
            mouse_enabled: options.mouse.unwrap_or(true),
            open_url: options.open_url,
            on_right_click_paste: options.on_right_click_paste,
            win32: options.win32_override.unwrap_or(cfg!(windows)),
            render_handle,
        };
        TuiAltScreen {
            inner: Arc::new(Mutex::new(inner)),
            terminal,
            schedule,
            pending: Arc::new(Mutex::new(Vec::new())),
            inbox: Arc::new(Mutex::new(VecDeque::new())),
            implicit_scroll_view,
            next_listener_id: Arc::new(AtomicU64::new(1)),
            next_overlay_id: Arc::new(AtomicU64::new(1)),
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

    /// `handleViewportInput` (tui-alt-screen.ts:385-460): the first input
    /// listener — focus events, wheel routing, mouse (paste / scrollbar /
    /// selection), the mouse-sequence catch-all, and the eight
    /// `tui.altScreen.*` keybinding actions. Returns `Some(consume)` when the
    /// input is swallowed.
    fn handle_viewport_input(&mut self, data: &str) -> Option<TuiInputListenerResult> {
        let consume = || TuiInputListenerResult {
            consume: true,
            data: None,
        };

        if data == FOCUS_OUT {
            let had_active_selection = self.selection_press_active;
            self.selection_press_active = false;
            self.stop_selection_auto_scroll();
            self.stop_scrollbar_hover();
            self.stop_scrollbar_drag();
            self.pressed_url = None;
            self.selection_dragged = false;
            if had_active_selection {
                self.selection_anchor = None;
                self.selection_focus = None;
                self.selection_granularity = SelectionGranularity::Character;
                self.selection_initial_range = None;
            }
            self.last_click = None;
            self.request_render(false);
            return Some(consume());
        }
        if data == FOCUS_IN {
            return Some(consume());
        }

        if let Some(wheel_event) = parse_wheel_event(data) {
            self.route_wheel(wheel_event);
            return Some(consume());
        }
        if let Some(mouse_event) = parse_sgr_mouse_event(data) {
            if self.handle_right_click_paste(mouse_event) {
                return Some(consume());
            }
            let handled = self.handle_scrollbar_mouse_event(mouse_event);
            if self.scrollbar_drag.is_none() {
                self.update_scrollbar_hover(mouse_event.x, mouse_event.y);
            }
            if !handled {
                self.handle_selection_mouse_event(mouse_event);
            }
            return Some(consume());
        }
        if is_mouse_sequence(data) {
            return Some(consume());
        }

        let is_release = is_key_release(data);
        let keybindings = get_keybindings()
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

    /// `handleRightClickPaste` (tui-alt-screen.ts:514-524): unmodified
    /// secondary-button press on Windows; clipboard paste is best-effort.
    fn handle_right_click_paste(&mut self, event: SgrMouseEvent) -> bool {
        if self.on_right_click_paste.is_none() || !self.win32 || event.release || event.button != 2
        {
            return false;
        }
        if let Some(callback) = &self.on_right_click_paste {
            callback();
        }
        true
    }

    // --- scrollbar (tui-alt-screen.ts:526-603) -----------------------------

    /// `getScrollbarTargetAt` (tui-alt-screen.ts:526-541): the first
    /// (deepest) scroll view whose visible scrollbar thumb covers the point.
    fn get_scrollbar_target_at(&self, x: isize, y: isize) -> Option<ScrollbarTarget> {
        if self.has_overlay() {
            return None;
        }
        let layout = self.current_layout.as_ref()?;
        for scroll_view in get_scroll_views_at(layout, x, y) {
            let geometry = get_scroll_view_box(layout, &scroll_view)
                .and_then(crate::layout::get_scrollbar_geometry);
            if let Some(geometry) = geometry {
                if x == geometry.column
                    && y >= geometry.thumb_top
                    && y < geometry.thumb_top + geometry.thumb_height as isize
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

    /// `setScrollbarHover` (tui-alt-screen.ts:543-548).
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

    /// `updateScrollbarHover` (tui-alt-screen.ts:550-552).
    fn update_scrollbar_hover(&mut self, x: u32, y: u32) {
        let target = self
            .get_scrollbar_target_at(x as isize, y as isize)
            .map(|target| target.scroll_view);
        self.set_scrollbar_hover(target);
    }

    /// `stopScrollbarHover` (tui-alt-screen.ts:554-556).
    fn stop_scrollbar_hover(&mut self) {
        self.set_scrollbar_hover(None);
    }

    /// `handleScrollbarMouseEvent` (tui-alt-screen.ts:558-599): thumb drag
    /// (motion maps the thumb offset back to `scrollTop`) and drag start on
    /// primary-button press over the thumb.
    fn handle_scrollbar_mouse_event(&mut self, event: SgrMouseEvent) -> bool {
        if let Some(drag) = self.scrollbar_drag.clone() {
            if event.release {
                self.stop_scrollbar_drag();
                return true;
            }
            let geometry = self.current_layout.as_ref().and_then(|layout| {
                get_scroll_view_box(layout, &drag.scroll_view)
                    .and_then(crate::layout::get_scrollbar_geometry)
            });
            if let Some(geometry) = geometry {
                let max_thumb_offset = geometry.track_height - geometry.thumb_height;
                let thumb_offset = (event.y as isize - geometry.track_top - drag.grab_offset)
                    .clamp(0, max_thumb_offset as isize);
                let scroll_top = if max_thumb_offset == 0 {
                    0
                } else {
                    ((thumb_offset as f64 / max_thumb_offset as f64)
                        * geometry.max_scroll_top as f64)
                        .round() as i64
                };
                with_scroll_view(&drag.scroll_view, |view| view.scroll_to(scroll_top));
            }
            return true;
        }

        if event.release || (event.button & 32) != 0 || (event.button & 3) != 0 {
            return false;
        }
        let Some(target) = self.get_scrollbar_target_at(event.x as isize, event.y as isize) else {
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
        self.scrollbar_drag = Some(ScrollbarDrag {
            scroll_view: target.scroll_view,
            grab_offset: event.y as isize - target.geometry.thumb_top,
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

    /// `getWordSelection` (tui-alt-screen.ts:644-658): the word segment
    /// covering the point's column (ICU word bounds over the stripped line).
    fn get_word_selection(&self, point: &SelectionPoint) -> Option<SelectionRange> {
        let line = strip_terminal_sequences(&self.get_selection_source_line(point));
        let segmenter = get_word_segmenter();
        let mut start = 0usize;
        for segment in segmenter.segment(&line) {
            let end = start + visible_width(segment);
            if point.col >= start && point.col < end {
                return Some(SelectionRange {
                    start: point.with(start, false),
                    end: point.with(end, true),
                });
            }
            start = end;
        }
        None
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

    /// `handleSelectionMouseEvent` (tui-alt-screen.ts:768-833): the primary
    /// button's press / drag-motion / release state machine. Orphan events
    /// (release or motion without an active press) are swallowed.
    fn handle_selection_mouse_event(&mut self, event: SgrMouseEvent) {
        if (event.button & 3) != 0 {
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
            let clicked_url = if !self.selection_dragged
                && same_optional_scroll_view(&anchor.scroll_view, &point.scroll_view)
                && anchor.row == point.row
                && anchor.col == point.col
            {
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
            self.copy_selection_to_clipboard();
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

    /// `copySelectionToClipboard` (tui-alt-screen.ts:873-897): emit the
    /// selected text as an OSC 52 clipboard write and flash "Copied!".
    fn copy_selection_to_clipboard(&mut self) {
        let Some(selection) = self.get_selection_bounds() else {
            return;
        };
        // The scroll-content-lines source (tui-alt-screen.ts:877-882); the
        // highlight transform does not apply to the copy path (upstream reads
        // rows/cols in source coordinates here).
        let source_lines: Option<Arc<[String]>> = match &selection.start.scroll_view {
            Some(scroll_view) => {
                let Some(layout) = &self.current_layout else {
                    return;
                };
                let Some(lines) = get_scroll_view_box(layout, scroll_view)
                    .and_then(|layout_box| layout_box.scroll_content_lines.clone())
                else {
                    return;
                };
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
            return;
        }
        let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        self.terminal().write(&format!("\x1b]52;c;{payload}\x07"));
        self.flashes.flash("Copied!", DEFAULT_DURATION_MS);
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
        let next_layout = render_layout_frame(&root, width, height, self.render_handle.clone());
        let mut screen: Vec<String> = next_layout
            .lines
            .iter()
            .map(|line| strip_osc133_zone_prefix(line).to_string())
            .collect();
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
    use crate::tui::{shared_component, Focusable};
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
        assert_eq!(
            trimmed_viewport(&terminal),
            vec!["line 16", "line 17", "line 18", "line 19", "line 20"]
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

        {
            let event = parse_sgr_mouse_event("\x1b[<0;1;1M").unwrap();
            let mut inner = tui.lock_inner();
            eprintln!("parsed");
            let sv = inner.current_layout.as_ref().and_then(|layout| {
                crate::layout::get_scroll_views_at(layout, event.x as isize, event.y as isize)
                    .into_iter()
                    .next()
            });
            eprintln!("hit test: {:?}", sv.is_some());
            let anchor = inner.get_selection_point(event, sv.as_ref());
            eprintln!(
                "anchor: row={} col={} sv={}",
                anchor.row,
                anchor.col,
                anchor.scroll_view.is_some()
            );
            let word = inner.get_word_selection(&anchor);
            eprintln!("word: {:?}", word.is_some());
            let cc = inner.get_click_count(&anchor, word.as_ref());
            eprintln!("click count: {cc}");
        }
        return;
        #[allow(unreachable_code)]
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
}
