//! Port of `packages/tui/src/tui-main-screen.ts` @ 4181f66: the main-screen
//! TUI renderer (upstream `TuiMainScreen`, tui-main-screen.ts:57) — the
//! differential render pipeline moved out of the pre-split `Tui`
//! (`do_render` / `full_render` / `composite_overlays` /
//! `position_hardware_cursor` / `render_children`, moved line-by-line), the
//! render-state capture/restore (tui-main-screen.ts:68-89) and the
//! `preserve_screen` stop hook (tui-main-screen.ts:101-109).
//!
//! [`TuiMainScreen`] is the clonable handle (the pre-split `Tui` struct) and
//! the only [`Tui`] implementation so far; it takes the stable-reference
//! role of upstream's renderer `Proxy` (b103937d3). If T32's runtime mode
//! switch needs `dyn Tui` forwarding, that lands with T31/T32. The shared
//! engine state and behavior live in `TuiBase` (tui_base.rs; see its module
//! header for the composition-over-inheritance mapping).
//!
//! Intentional differences: see the `tui.rs` header notes; this file adds
//! none of its own. The kitty image line helpers live here like upstream
//! (tui-main-screen.ts:7-40).

use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::oneshot;

use crate::terminal::{InputHandler, ResizeHandler, Terminal};
use crate::terminal_colors::{RgbColor, TerminalColorScheme};
use crate::terminal_image::{delete_kitty_image, is_image_line};
use crate::tui::{
    composite_tui_line, lock_component, lock_shared, same_component, OverlayHandle, OverlayOptions,
    RenderHandle, SharedComponent, SharedTerminal, TerminalColorSchemeListener, Tui,
    TuiInputListener, TuiMode, TuiStopOptions, CURSOR_MARKER, SEGMENT_RESET,
};
use crate::tui_base::{
    env_flag_is_1, schedule_render, OverlayStackEntry, PendingOsc11BackgroundQuery,
    PendingTerminalColorSchemeQuery, RenderSchedule, TerminalSizeCache, TuiBase,
};
use crate::utils::{normalize_terminal_output, slice_by_column, visible_width};

// =============================================================================
// Kitty image line helpers (tui.ts:21-58)
// =============================================================================

/// `KITTY_SEQUENCE_PREFIX` (tui.ts:21).
const KITTY_SEQUENCE_PREFIX: &str = "\x1b_G";

/// `KittyImageHeader` (tui.ts:23-26).
#[derive(Debug, Clone, PartialEq, Eq)]
struct KittyImageHeader {
    ids: Vec<u32>,
    rows: usize,
}

/// JS `Number(value)` for Kitty parameter values: optional surrounding
/// whitespace is tolerated, anything else fails to parse.
fn parse_kitty_param_number(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

/// `parseKittyImageHeader` (tui.ts:28-51).
fn parse_kitty_image_header(line: &str) -> Option<KittyImageHeader> {
    let sequence_start = line.find(KITTY_SEQUENCE_PREFIX)?;
    let params_start = sequence_start + KITTY_SEQUENCE_PREFIX.len();
    let params_end = line[params_start..]
        .find(';')
        .map(|pos| pos + params_start)?;

    let mut ids = Vec::new();
    let mut rows = 1;
    for param in line[params_start..params_end].split(',') {
        let mut parts = param.splitn(2, '=');
        let key = parts.next();
        let Some(value) = parts.next() else { continue };
        let Some(number_value) = parse_kitty_param_number(value) else {
            continue;
        };
        if number_value == 0 || number_value > 0xffffffff {
            continue;
        }
        match key {
            Some("i") => ids.push(number_value as u32),
            Some("r") => rows = number_value as usize,
            _ => {}
        }
    }
    Some(KittyImageHeader { ids, rows })
}

/// `extractKittyImageIds` (tui.ts:53-55).
fn extract_kitty_image_ids(line: &str) -> Vec<u32> {
    parse_kitty_image_header(line)
        .map(|header| header.ids)
        .unwrap_or_default()
}

/// `extractKittyImageRows` (tui.ts:57-59).
fn extract_kitty_image_rows(line: &str) -> usize {
    parse_kitty_image_header(line)
        .map(|header| header.rows)
        .unwrap_or(1)
}

/// `RPI_DEBUG_REDRAW` (ADR-0001 rename of `PI_DEBUG_REDRAW`, tui.ts:1331).
const ENV_DEBUG_REDRAW: &str = "RPI_DEBUG_REDRAW";
/// `RPI_TUI_DEBUG` (ADR-0001 rename of `PI_TUI_DEBUG`, tui.ts:1577).
const ENV_TUI_DEBUG: &str = "RPI_TUI_DEBUG";

/// `new Date().toISOString()` for log lines: `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC).
fn iso_timestamp_now() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let secs = millis / 1000;
    let ms = millis % 1000;
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let (hour, minute, second) = (
        secs_of_day / 3600,
        secs_of_day % 3600 / 60,
        secs_of_day % 60,
    );
    // Howard Hinnant's civil-from-days algorithm (same as terminal.rs).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{ms:03}Z")
}

/// `isTermuxSession` (tui.ts:163-165). `Boolean(process.env.TERMUX_VERSION)`:
/// an empty value is falsy in JS.
fn is_termux_session() -> bool {
    std::env::var("TERMUX_VERSION").is_ok_and(|value| !value.is_empty())
}

/// Cursor position extracted from [`CURSOR_MARKER`] (upstream
/// `{ row, col }`, tui.ts:1238).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CursorPos {
    row: i32,
    col: i32,
}

/// Screen row diff between the current hardware cursor and a target buffer
/// row (upstream `computeLineDiff`, tui.ts:1268-1272).
fn compute_line_diff(
    target_row: i32,
    hardware_cursor_row: i32,
    prev_viewport_top: i32,
    viewport_top: i32,
) -> i32 {
    let current_screen_row = hardware_cursor_row - prev_viewport_top;
    let target_screen_row = target_row - viewport_top;
    target_screen_row - current_screen_row
}

/// Raw terminal events queued by the `Terminal::start` callbacks and drained
/// by [`TuiMainScreen::tick`] (see header note on input delivery).
enum InboxEvent {
    Input(String),
    Resize,
}

// =============================================================================
// TuiMainScreen (upstream `TuiMainScreen`, tui-main-screen.ts:57)
// =============================================================================

/// Main-screen TUI renderer with differential rendering (upstream
/// `TuiMainScreen`, tui-main-screen.ts:57 @ 4181f66; the pre-split `Tui`
/// struct, upstream `TUI` tui.ts:295 @ 2efa728).
///
/// Clonable handle around shared state; all clones refer to the same TUI.
/// Single-threaded event-loop semantics mirror upstream: drive rendering with
/// [`TuiMainScreen::tick`] / [`TuiMainScreen::pump`] from the loop thread.
#[derive(Clone)]
pub struct TuiMainScreen {
    inner: Arc<Mutex<TuiMainScreenInner>>,
    /// Shared with `TuiBase::terminal`; used directly by `pump` /
    /// `with_terminal` so blocking terminal waits never hold the inner lock.
    terminal: SharedTerminal,
    schedule: Arc<Mutex<RenderSchedule>>,
    pending: Arc<Mutex<Vec<PendingOp>>>,
    inbox: Arc<Mutex<VecDeque<InboxEvent>>>,
    next_listener_id: Arc<AtomicU64>,
    next_overlay_id: Arc<AtomicU64>,
    /// Cached terminal dimensions for lock-free reads from component
    /// methods (the inner lock is held while components render).
    size_cache: TerminalSizeCache,
}

/// Mutation queued while the inner lock is held (see the `tui.rs` header
/// note on re-entrancy).
type PendingOp = Box<dyn FnOnce(&mut TuiMainScreenInner) + Send>;

/// Main-screen renderer state: the upstream `TuiMainScreen` private fields
/// (tui-main-screen.ts:59-66) plus the composed [`TuiBase`]. Rust has no
/// inheritance; the `Deref`/`DerefMut` to the base keeps the moved render
/// pipeline's base-field accesses (`self.overlay_stack`, `self.terminal()`,
/// ...) spelled exactly as they were on the pre-split `TuiInner`.
pub(crate) struct TuiMainScreenInner {
    base: TuiBase,
    previous_lines: Vec<String>,
    /// JS `Set<number>`: deduplicated, first-seen insertion order.
    previous_kitty_image_ids: Vec<u32>,
    /// 0 = no previous render; -1 after `requestRender(true)` (tui.ts:719).
    previous_width: i32,
    previous_height: i32,
    cursor_row: i32,
    hardware_cursor_row: i32,
    max_lines_rendered: usize,
    previous_viewport_top: i32,
}

impl Deref for TuiMainScreenInner {
    type Target = TuiBase;

    fn deref(&self) -> &TuiBase {
        &self.base
    }
}

impl DerefMut for TuiMainScreenInner {
    fn deref_mut(&mut self) -> &mut TuiBase {
        &mut self.base
    }
}

/// `TuiMainScreenRenderState` (tui-main-screen.ts:46-54 @ 4181f66): the seven
/// main-screen render-state fields captured/restored across a renderer swap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiMainScreenRenderState {
    pub previous_lines: Vec<String>,
    pub previous_width: i32,
    pub previous_height: i32,
    pub cursor_row: i32,
    pub hardware_cursor_row: i32,
    pub max_lines_rendered: usize,
    pub previous_viewport_top: i32,
}

impl TuiMainScreen {
    /// Upstream `new TUI(terminal)` (tui.ts:329).
    pub fn new(terminal: Box<dyn Terminal + Send>) -> TuiMainScreen {
        Self::with_options(terminal, None, None)
    }

    /// Upstream `new TUI(terminal, showHardwareCursor?, logDirectory?)`
    /// (tui.ts:329-336).
    pub fn with_options(
        terminal: Box<dyn Terminal + Send>,
        show_hardware_cursor: Option<bool>,
        log_directory: Option<PathBuf>,
    ) -> TuiMainScreen {
        let schedule = Arc::new(Mutex::new(RenderSchedule {
            requested: false,
            deadline: None,
            last_render_at: None,
        }));
        let size_cache = TerminalSizeCache {
            rows: Arc::new(AtomicU16::new(terminal.rows())),
            columns: Arc::new(AtomicU16::new(terminal.columns())),
        };
        let terminal: SharedTerminal = Arc::new(Mutex::new(terminal));
        let base = TuiBase::new(
            Arc::clone(&terminal),
            show_hardware_cursor,
            log_directory,
            Arc::clone(&schedule),
            size_cache.clone(),
        );
        let inner = TuiMainScreenInner {
            base,
            previous_lines: Vec::new(),
            previous_kitty_image_ids: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            cursor_row: 0,
            hardware_cursor_row: 0,
            max_lines_rendered: 0,
            previous_viewport_top: 0,
        };
        TuiMainScreen {
            inner: Arc::new(Mutex::new(inner)),
            terminal,
            schedule,
            pending: Arc::new(Mutex::new(Vec::new())),
            inbox: Arc::new(Mutex::new(VecDeque::new())),
            next_listener_id: Arc::new(AtomicU64::new(1)),
            next_overlay_id: Arc::new(AtomicU64::new(1)),
            size_cache,
        }
    }

    // --- lock helpers -----------------------------------------------------

    fn lock_inner(&self) -> MutexGuard<'_, TuiMainScreenInner> {
        lock_shared(&self.inner)
    }

    /// Run `op` against the inner state. When the inner lock is already held
    /// (re-entrant call from a component mid-dispatch, or from another
    /// thread), the op is queued and runs after the in-progress work
    /// completes (see header note on re-entrancy).
    pub(crate) fn run_or_queue(&self, op: impl FnOnce(&mut TuiMainScreenInner) + Send + 'static) {
        match self.inner.try_lock() {
            Ok(mut inner) => {
                op(&mut inner);
                drop(inner);
                self.drain_pending_ops();
            }
            Err(_) => lock_shared(&self.pending).push(Box::new(op)),
        }
    }

    /// Read from the inner state; returns `None` on lock contention
    /// (re-entrant read from a component mid-dispatch).
    pub(crate) fn try_read<R>(&self, read: impl FnOnce(&TuiMainScreenInner) -> R) -> Option<R> {
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

    /// Upstream `addChild` (container.ts via `Container`, tui.ts:259).
    pub fn add_child(&self, component: SharedComponent) {
        self.run_or_queue(move |inner| inner.children.push(component));
    }

    /// Upstream `removeChild` (identity comparison, tui.ts:263).
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
        });
    }

    /// Rust addition (T12-S5a `showSelector`): insert a child at a specific
    /// position. Upstream swaps `editorContainer` contents in place; the port
    /// swaps the editor child in the TUI's child list, so position-preserving
    /// replacement needs an insert. Out-of-range indexes append.
    pub fn insert_child_at(&self, index: usize, component: SharedComponent) {
        self.run_or_queue(move |inner| {
            let index = index.min(inner.children.len());
            inner.children.insert(index, component);
        });
    }

    /// Rust addition: atomically swap `old` for `new` in the child list,
    /// preserving `old`'s position. Unlike a `child_position` +
    /// `remove_child` + `insert_child_at` sequence, the position lookup runs
    /// inside the same locked op, so it is safe to call mid-dispatch (when
    /// the inner lock is already held, `child_position` would fail and the
    /// fallback would append at the end). If `old` is not mounted, `new` is
    /// appended unless it is already mounted.
    pub fn swap_child(&self, old: &SharedComponent, new: &SharedComponent) {
        let old = Arc::clone(old);
        let new = Arc::clone(new);
        self.run_or_queue(move |inner| {
            if let Some(index) = inner
                .children
                .iter()
                .position(|child| same_component(child, &old))
            {
                inner.children[index] = new;
            } else if !inner
                .children
                .iter()
                .any(|child| same_component(child, &new))
            {
                inner.children.push(new);
            }
        });
    }

    /// Rust addition (T12-S5a `showSelector`): the position of a child in
    /// the TUI's child list (identity comparison). `None` on lock contention
    /// or when not mounted.
    pub fn child_position(&self, component: &SharedComponent) -> Option<usize> {
        self.try_read(|inner| {
            inner
                .children
                .iter()
                .position(|child| same_component(child, component))
        })
        .flatten()
    }

    /// Rust addition (T12-S5a `showSelector`): the number of top-level
    /// children. 0 on lock contention.
    pub fn children_len(&self) -> usize {
        self.try_read(|inner| inner.children.len()).unwrap_or(0)
    }

    /// Upstream `clear` (tui.ts:270).
    pub fn clear(&self) {
        self.run_or_queue(|inner| inner.children.clear());
    }

    // --- focus ------------------------------------------------------------

    /// Upstream `setFocus` (tui.ts:368).
    pub fn set_focus(&self, component: Option<SharedComponent>) {
        self.run_or_queue(move |inner| inner.set_focus(component));
    }

    // --- overlay API ------------------------------------------------------

    /// Upstream `showOverlay` (tui.ts:495). Returns a handle to control the
    /// overlay's visibility and focus.
    pub fn show_overlay(
        &self,
        component: SharedComponent,
        options: Option<OverlayOptions>,
    ) -> OverlayHandle {
        let entry_id = self.next_overlay_id.fetch_add(1, Ordering::Relaxed);
        self.run_or_queue(move |inner| inner.show_overlay(entry_id, component, options));
        OverlayHandle::new(self.clone(), entry_id)
    }

    /// Upstream `hideOverlay`: hide the topmost overlay and restore previous
    /// focus (tui.ts:591).
    pub fn hide_overlay(&self) {
        self.run_or_queue(|inner| inner.hide_overlay());
    }

    /// Upstream `hasOverlay` (tui.ts:607). Returns false on lock contention.
    pub fn has_overlay(&self) -> bool {
        self.try_read(|inner| inner.has_overlay()).unwrap_or(false)
    }

    /// Upstream `get hasOverlayEntries` (tui.ts:358-360 @ 4181f66, made
    /// public in b103937d3): any overlay entries, visible or not. Returns
    /// false on lock contention.
    pub fn has_overlay_entries(&self) -> bool {
        self.try_read(|inner| inner.has_overlay_entries())
            .unwrap_or(false)
    }

    /// Upstream `getFocusedComponent` (tui.ts:414-416 @ 4181f66, made public
    /// in b103937d3). Returns `None` on lock contention.
    pub fn get_focused_component(&self) -> Option<SharedComponent> {
        self.try_read(|inner| inner.focused_component.clone())
            .flatten()
    }

    /// Access the terminal (upstream `public terminal`, tui.ts:296). Blocks on
    /// the terminal lock only (never the inner lock, so it cannot starve
    /// behind the driver's pump loop) — event-loop / setup code only, never
    /// from within a component's `handle_input` / `render`.
    pub fn with_terminal<R>(&self, f: impl FnOnce(&mut dyn Terminal) -> R) -> R {
        f(&mut **lock_shared(&self.terminal))
    }

    // --- listeners --------------------------------------------------------

    /// Upstream `addInputListener` (tui.ts:651). Returns an id for
    /// [`TuiMainScreen::remove_input_listener`] (upstream returns an unsubscribe
    /// closure).
    pub fn add_input_listener(&self, listener: TuiInputListener) -> u64 {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.run_or_queue(move |inner| inner.input_listeners.push((id, listener)));
        id
    }

    /// Upstream `removeInputListener` (tui.ts:658).
    pub fn remove_input_listener(&self, id: u64) {
        self.run_or_queue(move |inner| inner.input_listeners.retain(|(lid, _)| *lid != id));
    }

    /// Global callback for the debug key (Shift+Ctrl+D), invoked before input
    /// is forwarded to the focused component (upstream `onDebug`, tui.ts:305).
    pub fn set_on_debug(&self, on_debug: Option<Box<dyn FnMut() + Send>>) {
        self.run_or_queue(move |inner| inner.on_debug = on_debug);
    }

    /// Upstream `onTerminalColorSchemeChange` (tui.ts:662). Returns an id for
    /// [`TuiMainScreen::remove_terminal_color_scheme_listener`].
    pub fn on_terminal_color_scheme_change(&self, listener: TerminalColorSchemeListener) -> u64 {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.run_or_queue(move |inner| inner.terminal_color_scheme_listeners.push((id, listener)));
        id
    }

    /// Remove a color scheme listener registered with
    /// [`TuiMainScreen::on_terminal_color_scheme_change`].
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

    // --- terminal introspection queries -----------------------------------

    /// Upstream `queryTerminalBackgroundColor` (tui.ts:1670): OSC 11 query
    /// (`ESC ] 11 ; ? BEL`); resolves with the parsed RGB color, or `None` on
    /// timeout / parse failure. The timeout is fired by [`TuiMainScreen::tick`].
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

    /// Upstream `queryTerminalColorScheme` (tui.ts:1698): DSR `CSI ? 996 n`.
    /// Terminals that support the color palette notification protocol reply
    /// with `CSI ? 997 ; 1 n` (dark) or `CSI ? 997 ; 2 n` (light).
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

    // --- settings ---------------------------------------------------------

    /// Upstream `get fullRedraws` (tui.ts:338). Returns 0 on lock contention.
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

    /// Upstream `invalidate` override (tui.ts:686-689): children plus
    /// overlays.
    pub fn invalidate(&self) {
        self.run_or_queue(|inner| inner.invalidate());
    }

    /// Cloneable capability handle for timer-driven components (upstream
    /// passes the whole `TUI`; see frozen-contract note on [`RenderHandle`]).
    /// The closure captures the shared render schedule weakly — storing
    /// components are owned by the TUI's child list, so a strong capture
    /// would cycle. A `force = false` request touches only that schedule, so
    /// semantics are unchanged while the TUI is alive; the call is a no-op
    /// after the TUI is dropped.
    pub fn render_handle(&self) -> RenderHandle {
        let schedule = Arc::downgrade(&self.schedule);
        RenderHandle::new(move || {
            if let Some(schedule) = schedule.upgrade() {
                schedule_render(&schedule, false, Instant::now());
            }
        })
    }

    /// Terminal row count (upstream `terminal.rows`), cached for lock-free
    /// reads. Components read this from `render`/`handle_input` while the
    /// inner lock is held, so the cache is refreshed on every render frame
    /// and initialized at construction.
    pub fn terminal_rows(&self) -> u16 {
        self.size_cache.rows.load(Ordering::Relaxed)
    }

    // --- lifecycle and driving --------------------------------------------

    /// Upstream `start` (tui.ts:691-705 @ 4181f66); the terminal callbacks
    /// queue into the inbox (see the `tui.rs` header note on input delivery),
    /// the base runs the common start segment.
    pub fn start(&self) {
        let mut inner = self.lock_inner();
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
    /// `options.preserve_screen` skips the cursor-to-content-end sequence
    /// (upstream `beforeTerminalStop`, tui-main-screen.ts:101-109); the
    /// default path is unchanged.
    pub fn stop(&self, options: TuiStopOptions) {
        self.lock_inner().stop_internal(options);
    }

    /// Non-blocking [`TuiMainScreen::stop`] for the panic/signal recovery path
    /// (`recovery.rs`): returns `false` without restoring when the inner lock
    /// is held (e.g. the panicking thread holds it mid-render), so the caller
    /// can fall back to a fixed restore sequence instead of deadlocking.
    /// Poisoning is recovered the same way as [`lock_shared`].
    pub(crate) fn try_stop(&self, options: TuiStopOptions) -> bool {
        use std::sync::TryLockError;
        let mut inner = match self.inner.try_lock() {
            Ok(inner) => inner,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        inner.stop_internal(options);
        true
    }

    /// Enqueue a full [`TuiMainScreen::stop`] for the recovery fallback (`recovery.rs`)
    /// used when the inner lock cannot be taken: the op runs on the next
    /// [`TuiMainScreen::tick`] pending-op drain, writing the complete restore sequence
    /// and setting `stopped` so later renders short-circuit. When the panic
    /// happened on the event-loop thread itself nothing drains the op — no
    /// renders follow either, so the minimal fallback sequence stands.
    pub(crate) fn queue_stop(&self, options: TuiStopOptions) {
        lock_shared(&self.pending).push(Box::new(move |inner| inner.stop_internal(options)));
    }

    /// Upstream `requestRender(force)` (tui.ts:765-774 @ 4181f66, 29d9f087c).
    ///
    /// Records render intent; the actual render happens on [`TuiMainScreen::tick`] once
    /// the deadline is reached (immediate for `force`, 16ms-throttled
    /// otherwise). With `force`, the previous frame state is reset so the
    /// next render takes the full-clear path — upstream `resetRenderState` +
    /// `requestImmediateRender` (tui.ts:766-770); the immediate arm is the
    /// `deadline = now` set by the force branch of `schedule_render`.
    pub fn request_render(&self, force: bool) {
        if force {
            self.run_or_queue(TuiMainScreenInner::reset_render_state);
        }
        schedule_render(&self.schedule, force, Instant::now());
    }

    /// Upstream `renderNow` (tui.ts:757-763 @ 4181f66): render synchronously,
    /// bypassing the throttle. `force` resets the render state first so the
    /// render takes the full-clear path. While stopped, `do_render`
    /// short-circuits (the render is a no-op), matching upstream.
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

    /// `captureRenderState` (tui-main-screen.ts:68-78): shallow-copy the
    /// seven render-state fields for a later
    /// [`TuiMainScreen::restore_render_state`] (renderer swap). Takes the
    /// inner lock directly — event-loop code only, never from within a
    /// component's `handle_input` / `render` (same constraint as
    /// [`TuiMainScreen::with_terminal`]).
    pub fn capture_render_state(&self) -> TuiMainScreenRenderState {
        let inner = self.lock_inner();
        TuiMainScreenRenderState {
            previous_lines: inner.previous_lines.clone(),
            previous_width: inner.previous_width,
            previous_height: inner.previous_height,
            cursor_row: inner.cursor_row,
            hardware_cursor_row: inner.hardware_cursor_row,
            max_lines_rendered: inner.max_lines_rendered,
            previous_viewport_top: inner.previous_viewport_top,
        }
    }

    /// `restoreRenderState` (tui-main-screen.ts:80-89): image lines are
    /// filtered to `""` and the kitty image id set cleared — the images they
    /// referenced were deleted with the previous renderer's output.
    pub fn restore_render_state(&self, state: TuiMainScreenRenderState) {
        self.run_or_queue(move |inner| inner.restore_render_state(state));
    }

    /// The next instant at which [`TuiMainScreen::tick`] has work to do: a pending
    /// render deadline, the earliest unsettled introspection query timeout,
    /// or a terminal-side flush deadline (for event-loop timeout
    /// computation; Rust addition replacing upstream's implicit timers).
    /// While stopped, a pending render is deferred until the restart and not
    /// reported here (see header note); query timeouts fire regardless of
    /// stopped, like upstream's `setTimeout`.
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
        [render_deadline, query_deadline, terminal_deadline]
            .into_iter()
            .flatten()
            .min()
    }

    /// Whether unprocessed input events, queued mutations, a pending render
    /// or an expired introspection query timeout exist (event loops can use
    /// this to tick without waiting). While stopped, a pending render is
    /// deferred until the restart and does not count (see header note);
    /// unexpired query timeouts do not count either — [`TuiMainScreen::next_deadline`]
    /// covers them.
    pub fn has_pending_work(&self) -> bool {
        if !lock_shared(&self.inbox).is_empty() || !lock_shared(&self.pending).is_empty() {
            return true;
        }
        let render_requested = lock_shared(&self.schedule).requested;
        let inner = self.lock_inner();
        if render_requested && !inner.stopped {
            return true;
        }
        inner.has_expired_query(Instant::now())
    }

    /// Drive the TUI: drain queued input events through the upstream
    /// `handleInput` flow, run queued mutations, then fire the render
    /// deadline and introspection query timeouts that expired by `now`.
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
    /// then drive the TUI like [`TuiMainScreen::tick`]. Mirrors the upstream event
    /// loop: terminal events arrive on the terminal's event source during the
    /// wait and are dispatched (to the `start` callbacks, which queue them
    /// into the inbox) right after. Returns whether the terminal dispatched
    /// at least one event.
    pub fn pump(&self, timeout: Option<Duration>) -> bool {
        // The blocking wait happens on the terminal's event source WITHOUT
        // holding any lock: the driver parks here between frames, and
        // `std::sync::Mutex` is not FIFO-fair, so a lock held across the wait
        // starves blocking lockers on other threads for an unbounded time
        // (see `SharedTerminal`).
        let source = lock_shared(&self.terminal).event_source();
        let Some(source) = source else {
            // No lock-free event source (virtual test terminals, or before
            // `start` / after `stop`): legacy behavior, the wait happens
            // inside the terminal lock.
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
            // Drain events that queued up behind the first one.
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

impl TuiMainScreenInner {
    /// `resetRenderState` (tui-main-screen.ts:91-99 @ 4181f66), called by
    /// `request_render(true)` and `render_now(force)`; -1 sentinels
    /// trigger `widthChanged` / `heightChanged`, forcing a full clear.
    fn reset_render_state(&mut self) {
        self.previous_lines = Vec::new();
        self.previous_width = -1;
        self.previous_height = -1;
        self.cursor_row = 0;
        self.hardware_cursor_row = 0;
        self.max_lines_rendered = 0;
        self.previous_viewport_top = 0;
    }

    /// `stop` (tui.ts:745-755 @ 4181f66) with the main-screen
    /// `beforeTerminalStop` hook orchestrated by hand (Rust has no
    /// inheritance). A pending render deadline deliberately survives
    /// (upstream leaks it as `renderRequested`; see the `tui.rs` header note)
    /// and fires on the first `tick` after `start()`.
    fn stop_internal(&mut self, options: TuiStopOptions) {
        self.begin_stop();
        self.before_terminal_stop(options);
        self.end_stop();
    }

    /// `beforeTerminalStop` (tui-main-screen.ts:101-109): move the cursor to
    /// the end of the content to prevent overwriting/artifacts on exit.
    /// Skipped when the screen is preserved for a taking-over TUI, or when
    /// nothing was rendered.
    fn before_terminal_stop(&mut self, options: TuiStopOptions) {
        if options.preserve_screen || self.previous_lines.is_empty() {
            return;
        }
        // Overwrite the inverted cursor with a normal space to clear the artifact.
        self.terminal().write(" ");
        let target_row = self.previous_lines.len() as i32; // line after the last content
        let line_diff = target_row - self.hardware_cursor_row;
        if line_diff > 0 {
            self.terminal().write(&format!("\x1b[{line_diff}B"));
        } else if line_diff < 0 {
            self.terminal().write(&format!("\x1b[{}A", -line_diff));
        }
        self.terminal().write("\r\n");
    }

    /// `compositeOverlays` (tui.ts:1036-1095): composite all overlays into
    /// content lines (sorted by focusOrder, higher = on top).
    fn composite_overlays(
        &self,
        lines: Vec<String>,
        term_width: i32,
        term_height: i32,
    ) -> Vec<String> {
        if self.overlay_stack.is_empty() {
            return lines;
        }
        let mut result = lines;

        // Pre-render all visible overlays and calculate positions.
        struct Rendered {
            overlay_lines: Vec<String>,
            row: i32,
            col: i32,
            width: i32,
        }
        let mut rendered: Vec<Rendered> = Vec::new();
        let mut min_lines_needed = result.len() as i32;

        let mut visible_entries: Vec<&OverlayStackEntry> = self
            .overlay_stack
            .iter()
            .filter(|entry| self.is_overlay_visible(entry))
            .collect();
        visible_entries.sort_by_key(|entry| entry.focus_order);
        for entry in visible_entries {
            // Get layout with height=0 first to determine width and maxHeight
            // (width and maxHeight don't depend on overlay height).
            let initial =
                TuiBase::resolve_overlay_layout(entry.options.as_ref(), 0, term_width, term_height);

            // Render component at calculated width.
            let mut overlay_lines =
                lock_component(&entry.component).render(initial.width.max(0) as usize);

            // Apply maxHeight if specified.
            if let Some(max_height) = initial.max_height {
                if overlay_lines.len() as i32 > max_height {
                    overlay_lines.truncate(max_height.max(0) as usize);
                }
            }

            // Get final row/col with the actual overlay height.
            let layout = TuiBase::resolve_overlay_layout(
                entry.options.as_ref(),
                overlay_lines.len() as i32,
                term_width,
                term_height,
            );
            min_lines_needed = min_lines_needed.max(layout.row + overlay_lines.len() as i32);
            rendered.push(Rendered {
                overlay_lines,
                row: layout.row,
                col: layout.col,
                width: initial.width,
            });
        }

        // Pad to at least terminal height so overlays have screen-relative
        // positions. Excludes maxLinesRendered: the historical high-water mark
        // caused self-reinforcing inflation that pushed content into scrollback
        // on terminal widen.
        let working_height = (result.len() as i32).max(term_height).max(min_lines_needed);
        while (result.len() as i32) < working_height {
            result.push(String::new());
        }

        let viewport_start = 0.max(working_height - term_height);

        // Composite each overlay.
        for rendered_overlay in &rendered {
            for (i, overlay_line) in rendered_overlay.overlay_lines.iter().enumerate() {
                let index = viewport_start + rendered_overlay.row + i as i32;
                if index >= 0 && (index as usize) < result.len() {
                    // Defensive: truncate overlay line to declared width before
                    // compositing (components should already respect width, but
                    // this ensures it).
                    let truncated = if visible_width(overlay_line) as i32 > rendered_overlay.width {
                        slice_by_column(
                            overlay_line,
                            0,
                            rendered_overlay.width.max(0) as usize,
                            true,
                        )
                    } else {
                        overlay_line.clone()
                    };
                    result[index as usize] = composite_tui_line(
                        &result[index as usize],
                        &truncated,
                        rendered_overlay.col,
                        rendered_overlay.width,
                        term_width,
                    );
                }
            }
        }

        result
    }

    /// `applyLineResets` (tui.ts:1099-1108): normalize terminal output and
    /// append SGR + OSC 8 reset to every non-image line.
    fn apply_line_resets(lines: &mut [String]) {
        for line in lines {
            if !is_image_line(line) {
                *line = normalize_terminal_output(line) + SEGMENT_RESET;
            }
        }
    }

    /// `collectKittyImageIds` (tui.ts:1110-1118); Vec keeps the JS Set's
    /// first-seen insertion order.
    fn collect_kitty_image_ids(lines: &[String]) -> Vec<u32> {
        let mut ids = Vec::new();
        for line in lines {
            for id in extract_kitty_image_ids(line) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        ids
    }

    /// `deleteKittyImages` (tui.ts:1120-1126).
    fn delete_kitty_images(ids: &[u32]) -> String {
        let mut buffer = String::new();
        for id in ids {
            buffer.push_str(&delete_kitty_image(*id));
        }
        buffer
    }

    /// `getKittyImageReservedRows` (tui.ts:1128-1140).
    fn get_kitty_image_reserved_rows(lines: &[String], index: usize, max_index: usize) -> usize {
        let rows = extract_kitty_image_rows(lines.get(index).map_or("", String::as_str));
        if rows <= 1 {
            return 1;
        }

        let max_rows = rows.min(max_index - index + 1).min(lines.len() - index);
        let mut reserved_rows = 1;
        while reserved_rows < max_rows {
            let line = lines.get(index + reserved_rows).map_or("", String::as_str);
            if is_image_line(line) || visible_width(line) > 0 {
                break;
            }
            reserved_rows += 1;
        }
        reserved_rows
    }

    /// `expandChangedRangeForKittyImages` (tui.ts:1142-1163).
    fn expand_changed_range_for_kitty_images(
        &self,
        first_changed: i32,
        last_changed: i32,
        new_lines: &[String],
    ) -> (i32, i32) {
        let mut expanded_first = first_changed;
        let mut expanded_last = last_changed;
        let mut expand_for_lines = |lines: &[String]| {
            for (i, line) in lines.iter().enumerate() {
                if extract_kitty_image_ids(line).is_empty() {
                    continue;
                }
                let block_end =
                    i + Self::get_kitty_image_reserved_rows(lines, i, lines.len() - 1) - 1;
                if i as i32 >= first_changed
                    || (i as i32 <= last_changed && block_end as i32 >= first_changed)
                {
                    expanded_first = expanded_first.min(i as i32);
                    expanded_last = expanded_last.max(block_end as i32);
                }
            }
        };

        expand_for_lines(&self.previous_lines);
        expand_for_lines(new_lines);
        (expanded_first, expanded_last)
    }

    /// `deleteChangedKittyImages` (tui.ts:1165-1177).
    fn delete_changed_kitty_images(&self, first_changed: i32, last_changed: i32) -> String {
        if first_changed < 0 || last_changed < first_changed {
            return String::new();
        }

        let mut ids = Vec::new();
        let max_line = (last_changed as usize).min(self.previous_lines.len().saturating_sub(1));
        for i in first_changed as usize..=max_line {
            for id in extract_kitty_image_ids(self.previous_lines.get(i).map_or("", String::as_str))
            {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }

        Self::delete_kitty_images(&ids)
    }

    /// `extractCursorPosition` (tui.ts:1238-1256): find CURSOR_MARKER in the
    /// visible viewport, strip it, and return its position.
    fn extract_cursor_position(lines: &mut [String], height: i32) -> Option<CursorPos> {
        // Only scan the bottom `height` lines (visible viewport).
        let viewport_top = 0.max(lines.len() as i32 - height);
        for row in (viewport_top..lines.len() as i32).rev() {
            let line = &lines[row as usize];
            if let Some(marker_index) = line.find(CURSOR_MARKER) {
                // Visual column = width of text before the marker.
                let col = visible_width(&line[..marker_index]) as i32;
                lines[row as usize] = format!(
                    "{}{}",
                    &line[..marker_index],
                    &line[marker_index + CURSOR_MARKER.len()..]
                );
                return Some(CursorPos { row, col });
            }
        }
        None
    }

    /// Full render helper (upstream `fullRender` closure, tui.ts:1288-1329):
    /// optionally clear scrollback and viewport, then render all new lines
    /// wrapped in synchronized output.
    fn full_render(
        &mut self,
        clear: bool,
        new_lines: Vec<String>,
        cursor_pos: Option<CursorPos>,
        width: i32,
        height: i32,
    ) {
        self.full_redraw_count += 1;
        let mut buffer = String::from("\x1b[?2026h"); // begin synchronized output
        if clear {
            buffer.push_str(&Self::delete_kitty_images(&self.previous_kitty_image_ids));
            // Clear screen, home, then clear scrollback.
            buffer.push_str("\x1b[2J\x1b[H\x1b[3J");
        }
        let mut i = 0;
        while i < new_lines.len() {
            if i > 0 {
                buffer.push_str("\r\n");
            }
            let line = &new_lines[i];
            let is_image = is_image_line(line);
            let image_reserved_rows = if is_image {
                Self::get_kitty_image_reserved_rows(&new_lines, i, new_lines.len() - 1)
            } else {
                1
            };
            if image_reserved_rows > 1 && image_reserved_rows as i32 <= height {
                for _ in 1..image_reserved_rows {
                    buffer.push_str("\r\n");
                }
                buffer.push_str(&format!("\x1b[{}A", image_reserved_rows - 1));
                buffer.push_str(line);
                buffer.push_str(&format!("\x1b[{}B", image_reserved_rows - 1));
                i += image_reserved_rows;
                continue;
            }
            buffer.push_str(line);
            i += 1;
        }
        buffer.push_str("\x1b[?2026l"); // end synchronized output
        self.terminal().write(&buffer);
        self.cursor_row = 0.max(new_lines.len() as i32 - 1);
        self.hardware_cursor_row = self.cursor_row;
        // Reset max lines when clearing, otherwise track growth.
        if clear {
            self.max_lines_rendered = new_lines.len();
        } else {
            self.max_lines_rendered = self.max_lines_rendered.max(new_lines.len());
        }
        let buffer_length = height.max(new_lines.len() as i32);
        self.previous_viewport_top = 0.max(buffer_length - height);
        self.position_hardware_cursor(cursor_pos, new_lines.len() as i32);
        self.previous_kitty_image_ids = Self::collect_kitty_image_ids(&new_lines);
        self.previous_lines = new_lines;
        self.previous_width = width;
        self.previous_height = height;
    }

    /// `logRedraw` (tui.ts:1331-1338); env var renamed to `RPI_DEBUG_REDRAW`
    /// (ADR-0001) and the log file to `rpi-debug.log`.
    fn log_redraw(&self, reason: &str, new_lines_len: usize, height: i32) {
        if !env_flag_is_1(ENV_DEBUG_REDRAW) {
            return;
        }
        let log_path = self.log_directory.join("rpi-debug.log");
        let message = format!(
            "[{}] fullRender: {} (prev={}, new={}, height={})\n",
            iso_timestamp_now(),
            reason,
            self.previous_lines.len(),
            new_lines_len,
            height
        );
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .and_then(|mut file| {
                use std::io::Write;
                file.write_all(message.as_bytes())
            });
    }

    /// `doRender` (tui.ts:1258-1625).
    fn do_render(&mut self) {
        if self.stopped {
            return;
        }
        let width = i32::from(self.terminal().columns());
        let height = i32::from(self.terminal().rows());
        // Refresh the lock-free size cache (read by components, e.g. the
        // Editor's `terminal_rows`, which cannot take the inner lock).
        self.size_cache
            .rows
            .store(self.terminal().rows(), Ordering::Relaxed);
        self.size_cache
            .columns
            .store(self.terminal().columns(), Ordering::Relaxed);
        let width_changed = self.previous_width != 0 && self.previous_width != width;
        let height_changed = self.previous_height != 0 && self.previous_height != height;
        let previous_buffer_length = if self.previous_height > 0 {
            self.previous_viewport_top + self.previous_height
        } else {
            height
        };
        let mut prev_viewport_top = if height_changed {
            0.max(previous_buffer_length - height)
        } else {
            self.previous_viewport_top
        };
        let mut viewport_top = prev_viewport_top;
        let mut hardware_cursor_row = self.hardware_cursor_row;

        // Render all components to get new lines.
        let mut new_lines = self.render_children(width);

        // Composite overlays into the rendered lines (before differential compare).
        if !self.overlay_stack.is_empty() {
            new_lines = self.composite_overlays(new_lines, width, height);
        }

        // Extract cursor position before applying line resets (marker must be
        // found first).
        let cursor_pos = Self::extract_cursor_position(&mut new_lines, height);

        Self::apply_line_resets(&mut new_lines);

        // First render - just output everything without clearing (assumes a
        // clean screen).
        if self.previous_lines.is_empty() && !width_changed && !height_changed {
            self.log_redraw("first render", new_lines.len(), height);
            self.full_render(false, new_lines, cursor_pos, width, height);
            return;
        }

        // Width changes always need a full re-render because wrapping changes.
        if width_changed {
            self.log_redraw(
                &format!(
                    "terminal width changed ({} -> {})",
                    self.previous_width, width
                ),
                new_lines.len(),
                height,
            );
            self.full_render(true, new_lines, cursor_pos, width, height);
            return;
        }

        // Height changes normally need a full re-render to keep the visible
        // viewport aligned, but Termux changes height when the software
        // keyboard shows or hides. In that environment, a full redraw causes
        // the entire history to replay on every toggle.
        if height_changed && !is_termux_session() {
            self.log_redraw(
                &format!(
                    "terminal height changed ({} -> {})",
                    self.previous_height, height
                ),
                new_lines.len(),
                height,
            );
            self.full_render(true, new_lines, cursor_pos, width, height);
            return;
        }

        // Content shrunk below the working area and no overlays - re-render to
        // clear empty rows (overlays need the padding, so only do this when no
        // overlays are active).
        if self.clear_on_shrink
            && new_lines.len() < self.max_lines_rendered
            && self.overlay_stack.is_empty()
        {
            self.log_redraw(
                &format!(
                    "clearOnShrink (maxLinesRendered={})",
                    self.max_lines_rendered
                ),
                new_lines.len(),
                height,
            );
            self.full_render(true, new_lines, cursor_pos, width, height);
            return;
        }

        // Find first and last changed lines.
        let mut first_changed: i32 = -1;
        let mut last_changed: i32 = -1;
        let max_lines = new_lines.len().max(self.previous_lines.len());
        for i in 0..max_lines {
            let old_line = self.previous_lines.get(i).map_or("", String::as_str);
            let new_line = new_lines.get(i).map_or("", String::as_str);
            if old_line != new_line {
                if first_changed == -1 {
                    first_changed = i as i32;
                }
                last_changed = i as i32;
            }
        }
        let appended_lines = new_lines.len() > self.previous_lines.len();
        if appended_lines {
            if first_changed == -1 {
                first_changed = self.previous_lines.len() as i32;
            }
            last_changed = new_lines.len() as i32 - 1;
        }
        if first_changed != -1 {
            let (expanded_first, expanded_last) =
                self.expand_changed_range_for_kitty_images(first_changed, last_changed, &new_lines);
            first_changed = expanded_first;
            last_changed = expanded_last;
        }
        let append_start = appended_lines
            && first_changed == self.previous_lines.len() as i32
            && first_changed > 0;

        // No changes - but still need to update the hardware cursor position
        // if it moved.
        if first_changed == -1 {
            self.position_hardware_cursor(cursor_pos, new_lines.len() as i32);
            self.previous_viewport_top = prev_viewport_top;
            self.previous_height = height;
            return;
        }

        // All changes are in deleted lines (nothing to render, just clear).
        if first_changed >= new_lines.len() as i32 {
            if self.previous_lines.len() > new_lines.len() {
                let mut buffer = String::from("\x1b[?2026h");
                buffer.push_str(&self.delete_changed_kitty_images(first_changed, last_changed));
                // Move to the end of new content (clamp to 0 for empty content).
                let target_row = 0.max(new_lines.len() as i32 - 1);
                if target_row < prev_viewport_top {
                    self.log_redraw(
                        &format!(
                            "deleted lines moved viewport up ({target_row} < {prev_viewport_top})"
                        ),
                        new_lines.len(),
                        height,
                    );
                    self.full_render(true, new_lines, cursor_pos, width, height);
                    return;
                }
                let line_diff = compute_line_diff(
                    target_row,
                    hardware_cursor_row,
                    prev_viewport_top,
                    viewport_top,
                );
                if line_diff > 0 {
                    buffer.push_str(&format!("\x1b[{line_diff}B"));
                } else if line_diff < 0 {
                    buffer.push_str(&format!("\x1b[{}A", -line_diff));
                }
                buffer.push('\r');
                // Clear extra lines without scrolling.
                let extra_lines = self.previous_lines.len() - new_lines.len();
                if extra_lines > height.max(0) as usize {
                    self.log_redraw(
                        &format!("extraLines > height ({extra_lines} > {height})"),
                        new_lines.len(),
                        height,
                    );
                    self.full_render(true, new_lines, cursor_pos, width, height);
                    return;
                }
                let clear_start_offset: i32 = if new_lines.is_empty() { 0 } else { 1 };
                if extra_lines > 0 && clear_start_offset > 0 {
                    buffer.push_str(&format!("\x1b[{clear_start_offset}B"));
                }
                for i in 0..extra_lines {
                    buffer.push_str("\r\x1b[2K");
                    if i < extra_lines - 1 {
                        buffer.push_str("\x1b[1B");
                    }
                }
                let move_back = 0.max(extra_lines as i32 - 1 + clear_start_offset);
                if move_back > 0 {
                    buffer.push_str(&format!("\x1b[{move_back}A"));
                }
                buffer.push_str("\x1b[?2026l");
                self.terminal().write(&buffer);
                self.cursor_row = target_row;
                self.hardware_cursor_row = target_row;
            }
            self.position_hardware_cursor(cursor_pos, new_lines.len() as i32);
            self.previous_kitty_image_ids = Self::collect_kitty_image_ids(&new_lines);
            self.previous_lines = new_lines;
            self.previous_width = width;
            self.previous_height = height;
            self.previous_viewport_top = prev_viewport_top;
            return;
        }

        // Differential rendering can only touch what was actually visible. If
        // the first changed line is above the previous viewport, we need a
        // full redraw.
        if first_changed < prev_viewport_top {
            self.log_redraw(
                &format!("firstChanged < viewportTop ({first_changed} < {prev_viewport_top})"),
                new_lines.len(),
                height,
            );
            self.full_render(true, new_lines, cursor_pos, width, height);
            return;
        }

        // Render from the first changed line to the end; all updates are
        // wrapped in synchronized output.
        let mut buffer = String::from("\x1b[?2026h");
        buffer.push_str(&self.delete_changed_kitty_images(first_changed, last_changed));
        let prev_viewport_bottom = prev_viewport_top + height - 1;
        let move_target_row = if append_start {
            first_changed - 1
        } else {
            first_changed
        };
        if move_target_row > prev_viewport_bottom {
            let current_screen_row =
                0.max((height - 1).min(hardware_cursor_row - prev_viewport_top));
            let move_to_bottom = height - 1 - current_screen_row;
            if move_to_bottom > 0 {
                buffer.push_str(&format!("\x1b[{move_to_bottom}B"));
            }
            let scroll = move_target_row - prev_viewport_bottom;
            for _ in 0..scroll {
                buffer.push_str("\r\n");
            }
            prev_viewport_top += scroll;
            viewport_top += scroll;
            hardware_cursor_row = move_target_row;
        }

        // Move cursor to the first changed line (use hardwareCursorRow for the
        // actual position).
        let line_diff = compute_line_diff(
            move_target_row,
            hardware_cursor_row,
            prev_viewport_top,
            viewport_top,
        );
        if line_diff > 0 {
            buffer.push_str(&format!("\x1b[{line_diff}B"));
        } else if line_diff < 0 {
            buffer.push_str(&format!("\x1b[{}A", -line_diff));
        }

        // Move to column 0.
        buffer.push_str(if append_start { "\r\n" } else { "\r" });

        // Only render changed lines (firstChanged to lastChanged), not all
        // lines to end. This reduces flicker when only a single line changes
        // (e.g., spinner animation).
        let render_end = last_changed.min(new_lines.len() as i32 - 1);
        let mut i = first_changed;
        while i <= render_end {
            if i > first_changed {
                buffer.push_str("\r\n");
            }
            let line = new_lines[i as usize].clone();
            let is_image = is_image_line(&line);
            let image_reserved_rows = if is_image {
                Self::get_kitty_image_reserved_rows(
                    &new_lines,
                    i as usize,
                    render_end.max(0) as usize,
                )
            } else {
                1
            };
            if image_reserved_rows > 1 {
                let image_start_screen_row = i - viewport_top;
                if image_start_screen_row < 0
                    || image_start_screen_row + image_reserved_rows as i32 > height
                {
                    self.log_redraw(
                        &format!(
                            "kitty image pre-clear would scroll ({image_start_screen_row} + {image_reserved_rows} > {height})"
                        ),
                        new_lines.len(),
                        height,
                    );
                    self.full_render(true, new_lines, cursor_pos, width, height);
                    return;
                }

                buffer.push_str("\x1b[2K");
                for _ in 1..image_reserved_rows {
                    buffer.push_str("\r\n\x1b[2K");
                }
                buffer.push_str(&format!("\x1b[{}A", image_reserved_rows - 1));
                buffer.push_str(&line);
                buffer.push_str(&format!("\x1b[{}B", image_reserved_rows - 1));
                i += image_reserved_rows as i32;
                continue;
            }

            buffer.push_str("\x1b[2K"); // clear current line
            if !is_image && visible_width(&line) > width.max(0) as usize {
                // Log all lines to a crash file for debugging.
                let crash_log_path = self.log_directory.join("rpi-crash.log");
                let mut crash_data = format!(
                    "Crash at {}\nTerminal width: {}\nLine {} visible width: {}\n\n=== All rendered lines ===\n",
                    iso_timestamp_now(),
                    width,
                    i,
                    visible_width(&line)
                );
                for (index, rendered) in new_lines.iter().enumerate() {
                    crash_data.push_str(&format!(
                        "[{index}] (w={}) {rendered}\n",
                        visible_width(rendered)
                    ));
                }
                crash_data.push('\n');
                if let Some(parent) = crash_log_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&crash_log_path, crash_data);

                // Clean up terminal state before throwing.
                self.stop_internal(TuiStopOptions::default());

                let error_message = format!(
                    "Rendered line {} exceeds terminal width ({} > {}).\n\nThis is likely caused by a custom TUI component not truncating its output.\nUse visibleWidth() to measure and truncateToWidth() to truncate lines.\n\nDebug log written to: {}",
                    i,
                    visible_width(&line),
                    width,
                    crash_log_path.display()
                );
                panic!("{error_message}");
            }
            buffer.push_str(&line);
            i += 1;
        }

        // Track where the cursor ended up after rendering.
        let mut final_cursor_row = render_end;

        // If we had more lines before, clear them and move the cursor back.
        if self.previous_lines.len() > new_lines.len() {
            // Move to the end of new content first if we stopped before it.
            if render_end < new_lines.len() as i32 - 1 {
                let move_down = new_lines.len() as i32 - 1 - render_end;
                buffer.push_str(&format!("\x1b[{move_down}B"));
                final_cursor_row = new_lines.len() as i32 - 1;
            }
            let extra_lines = self.previous_lines.len() - new_lines.len();
            for _ in 0..extra_lines {
                buffer.push_str("\r\n\x1b[2K");
            }
            // Move cursor back to the end of new content.
            buffer.push_str(&format!("\x1b[{extra_lines}A"));
        }

        buffer.push_str("\x1b[?2026l"); // end synchronized output

        if env_flag_is_1(ENV_TUI_DEBUG) {
            static DEBUG_DUMP_COUNTER: AtomicU64 = AtomicU64::new(0);
            let debug_dir = PathBuf::from("/tmp/rpi-tui");
            let _ = std::fs::create_dir_all(&debug_dir);
            let millis = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or(0);
            let debug_path = debug_dir.join(format!(
                "render-{}-{}.log",
                millis,
                DEBUG_DUMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let mut debug_data = format!(
                "firstChanged: {first_changed}\nviewportTop: {viewport_top}\ncursorRow: {}\nheight: {height}\nlineDiff: {line_diff}\nhardwareCursorRow: {hardware_cursor_row}\nrenderEnd: {render_end}\nfinalCursorRow: {final_cursor_row}\ncursorPos: {cursor_pos:?}\nnewLines.length: {}\npreviousLines.length: {}\n\n=== newLines ===\n{:#?}\n\n=== previousLines ===\n{:#?}\n\n=== buffer ===\n{:?}\n",
                self.cursor_row,
                new_lines.len(),
                self.previous_lines.len(),
                new_lines,
                self.previous_lines,
                buffer
            );
            debug_data.push('\n');
            let _ = std::fs::write(debug_path, debug_data);
        }

        // Write the entire buffer at once.
        self.terminal().write(&buffer);

        // Track cursor position for the next render. cursorRow tracks the end
        // of content (for viewport calculation); hardwareCursorRow tracks the
        // actual terminal cursor position (for movement).
        self.cursor_row = 0.max(new_lines.len() as i32 - 1);
        self.hardware_cursor_row = final_cursor_row;
        // Track the terminal's working area (grows but doesn't shrink unless cleared).
        self.max_lines_rendered = self.max_lines_rendered.max(new_lines.len());
        self.previous_viewport_top = prev_viewport_top.max(final_cursor_row - height + 1);

        // Position the hardware cursor for IME.
        self.position_hardware_cursor(cursor_pos, new_lines.len() as i32);

        self.previous_kitty_image_ids = Self::collect_kitty_image_ids(&new_lines);
        self.previous_lines = new_lines;
        self.previous_width = width;
        self.previous_height = height;
    }

    /// Render all children (upstream `Container.render` on the TUI itself,
    /// tui.ts:280-289).
    fn render_children(&self, width: i32) -> Vec<String> {
        let mut lines = Vec::new();
        for child in &self.children {
            lines.extend(lock_component(child).render(width.max(0) as usize));
        }
        lines
    }

    /// `positionHardwareCursor` (tui.ts:1632-1663): position the hardware
    /// cursor for the IME candidate window.
    fn position_hardware_cursor(&mut self, cursor_pos: Option<CursorPos>, total_lines: i32) {
        let Some(cursor_pos) = cursor_pos.filter(|_| total_lines > 0) else {
            self.terminal().hide_cursor();
            return;
        };

        // Clamp cursor position to valid range.
        let target_row = cursor_pos.row.clamp(0, total_lines - 1);
        let target_col = cursor_pos.col.max(0);

        // Move cursor from the current position to the target.
        let row_delta = target_row - self.hardware_cursor_row;
        let mut buffer = String::new();
        if row_delta > 0 {
            buffer.push_str(&format!("\x1b[{row_delta}B"));
        } else if row_delta < 0 {
            buffer.push_str(&format!("\x1b[{}A", -row_delta));
        }
        // Move to absolute column (1-indexed).
        buffer.push_str(&format!("\x1b[{}G", target_col + 1));

        if !buffer.is_empty() {
            self.terminal().write(&buffer);
        }

        self.hardware_cursor_row = target_row;
        if self.show_hardware_cursor {
            self.terminal().show_cursor();
        } else {
            self.terminal().hide_cursor();
        }
    }

    /// Fire expired deadlines (render throttle via the base, then the
    /// subclass `do_render` hook; introspection query timeouts). Called by
    /// [`TuiMainScreen::tick`].
    fn tick(&mut self, now: Instant) {
        if self.take_render_due(now) {
            self.do_render();
        }
        self.fire_expired_queries(now);
    }

    /// `restoreRenderState` body (tui-main-screen.ts:80-89): image lines are
    /// filtered to `""` (their pixels do not survive a renderer swap) and the
    /// kitty image id set is cleared.
    fn restore_render_state(&mut self, state: TuiMainScreenRenderState) {
        self.previous_lines = state
            .previous_lines
            .into_iter()
            .map(|line| {
                if is_image_line(&line) {
                    String::new()
                } else {
                    line
                }
            })
            .collect();
        self.previous_kitty_image_ids = Vec::new();
        self.previous_width = state.previous_width;
        self.previous_height = state.previous_height;
        self.cursor_row = state.cursor_row;
        self.hardware_cursor_row = state.hardware_cursor_row;
        self.max_lines_rendered = state.max_lines_rendered;
        self.previous_viewport_top = state.previous_viewport_top;
    }
}

impl Tui for TuiMainScreen {
    fn mode(&self) -> TuiMode {
        TuiMode::Regular
    }

    fn terminal(&self) -> SharedTerminal {
        Arc::clone(&self.terminal)
    }

    fn full_redraws(&self) -> u64 {
        TuiMainScreen::full_redraws(self)
    }

    fn add_child(&self, component: SharedComponent) {
        TuiMainScreen::add_child(self, component);
    }

    fn remove_child(&self, component: &SharedComponent) {
        TuiMainScreen::remove_child(self, component);
    }

    fn clear(&self) {
        TuiMainScreen::clear(self);
    }

    fn get_show_hardware_cursor(&self) -> bool {
        TuiMainScreen::get_show_hardware_cursor(self)
    }

    fn set_show_hardware_cursor(&self, enabled: bool) {
        TuiMainScreen::set_show_hardware_cursor(self, enabled);
    }

    fn get_clear_on_shrink(&self) -> bool {
        TuiMainScreen::get_clear_on_shrink(self)
    }

    fn set_clear_on_shrink(&self, enabled: bool) {
        TuiMainScreen::set_clear_on_shrink(self, enabled);
    }

    fn set_focus(&self, component: Option<SharedComponent>) {
        TuiMainScreen::set_focus(self, component);
    }

    fn get_focused_component(&self) -> Option<SharedComponent> {
        TuiMainScreen::get_focused_component(self)
    }

    fn show_overlay(
        &self,
        component: SharedComponent,
        options: Option<OverlayOptions>,
    ) -> OverlayHandle {
        TuiMainScreen::show_overlay(self, component, options)
    }

    fn hide_overlay(&self) {
        TuiMainScreen::hide_overlay(self);
    }

    fn has_overlay(&self) -> bool {
        TuiMainScreen::has_overlay(self)
    }

    fn has_overlay_entries(&self) -> bool {
        TuiMainScreen::has_overlay_entries(self)
    }

    fn start(&self) {
        TuiMainScreen::start(self);
    }

    fn stop(&self, options: TuiStopOptions) {
        TuiMainScreen::stop(self, options);
    }

    fn render_now(&self, force: bool) {
        TuiMainScreen::render_now(self, force);
    }

    fn request_render(&self, force: bool) {
        TuiMainScreen::request_render(self, force);
    }

    fn add_input_listener(&self, listener: TuiInputListener) -> u64 {
        TuiMainScreen::add_input_listener(self, listener)
    }

    fn remove_input_listener(&self, id: u64) {
        TuiMainScreen::remove_input_listener(self, id);
    }

    fn on_terminal_color_scheme_change(&self, listener: TerminalColorSchemeListener) -> u64 {
        TuiMainScreen::on_terminal_color_scheme_change(self, listener)
    }

    fn set_terminal_color_scheme_notifications(&self, enabled: bool) {
        TuiMainScreen::set_terminal_color_scheme_notifications(self, enabled);
    }

    fn query_terminal_background_color(
        &self,
        timeout: Duration,
    ) -> oneshot::Receiver<Option<RgbColor>> {
        TuiMainScreen::query_terminal_background_color(self, timeout)
    }

    fn query_terminal_color_scheme(
        &self,
        timeout: Duration,
    ) -> oneshot::Receiver<Option<TerminalColorScheme>> {
        TuiMainScreen::query_terminal_color_scheme(self, timeout)
    }

    fn invalidate(&self) {
        TuiMainScreen::invalidate(self);
    }
}

#[cfg(test)]
mod tests {
    //! Ports of the upstream test files (intent, not line-by-line):
    //! `test/tui-render.test.ts`, `test/overlay-options.test.ts`,
    //! `test/overlay-non-capturing.test.ts`, `test/overlay-short-content.test.ts`,
    //! `test/tui-shrink.test.ts`, `test/tui-cell-size-input.test.ts`,
    //! `test/tui-overlay-style-leak.test.ts`, `test/viewport-overwrite-repro.ts`
    //! (automated), the TUI-level cases of `test/tab-width.test.ts` and
    //! `test/regression-overlay-cjk-boundary.test.ts`, and the
    //! `TUI.queryTerminalBackgroundColor` cases of `test/terminal-colors.test.ts`.
    //!
    //! `VirtualTerminal` below ports `test/virtual-terminal.ts`; upstream uses
    //! `@xterm/headless`, here a minimal screen emulator covers exactly the
    //! sequences the TUI emits (CSI A/B/G/H/J/K, SGR with per-cell italic
    //! tracking for the style-leak assertions, CR/LF with scrolling, private
    //! modes, OSC/APC/DCS skipping). `translateToString(true)` parity: cells
    //! never written are dropped from the end of a line; explicitly written
    //! spaces count.
    //!
    //! Async timing (`waitForRender` = nextTick + 20ms settle + flush) becomes
    //! the explicit `settle` helper driving `TuiMainScreen::tick` with synthetic instants.

    use super::*;
    use crate::keys::{is_key_release, matches_key};
    use crate::tui::{
        shared_component, Component, Focusable, OverlayAnchor, OverlayMargin, OverlayMarginSpec,
        OverlayOptions, OverlayUnfocusOptions, SizeValue, TuiInputListenerResult,
    };
    use crate::tui_base::TuiBase;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;

    use unicode_width::UnicodeWidthChar;

    use crate::terminal_image::{
        reset_capabilities_cache, set_capabilities, set_cell_dimensions, CellDimensions,
        TerminalCapabilities,
    };

    /// Serializes tests that mutate process env or module globals
    /// (capabilities cache, cell dimensions); cargo runs tests in one binary
    /// with parallel threads. Mirrors terminal_image.rs's TEST_STATE_LOCK.
    static TEST_STATE_LOCK: Mutex<()> = Mutex::new(());

    const KITTY_CAPS: TerminalCapabilities = TerminalCapabilities {
        images: Some(crate::terminal_image::ImageProtocol::Kitty),
        true_color: true,
        hyperlinks: true,
    };

    // -------------------------------------------------------------------------
    // VirtualTerminal (port of test/virtual-terminal.ts)
    // -------------------------------------------------------------------------

    #[derive(Clone, Default)]
    struct VtCell {
        text: String,
        italic: bool,
        /// Second cell of a wide (CJK) character: emits nothing.
        continuation: bool,
    }

    /// `None` = never written / cleared (xterm null cell).
    type VtLine = Vec<Option<VtCell>>;

    struct VtState {
        cols: usize,
        rows: usize,
        /// Scrollback + screen; the screen is the last `rows` entries.
        /// Invariant: `lines.len() >= rows`.
        lines: Vec<VtLine>,
        /// Screen-relative cursor row.
        cursor_row: usize,
        /// Cursor column; may equal `cols` (pending wrap, like xterm).
        cursor_col: usize,
        italic: bool,
        cursor_hidden: bool,
        /// Every `write` recorded (upstream `LoggingVirtualTerminal`).
        writes: Vec<String>,
        input_handler: Option<InputHandler>,
        resize_handler: Option<ResizeHandler>,
    }

    /// Clonable handle so tests keep access after the TUI takes ownership.
    #[derive(Clone)]
    struct VirtualTerminal {
        state: Arc<Mutex<VtState>>,
    }

    impl VirtualTerminal {
        fn new(columns: usize, rows: usize) -> Self {
            VirtualTerminal {
                state: Arc::new(Mutex::new(VtState {
                    cols: columns,
                    rows,
                    lines: vec![vec![None; columns]; rows],
                    cursor_row: 0,
                    cursor_col: 0,
                    italic: false,
                    cursor_hidden: false,
                    writes: Vec::new(),
                    input_handler: None,
                    resize_handler: None,
                })),
            }
        }

        fn lock(&self) -> MutexGuard<'_, VtState> {
            lock_shared(&self.state)
        }

        /// Simulate keyboard input (upstream `sendInput`).
        fn send_input(&self, data: &str) {
            let handler = self.lock().input_handler.take();
            if let Some(mut handler) = handler {
                handler(data);
                let mut state = self.lock();
                if state.input_handler.is_none() {
                    state.input_handler = Some(handler);
                }
            }
        }

        /// Resize the terminal (upstream `resize`).
        fn resize(&self, columns: usize, rows: usize) {
            {
                let mut state = self.lock();
                state.cols = columns;
                state.rows = rows;
                for line in &mut state.lines {
                    line.resize(columns, None);
                }
                while state.lines.len() < rows {
                    state.lines.push(vec![None; columns]);
                }
                state.cursor_row = state.cursor_row.min(rows - 1);
                state.cursor_col = state.cursor_col.min(columns);
            }
            let handler = self.lock().resize_handler.take();
            if let Some(mut handler) = handler {
                handler();
                let mut state = self.lock();
                if state.resize_handler.is_none() {
                    state.resize_handler = Some(handler);
                }
            }
        }

        /// The visible viewport (upstream `getViewport`).
        fn get_viewport(&self) -> Vec<String> {
            let state = self.lock();
            let screen_top = state.lines.len() - state.rows;
            state.lines[screen_top..]
                .iter()
                .map(translate_line)
                .collect()
        }

        /// The entire scroll buffer (upstream `getScrollBuffer`).
        fn get_scroll_buffer(&self) -> Vec<String> {
            self.lock().lines.iter().map(translate_line).collect()
        }

        /// Cursor position, screen-relative (upstream `getCursorPosition`).
        fn get_cursor_position(&self) -> (usize, usize) {
            let state = self.lock();
            (state.cursor_col, state.cursor_row)
        }

        /// Whether the cursor is hidden (tracks `?25l` / `?25h`).
        fn cursor_hidden(&self) -> bool {
            self.lock().cursor_hidden
        }

        /// xterm `cell.isItalic()` for the style-leak assertions.
        fn cell_italic(&self, row: usize, col: usize) -> bool {
            let state = self.lock();
            let screen_top = state.lines.len() - state.rows;
            state.lines[screen_top + row]
                .get(col)
                .and_then(|cell| cell.as_ref())
                .is_some_and(|cell| cell.italic)
        }

        /// Concatenated recorded writes (upstream `getWrites`).
        fn get_writes(&self) -> String {
            self.lock().writes.concat()
        }

        fn clear_writes(&self) {
            self.lock().writes.clear();
        }
    }

    /// xterm `line.translateToString(true)`: trailing never-written cells are
    /// dropped; written spaces and wide-char continuations are handled.
    fn translate_line(line: &VtLine) -> String {
        let last_written = line.iter().rposition(Option::is_some);
        let Some(last) = last_written else {
            return String::new();
        };
        let mut out = String::new();
        for cell in &line[..=last] {
            match cell {
                None => out.push(' '),
                Some(cell) if cell.continuation => {}
                Some(cell) => out.push_str(&cell.text),
            }
        }
        out
    }

    impl VtState {
        fn blank_line(cols: usize) -> VtLine {
            vec![None; cols]
        }

        fn screen_top(&self) -> usize {
            self.lines.len() - self.rows
        }

        fn abs_row(&self) -> usize {
            self.screen_top() + self.cursor_row
        }

        fn line_feed(&mut self) {
            if self.cursor_row == self.rows - 1 {
                self.lines.push(Self::blank_line(self.cols));
            } else {
                self.cursor_row += 1;
            }
        }

        fn put_char(&mut self, ch: char) {
            let width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if width == 0 {
                // Combining char: merge into the previous cell's text.
                if self.cursor_col > 0 {
                    let abs = self.abs_row();
                    if let Some(Some(cell)) = self.lines[abs].get_mut(self.cursor_col - 1) {
                        if !cell.continuation {
                            cell.text.push(ch);
                        }
                    }
                }
                return;
            }
            if self.cursor_col >= self.cols {
                // Pending wrap: move to the next line first (xterm wraparound).
                self.cursor_col = 0;
                self.line_feed();
            }
            let abs = self.abs_row();
            let col = self.cursor_col;
            let italic = self.italic;
            self.lines[abs][col] = Some(VtCell {
                text: ch.to_string(),
                italic,
                continuation: false,
            });
            if width == 2 && col + 1 < self.cols {
                self.lines[abs][col + 1] = Some(VtCell {
                    text: String::new(),
                    italic,
                    continuation: true,
                });
            }
            self.cursor_col += width;
        }

        fn clear_line_range(&mut self, from: usize, to: usize) {
            let abs = self.abs_row();
            for col in from..=to.min(self.cols - 1) {
                if col < self.cols {
                    self.lines[abs][col] = None;
                }
            }
        }

        fn handle_csi(&mut self, params: &str, final_byte: char) {
            let first_param = || {
                params
                    .split(';')
                    .next()
                    .and_then(|p| p.parse::<usize>().ok())
                    .filter(|&n| n > 0)
                    .unwrap_or(1)
            };
            match final_byte {
                'A' => self.cursor_row = self.cursor_row.saturating_sub(first_param()),
                'B' => self.cursor_row = (self.cursor_row + first_param()).min(self.rows - 1),
                'C' => self.cursor_col = (self.cursor_col + first_param()).min(self.cols - 1),
                'D' => self.cursor_col = self.cursor_col.saturating_sub(first_param()),
                'G' => {
                    self.cursor_col = first_param().saturating_sub(1).min(self.cols - 1);
                }
                'H' => {
                    self.cursor_row = 0;
                    self.cursor_col = 0;
                }
                'J' => match params {
                    "2" => {
                        let top = self.screen_top();
                        let blank = Self::blank_line(self.cols);
                        for line in &mut self.lines[top..] {
                            *line = blank.clone();
                        }
                    }
                    "3" => {
                        // Clear scrollback: keep only the screen lines.
                        let top = self.screen_top();
                        self.lines.drain(..top);
                    }
                    _ => {
                        // Clear from cursor to end of screen.
                        let abs = self.abs_row();
                        for col in self.cursor_col..self.cols {
                            self.lines[abs][col] = None;
                        }
                        let blank = Self::blank_line(self.cols);
                        for row in abs + 1..self.lines.len() {
                            self.lines[row] = blank.clone();
                        }
                    }
                },
                'K' => match params {
                    "2" => {
                        let abs = self.abs_row();
                        self.lines[abs] = Self::blank_line(self.cols);
                    }
                    "1" => {
                        let col = self.cursor_col;
                        self.clear_line_range(0, col);
                    }
                    _ => {
                        let col = self.cursor_col;
                        self.clear_line_range(col, self.cols - 1);
                    }
                },
                'm' => {
                    for param in params.split(';') {
                        match param {
                            "" | "0" => self.italic = false,
                            "3" => self.italic = true,
                            "23" => self.italic = false,
                            _ => {}
                        }
                    }
                }
                'h' | 'l' if params == "?25" => {
                    self.cursor_hidden = final_byte == 'l';
                }
                _ => {}
            }
        }

        /// Feed output into the emulator (and record it).
        fn write_raw(&mut self, data: &str) {
            self.writes.push(data.to_string());
            let chars: Vec<char> = data.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                match chars[i] {
                    '\x1b' => match chars.get(i + 1) {
                        Some('[') => {
                            let mut j = i + 2;
                            let mut params = String::new();
                            while j < chars.len() {
                                let ch = chars[j];
                                if ('\x40'..='\x7e').contains(&ch) {
                                    break;
                                }
                                params.push(ch);
                                j += 1;
                            }
                            if let Some(&final_byte) = chars.get(j) {
                                self.handle_csi(&params, final_byte);
                                j += 1;
                            }
                            i = j;
                        }
                        // OSC / APC / DCS / SOS / PM: skip to BEL or ST.
                        Some(']') | Some('_') | Some('P') | Some('X') | Some('^') => {
                            let mut j = i + 2;
                            while j < chars.len() {
                                if chars[j] == '\x07' {
                                    j += 1;
                                    break;
                                }
                                if chars[j] == '\x1b' && chars.get(j + 1) == Some(&'\\') {
                                    j += 2;
                                    break;
                                }
                                j += 1;
                            }
                            i = j;
                        }
                        Some(_) => i += 2,
                        None => i += 1,
                    },
                    '\r' => {
                        self.cursor_col = 0;
                        i += 1;
                    }
                    '\n' => {
                        self.line_feed();
                        i += 1;
                    }
                    '\t' => {
                        let next_tab_stop = (self.cursor_col / 8 + 1) * 8;
                        while self.cursor_col < next_tab_stop.min(self.cols) {
                            self.put_char(' ');
                        }
                        i += 1;
                    }
                    ch if (ch as u32) < 0x20 => i += 1,
                    ch => {
                        self.put_char(ch);
                        i += 1;
                    }
                }
            }
        }
    }

    impl Terminal for VirtualTerminal {
        fn start(&mut self, on_input: InputHandler, on_resize: ResizeHandler) {
            let mut state = self.lock();
            state.input_handler = Some(on_input);
            state.resize_handler = Some(on_resize);
            // Enable bracketed paste mode for consistency with ProcessTerminal.
            state.write_raw("\x1b[?2004h");
        }

        fn stop(&mut self) {
            let mut state = self.lock();
            state.write_raw("\x1b[?2004l");
            state.input_handler = None;
            state.resize_handler = None;
        }

        fn drain_input(
            &mut self,
            _max_ms: Option<u64>,
            _idle_ms: Option<u64>,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            // No-op for the virtual terminal — no stdin to drain.
            Box::pin(async {})
        }

        fn write(&mut self, data: &str) {
            self.lock().write_raw(data);
        }

        fn columns(&self) -> u16 {
            self.lock().cols as u16
        }

        fn rows(&self) -> u16 {
            self.lock().rows as u16
        }

        fn kitty_protocol_active(&self) -> bool {
            // The virtual terminal always reports Kitty protocol as active.
            true
        }

        fn move_by(&mut self, lines: i32) {
            if lines > 0 {
                self.write(&format!("\x1b[{lines}B"));
            } else if lines < 0 {
                self.write(&format!("\x1b[{}A", -lines));
            }
        }

        fn hide_cursor(&mut self) {
            self.write("\x1b[?25l");
        }

        fn show_cursor(&mut self) {
            self.write("\x1b[?25h");
        }

        fn clear_line(&mut self) {
            self.write("\x1b[K");
        }

        fn clear_from_cursor(&mut self) {
            self.write("\x1b[J");
        }

        fn clear_screen(&mut self) {
            self.write("\x1b[2J\x1b[H");
        }

        fn set_title(&mut self, title: &str) {
            self.write(&format!("\x1b]0;{title}\x07"));
        }

        fn set_progress(&mut self, _active: bool) {}
    }

    // -------------------------------------------------------------------------
    // Test helpers
    // -------------------------------------------------------------------------

    /// Upstream `waitForRender` (virtual-terminal.ts:212-217): drive the TUI
    /// until no work is pending, advancing synthetic time in 20ms steps (the
    /// upstream settle delay). Returns the final synthetic instant so throttle
    /// tests can schedule relative to it.
    fn settle(tui: &TuiMainScreen) -> Instant {
        let mut now = Instant::now();
        loop {
            tui.tick(now);
            if !tui.has_pending_work() {
                return now;
            }
            now += Duration::from_millis(20);
        }
    }

    /// Upstream `renderAndFlush`: forced render + settle.
    fn render_and_flush(tui: &TuiMainScreen) {
        tui.request_render(true);
        settle(tui);
    }

    /// `sendInput` + processing tick (input is queued to the inbox and drained
    /// by the tick; see header note on input delivery).
    fn send_input(terminal: &VirtualTerminal, tui: &TuiMainScreen, data: &str) {
        terminal.send_input(data);
        tui.tick(Instant::now());
    }

    /// `TestComponent` (tui-render.test.ts:18-24): render returns shared lines.
    struct TestComponent {
        lines: Arc<Mutex<Vec<String>>>,
    }

    impl Component for TestComponent {
        fn render(&self, _width: usize) -> Vec<String> {
            lock_shared(&self.lines).clone()
        }
    }

    fn test_component(lines: &[&str]) -> (SharedComponent, Arc<Mutex<Vec<String>>>) {
        let shared = Arc::new(Mutex::new(
            lines
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>(),
        ));
        (
            shared_component(TestComponent {
                lines: Arc::clone(&shared),
            }),
            shared,
        )
    }

    fn set_lines(handle: &Arc<Mutex<Vec<String>>>, lines: &[&str]) {
        *lock_shared(handle) = lines.iter().map(|line| line.to_string()).collect();
    }

    /// `EmptyContent` (overlay tests): renders nothing.
    struct EmptyContent;

    impl Component for EmptyContent {
        fn render(&self, _width: usize) -> Vec<String> {
            Vec::new()
        }
    }

    fn empty_content() -> SharedComponent {
        shared_component(EmptyContent)
    }

    /// `FocusableOverlay` (overlay-non-capturing.test.ts:28-46): records
    /// inputs, tracks focus, and supports replacing `handleInput` with a
    /// custom handler (as upstream tests do).
    type TestInputHandler = Box<dyn FnMut(&str) + Send>;

    struct FocusableState {
        focused: bool,
        inputs: Vec<String>,
        handler: Option<TestInputHandler>,
    }

    /// Test-side handle to a `FocusableOverlay`'s state.
    #[derive(Clone)]
    struct FocusableHandle(Arc<Mutex<FocusableState>>);

    impl FocusableHandle {
        fn focused(&self) -> bool {
            lock_shared(&self.0).focused
        }

        fn inputs(&self) -> Vec<String> {
            lock_shared(&self.0).inputs.clone()
        }

        fn set_handler(&self, handler: TestInputHandler) {
            lock_shared(&self.0).handler = Some(handler);
        }
    }

    struct FocusableOverlay {
        lines: Vec<String>,
        state: Arc<Mutex<FocusableState>>,
        wants_key_release: bool,
    }

    impl Component for FocusableOverlay {
        fn render(&self, _width: usize) -> Vec<String> {
            self.lines.clone()
        }

        fn handle_input(&mut self, data: &str) {
            let handler = {
                let mut state = lock_shared(&self.state);
                state.inputs.push(data.to_string());
                state.handler.take()
            };
            if let Some(mut handler) = handler {
                handler(data);
                let mut state = lock_shared(&self.state);
                if state.handler.is_none() {
                    state.handler = Some(handler);
                }
            }
        }

        fn wants_key_release(&self) -> bool {
            self.wants_key_release
        }

        fn as_focusable(&self) -> Option<&dyn Focusable> {
            Some(self)
        }

        fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
            Some(self)
        }
    }

    impl Focusable for FocusableOverlay {
        fn focused(&self) -> bool {
            lock_shared(&self.state).focused
        }

        fn set_focused(&mut self, focused: bool) {
            lock_shared(&self.state).focused = focused;
        }
    }

    fn focusable_overlay(lines: &[&str]) -> (SharedComponent, FocusableHandle) {
        focusable_overlay_with(lines, false)
    }

    fn focusable_overlay_with(
        lines: &[&str],
        wants_key_release: bool,
    ) -> (SharedComponent, FocusableHandle) {
        let state = Arc::new(Mutex::new(FocusableState {
            focused: false,
            inputs: Vec::new(),
            handler: None,
        }));
        (
            shared_component(FocusableOverlay {
                lines: lines.iter().map(|line| line.to_string()).collect(),
                state: Arc::clone(&state),
                wants_key_release,
            }),
            FocusableHandle(state),
        )
    }

    /// `StaticOverlay` (overlay-options.test.ts:7-23): records the width it
    /// was rendered at.
    struct StaticOverlay {
        lines: Vec<String>,
        requested_width: Arc<Mutex<Option<usize>>>,
    }

    impl Component for StaticOverlay {
        fn render(&self, width: usize) -> Vec<String> {
            *lock_shared(&self.requested_width) = Some(width);
            self.lines.clone()
        }
    }

    fn static_overlay(lines: &[&str]) -> (SharedComponent, Arc<Mutex<Option<usize>>>) {
        let requested_width = Arc::new(Mutex::new(None));
        (
            shared_component(StaticOverlay {
                lines: lines.iter().map(|line| line.to_string()).collect(),
                requested_width: Arc::clone(&requested_width),
            }),
            requested_width,
        )
    }

    fn static_overlay_simple(lines: &[&str]) -> SharedComponent {
        static_overlay(lines).0
    }

    /// Container with shared children (upstream tests use `Container` with
    /// `addChild`/`clear`; here children are shared references so the TUI can
    /// still address them individually).
    struct SharedContainer {
        children: Arc<Mutex<Vec<SharedComponent>>>,
    }

    impl Component for SharedContainer {
        fn render(&self, width: usize) -> Vec<String> {
            // Clone the child list first: never hold the children lock while
            // locking child components (lock order is component → children
            // on the dispatch path).
            let children = lock_shared(&self.children).clone();
            let mut lines = Vec::new();
            for child in &children {
                lines.extend(lock_component(child).render(width));
            }
            lines
        }

        fn shared_children(&self) -> Option<Vec<SharedComponent>> {
            Some(lock_shared(&self.children).clone())
        }
    }

    #[derive(Clone)]
    struct ContainerHandle(Arc<Mutex<Vec<SharedComponent>>>);

    impl ContainerHandle {
        fn add_child(&self, component: SharedComponent) {
            lock_shared(&self.0).push(component);
        }

        fn clear(&self) {
            lock_shared(&self.0).clear();
        }
    }

    fn shared_container(children: Vec<SharedComponent>) -> (SharedComponent, ContainerHandle) {
        let shared = Arc::new(Mutex::new(children));
        (
            shared_component(SharedContainer {
                children: Arc::clone(&shared),
            }),
            ContainerHandle(shared),
        )
    }

    // -------------------------------------------------------------------------
    // Env / global-state guards
    // -------------------------------------------------------------------------

    /// Unique temp log directory per TUI instance so debug-log writes never
    /// touch the real `~/.rpi/agent`, even if an env flag leaks across tests.
    fn temp_log_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "rpi-tui-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn new_tui(terminal: &VirtualTerminal) -> TuiMainScreen {
        TuiMainScreen::with_options(Box::new(terminal.clone()), None, Some(temp_log_dir()))
    }

    /// `withEnv` (tui-render.test.ts:43-65): set/unset an env var, restore on drop.
    struct EnvGuard {
        key: &'static str,
        saved: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> EnvGuard {
            let saved = std::env::var(key).ok();
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
            EnvGuard { key, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.saved {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// Sets Kitty image capabilities + cell dimensions, restores on drop
    /// (tui-render.test.ts try/finally blocks).
    struct CapsGuard;

    impl CapsGuard {
        fn kitty(cell_px: f64) -> CapsGuard {
            set_capabilities(KITTY_CAPS);
            set_cell_dimensions(CellDimensions {
                width_px: cell_px,
                height_px: cell_px,
            });
            CapsGuard
        }
    }

    impl Drop for CapsGuard {
        fn drop(&mut self) {
            reset_capabilities_cache();
            set_cell_dimensions(CellDimensions::default());
        }
    }

    fn state_lock() -> MutexGuard<'static, ()> {
        TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    use crate::terminal_image::{allocate_image_id, encode_kitty, KittyEncodeOptions};

    /// Kitty image lines like the upstream `Image` component produces
    /// (image.ts:90-98): the placement sequence followed by `rows - 1`
    /// empty reservation lines.
    fn kitty_image_lines(columns: u32, rows: u32) -> Vec<String> {
        let sequence = encode_kitty(
            "AAAA",
            &KittyEncodeOptions {
                columns: Some(columns),
                rows: Some(rows),
                image_id: Some(allocate_image_id()),
                move_cursor: Some(false),
            },
        );
        let mut lines = vec![sequence];
        for _ in 1..rows {
            lines.push(String::new());
        }
        lines
    }

    // -------------------------------------------------------------------------
    // tui-render.test.ts: "TUI debug logging"
    // -------------------------------------------------------------------------

    #[test]
    fn debug_logging_writes_redraw_logs_to_the_provided_directory() {
        let _lock = state_lock();
        let _env = EnvGuard::set(ENV_DEBUG_REDRAW, Some("1"));
        let log_dir = temp_log_dir();
        let terminal = VirtualTerminal::new(40, 10);
        let tui =
            TuiMainScreen::with_options(Box::new(terminal.clone()), None, Some(log_dir.clone()));
        let (component, lines) = test_component(&["test"]);
        let _ = lines;
        tui.add_child(component);
        tui.start();
        settle(&tui);

        let log = std::fs::read_to_string(log_dir.join("rpi-debug.log"))
            .unwrap_or_else(|err| panic!("missing rpi-debug.log: {err}"));
        assert!(
            log.contains("fullRender: first render"),
            "expected redraw log, got: {log}"
        );
        tui.stop(TuiStopOptions::default());
        let _ = std::fs::remove_dir_all(&log_dir);
    }

    // -------------------------------------------------------------------------
    // tui-render.test.ts: "TUI Kitty image cleanup"
    // -------------------------------------------------------------------------

    #[test]
    fn kitty_clears_reserved_rows_before_drawing_appended_image_placements() {
        let _lock = state_lock();
        let _caps = CapsGuard::kitty(10.0);
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&["before"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);
        terminal.clear_writes();

        let image_lines = kitty_image_lines(2, 2);
        let image_sequence = image_lines[0].clone();
        let mut new_lines = vec!["before".to_string()];
        new_lines.extend(image_lines);
        new_lines.push("after".to_string());
        *lock_shared(&lines) = new_lines;
        tui.request_render(false);
        settle(&tui);

        let writes = terminal.get_writes();
        assert!(
            writes.contains(&format!("\x1b[2K\r\n\x1b[2K\x1b[1A{image_sequence}\x1b[1B")),
            "reserved rows should be cleared before the image placement is drawn"
        );
        assert!(
            !writes.contains(&format!("{image_sequence}\r\n\x1b[2K")),
            "reserved row clears must not run after the image placement is drawn"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn kitty_falls_back_to_full_redraw_when_pre_clear_would_scroll() {
        let _lock = state_lock();
        let _caps = CapsGuard::kitty(10.0);
        let terminal = VirtualTerminal::new(40, 2);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&["before"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);
        let redraws_before_image = tui.full_redraws();
        terminal.clear_writes();

        let image_lines = kitty_image_lines(3, 3);
        let mut new_lines = vec!["before".to_string()];
        new_lines.extend(image_lines);
        new_lines.push("after".to_string());
        *lock_shared(&lines) = new_lines;
        tui.request_render(false);
        settle(&tui);

        assert!(
            tui.full_redraws() > redraws_before_image,
            "unsafe image pre-clear should force a full redraw"
        );
        assert!(
            terminal.get_writes().contains("\x1b[2J"),
            "fallback should clear and fully redraw"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn kitty_reserves_rows_before_drawing_during_full_redraw_fallbacks() {
        let _lock = state_lock();
        let _caps = CapsGuard::kitty(10.0);
        let terminal = VirtualTerminal::new(40, 5);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&["l0", "l1", "l2", "l3", "l4"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);
        let redraws_before_image = tui.full_redraws();
        terminal.clear_writes();

        let image_lines = kitty_image_lines(3, 3);
        let image_sequence = image_lines[0].clone();
        let mut new_lines: Vec<String> = ["l0", "l1", "l2", "l3", "l4"]
            .iter()
            .map(|line| line.to_string())
            .collect();
        new_lines.extend(image_lines);
        new_lines.push("after".to_string());
        *lock_shared(&lines) = new_lines;
        tui.request_render(false);
        settle(&tui);

        let writes = terminal.get_writes();
        assert!(
            tui.full_redraws() > redraws_before_image,
            "scrolling image append should force a full redraw"
        );
        assert!(
            writes.contains(&format!("\r\n\r\n\x1b[2A{image_sequence}\x1b[2B")),
            "full redraw should reserve visible image rows before drawing the placement"
        );
        assert!(
            !writes.contains(&format!("{image_sequence}\r\n\x1b[0m")),
            "full redraw must not write reserved padding rows after drawing the placement"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn kitty_does_not_use_cursor_up_placement_for_images_taller_than_viewport() {
        let _lock = state_lock();
        let _caps = CapsGuard::kitty(10.0);
        let terminal = VirtualTerminal::new(40, 5);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&["before"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);
        terminal.clear_writes();

        let image_lines = kitty_image_lines(6, 6);
        let image_sequence = image_lines[0].clone();
        assert!(
            image_lines.len() > terminal.lock().rows,
            "test image should exceed the viewport height"
        );
        let mut new_lines = vec!["before".to_string()];
        new_lines.extend(image_lines.clone());
        new_lines.push("after".to_string());
        *lock_shared(&lines) = new_lines;
        tui.request_render(true);
        settle(&tui);

        let writes = terminal.get_writes();
        assert!(
            writes.contains(&image_sequence),
            "image placement should be drawn"
        );
        assert!(
            !writes.contains(&format!("\x1b[{}A{image_sequence}", image_lines.len() - 1)),
            "taller-than-viewport images must keep the first-row placement path"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn kitty_deletes_changed_image_ids_before_drawing_moved_placements() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&[]);
        tui.add_child(component);

        let old_image = encode_kitty(
            "AAAA",
            &KittyEncodeOptions {
                columns: Some(2),
                rows: Some(2),
                image_id: Some(42),
                move_cursor: Some(false),
            },
        );
        set_lines(&lines, &["top", &old_image]);
        tui.start();
        settle(&tui);
        terminal.clear_writes();

        let new_image = encode_kitty(
            "BBBB",
            &KittyEncodeOptions {
                columns: Some(2),
                rows: Some(1),
                image_id: Some(42),
                move_cursor: Some(false),
            },
        );
        set_lines(&lines, &[&new_image, ""]);
        tui.request_render(false);
        settle(&tui);

        let writes = terminal.get_writes();
        let delete_index = writes.find(&delete_kitty_image(42));
        let draw_index = writes.find(&new_image);
        assert!(
            delete_index.is_some(),
            "changed old image should be deleted"
        );
        assert!(draw_index.is_some(), "new image should be drawn");
        assert!(
            delete_index < draw_index,
            "old image must be deleted before the new placement is drawn"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn kitty_redraws_image_lines_when_an_earlier_reserved_row_changes() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&[]);
        tui.add_child(component);

        let image = encode_kitty(
            "AAAA",
            &KittyEncodeOptions {
                columns: Some(2),
                rows: Some(2),
                image_id: Some(88),
                move_cursor: Some(false),
            },
        );
        set_lines(&lines, &["", &image]);
        tui.start();
        settle(&tui);
        terminal.clear_writes();

        set_lines(&lines, &["covered", &image]);
        tui.request_render(false);
        settle(&tui);

        let writes = terminal.get_writes();
        let delete_index = writes.find(&delete_kitty_image(88));
        let draw_index = writes.find(&image);
        assert!(
            delete_index.is_some(),
            "image should be deleted when a reserved row changes"
        );
        assert!(
            draw_index.is_some(),
            "unchanged image line should be redrawn after deleting the placement"
        );
        assert!(
            delete_index < draw_index,
            "old placement must be deleted before the image line is redrawn"
        );
        assert!(
            !writes.contains("\x1b[2J"),
            "reserved row changes should not force a full redraw"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn kitty_deletes_previously_rendered_image_ids_during_full_redraws() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&[]);
        tui.add_child(component);

        set_lines(
            &lines,
            &[&encode_kitty(
                "AAAA",
                &KittyEncodeOptions {
                    columns: Some(2),
                    rows: Some(2),
                    image_id: Some(77),
                    move_cursor: Some(false),
                },
            )],
        );
        tui.start();
        settle(&tui);
        terminal.clear_writes();

        set_lines(&lines, &["plain text"]);
        tui.request_render(true);
        settle(&tui);

        let writes = terminal.get_writes();
        let delete_index = writes.find(&delete_kitty_image(77));
        let clear_index = writes.find("\x1b[2J");
        assert!(
            delete_index.is_some(),
            "previous image should be deleted during full redraw"
        );
        assert!(clear_index.is_some(), "full redraw should clear the screen");
        assert!(
            delete_index < clear_index,
            "old image should be deleted before the screen is cleared"
        );
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // tui-render.test.ts: "TUI resize handling"
    // -------------------------------------------------------------------------

    #[test]
    fn resize_triggers_full_rerender_when_terminal_height_changes() {
        let _lock = state_lock();
        let _env = EnvGuard::set("TERMUX_VERSION", None);
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, _lines) = test_component(&["Line 0", "Line 1", "Line 2"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        let initial_redraws = tui.full_redraws();
        terminal.resize(40, 15);
        settle(&tui);

        assert!(
            tui.full_redraws() > initial_redraws,
            "height change should trigger full redraw"
        );
        let viewport = terminal.get_viewport();
        assert!(
            viewport[0].contains("Line 0"),
            "content preserved after height change"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn resize_skips_full_rerender_on_height_changes_in_termux() {
        let _lock = state_lock();
        let _env = EnvGuard::set("TERMUX_VERSION", Some("1"));
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let lines: Vec<String> = (0..20).map(|i| format!("Line {i}")).collect();
        let (component, shared) = test_component(&[]);
        *lock_shared(&shared) = lines;
        tui.add_child(component);
        tui.start();
        settle(&tui);
        terminal.clear_writes();

        let initial_redraws = tui.full_redraws();
        for height in [15usize, 8, 14, 11] {
            terminal.resize(40, height);
            settle(&tui);
        }

        assert_eq!(
            tui.full_redraws(),
            initial_redraws,
            "height change should not trigger full redraw"
        );
        let writes = terminal.get_writes();
        assert!(
            !writes.contains("\x1b[2J"),
            "height change should not clear the screen"
        );
        assert!(
            !writes.contains("\x1b[3J"),
            "height change should not clear scrollback"
        );

        let viewport = terminal.get_viewport();
        assert!(
            viewport.join("\n").contains("Line 19"),
            "latest content remains visible after resize"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn resize_triggers_full_rerender_when_terminal_width_changes() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, _lines) = test_component(&["Line 0", "Line 1", "Line 2"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        let initial_redraws = tui.full_redraws();
        terminal.resize(60, 10);
        settle(&tui);

        assert!(
            tui.full_redraws() > initial_redraws,
            "width change should trigger full redraw"
        );
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // tui-render.test.ts: "TUI content shrinkage"
    // -------------------------------------------------------------------------

    #[test]
    fn shrink_clears_empty_rows_when_content_shrinks_significantly() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        tui.set_clear_on_shrink(true);
        let (component, lines) =
            test_component(&["Line 0", "Line 1", "Line 2", "Line 3", "Line 4", "Line 5"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        let initial_redraws = tui.full_redraws();
        set_lines(&lines, &["Line 0", "Line 1"]);
        tui.request_render(false);
        settle(&tui);

        assert!(
            tui.full_redraws() > initial_redraws,
            "content shrinkage should trigger full redraw"
        );
        let viewport = terminal.get_viewport();
        assert!(viewport[0].contains("Line 0"), "first line preserved");
        assert!(viewport[1].contains("Line 1"), "second line preserved");
        assert_eq!(viewport[2].trim(), "", "line 2 should be cleared");
        assert_eq!(viewport[3].trim(), "", "line 3 should be cleared");
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn shrink_handles_shrink_to_single_line() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        tui.set_clear_on_shrink(true);
        let (component, lines) = test_component(&["Line 0", "Line 1", "Line 2", "Line 3"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        set_lines(&lines, &["Only line"]);
        tui.request_render(false);
        settle(&tui);

        let viewport = terminal.get_viewport();
        assert!(viewport[0].contains("Only line"), "single line rendered");
        assert_eq!(viewport[1].trim(), "", "line 1 should be cleared");
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn shrink_handles_shrink_to_empty() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        tui.set_clear_on_shrink(true);
        let (component, lines) = test_component(&["Line 0", "Line 1", "Line 2"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        set_lines(&lines, &[]);
        tui.request_render(false);
        settle(&tui);

        let viewport = terminal.get_viewport();
        assert_eq!(viewport[0].trim(), "", "line 0 should be cleared");
        assert_eq!(viewport[1].trim(), "", "line 1 should be cleared");
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // tui-render.test.ts: "TUI differential rendering"
    // -------------------------------------------------------------------------

    #[test]
    fn diff_tracks_cursor_when_content_shrinks_with_unchanged_remaining_lines() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) =
            test_component(&["Line 0", "Line 1", "Line 2", "Line 3", "Line 4"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        set_lines(&lines, &["Line 0", "Line 1", "Line 2"]);
        tui.request_render(false);
        settle(&tui);

        set_lines(&lines, &["Line 0", "CHANGED", "Line 2"]);
        tui.request_render(false);
        settle(&tui);

        let viewport = terminal.get_viewport();
        assert!(
            viewport[1].contains("CHANGED"),
            "expected CHANGED on line 1, got: {}",
            viewport[1]
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn diff_renders_correctly_when_only_a_middle_line_changes_spinner_case() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&["Header", "Working...", "Footer"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        for frame in ["|", "/", "-", "\\"] {
            set_lines(&lines, &["Header", &format!("Working {frame}"), "Footer"]);
            tui.request_render(false);
            settle(&tui);

            let viewport = terminal.get_viewport();
            assert!(
                viewport[0].contains("Header"),
                "header preserved: {}",
                viewport[0]
            );
            assert!(
                viewport[1].contains(&format!("Working {frame}")),
                "spinner updated: {}",
                viewport[1]
            );
            assert!(
                viewport[2].contains("Footer"),
                "footer preserved: {}",
                viewport[2]
            );
        }
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn diff_resets_styles_after_each_rendered_line() {
        let terminal = VirtualTerminal::new(20, 6);
        let tui = new_tui(&terminal);
        let (component, _lines) = test_component(&["\x1b[3mItalic", "Plain"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        assert!(!terminal.cell_italic(1, 0));
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn diff_renders_correctly_when_first_line_changes_but_rest_stays_same() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&["Line 0", "Line 1", "Line 2", "Line 3"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        set_lines(&lines, &["CHANGED", "Line 1", "Line 2", "Line 3"]);
        tui.request_render(false);
        settle(&tui);

        let viewport = terminal.get_viewport();
        assert!(viewport[0].contains("CHANGED"));
        assert!(viewport[1].contains("Line 1"));
        assert!(viewport[2].contains("Line 2"));
        assert!(viewport[3].contains("Line 3"));
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn diff_renders_correctly_when_last_line_changes_but_rest_stays_same() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&["Line 0", "Line 1", "Line 2", "Line 3"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        set_lines(&lines, &["Line 0", "Line 1", "Line 2", "CHANGED"]);
        tui.request_render(false);
        settle(&tui);

        let viewport = terminal.get_viewport();
        assert!(viewport[0].contains("Line 0"));
        assert!(viewport[1].contains("Line 1"));
        assert!(viewport[2].contains("Line 2"));
        assert!(viewport[3].contains("CHANGED"));
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn diff_renders_correctly_when_multiple_non_adjacent_lines_change() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) =
            test_component(&["Line 0", "Line 1", "Line 2", "Line 3", "Line 4"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        set_lines(
            &lines,
            &["Line 0", "CHANGED 1", "Line 2", "CHANGED 3", "Line 4"],
        );
        tui.request_render(false);
        settle(&tui);

        let viewport = terminal.get_viewport();
        assert!(viewport[0].contains("Line 0"));
        assert!(viewport[1].contains("CHANGED 1"));
        assert!(viewport[2].contains("Line 2"));
        assert!(viewport[3].contains("CHANGED 3"));
        assert!(viewport[4].contains("Line 4"));
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn diff_handles_transition_from_content_to_empty_and_back_to_content() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&["Line 0", "Line 1", "Line 2"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        let viewport = terminal.get_viewport();
        assert!(viewport[0].contains("Line 0"), "initial content rendered");

        set_lines(&lines, &[]);
        tui.request_render(false);
        settle(&tui);

        set_lines(&lines, &["New Line 0", "New Line 1"]);
        tui.request_render(false);
        settle(&tui);

        let viewport = terminal.get_viewport();
        assert!(
            viewport[0].contains("New Line 0"),
            "new content rendered: {}",
            viewport[0]
        );
        assert!(
            viewport[1].contains("New Line 1"),
            "new content line 1: {}",
            viewport[1]
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn diff_full_rerenders_when_deleted_lines_move_the_viewport_upward() {
        let terminal = VirtualTerminal::new(20, 5);
        let tui = new_tui(&terminal);
        let lines: Vec<String> = (0..12).map(|i| format!("Line {i}")).collect();
        let (component, shared) = test_component(&[]);
        *lock_shared(&shared) = lines;
        tui.add_child(component);
        tui.start();
        settle(&tui);

        let initial_redraws = tui.full_redraws();
        *lock_shared(&shared) = (0..7).map(|i| format!("Line {i}")).collect();
        tui.request_render(false);
        settle(&tui);

        assert!(
            tui.full_redraws() > initial_redraws,
            "shrink should trigger a full redraw"
        );
        assert_eq!(
            terminal.get_viewport(),
            vec!["Line 2", "Line 3", "Line 4", "Line 5", "Line 6"]
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn diff_appends_after_a_shrink_without_another_full_redraw() {
        let terminal = VirtualTerminal::new(20, 5);
        let tui = new_tui(&terminal);
        let lines: Vec<String> = (0..8).map(|i| format!("Line {i}")).collect();
        let (component, shared) = test_component(&[]);
        *lock_shared(&shared) = lines;
        tui.add_child(component);
        tui.start();
        settle(&tui);

        let initial_redraws = tui.full_redraws();
        set_lines(&shared, &["Line 0", "Line 1"]);
        tui.request_render(false);
        settle(&tui);

        assert!(
            tui.full_redraws() > initial_redraws,
            "shrink should reset the viewport with a full redraw"
        );
        let redraws_after_shrink = tui.full_redraws();

        set_lines(&shared, &["Line 0", "Line 1", "Line 2"]);
        tui.request_render(false);
        settle(&tui);

        assert_eq!(
            tui.full_redraws(),
            redraws_after_shrink,
            "append should stay on the differential path"
        );
        assert_eq!(
            terminal.get_viewport(),
            vec!["Line 0", "Line 1", "Line 2", "", ""]
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn diff_clears_stale_content_when_max_lines_rendered_was_inflated() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (chat, chat_lines) = test_component(&[]);
        let (editor, editor_lines) = test_component(&[]);
        tui.add_child(chat);
        tui.add_child(editor);

        let editor_content = ["Editor 0", "Editor 1", "Editor 2"];
        *lock_shared(&chat_lines) = (0..15).map(|i| format!("Chat {i}")).collect();
        set_lines(&editor_lines, &editor_content);
        tui.start();
        settle(&tui);

        *lock_shared(&editor_lines) = (0..8).map(|i| format!("Selector {i}")).collect();
        tui.request_render(false);
        settle(&tui);

        set_lines(&editor_lines, &editor_content);
        tui.request_render(false);
        settle(&tui);

        let redraws_before_switch = tui.full_redraws();
        *lock_shared(&chat_lines) = (0..12).map(|i| format!("Chat {i}")).collect();
        tui.request_render(false);
        settle(&tui);

        assert!(
            tui.full_redraws() > redraws_before_switch,
            "branch switch should trigger a full redraw"
        );

        let viewport = terminal.get_viewport();
        for (i, line) in viewport.iter().enumerate().take(10) {
            assert!(
                !line.contains("Chat 12"),
                "stale Chat 12 at viewport row {i}"
            );
            assert!(
                !line.contains("Chat 13"),
                "stale Chat 13 at viewport row {i}"
            );
            assert!(
                !line.contains("Chat 14"),
                "stale Chat 14 at viewport row {i}"
            );
        }
        assert_eq!(
            viewport,
            vec![
                "Chat 5", "Chat 6", "Chat 7", "Chat 8", "Chat 9", "Chat 10", "Chat 11", "Editor 0",
                "Editor 1", "Editor 2",
            ]
        );
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // overlay-options.test.ts
    // -------------------------------------------------------------------------

    fn overlay_viewport(terminal: &VirtualTerminal, tui: &TuiMainScreen) -> Vec<String> {
        tui.start();
        render_and_flush(tui);
        terminal.get_viewport()
    }

    // ---- "width overflow protection" ----

    #[test]
    fn overlay_truncates_lines_that_exceed_declared_width() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let long_line = "X".repeat(100);
        let (overlay, _w) = static_overlay(&[&long_line]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                width: Some(SizeValue::Absolute(20)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        // Must not crash; no viewport line may exceed the terminal width.
        assert_eq!(viewport.len(), 24);
        for line in &viewport {
            assert!(
                visible_width(line) <= 80,
                "line exceeds terminal width: {line:?}"
            );
        }
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_handles_complex_ansi_sequences_without_crashing() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let complex_line = format!(
            "\x1b[48;2;40;50;40m \x1b[38;2;128;128;128mSome styled content\x1b[39m\x1b[49m\x1b]8;;http://example.com\x07link\x1b]8;;\x07{}",
            " more content ".repeat(10)
        );
        let (overlay, _w) = static_overlay(&[&complex_line, &complex_line, &complex_line]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                width: Some(SizeValue::Absolute(60)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(!viewport.is_empty());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_handles_compositing_on_styled_base_content() {
        struct StyledContent;
        impl Component for StyledContent {
            fn render(&self, width: usize) -> Vec<String> {
                (0..3)
                    .map(|_| format!("\x1b[1m\x1b[38;2;255;0;0m{}\x1b[0m", "X".repeat(width)))
                    .collect()
            }
        }

        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(shared_component(StyledContent));
        let (overlay, _w) = static_overlay(&["OVERLAY"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                width: Some(SizeValue::Absolute(20)),
                anchor: Some(OverlayAnchor::Center),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(
            viewport.iter().any(|line| line.contains("OVERLAY")),
            "overlay should be visible"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_handles_wide_characters_at_boundary() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["中文日本語한글テスト漢字"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                // Odd width to potentially hit a boundary.
                width: Some(SizeValue::Absolute(15)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(!viewport.is_empty());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_handles_positioning_at_terminal_edge() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let long_line = "X".repeat(50);
        let (overlay, _w) = static_overlay(&[&long_line]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                col: Some(SizeValue::Absolute(60)),
                width: Some(SizeValue::Absolute(20)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(!viewport.is_empty());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_handles_base_content_with_osc_sequences() {
        struct HyperlinkContent;
        impl Component for HyperlinkContent {
            fn render(&self, width: usize) -> Vec<String> {
                let link = "\x1b]8;;file:///path/to/file.ts\x07file.ts\x1b]8;;\x07";
                let line = format!("See {link} for details {}", "X".repeat(width - 30));
                (0..3).map(|_| line.clone()).collect()
            }
        }

        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(shared_component(HyperlinkContent));
        let (overlay, _w) = static_overlay(&["OVERLAY-TEXT"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::Center),
                width: Some(SizeValue::Absolute(20)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(!viewport.is_empty());
        tui.stop(TuiStopOptions::default());
    }

    // ---- "width percentage" ----

    #[test]
    fn overlay_renders_at_percentage_of_terminal_width() {
        let terminal = VirtualTerminal::new(100, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, requested_width) = static_overlay(&["test"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                width: Some(SizeValue::Percent(50.0)),
                ..Default::default()
            }),
        );
        overlay_viewport(&terminal, &tui);
        assert_eq!(*lock_shared(&requested_width), Some(50));
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_respects_min_width_when_percentage_results_in_smaller_width() {
        let terminal = VirtualTerminal::new(100, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, requested_width) = static_overlay(&["test"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                width: Some(SizeValue::Percent(10.0)),
                min_width: Some(30),
                ..Default::default()
            }),
        );
        overlay_viewport(&terminal, &tui);
        assert_eq!(*lock_shared(&requested_width), Some(30));
        tui.stop(TuiStopOptions::default());
    }

    // ---- "anchor positioning" ----

    #[test]
    fn overlay_positions_at_top_left() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["TOP-LEFT"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::TopLeft),
                width: Some(SizeValue::Absolute(10)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(
            viewport[0].starts_with("TOP-LEFT"),
            "expected TOP-LEFT at start, got: {}",
            viewport[0]
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_positions_at_bottom_right() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["BTM-RIGHT"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::BottomRight),
                width: Some(SizeValue::Absolute(10)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        let last_row = &viewport[23];
        assert!(
            last_row.contains("BTM-RIGHT"),
            "expected BTM-RIGHT on last row: {last_row}"
        );
        assert!(
            last_row.trim_end().ends_with("BTM-RIGHT"),
            "expected BTM-RIGHT at end of last row: {last_row}"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_positions_at_top_center() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["CENTERED"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::TopCenter),
                width: Some(SizeValue::Absolute(10)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        let first_row = &viewport[0];
        assert!(first_row.contains("CENTERED"));
        let col_index = first_row.find("CENTERED").map(|i| i as i32).unwrap_or(-1);
        assert!(
            (30..=40).contains(&col_index),
            "expected centered, got col {col_index}"
        );
        tui.stop(TuiStopOptions::default());
    }

    // ---- "margin" ----

    #[test]
    fn overlay_clamps_negative_margins_to_zero() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["NEG-MARGIN"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::TopLeft),
                width: Some(SizeValue::Absolute(12)),
                margin: Some(OverlayMarginSpec::Edges(OverlayMargin {
                    top: Some(-5),
                    left: Some(-10),
                    right: Some(0),
                    bottom: Some(0),
                })),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(
            viewport[0].starts_with("NEG-MARGIN"),
            "expected NEG-MARGIN at start of row 0, got: {}",
            viewport[0]
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_respects_margin_as_number() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["MARGIN"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::TopLeft),
                width: Some(SizeValue::Absolute(10)),
                margin: Some(OverlayMarginSpec::Uniform(5)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(!viewport[0].contains("MARGIN"), "should not be on row 0");
        assert!(!viewport[4].contains("MARGIN"), "should not be on row 4");
        assert!(
            viewport[5].contains("MARGIN"),
            "expected MARGIN on row 5, got: {}",
            viewport[5]
        );
        assert_eq!(
            viewport[5].find("MARGIN").map(|i| i as i32).unwrap_or(-1),
            5,
            "expected col 5"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_respects_margin_object() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["MARGIN"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::TopLeft),
                width: Some(SizeValue::Absolute(10)),
                margin: Some(OverlayMarginSpec::Edges(OverlayMargin {
                    top: Some(2),
                    left: Some(3),
                    right: Some(0),
                    bottom: Some(0),
                })),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(viewport[2].contains("MARGIN"));
        assert_eq!(
            viewport[2].find("MARGIN").map(|i| i as i32).unwrap_or(-1),
            3,
            "expected col 3"
        );
        tui.stop(TuiStopOptions::default());
    }

    // ---- "offset" ----

    #[test]
    fn overlay_applies_offset_x_and_offset_y_from_anchor_position() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["OFFSET"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::TopLeft),
                width: Some(SizeValue::Absolute(10)),
                offset_x: Some(10),
                offset_y: Some(5),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(viewport[5].contains("OFFSET"));
        assert_eq!(
            viewport[5].find("OFFSET").map(|i| i as i32).unwrap_or(-1),
            10,
            "expected col 10"
        );
        tui.stop(TuiStopOptions::default());
    }

    // ---- "percentage positioning" ----

    #[test]
    fn overlay_positions_with_row_and_col_percent() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["PCT"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                width: Some(SizeValue::Absolute(10)),
                row: Some(SizeValue::Percent(50.0)),
                col: Some(SizeValue::Percent(50.0)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        let found_row = viewport
            .iter()
            .position(|line| line.contains("PCT"))
            .map(|i| i as i32)
            .unwrap_or(-1);
        assert!(
            (10..=13).contains(&found_row),
            "expected centered row, got {found_row}"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_row_percent_0_positions_at_top() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["TOP"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                width: Some(SizeValue::Absolute(10)),
                row: Some(SizeValue::Percent(0.0)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(
            viewport[0].contains("TOP"),
            "expected TOP on row 0, got: {}",
            viewport[0]
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_row_percent_100_positions_at_bottom() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["BOTTOM"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                width: Some(SizeValue::Absolute(10)),
                row: Some(SizeValue::Percent(100.0)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(
            viewport[23].contains("BOTTOM"),
            "expected BOTTOM on last row, got: {}",
            viewport[23]
        );
        tui.stop(TuiStopOptions::default());
    }

    // ---- "maxHeight" ----

    #[test]
    fn overlay_truncates_to_max_height() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["Line 1", "Line 2", "Line 3", "Line 4", "Line 5"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                max_height: Some(SizeValue::Absolute(3)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        let content = viewport.join("\n");
        assert!(content.contains("Line 1"), "should include Line 1");
        assert!(content.contains("Line 2"), "should include Line 2");
        assert!(content.contains("Line 3"), "should include Line 3");
        assert!(!content.contains("Line 4"), "should NOT include Line 4");
        assert!(!content.contains("Line 5"), "should NOT include Line 5");
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_truncates_to_max_height_percent() {
        let terminal = VirtualTerminal::new(80, 10);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) =
            static_overlay(&["L1", "L2", "L3", "L4", "L5", "L6", "L7", "L8", "L9", "L10"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                max_height: Some(SizeValue::Percent(50.0)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        let content = viewport.join("\n");
        assert!(content.contains("L1"), "should include L1");
        assert!(content.contains("L5"), "should include L5");
        assert!(!content.contains("L6"), "should NOT include L6");
        tui.stop(TuiStopOptions::default());
    }

    // ---- "absolute positioning" ----

    #[test]
    fn overlay_row_and_col_override_anchor() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay, _w) = static_overlay(&["ABSOLUTE"]);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::BottomRight),
                row: Some(SizeValue::Absolute(3)),
                col: Some(SizeValue::Absolute(5)),
                width: Some(SizeValue::Absolute(10)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(viewport[3].contains("ABSOLUTE"));
        assert_eq!(
            viewport[3].find("ABSOLUTE").map(|i| i as i32).unwrap_or(-1),
            5,
            "expected col 5"
        );
        tui.stop(TuiStopOptions::default());
    }

    // ---- "stacked overlays" ----

    #[test]
    fn overlay_renders_multiple_with_later_ones_on_top() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay1, _w) = static_overlay(&["FIRST-OVERLAY"]);
        tui.show_overlay(
            overlay1,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::TopLeft),
                width: Some(SizeValue::Absolute(20)),
                ..Default::default()
            }),
        );
        let (overlay2, _w) = static_overlay(&["SECOND"]);
        tui.show_overlay(
            overlay2,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::TopLeft),
                width: Some(SizeValue::Absolute(10)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(
            viewport[0].contains("SECOND"),
            "expected SECOND on row 0, got: {}",
            viewport[0]
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_handles_different_positions_without_interference() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay1, _w) = static_overlay(&["TOP-LEFT"]);
        tui.show_overlay(
            overlay1,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::TopLeft),
                width: Some(SizeValue::Absolute(15)),
                ..Default::default()
            }),
        );
        let (overlay2, _w) = static_overlay(&["BTM-RIGHT"]);
        tui.show_overlay(
            overlay2,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::BottomRight),
                width: Some(SizeValue::Absolute(15)),
                ..Default::default()
            }),
        );
        let viewport = overlay_viewport(&terminal, &tui);
        assert!(viewport[0].contains("TOP-LEFT"));
        assert!(viewport[23].contains("BTM-RIGHT"));
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_hides_in_stack_order() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let (overlay1, _w) = static_overlay(&["FIRST"]);
        tui.show_overlay(
            overlay1,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::TopLeft),
                width: Some(SizeValue::Absolute(10)),
                ..Default::default()
            }),
        );
        let (overlay2, _w) = static_overlay(&["SECOND"]);
        tui.show_overlay(
            overlay2,
            Some(OverlayOptions {
                anchor: Some(OverlayAnchor::TopLeft),
                width: Some(SizeValue::Absolute(10)),
                ..Default::default()
            }),
        );

        tui.start();
        render_and_flush(&tui);
        assert!(
            terminal.get_viewport()[0].contains("SECOND"),
            "SECOND should be visible initially"
        );

        tui.hide_overlay();
        render_and_flush(&tui);
        assert!(
            terminal.get_viewport()[0].contains("FIRST"),
            "FIRST should be visible after hiding SECOND"
        );
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // overlay-non-capturing.test.ts: "focus management"
    // -------------------------------------------------------------------------

    #[test]
    fn non_capturing_overlay_preserves_focus_on_creation() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        render_and_flush(&tui);
        assert!(editor_h.focused());
        assert!(!overlay_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_handle_focus_transfers_focus_to_the_overlay() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let handle = tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        handle.focus();
        render_and_flush(&tui);
        assert!(!editor_h.focused());
        assert!(overlay_h.focused());
        assert!(handle.is_focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn overlay_handle_unfocus_restores_previous_focus() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let handle = tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        handle.focus();
        handle.unfocus(None);
        render_and_flush(&tui);
        assert!(editor_h.focused());
        assert!(!overlay_h.focused());
        assert!(!handle.is_focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn set_hidden_false_on_non_capturing_overlay_does_not_auto_focus() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let handle = tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        handle.set_hidden(true);
        handle.set_hidden(false);
        render_and_flush(&tui);
        assert!(editor_h.focused());
        assert!(!overlay_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn hide_when_overlay_is_not_focused_does_not_change_focus() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, _overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let handle = tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        handle.hide();
        render_and_flush(&tui);
        assert!(editor_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn hide_when_focused_restores_focus_correctly() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let handle = tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        handle.focus();
        handle.hide();
        render_and_flush(&tui);
        assert!(editor_h.focused());
        assert!(!overlay_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn capturing_overlay_removed_with_non_capturing_below_restores_focus_to_editor() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (non_capturing, non_capturing_h) = focusable_overlay(&["NC"]);
        let (capturing, capturing_h) = focusable_overlay(&["CAP"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        tui.show_overlay(
            non_capturing,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        let handle = tui.show_overlay(capturing, None);
        assert!(capturing_h.focused());
        handle.hide();
        render_and_flush(&tui);
        assert!(editor_h.focused());
        assert!(!non_capturing_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn sub_overlay_cleanup_then_hide_overlay_restores_focus_and_input_to_editor() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (timer, timer_h) = focusable_overlay(&["TIMER"]);
        let (controller, controller_h) = focusable_overlay(&["CONTROLLER"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let timer_handle = tui.show_overlay(
            timer,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        tui.show_overlay(controller, None);
        assert!(controller_h.focused());
        assert!(!editor_h.focused());
        timer_handle.hide();
        tui.hide_overlay();
        render_and_flush(&tui);
        assert!(editor_h.focused());
        assert!(!controller_h.focused());
        assert!(!timer_h.focused());
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(editor_h.inputs(), vec!["x"]);
        assert_eq!(controller_h.inputs(), Vec::<String>::new());
        assert_eq!(timer_h.inputs(), Vec::<String>::new());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn removed_focused_child_overlay_does_not_become_parent_overlay_fallback() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (child, child_h) = focusable_overlay(&["CHILD"]);
        let (parent, parent_h) = focusable_overlay(&["PARENT"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let child_handle = tui.show_overlay(
            child,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        child_handle.focus();
        let parent_handle = tui.show_overlay(parent, None);
        assert!(parent_h.focused());
        child_handle.hide();
        parent_handle.hide();
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(editor_h.inputs(), vec!["x"]);
        assert_eq!(child_h.inputs(), Vec::<String>::new());
        assert_eq!(parent_h.inputs(), Vec::<String>::new());
        assert!(editor_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn microtask_deferred_sub_overlay_pattern_restores_focus() {
        // Simulates showExtensionCustom: the factory creates the timer
        // synchronously, then a microtask pushes the controller overlay.
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (timer, timer_h) = focusable_overlay(&["TIMER"]);
        let (controller, controller_h) = focusable_overlay(&["CONTROLLER"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let timer_handle = tui.show_overlay(
            timer,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        // Upstream runs this in a `.then()` microtask; sequential here.
        tui.show_overlay(controller, None);
        render_and_flush(&tui);
        assert!(controller_h.focused());
        assert!(!editor_h.focused());
        // Simulate Esc: cleanup + close (from inside handleInput upstream).
        timer_handle.hide();
        tui.hide_overlay();
        render_and_flush(&tui);
        assert!(editor_h.focused(), "editor should regain focus");
        assert!(!controller_h.focused());
        assert!(!timer_h.focused());
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(
            editor_h.inputs(),
            vec!["x"],
            "editor should receive input after close"
        );
        assert_eq!(controller_h.inputs(), Vec::<String>::new());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn handle_input_redirection_skips_non_capturing_overlays_when_focused_becomes_invisible() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, _editor_h) = focusable_overlay(&["EDITOR"]);
        let (fallback_capturing, fallback_h) = focusable_overlay(&["FALLBACK"]);
        let (non_capturing, non_capturing_h) = focusable_overlay(&["NC"]);
        let (primary, primary_h) = focusable_overlay(&["PRIMARY"]);
        let is_visible = Arc::new(AtomicBool::new(true));
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        tui.show_overlay(fallback_capturing, None);
        tui.show_overlay(
            non_capturing,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        let visible_flag = Arc::clone(&is_visible);
        tui.show_overlay(
            primary,
            Some(OverlayOptions {
                visible: Some(Box::new(move |_, _| visible_flag.load(Ordering::Relaxed))),
                ..Default::default()
            }),
        );
        assert!(primary_h.focused());
        is_visible.store(false, Ordering::Relaxed);
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(primary_h.inputs(), Vec::<String>::new());
        assert_eq!(non_capturing_h.inputs(), Vec::<String>::new());
        assert_eq!(fallback_h.inputs(), vec!["x"]);
        assert!(fallback_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn active_base_focus_replacement_receives_close_input_before_overlay_restore() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, _editor_h) = focusable_overlay(&["EDITOR"]);
        let (replacement, replacement_h) = focusable_overlay(&["REPLACEMENT"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        {
            let tui = tui.clone();
            let replacement = replacement.clone();
            overlay_h.set_handler(Box::new(move |data| {
                if data == "b" {
                    tui.set_focus(Some(replacement.clone()));
                }
            }));
        }
        {
            let tui = tui.clone();
            replacement_h.set_handler(Box::new(move |data| {
                if data == "\r" {
                    tui.set_focus(Some(editor.clone()));
                }
            }));
        }
        tui.show_overlay(overlay, None);
        assert!(overlay_h.focused());
        send_input(&terminal, &tui, "b");
        render_and_flush(&tui);
        assert!(replacement_h.focused());
        send_input(&terminal, &tui, "\r");
        render_and_flush(&tui);
        assert_eq!(replacement_h.inputs(), vec!["\r"]);
        assert_eq!(overlay_h.inputs(), vec!["b"]);
        assert!(overlay_h.focused());
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(overlay_h.inputs(), vec!["b", "x"]);
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn active_replacement_still_receives_input_when_it_is_another_overlay_pre_focus() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, _editor_h) = focusable_overlay(&["EDITOR"]);
        let (replacement, replacement_h) = focusable_overlay(&["REPLACEMENT"]);
        let (passive, _passive_h) = focusable_overlay(&["PASSIVE"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        {
            let tui = tui.clone();
            let replacement = replacement.clone();
            overlay_h.set_handler(Box::new(move |data| {
                if data == "b" {
                    tui.set_focus(Some(replacement.clone()));
                }
            }));
        }
        {
            let tui = tui.clone();
            let editor = editor.clone();
            replacement_h.set_handler(Box::new(move |data| {
                if data == "\r" {
                    tui.set_focus(Some(editor.clone()));
                }
            }));
        }
        // Pre-show dance: make the replacement another overlay's preFocus.
        tui.set_focus(Some(replacement.clone()));
        tui.show_overlay(
            passive,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        tui.set_focus(Some(editor.clone()));
        tui.show_overlay(overlay, None);
        send_input(&terminal, &tui, "b");
        render_and_flush(&tui);
        assert!(replacement_h.focused());
        send_input(&terminal, &tui, "1");
        send_input(&terminal, &tui, "\r");
        render_and_flush(&tui);
        assert_eq!(replacement_h.inputs(), vec!["1", "\r"]);
        assert_eq!(overlay_h.inputs(), vec!["b"]);
        assert!(overlay_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn blocked_replacement_can_move_focus_internally_before_overlay_restore() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, _editor_h) = focusable_overlay(&["EDITOR"]);
        let (first_replacement, first_h) = focusable_overlay(&["FIRST"]);
        let (second_replacement, second_h) = focusable_overlay(&["SECOND"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        let (base, base_h) = shared_container(vec![
            editor.clone(),
            first_replacement.clone(),
            second_replacement.clone(),
        ]);
        tui.add_child(base);
        tui.set_focus(Some(editor.clone()));
        tui.start();
        {
            let tui = tui.clone();
            let first = first_replacement.clone();
            overlay_h.set_handler(Box::new(move |data| {
                if data == "b" {
                    tui.set_focus(Some(first.clone()));
                }
            }));
        }
        {
            let tui = tui.clone();
            let second = second_replacement.clone();
            first_h.set_handler(Box::new(move |data| {
                if data == "n" {
                    tui.set_focus(Some(second.clone()));
                }
            }));
        }
        {
            let tui = tui.clone();
            let base_h = base_h.clone();
            second_h.set_handler(Box::new(move |data| {
                if data == "\r" {
                    base_h.clear();
                    base_h.add_child(editor.clone());
                    tui.set_focus(Some(editor.clone()));
                }
            }));
        }
        tui.show_overlay(overlay, None);
        send_input(&terminal, &tui, "b");
        render_and_flush(&tui);
        send_input(&terminal, &tui, "n");
        render_and_flush(&tui);
        send_input(&terminal, &tui, "2");
        send_input(&terminal, &tui, "\r");
        render_and_flush(&tui);
        assert_eq!(overlay_h.inputs(), vec!["b"]);
        assert_eq!(first_h.inputs(), vec!["n"]);
        assert_eq!(second_h.inputs(), vec!["2", "\r"]);
        assert!(overlay_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn removed_replacement_restores_overlay_even_when_pre_focus_differs_from_next_focus() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (palette, _palette_h) = focusable_overlay(&["PALETTE"]);
        let (replacement, replacement_h) = focusable_overlay(&["REPLACEMENT"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        let (base, base_h) =
            shared_container(vec![editor.clone(), palette.clone(), replacement.clone()]);
        tui.add_child(base);
        tui.set_focus(Some(palette));
        tui.start();
        {
            let tui = tui.clone();
            let replacement = replacement.clone();
            overlay_h.set_handler(Box::new(move |data| {
                if data == "b" {
                    tui.set_focus(Some(replacement.clone()));
                }
            }));
        }
        {
            let tui = tui.clone();
            replacement_h.set_handler(Box::new(move |data| {
                if data == "\r" {
                    base_h.clear();
                    base_h.add_child(editor.clone());
                    tui.set_focus(Some(editor.clone()));
                }
            }));
        }
        tui.show_overlay(overlay, None);
        send_input(&terminal, &tui, "b");
        render_and_flush(&tui);
        send_input(&terminal, &tui, "\r");
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(overlay_h.inputs(), vec!["b", "x"]);
        assert_eq!(replacement_h.inputs(), vec!["\r"]);
        assert_eq!(editor_h.inputs(), Vec::<String>::new());
        assert!(overlay_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn unfocus_target_releases_a_blocked_overlay_while_replacement_remains_focused() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (fallback, fallback_h) = focusable_overlay(&["FALLBACK"]);
        let (target, target_h) = focusable_overlay(&["TARGET"]);
        let (replacement, replacement_h) = focusable_overlay(&["REPLACEMENT"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.start();
        {
            let tui = tui.clone();
            let fallback = fallback.clone();
            replacement_h.set_handler(Box::new(move |data| {
                if data == "\r" {
                    tui.set_focus(Some(fallback.clone()));
                }
            }));
        }
        let overlay_handle = tui.show_overlay(overlay, None);
        {
            let tui = tui.clone();
            let replacement = replacement.clone();
            let overlay_handle = overlay_handle.clone();
            let target = target.clone();
            overlay_h.set_handler(Box::new(move |data| {
                if data == "b" {
                    tui.set_focus(Some(replacement.clone()));
                    overlay_handle.unfocus(Some(OverlayUnfocusOptions {
                        target: Some(target.clone()),
                    }));
                }
            }));
        }
        send_input(&terminal, &tui, "b");
        render_and_flush(&tui);
        assert!(replacement_h.focused());
        send_input(&terminal, &tui, "\r");
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(overlay_h.inputs(), vec!["b"]);
        assert_eq!(replacement_h.inputs(), vec!["\r"]);
        assert_eq!(fallback_h.inputs(), Vec::<String>::new());
        assert_eq!(target_h.inputs(), vec!["x"]);
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn handle_input_restores_focus_to_visible_focused_overlay_after_base_focus_steal() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (replacement, _replacement_h) = focusable_overlay(&["REPLACEMENT"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        tui.show_overlay(overlay, None);
        assert!(overlay_h.focused());
        tui.set_focus(Some(replacement));
        tui.set_focus(Some(editor.clone()));
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(overlay_h.inputs(), vec!["x"]);
        assert_eq!(editor_h.inputs(), Vec::<String>::new());
        assert!(overlay_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn handle_input_restores_focus_to_explicitly_focused_raw_sub_overlay_after_base_focus_steal() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (controller, controller_h) = focusable_overlay(&["CONTROLLER"]);
        let (sub_overlay, sub_overlay_h) = focusable_overlay(&["SUB"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        tui.show_overlay(controller, None);
        let sub_handle = tui.show_overlay(
            sub_overlay,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        sub_handle.focus();
        tui.set_focus(Some(editor.clone()));
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(sub_overlay_h.inputs(), vec!["x"]);
        assert_eq!(controller_h.inputs(), Vec::<String>::new());
        assert_eq!(editor_h.inputs(), Vec::<String>::new());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn passive_non_capturing_overlay_does_not_regain_input_after_base_focus() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (passive, passive_h) = focusable_overlay(&["PASSIVE"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        tui.show_overlay(
            passive,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(editor_h.inputs(), vec!["x"]);
        assert_eq!(passive_h.inputs(), Vec::<String>::new());
        assert!(editor_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn explicitly_focused_non_capturing_overlay_regains_input_after_base_focus_steal() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let handle = tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        handle.focus();
        tui.set_focus(Some(editor.clone()));
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(overlay_h.inputs(), vec!["x"]);
        assert_eq!(editor_h.inputs(), Vec::<String>::new());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn unfocus_prevents_visible_overlay_from_regaining_input() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let handle = tui.show_overlay(overlay, None);
        handle.unfocus(None);
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(editor_h.inputs(), vec!["x"]);
        assert_eq!(overlay_h.inputs(), Vec::<String>::new());
        assert!(editor_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn set_focus_null_explicitly_clears_visible_overlay_restore() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.start();
        tui.show_overlay(overlay, None);
        tui.set_focus(None);
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(overlay_h.inputs(), Vec::<String>::new());
        assert!(!overlay_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn blocked_replacement_set_focus_null_resumes_the_visible_overlay() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (replacement, replacement_h) = focusable_overlay(&["REPLACEMENT"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.start();
        {
            let tui = tui.clone();
            replacement_h.set_handler(Box::new(move |data| {
                if data == "\r" {
                    tui.set_focus(None);
                }
            }));
        }
        {
            let tui = tui.clone();
            let replacement = replacement.clone();
            overlay_h.set_handler(Box::new(move |data| {
                if data == "b" {
                    tui.set_focus(Some(replacement.clone()));
                }
            }));
        }
        tui.show_overlay(overlay, None);
        send_input(&terminal, &tui, "b");
        render_and_flush(&tui);
        send_input(&terminal, &tui, "\r");
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(replacement_h.inputs(), vec!["\r"]);
        assert_eq!(overlay_h.inputs(), vec!["b", "x"]);
        assert!(overlay_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn temporarily_invisible_focused_overlay_falls_back_without_losing_restore_eligibility() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        let visible = Arc::new(AtomicBool::new(true));
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let visible_flag = Arc::clone(&visible);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                visible: Some(Box::new(move |_, _| visible_flag.load(Ordering::Relaxed))),
                ..Default::default()
            }),
        );
        // Base focus steal.
        tui.set_focus(Some(editor.clone()));
        visible.store(false, Ordering::Relaxed);
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(editor_h.inputs(), vec!["x"]);
        assert_eq!(overlay_h.inputs(), Vec::<String>::new());
        visible.store(true, Ordering::Relaxed);
        send_input(&terminal, &tui, "y");
        render_and_flush(&tui);
        assert_eq!(editor_h.inputs(), vec!["x"]);
        assert_eq!(overlay_h.inputs(), vec!["y"]);
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn temporarily_invisible_focused_overlay_with_null_pre_focus_restores_when_visible_again() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        let visible = Arc::new(AtomicBool::new(true));
        tui.add_child(empty_content());
        tui.start();
        let visible_flag = Arc::clone(&visible);
        tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                visible: Some(Box::new(move |_, _| visible_flag.load(Ordering::Relaxed))),
                ..Default::default()
            }),
        );
        visible.store(false, Ordering::Relaxed);
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(overlay_h.inputs(), Vec::<String>::new());
        visible.store(true, Ordering::Relaxed);
        send_input(&terminal, &tui, "y");
        render_and_flush(&tui);
        assert_eq!(overlay_h.inputs(), vec!["y"]);
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn cyclic_overlay_pre_focus_ancestry_does_not_hang_focus_changes() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        // The overlay's own preFocus becomes itself.
        tui.set_focus(Some(overlay.clone()));
        tui.start();
        let handle = tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        handle.focus();
        tui.set_focus(Some(editor.clone()));
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(editor_h.inputs(), vec!["x"]);
        assert_eq!(overlay_h.inputs(), Vec::<String>::new());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn handle_input_restores_the_focus_order_top_overlay_after_base_focus_steal() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (lower, lower_h) = focusable_overlay(&["LOWER"]);
        let (upper, upper_h) = focusable_overlay(&["UPPER"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let lower_handle = tui.show_overlay(lower, None);
        tui.show_overlay(upper, None);
        lower_handle.focus();
        tui.set_focus(Some(editor.clone()));
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(lower_h.inputs(), vec!["x"]);
        assert_eq!(upper_h.inputs(), Vec::<String>::new());
        assert_eq!(editor_h.inputs(), Vec::<String>::new());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn hide_overlay_does_not_reassign_focus_when_topmost_overlay_is_non_capturing() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, _editor_h) = focusable_overlay(&["EDITOR"]);
        let (capturing, capturing_h) = focusable_overlay(&["CAP"]);
        let (non_capturing, _nc_h) = focusable_overlay(&["NC"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        tui.show_overlay(capturing, None);
        tui.show_overlay(
            non_capturing,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        assert!(capturing_h.focused());
        tui.hide_overlay();
        render_and_flush(&tui);
        assert!(capturing_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn multiple_capturing_and_non_capturing_overlays_restore_focus_through_removals() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (c1, c1_h) = focusable_overlay(&["C1"]);
        let (n1, _n1_h) = focusable_overlay(&["N1"]);
        let (c2, c2_h) = focusable_overlay(&["C2"]);
        let (n2, _n2_h) = focusable_overlay(&["N2"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let c1_handle = tui.show_overlay(c1, None);
        tui.show_overlay(
            n1,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        let c2_handle = tui.show_overlay(c2, None);
        tui.show_overlay(
            n2,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        assert!(c2_h.focused());
        c2_handle.hide();
        render_and_flush(&tui);
        assert!(c1_h.focused());
        c1_handle.hide();
        render_and_flush(&tui);
        assert!(editor_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn capturing_overlay_unfocus_on_topmost_falls_back_to_pre_focus() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (capturing, capturing_h) = focusable_overlay(&["CAP"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let handle = tui.show_overlay(capturing, None);
        assert!(capturing_h.focused());
        handle.unfocus(None);
        render_and_flush(&tui);
        assert!(editor_h.focused());
        assert!(!capturing_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // overlay-non-capturing.test.ts: "no-op guards"
    // -------------------------------------------------------------------------

    #[test]
    fn focus_on_hidden_overlay_is_a_no_op() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, _overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let handle = tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        handle.set_hidden(true);
        handle.focus();
        render_and_flush(&tui);
        assert!(editor_h.focused());
        assert!(!handle.is_focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn focus_after_hide_is_a_no_op() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, _overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let handle = tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        handle.hide();
        handle.focus();
        render_and_flush(&tui);
        assert!(editor_h.focused());
        assert!(!handle.is_focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn unfocus_when_overlay_does_not_have_focus_is_a_no_op() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let handle = tui.show_overlay(
            overlay,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        handle.unfocus(None);
        render_and_flush(&tui);
        assert!(editor_h.focused());
        assert!(!overlay_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn unfocus_with_null_pre_focus_clears_focus_and_does_not_route_input_back() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.start();
        let handle = tui.show_overlay(overlay, None);
        assert!(overlay_h.focused());
        handle.unfocus(None);
        assert!(!overlay_h.focused());
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(overlay_h.inputs(), Vec::<String>::new());
        assert!(!handle.is_focused());
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // overlay-non-capturing.test.ts: "focus cycle prevention"
    // -------------------------------------------------------------------------

    #[test]
    fn toggle_focus_between_non_capturing_overlays_then_unfocus_returns_to_editor() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (a, a_h) = focusable_overlay(&["A"]);
        let (b, b_h) = focusable_overlay(&["B"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let a_handle = tui.show_overlay(
            a,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        let b_handle = tui.show_overlay(
            b,
            Some(OverlayOptions {
                non_capturing: true,
                ..Default::default()
            }),
        );
        a_handle.focus();
        b_handle.focus();
        a_handle.focus();
        a_handle.unfocus(None);
        render_and_flush(&tui);
        assert!(editor_h.focused());
        assert!(!a_h.focused());
        assert!(!b_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn explicit_unfocus_target_supports_cycling_between_three_overlays_and_editor() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, editor_h) = focusable_overlay(&["EDITOR"]);
        let (a, a_h) = focusable_overlay(&["A"]);
        let (b, b_h) = focusable_overlay(&["B"]);
        let (c, c_h) = focusable_overlay(&["C"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let a_handle = tui.show_overlay(a, None);
        let b_handle = tui.show_overlay(b, None);
        let c_handle = tui.show_overlay(c, None);

        a_handle.focus();
        send_input(&terminal, &tui, "a");
        render_and_flush(&tui);
        assert_eq!(a_h.inputs(), vec!["a"]);

        b_handle.focus();
        send_input(&terminal, &tui, "b");
        render_and_flush(&tui);
        assert_eq!(b_h.inputs(), vec!["b"]);

        c_handle.focus();
        send_input(&terminal, &tui, "c");
        render_and_flush(&tui);
        assert_eq!(c_h.inputs(), vec!["c"]);

        c_handle.unfocus(Some(OverlayUnfocusOptions {
            target: Some(editor.clone()),
        }));
        send_input(&terminal, &tui, "e");
        render_and_flush(&tui);
        assert_eq!(editor_h.inputs(), vec!["e"]);

        a_handle.focus();
        send_input(&terminal, &tui, "A");
        render_and_flush(&tui);
        assert_eq!(a_h.inputs(), vec!["a", "A"]);

        a_handle.unfocus(Some(OverlayUnfocusOptions {
            target: Some(editor.clone()),
        }));
        send_input(&terminal, &tui, "E");
        render_and_flush(&tui);
        assert_eq!(editor_h.inputs(), vec!["e", "E"]);

        assert_eq!(a_h.inputs(), vec!["a", "A"]);
        assert_eq!(b_h.inputs(), vec!["b"]);
        assert_eq!(c_h.inputs(), vec!["c"]);
        assert_eq!(editor_h.inputs(), vec!["e", "E"]);
        assert!(editor_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn explicit_null_unfocus_target_clears_focus_without_restoring_overlays() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (overlay, overlay_h) = focusable_overlay(&["OVERLAY"]);
        tui.add_child(empty_content());
        tui.start();
        let handle = tui.show_overlay(overlay, None);
        handle.unfocus(Some(OverlayUnfocusOptions { target: None }));
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(overlay_h.inputs(), Vec::<String>::new());
        assert!(!handle.is_focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn hiding_focused_overlay_falls_back_to_next_visual_frontmost_overlay() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (editor, _editor_h) = focusable_overlay(&["EDITOR"]);
        let (a, a_h) = focusable_overlay(&["A"]);
        let (b, b_h) = focusable_overlay(&["B"]);
        let (c, c_h) = focusable_overlay(&["C"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let a_handle = tui.show_overlay(a, None);
        let b_handle = tui.show_overlay(b, None);
        tui.show_overlay(c, None);
        a_handle.focus();
        b_handle.focus();
        b_handle.set_hidden(true);
        send_input(&terminal, &tui, "x");
        render_and_flush(&tui);
        assert_eq!(a_h.inputs(), vec!["x"]);
        assert_eq!(c_h.inputs(), Vec::<String>::new());
        assert!(a_h.focused());
        assert!(!b_h.focused());
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // overlay-non-capturing.test.ts: "rendering order"
    // -------------------------------------------------------------------------

    fn pinned_overlay_options() -> OverlayOptions {
        OverlayOptions {
            row: Some(SizeValue::Absolute(0)),
            col: Some(SizeValue::Absolute(0)),
            width: Some(SizeValue::Absolute(1)),
            non_capturing: true,
            ..Default::default()
        }
    }

    fn first_viewport_char(terminal: &VirtualTerminal) -> Option<char> {
        terminal.get_viewport()[0].chars().next()
    }

    #[test]
    fn focus_on_already_focused_overlay_bumps_visual_order() {
        let terminal = VirtualTerminal::new(20, 6);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let a_handle = tui.show_overlay(
            static_overlay_simple(&["A"]),
            Some(pinned_overlay_options()),
        );
        tui.show_overlay(
            static_overlay_simple(&["B"]),
            Some(pinned_overlay_options()),
        );
        a_handle.focus();
        tui.show_overlay(
            static_overlay_simple(&["C"]),
            Some(pinned_overlay_options()),
        );
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('C'));
        a_handle.focus();
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('A'));
        assert!(a_handle.is_focused());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn default_rendering_order_for_overlapping_overlays_follows_creation_order() {
        let terminal = VirtualTerminal::new(20, 6);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        tui.show_overlay(
            static_overlay_simple(&["A"]),
            Some(pinned_overlay_options()),
        );
        tui.show_overlay(
            static_overlay_simple(&["B"]),
            Some(pinned_overlay_options()),
        );
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('B'));
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn focus_on_lower_overlay_renders_it_on_top() {
        let terminal = VirtualTerminal::new(20, 6);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        let lower = tui.show_overlay(
            static_overlay_simple(&["A"]),
            Some(pinned_overlay_options()),
        );
        tui.show_overlay(
            static_overlay_simple(&["B"]),
            Some(pinned_overlay_options()),
        );
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('B'));
        lower.focus();
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('A'));
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn focusing_middle_overlay_places_it_on_top_while_preserving_others_relative_order() {
        let terminal = VirtualTerminal::new(20, 6);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        tui.show_overlay(
            static_overlay_simple(&["A"]),
            Some(pinned_overlay_options()),
        );
        let middle = tui.show_overlay(
            static_overlay_simple(&["B"]),
            Some(pinned_overlay_options()),
        );
        let top = tui.show_overlay(
            static_overlay_simple(&["C"]),
            Some(pinned_overlay_options()),
        );
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('C'));
        middle.focus();
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('B'));
        middle.hide();
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('C'));
        top.hide();
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('A'));
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn capturing_overlay_hidden_and_shown_again_renders_on_top_after_unhide() {
        let terminal = VirtualTerminal::new(20, 6);
        let tui = new_tui(&terminal);
        tui.add_child(empty_content());
        tui.show_overlay(
            static_overlay_simple(&["A"]),
            Some(pinned_overlay_options()),
        );
        let capturing = tui.show_overlay(
            static_overlay_simple(&["B"]),
            Some(OverlayOptions {
                row: Some(SizeValue::Absolute(0)),
                col: Some(SizeValue::Absolute(0)),
                width: Some(SizeValue::Absolute(1)),
                ..Default::default()
            }),
        );
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('B'));
        capturing.set_hidden(true);
        tui.show_overlay(
            static_overlay_simple(&["C"]),
            Some(pinned_overlay_options()),
        );
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('C'));
        capturing.set_hidden(false);
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('B'));
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn unfocus_does_not_change_visual_order_until_another_overlay_is_focused() {
        let terminal = VirtualTerminal::new(20, 6);
        let tui = new_tui(&terminal);
        let (editor, _editor_h) = focusable_overlay(&["EDITOR"]);
        tui.add_child(empty_content());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        let a = tui.show_overlay(
            static_overlay_simple(&["A"]),
            Some(pinned_overlay_options()),
        );
        let b = tui.show_overlay(
            static_overlay_simple(&["B"]),
            Some(pinned_overlay_options()),
        );
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('B'));
        a.focus();
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('A'));
        a.unfocus(None);
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('A'));
        b.focus();
        render_and_flush(&tui);
        assert_eq!(first_viewport_char(&terminal), Some('B'));
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // overlay-short-content.test.ts
    // -------------------------------------------------------------------------

    #[test]
    fn overlay_renders_when_content_is_shorter_than_terminal_height() {
        // Terminal has 24 rows, but content only has 3 lines.
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (content, _lines) = test_component(&["Line 1", "Line 2", "Line 3"]);
        tui.add_child(content);

        struct SimpleOverlay;
        impl Component for SimpleOverlay {
            fn render(&self, _width: usize) -> Vec<String> {
                vec![
                    "OVERLAY_TOP".into(),
                    "OVERLAY_MID".into(),
                    "OVERLAY_BOT".into(),
                ]
            }
        }
        tui.show_overlay(shared_component(SimpleOverlay), None);

        tui.start();
        settle(&tui);

        let viewport = terminal.get_viewport();
        assert!(
            viewport.iter().any(|line| line.contains("OVERLAY")),
            "overlay should be visible when content is shorter than terminal"
        );
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // tui-shrink.test.ts
    // -------------------------------------------------------------------------

    #[test]
    fn shrink_clears_all_rendered_lines_when_content_shrinks_to_zero() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (content, _lines) = test_component(&["first", "second", "third"]);
        tui.add_child(content);
        tui.start();
        settle(&tui);

        assert!(terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("first")));
        assert!(terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("second")));
        assert!(terminal
            .get_viewport()
            .iter()
            .any(|line| line.contains("third")));

        tui.clear();
        tui.request_render(false);
        settle(&tui);

        let viewport = terminal.get_viewport();
        assert!(
            !viewport.iter().any(|line| line.contains("first")),
            "first line should be cleared"
        );
        assert!(
            !viewport.iter().any(|line| line.contains("second")),
            "second line should be cleared"
        );
        assert!(
            !viewport.iter().any(|line| line.contains("third")),
            "third line should be cleared"
        );
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // tui-cell-size-input.test.ts
    // -------------------------------------------------------------------------

    /// `InputRecorder` (tui-cell-size-input.test.ts:7-19).
    fn input_recorder() -> (SharedComponent, FocusableHandle) {
        focusable_overlay(&[""])
    }

    #[test]
    fn cell_size_forwards_bare_escape_even_when_query_was_sent_at_startup() {
        let _lock = state_lock();
        let _caps = CapsGuard::kitty(10.0);
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (recorder, recorder_h) = input_recorder();
        tui.set_focus(Some(recorder));
        tui.start();

        send_input(&terminal, &tui, "\x1b");
        assert_eq!(recorder_h.inputs(), vec!["\x1b"]);
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn cell_size_consumes_responses_and_still_forwards_later_user_input() {
        let _lock = state_lock();
        let _caps = CapsGuard::kitty(10.0);
        set_cell_dimensions(CellDimensions {
            width_px: 9.0,
            height_px: 18.0,
        });
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (recorder, recorder_h) = input_recorder();
        tui.set_focus(Some(recorder));
        tui.start();

        send_input(&terminal, &tui, "\x1b[6;20;10t");
        assert_eq!(recorder_h.inputs(), Vec::<String>::new());
        let dims = crate::terminal_image::get_cell_dimensions();
        assert_eq!(dims.width_px, 10.0);
        assert_eq!(dims.height_px, 20.0);

        send_input(&terminal, &tui, "q");
        assert_eq!(recorder_h.inputs(), vec!["q"]);
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // tui-overlay-style-leak.test.ts
    // -------------------------------------------------------------------------

    #[test]
    fn style_leak_trailing_reset_beyond_last_visible_column_no_overlay() {
        let width = 20;
        let base_line = format!("\x1b[3m{}\x1b[23m", "X".repeat(width));

        let terminal = VirtualTerminal::new(width, 6);
        let tui = new_tui(&terminal);
        let (content, _lines) = test_component(&[&base_line, "INPUT"]);
        tui.add_child(content);
        tui.start();
        render_and_flush(&tui);
        assert!(!terminal.cell_italic(1, 0));
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn style_leak_when_overlay_slicing_drops_trailing_sgr_resets() {
        let width = 20;
        let base_line = format!("\x1b[3m{}\x1b[23m", "X".repeat(width));

        let terminal = VirtualTerminal::new(width, 6);
        let tui = new_tui(&terminal);
        let (content, _lines) = test_component(&[&base_line, "INPUT"]);
        tui.add_child(content);

        tui.show_overlay(
            static_overlay_simple(&["OVR"]),
            Some(OverlayOptions {
                row: Some(SizeValue::Absolute(0)),
                col: Some(SizeValue::Absolute(5)),
                width: Some(SizeValue::Absolute(3)),
                ..Default::default()
            }),
        );
        tui.start();
        render_and_flush(&tui);
        assert!(!terminal.cell_italic(1, 0));
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // viewport-overwrite-repro.ts (manual repro script; automated here)
    // -------------------------------------------------------------------------

    #[test]
    fn viewport_overwrite_repro_streamed_lines_do_not_overwrite_earlier_content() {
        let terminal = VirtualTerminal::new(80, 12);
        let tui = new_tui(&terminal);
        let (buffer, lines) = test_component(&[]);
        tui.add_child(buffer);
        tui.start();

        let height = 12usize;
        let pre_count = height + 8;
        let tool_count = height + 12;
        let post_count = 6;

        let append = |lines: &Arc<Mutex<Vec<String>>>, new: &[String]| {
            lock_shared(lines).extend(new.iter().cloned());
            tui.request_render(false);
            settle(&tui);
        };

        append(
            &lines,
            &[
                "TUI viewport overwrite repro".to_string(),
                format!("Viewport rows detected: {height}"),
                "".to_string(),
                "=== PRE-TOOL STREAM ===".to_string(),
            ],
        );
        // Phase 1: stream pre-tool text until the viewport is exceeded.
        for i in 1..=pre_count {
            append(&lines, &[format!("PRE-TOOL LINE {i:02}")]);
        }
        // Phase 2: tool call pause and tool output.
        append(
            &lines,
            &[
                "".to_string(),
                "--- TOOL CALL START ---".to_string(),
                "(pause...)".to_string(),
                "".to_string(),
            ],
        );
        for i in 1..=tool_count {
            append(&lines, &[format!("TOOL OUT {i:02}")]);
        }
        // Phase 3: post-tool streaming.
        append(
            &lines,
            &["".to_string(), "=== POST-TOOL STREAM ===".to_string()],
        );
        for i in 1..=post_count {
            append(&lines, &[format!("POST-TOOL LINE {i:02}")]);
        }

        let buffer_text = terminal.get_scroll_buffer().join("\n");
        let pre_first = buffer_text.find("PRE-TOOL LINE 01");
        let tool_start = buffer_text.find("--- TOOL CALL START ---");
        let post_last = buffer_text.find("POST-TOOL LINE 06");
        assert!(
            pre_first.is_some(),
            "PRE-TOOL lines remain in the scrollback"
        );
        assert!(tool_start.is_some(), "tool output present");
        assert!(post_last.is_some(), "post-tool lines appended");
        assert!(
            pre_first < tool_start && tool_start < post_last,
            "content order preserved: pre-tool before tool output before post-tool"
        );
        // No line is overwritten: every streamed line appears exactly once.
        assert_eq!(buffer_text.matches("PRE-TOOL LINE 08").count(), 1);
        assert_eq!(buffer_text.matches("TOOL OUT 05").count(), 1);
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // regression-overlay-cjk-boundary.test.ts (TUI-level composite cases; the
    // extract_segments cases live in utils.rs tests)
    // -------------------------------------------------------------------------

    #[test]
    fn cjk_boundary_composites_overlay_at_requested_column_starting_inside_wide_grapheme() {
        let out = composite_tui_line("abcd让EFGH", "│XX│", 5, 4, 20);
        let prefix = slice_by_column(&out, 0, 5, true);
        let overlay = slice_by_column(&out, 5, 4, true);

        assert!(!out.contains('让'));
        assert_eq!(visible_width(&out), 20);
        assert_eq!(visible_width(&prefix), 5);
        assert_eq!(visible_width(&overlay), 4);
        assert!(overlay.contains("│XX│"));
    }

    #[test]
    fn cjk_boundary_composites_overlay_at_wide_grapheme_boundary() {
        let out = composite_tui_line("abcd让EFGH", "│XX│", 4, 4, 20);
        let overlay = slice_by_column(&out, 4, 4, true);

        assert!(!out.contains('让'));
        assert_eq!(visible_width(&out), 20);
        assert_eq!(visible_width(&overlay), 4);
        assert!(overlay.contains("│XX│"));
    }

    // -------------------------------------------------------------------------
    // tab-width.test.ts: TUI-level integration case
    // -------------------------------------------------------------------------

    #[test]
    fn tab_containing_overlays_stay_on_one_physical_terminal_row() {
        struct FullViewportContent;
        impl Component for FullViewportContent {
            fn render(&self, width: usize) -> Vec<String> {
                ["base 0", "base 1", "base 2"]
                    .map(|line| format!("{line:width$}", width = width))
                    .to_vec()
            }
        }
        struct TabStatusOverlay;
        impl Component for TabStatusOverlay {
            fn render(&self, _width: usize) -> Vec<String> {
                vec!["\tX".to_string()]
            }
        }

        let terminal = VirtualTerminal::new(16, 3);
        let tui = new_tui(&terminal);
        tui.add_child(shared_component(FullViewportContent));
        tui.show_overlay(
            shared_component(TabStatusOverlay),
            Some(OverlayOptions {
                width: Some(SizeValue::Absolute(4)),
                row: Some(SizeValue::Absolute(1)),
                col: Some(SizeValue::Absolute(4)),
                ..Default::default()
            }),
        );
        tui.start();
        settle(&tui);

        assert_eq!(
            terminal.get_viewport(),
            vec!["base 0          ", "base   X        ", "base 2          ",]
        );
        assert!(!terminal.get_writes().contains('\t'));
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // terminal-colors.test.ts: "TUI.queryTerminalBackgroundColor"
    // -------------------------------------------------------------------------

    fn colors_test_setup() -> (
        VirtualTerminal,
        TuiMainScreen,
        FocusableHandle,
        Arc<Mutex<Vec<String>>>,
    ) {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (component, component_h) = input_recorder();
        let listener_inputs = Arc::new(Mutex::new(Vec::new()));
        tui.add_child(component.clone());
        tui.set_focus(Some(component));
        let listener_inputs_clone = Arc::clone(&listener_inputs);
        tui.add_input_listener(Box::new(move |data: &str| {
            lock_shared(&listener_inputs_clone).push(data.to_string());
            None
        }));
        tui.start();
        (terminal, tui, component_h, listener_inputs)
    }

    #[test]
    fn osc11_writes_query_and_resolves_with_parsed_rgb_reply() {
        let (terminal, tui, _component, _listeners) = colors_test_setup();
        let query = tui.query_terminal_background_color(Duration::from_millis(1000));
        assert!(terminal.get_writes().contains("\x1b]11;?\x07"));

        send_input(&terminal, &tui, "\x1b]11;#ffffff\x07");
        assert_eq!(
            query.blocking_recv().ok().flatten(),
            Some(RgbColor {
                r: 255,
                g: 255,
                b: 255
            })
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn osc11_consumes_replies_before_listeners_and_focused_dispatch() {
        let (terminal, tui, component_h, listener_inputs) = colors_test_setup();
        let query = tui.query_terminal_background_color(Duration::from_millis(1000));

        send_input(&terminal, &tui, "\x1b]11;#000000\x07");

        assert_eq!(
            query.blocking_recv().ok().flatten(),
            Some(RgbColor { r: 0, g: 0, b: 0 })
        );
        assert_eq!(*lock_shared(&listener_inputs), Vec::<String>::new());
        assert_eq!(component_h.inputs(), Vec::<String>::new());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn osc11_consumes_unparseable_strict_replies_and_resolves_none() {
        let (terminal, tui, component_h, listener_inputs) = colors_test_setup();
        let query = tui.query_terminal_background_color(Duration::from_millis(1000));

        send_input(&terminal, &tui, "\x1b]11;not-a-color\x07");

        assert_eq!(query.blocking_recv().ok().flatten(), None);
        assert_eq!(*lock_shared(&listener_inputs), Vec::<String>::new());
        assert_eq!(component_h.inputs(), Vec::<String>::new());
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn osc11_dispatches_non_matching_input_normally_while_waiting() {
        let (terminal, tui, component_h, listener_inputs) = colors_test_setup();
        let mut query = tui.query_terminal_background_color(Duration::from_millis(1000));

        send_input(&terminal, &tui, "x");
        assert!(query.try_recv().is_err(), "query should still be pending");
        assert_eq!(*lock_shared(&listener_inputs), vec!["x"]);
        assert_eq!(component_h.inputs(), vec!["x"]);

        send_input(&terminal, &tui, "\x1b]11;#ffffff\x07");
        assert_eq!(
            query.blocking_recv().ok().flatten(),
            Some(RgbColor {
                r: 255,
                g: 255,
                b: 255
            })
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn osc11_keeps_consuming_a_late_reply_after_timeout() {
        let (terminal, tui, component_h, listener_inputs) = colors_test_setup();
        let query = tui.query_terminal_background_color(Duration::from_millis(1));
        // Fire the explicit deadline (upstream waits 5ms of real time).
        tui.tick(Instant::now() + Duration::from_millis(5));

        assert_eq!(query.blocking_recv().ok().flatten(), None);

        send_input(&terminal, &tui, "\x1b]11;#ffffff\x07");

        assert_eq!(*lock_shared(&listener_inputs), Vec::<String>::new());
        assert_eq!(component_h.inputs(), Vec::<String>::new());
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // Engine-level additions (T11 self-test checklist; no direct upstream test)
    // -------------------------------------------------------------------------

    #[test]
    fn render_requests_are_throttled_to_16ms() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&["a"]);
        tui.add_child(component);
        tui.start();
        let rendered_at = settle(&tui);
        terminal.clear_writes();

        set_lines(&lines, &["b"]);
        tui.request_render(false);
        // 8ms after the last render: below the 16ms interval, no render.
        tui.tick(rendered_at + Duration::from_millis(8));
        assert!(
            !terminal.get_writes().contains('b'),
            "render should be throttled below 16ms"
        );
        // 20ms after the last render: the deadline has passed.
        tui.tick(rendered_at + Duration::from_millis(20));
        assert!(
            terminal.get_writes().contains('b'),
            "render should fire once 16ms elapsed"
        );
        tui.stop(TuiStopOptions::default());
    }

    // -------------------------------------------------------------------------
    // TUI render scheduling (tui-render.test.ts:91-115, 29d9f087c)
    // -------------------------------------------------------------------------

    /// `InputComponent` (tui-render.test.ts:27-38): counts renders; handled
    /// input replaces the rendered lines.
    struct InputComponent {
        lines: Arc<Mutex<Vec<String>>>,
        render_count: Arc<Mutex<u64>>,
    }

    impl Component for InputComponent {
        fn render(&self, _width: usize) -> Vec<String> {
            *lock_shared(&self.render_count) += 1;
            lock_shared(&self.lines).clone()
        }

        fn handle_input(&mut self, data: &str) {
            *lock_shared(&self.lines) = vec![data.to_string()];
        }
    }

    #[test]
    fn renders_keyboard_input_without_waiting_for_a_throttled_frame() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let lines = Arc::new(Mutex::new(vec!["initial".to_string()]));
        let render_count = Arc::new(Mutex::new(0u64));
        let component = shared_component(InputComponent {
            lines: Arc::clone(&lines),
            render_count: Arc::clone(&render_count),
        });
        tui.add_child(component.clone());
        tui.set_focus(Some(component));
        tui.start();
        settle(&tui);
        let render_count_before_input = *lock_shared(&render_count);

        // Queue a normal throttled render first. Keyboard input should preempt it.
        *lock_shared(&lines) = vec!["pending".to_string()];
        tui.request_render(false);
        terminal.send_input("first");
        terminal.send_input("second");
        terminal.send_input("typed");
        // Upstream awaits a single `process.nextTick`; here the first tick
        // drains all queued inputs (arming the immediate deadline) and the
        // second fires it — both well inside the 16ms throttle window of the
        // queued frame.
        tui.tick(Instant::now());
        tui.tick(Instant::now());

        assert_eq!(
            *lock_shared(&render_count),
            render_count_before_input + 1,
            "three inputs in one tick render exactly once, without waiting for the throttled frame"
        );
        assert_eq!(*lock_shared(&lines), vec!["typed".to_string()]);
        // The preempted throttled frame must not render a second time once its
        // original deadline passes.
        tui.tick(Instant::now() + Duration::from_millis(20));
        assert_eq!(*lock_shared(&render_count), render_count_before_input + 1);
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn pending_render_survives_stop_and_fires_after_restart() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&["a"]);
        tui.add_child(component);
        tui.start();
        let rendered_at = settle(&tui);
        terminal.clear_writes();

        // A throttled render pending at stop() keeps its deadline (upstream
        // leaks renderRequested here; see header note) and fires on the
        // first tick after start().
        set_lines(&lines, &["b"]);
        tui.request_render(false);
        tui.stop(TuiStopOptions::default());
        tui.start();
        terminal.clear_writes();
        tui.tick(rendered_at + Duration::from_millis(20));
        assert!(
            terminal.get_writes().contains('b'),
            "pending render fires on the first tick after restart"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn stopped_tui_defers_render_deadline_but_keeps_query_timeouts() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&["a"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        set_lines(&lines, &["b"]);
        tui.request_render(false);
        assert!(
            tui.next_deadline().is_some(),
            "pending render sets a deadline"
        );
        // A live introspection query keeps its timeout in next_deadline even
        // while stopped (tick fires it regardless of stopped).
        let _query = tui.query_terminal_color_scheme(Duration::from_millis(500));
        tui.stop(TuiStopOptions::default());
        let stopped_deadline = tui
            .next_deadline()
            .expect("query timeout is still reported while stopped");
        assert!(
            stopped_deadline >= Instant::now() + Duration::from_millis(400),
            "the deferred render deadline is not reported while stopped, got {stopped_deadline:?}"
        );
        assert!(
            !tui.has_pending_work(),
            "deferred render and unexpired query are not pending work"
        );
        tui.start();
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn next_deadline_includes_pending_query_timeouts() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, _lines) = test_component(&["a"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);
        assert_eq!(tui.next_deadline(), None, "idle TUI has no deadline");

        // No render pending: the query timeout becomes the next deadline.
        let requested_at = Instant::now();
        let _query = tui.query_terminal_color_scheme(Duration::from_millis(500));
        let deadline = tui
            .next_deadline()
            .expect("query timeout must contribute a deadline");
        assert!(
            deadline >= requested_at + Duration::from_millis(500),
            "deadline is the query timeout"
        );
        assert!(
            !tui.has_pending_work(),
            "an unexpired query timeout is not pending work"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn expired_query_timeout_is_pending_work() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, _lines) = test_component(&["a"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        let _query = tui.query_terminal_background_color(Duration::ZERO);
        assert!(
            tui.has_pending_work(),
            "an expired query timeout is pending work"
        );
        tui.tick(Instant::now() + Duration::from_millis(1));
        assert!(
            !tui.has_pending_work(),
            "the fired timeout is reaped, nothing pending"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn settled_queries_are_reaped_from_pending_queues() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, _lines) = test_component(&["a"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        let _osc11 = tui.query_terminal_background_color(Duration::ZERO);
        let _scheme = tui.query_terminal_color_scheme(Duration::ZERO);
        tui.tick(Instant::now() + Duration::from_millis(1));
        let inner = tui.lock_inner();
        assert!(
            inner.pending_osc11_background_queries.is_empty(),
            "timed-out osc11 queries are reaped"
        );
        assert!(
            inner.pending_terminal_color_scheme_queries.is_empty(),
            "timed-out color scheme queries are reaped"
        );
        drop(inner);
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn render_handle_does_not_keep_the_tui_alive() {
        // Loader-shaped holder: the component stores its RenderHandle while
        // the TUI owns the component (components/loader.rs's pattern).
        struct HandleHolder {
            #[allow(dead_code)]
            handle: RenderHandle,
        }

        impl Component for HandleHolder {
            fn render(&self, _width: usize) -> Vec<String> {
                vec!["x".to_string()]
            }
        }

        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let handle = tui.render_handle();
        let component = shared_component(HandleHolder {
            handle: handle.clone(),
        });
        tui.add_child(component);

        // While the TUI is alive the handle requests a throttled render.
        handle.request_render();
        assert!(
            lock_shared(&tui.schedule).requested,
            "handle behaves like request_render(false) while alive"
        );

        let weak_inner = Arc::downgrade(&tui.inner);
        drop(tui);
        drop(handle);
        assert!(
            weak_inner.upgrade().is_none(),
            "RenderHandle must not close a TuiMainScreen -> children -> component -> TuiMainScreen cycle"
        );
    }

    #[test]
    fn contains_component_terminates_on_cyclic_shared_children() {
        // Shared-child cycle: A lists B, B lists A.
        struct CyclicContainer {
            children: Arc<Mutex<Vec<SharedComponent>>>,
        }

        impl Component for CyclicContainer {
            fn render(&self, _width: usize) -> Vec<String> {
                Vec::new()
            }

            fn shared_children(&self) -> Option<Vec<SharedComponent>> {
                Some(lock_shared(&self.children).clone())
            }
        }

        let children_a = Arc::new(Mutex::new(Vec::new()));
        let children_b = Arc::new(Mutex::new(Vec::new()));
        let a = shared_component(CyclicContainer {
            children: Arc::clone(&children_a),
        });
        let b = shared_component(CyclicContainer {
            children: Arc::clone(&children_b),
        });
        lock_shared(&children_a).push(Arc::clone(&b));
        lock_shared(&children_b).push(Arc::clone(&a));

        let (target, _lines) = test_component(&["t"]);
        assert!(TuiBase::contains_component(&a, &a));
        assert!(
            !TuiBase::contains_component(&a, &target),
            "cyclic shared children must terminate, not hang"
        );
    }

    /// The `recovery.rs` fallback (`try_stop` fails with the inner lock
    /// held) must queue a full stop so the event loop writes the complete
    /// restore sequence and stops rendering. Tested here because driving
    /// input through the lock-holding dispatch needs the virtual terminal.
    #[test]
    fn recovery_fallback_queues_full_stop_when_lock_held() {
        struct RecoveryComponent {
            tui: TuiMainScreen,
        }

        impl Component for RecoveryComponent {
            fn render(&self, _width: usize) -> Vec<String> {
                vec!["x".to_string()]
            }

            // handle_input runs with the inner lock held, so the recovery
            // path takes its locked-TuiMainScreen fallback.
            fn handle_input(&mut self, _data: &str) {
                crate::recovery::restore_terminal(&self.tui);
            }
        }

        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let component = shared_component(RecoveryComponent { tui: tui.clone() });
        tui.add_child(component.clone());
        tui.set_focus(Some(component));
        tui.start();
        settle(&tui);
        terminal.clear_writes();

        send_input(&terminal, &tui, "a");

        // The fallback itself writes only the minimal sequence to stdout; the
        // queued stop op must have run on the same tick's pending-op drain,
        // writing the full restore (cursor shown) through the terminal.
        assert!(
            terminal.get_writes().contains("\x1b[?25h"),
            "queued stop op wrote the full restore sequence"
        );
        // Stopped: later renders short-circuit instead of clobbering the
        // restore sequence.
        terminal.clear_writes();
        tui.request_render(true);
        tui.tick(Instant::now());
        assert!(
            !terminal.get_writes().contains('x'),
            "stopped TUI must not render over the restore sequence"
        );
    }

    #[test]
    fn key_release_events_are_filtered_unless_component_opts_in() {
        // Kitty release event for 'a' (97): CSI 97;1:3u.
        let release = "\x1b[97;1:3u";
        assert!(is_key_release(release));

        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (component, component_h) = input_recorder();
        tui.add_child(component.clone());
        tui.set_focus(Some(component));
        tui.start();
        send_input(&terminal, &tui, release);
        assert_eq!(
            component_h.inputs(),
            Vec::<String>::new(),
            "release events are filtered by default"
        );
        tui.stop(TuiStopOptions::default());

        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (component, component_h) = focusable_overlay_with(&[""], true);
        tui.add_child(component.clone());
        tui.set_focus(Some(component));
        tui.start();
        send_input(&terminal, &tui, release);
        assert_eq!(
            component_h.inputs(),
            vec![release],
            "opted-in component receives release events"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn cursor_marker_positions_hardware_cursor_and_is_stripped() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (editor, marker_col) = cursor_editor();
        tui.add_child(editor.clone());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        settle(&tui);

        assert!(
            !terminal.get_writes().contains(CURSOR_MARKER),
            "marker must be stripped from the output"
        );
        assert_eq!(terminal.get_viewport()[0], "abcd");
        assert_eq!(
            terminal.get_cursor_position(),
            (1, 0),
            "hardware cursor at the marker position"
        );
        assert!(terminal.cursor_hidden(), "cursor hidden by default");

        // Moving only the marker keeps the rendered line identical: the
        // no-change path must still move the hardware cursor.
        terminal.clear_writes();
        *lock_shared(&marker_col) = 3;
        tui.request_render(false);
        settle(&tui);
        assert_eq!(terminal.get_cursor_position(), (3, 0));
        assert!(
            !terminal.get_writes().contains("abcd"),
            "identical content must not be re-rendered"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn show_hardware_cursor_makes_cursor_visible() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = TuiMainScreen::with_options(
            Box::new(terminal.clone()),
            Some(true),
            Some(temp_log_dir()),
        );
        let (editor, _marker_col) = cursor_editor();
        tui.add_child(editor.clone());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        settle(&tui);

        assert!(!terminal.cursor_hidden(), "cursor visible when enabled");
        assert!(terminal.get_writes().contains("\x1b[?25h"));
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn every_rendered_line_ends_with_sgr_and_osc8_reset() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, _lines) = test_component(&["abc"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        assert!(
            terminal.get_writes().contains("abc\x1b[0m\x1b]8;;\x07"),
            "line ends with SGR + OSC 8 reset"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn input_listeners_can_consume_and_rewrite_input_in_order() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (component, component_h) = input_recorder();
        tui.add_child(component.clone());
        tui.set_focus(Some(component));
        // First listener rewrites, second observes the rewritten data
        // (insertion order, upstream Set semantics).
        tui.add_input_listener(Box::new(|data: &str| {
            if data == "a" {
                Some(TuiInputListenerResult {
                    consume: false,
                    data: Some("b".to_string()),
                })
            } else {
                None
            }
        }));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);
        tui.add_input_listener(Box::new(move |data: &str| {
            lock_shared(&seen_clone).push(data.to_string());
            if data == "q" {
                Some(TuiInputListenerResult {
                    consume: true,
                    data: None,
                })
            } else {
                None
            }
        }));
        tui.start();

        send_input(&terminal, &tui, "a");
        assert_eq!(
            *lock_shared(&seen),
            vec!["b"],
            "second listener sees rewritten data"
        );
        assert_eq!(
            component_h.inputs(),
            vec!["b"],
            "focused component gets rewritten data"
        );

        send_input(&terminal, &tui, "q");
        assert_eq!(
            component_h.inputs(),
            vec!["b"],
            "consumed input is not dispatched"
        );

        // A listener rewriting to the empty string swallows the input.
        send_input(&terminal, &tui, "z");
        tui.stop(TuiStopOptions::default());

        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (component, component_h) = input_recorder();
        tui.add_child(component.clone());
        tui.set_focus(Some(component));
        tui.add_input_listener(Box::new(|_data: &str| {
            Some(TuiInputListenerResult {
                consume: false,
                data: Some(String::new()),
            })
        }));
        tui.start();
        send_input(&terminal, &tui, "z");
        assert_eq!(
            component_h.inputs(),
            Vec::<String>::new(),
            "empty rewritten data is swallowed"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn debug_key_invokes_on_debug_before_focused_dispatch() {
        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (component, component_h) = input_recorder();
        tui.add_child(component.clone());
        tui.set_focus(Some(component));
        let debug_calls = Arc::new(AtomicU64::new(0));
        let debug_calls_clone = Arc::clone(&debug_calls);
        tui.set_on_debug(Some(Box::new(move || {
            debug_calls_clone.fetch_add(1, Ordering::Relaxed);
        })));
        tui.start();

        // Kitty shift+ctrl+d: CSI 100;6u ('d' = 100, shift|ctrl = 1+4, +1).
        let debug_sequence = "\x1b[100;6u";
        assert!(matches_key(debug_sequence, "shift+ctrl+d"));
        send_input(&terminal, &tui, debug_sequence);
        assert_eq!(debug_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            component_h.inputs(),
            Vec::<String>::new(),
            "debug key is not forwarded to the focused component"
        );
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn stop_moves_cursor_below_content_and_shows_it() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, _lines) = test_component(&["one", "two"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        tui.stop(TuiStopOptions::default());
        assert!(!terminal.cursor_hidden(), "stop shows the cursor");
        let (_col, row) = terminal.get_cursor_position();
        assert!(row >= 2, "cursor moved below the content, got row {row}");
        // Content stays on screen.
        let viewport = terminal.get_viewport();
        assert!(viewport[0].contains("one"));
        assert!(viewport[1].contains("two"));
    }

    /// Focusable component emitting [`CURSOR_MARKER`] at a configurable column
    /// when focused (IME cursor positioning tests).
    struct CursorEditor {
        marker_col: Arc<Mutex<usize>>,
        focused: Arc<Mutex<bool>>,
    }

    impl Component for CursorEditor {
        fn render(&self, _width: usize) -> Vec<String> {
            if !*lock_shared(&self.focused) {
                return vec!["abcd".to_string()];
            }
            let col = *lock_shared(&self.marker_col);
            let mut line = String::from("abcd");
            line.insert_str(col, CURSOR_MARKER);
            vec![line]
        }

        fn as_focusable(&self) -> Option<&dyn Focusable> {
            Some(self)
        }

        fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
            Some(self)
        }
    }

    impl Focusable for CursorEditor {
        fn focused(&self) -> bool {
            *lock_shared(&self.focused)
        }

        fn set_focused(&mut self, focused: bool) {
            *lock_shared(&self.focused) = focused;
        }
    }

    fn cursor_editor() -> (SharedComponent, Arc<Mutex<usize>>) {
        let marker_col = Arc::new(Mutex::new(1usize));
        (
            shared_component(CursorEditor {
                marker_col: Arc::clone(&marker_col),
                focused: Arc::new(Mutex::new(false)),
            }),
            marker_col,
        )
    }
    // -------------------------------------------------------------------------
    // T28 additions: stop(options) / capture+restore / render_now / trait
    // surface (new API; upstream tui-main-screen.ts:68-89, 101-109 and
    // tui.ts:757-763 @ 4181f66)
    // -------------------------------------------------------------------------

    /// `stop({ preserveScreen: true })` skips the cursor-to-content-end
    /// sequence (upstream `beforeTerminalStop`, tui-main-screen.ts:101-109);
    /// the default path keeps writing it.
    #[test]
    fn stop_preserve_screen_skips_cursor_to_content_end() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, _lines) = test_component(&["hello"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);
        terminal.clear_writes();

        tui.stop(TuiStopOptions {
            preserve_screen: true,
        });
        assert_eq!(
            terminal.get_writes(),
            "\x1b[?25h\x1b[?2004l",
            "preserve_screen: only show-cursor + terminal stop are written"
        );

        // Contrast: the default options keep the pre-split stop bytes.
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, _lines) = test_component(&["hello"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);
        terminal.clear_writes();

        tui.stop(TuiStopOptions::default());
        let writes = terminal.get_writes();
        assert!(
            writes.starts_with(" \x1b[1B\r\n"),
            "default stop moves the cursor past the content: {writes:?}"
        );
    }

    /// `captureRenderState` / `restoreRenderState` (tui-main-screen.ts:68-89):
    /// round-trip of the seven render-state fields; restore filters image
    /// lines to `""` and clears the kitty image id set.
    #[test]
    fn capture_and_restore_render_state_roundtrip() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, _lines) = test_component(&["one", "two"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        let state = tui.capture_render_state();
        assert_eq!(state.previous_lines.len(), 2);
        assert!(state.previous_lines[0].contains("one"));
        assert_eq!(state.previous_width, 40);
        assert_eq!(state.previous_height, 10);
        assert_eq!(state.cursor_row, 1);
        assert_eq!(state.hardware_cursor_row, 1);
        assert_eq!(state.max_lines_rendered, 2);
        assert_eq!(state.previous_viewport_top, 0);

        tui.restore_render_state(TuiMainScreenRenderState::default());
        assert_eq!(
            tui.capture_render_state(),
            TuiMainScreenRenderState::default()
        );

        tui.restore_render_state(state.clone());
        assert_eq!(tui.capture_render_state(), state);
        tui.stop(TuiStopOptions::default());
    }

    #[test]
    fn restore_render_state_filters_image_lines_and_clears_kitty_ids() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let image_line = "\x1b_Gi=7,a=T;AAAA".to_string();
        assert!(is_image_line(&image_line));
        tui.restore_render_state(TuiMainScreenRenderState {
            previous_lines: vec![image_line, "plain".to_string()],
            previous_width: 40,
            previous_height: 10,
            ..TuiMainScreenRenderState::default()
        });
        let restored = tui.capture_render_state();
        assert_eq!(
            restored.previous_lines,
            vec![String::new(), "plain".to_string()]
        );
        assert!(lock_shared(&tui.inner).previous_kitty_image_ids.is_empty());
        tui.stop(TuiStopOptions::default());
    }

    /// `renderNow` (tui.ts:757-763): synchronous render bypassing the
    /// throttle; `force` resets the render state (full-clear path); stopped
    /// is a no-op.
    #[test]
    fn render_now_renders_synchronously_and_force_resets_state() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, lines) = test_component(&["a"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);
        terminal.clear_writes();

        // Non-force: differential render happens without a tick, and no
        // throttled render remains pending afterwards.
        lock_shared(&lines).push("b".to_string());
        tui.render_now(false);
        assert!(
            terminal.get_writes().contains("b\x1b[0m"),
            "render_now renders synchronously: {:?}",
            terminal.get_writes()
        );
        assert!(!tui.has_pending_work());

        // Force: render state reset takes the full-clear path.
        let redraws = tui.full_redraws();
        terminal.clear_writes();
        tui.render_now(true);
        assert!(
            terminal.get_writes().contains("\x1b[2J"),
            "force resets render state → full clear: {:?}",
            terminal.get_writes()
        );
        assert_eq!(tui.full_redraws(), redraws + 1);

        // Stopped: the render short-circuits (upstream `doRender` early
        // return), no bytes are written.
        tui.stop(TuiStopOptions::default());
        terminal.clear_writes();
        tui.render_now(true);
        assert!(
            !terminal.get_writes().contains("\x1b[?2026h"),
            "stopped render_now must not render: {:?}",
            terminal.get_writes()
        );
    }

    /// The `Tui` trait is object-safe and `TuiMainScreen` reports
    /// `TuiMode::Regular` (upstream `readonly mode = "regular"`,
    /// tui-main-screen.ts:58).
    #[test]
    fn tui_trait_is_object_safe_and_reports_regular_mode() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let as_trait: &dyn Tui = &tui;
        assert_eq!(as_trait.mode(), TuiMode::Regular);
        assert_eq!(as_trait.full_redraws(), 0);
        tui.stop(TuiStopOptions::default());
    }

    /// `TuiMode` serde shape (tui.ts:284 @ 4181f66): `"regular" |
    /// "fullscreen"`, matching the ext-sdk placeholder naming.
    #[test]
    fn tui_mode_serializes_lowercase_like_upstream() {
        assert_eq!(
            serde_json::to_string(&TuiMode::Regular).unwrap(),
            "\"regular\""
        );
        assert_eq!(
            serde_json::to_string(&TuiMode::Fullscreen).unwrap(),
            "\"fullscreen\""
        );
        assert_eq!(
            serde_json::from_str::<TuiMode>("\"fullscreen\"").unwrap(),
            TuiMode::Fullscreen
        );
    }
}
