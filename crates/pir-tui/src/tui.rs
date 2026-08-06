//! Port of `packages/tui/src/tui.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: see below.
//!
//! The component contract section (Component / Focusable traits, is_focusable,
//! CURSOR_MARKER, RenderHandle, Container) is a FROZEN contract for components
//! and must only be extended, not broken (components are implemented against
//! it in parallel). Everything below it is the T11 engine port: the `Tui`
//! class (upstream `TUI`, tui.ts:295) with the differential render pipeline,
//! overlay stack, focus-restore state machine, hardware cursor positioning and
//! terminal introspection queries.
//!
//! Differences from the upstream type layout (behavior unchanged):
//! - Upstream `Component` is a TS interface with optional `handleInput` /
//!   `wantsKeyRelease` members; here they are defaulted trait methods.
//! - Upstream `Focusable` is a structural `{ focused: boolean }` check
//!   (`isFocusable` = `"focused" in component`); here it is a sub-trait
//!   reached via `Component::as_focusable{,_mut}`.
//! - Components that re-render on a timer (Loader) hold a `RenderHandle`
//!   instead of a `&TUI` reference (upstream passes the TUI instance).
//! - Upstream `TUI extends Container`; here `Tui` holds its own child list of
//!   [`SharedComponent`]s and mirrors the Container API (`add_child` /
//!   `remove_child` / `clear`). The frozen `Container` (Box-owned children)
//!   stays as-is for component composition.
//!
//! Intentional differences (T11 engine):
//! - Ownership/identity: upstream relies on JS reference identity
//!   (`focusedComponent === component`, overlay entries holding live component
//!   references, `instanceof Container` walks). Here components are shared as
//!   [`SharedComponent`] (`Arc<Mutex<Box<dyn Component>>>`), identity is
//!   `Arc::ptr_eq`, overlay stack entries carry a unique `id` used in place of
//!   upstream's entry-object identity (`restoreState.overlay === entry`), and
//!   the `containsComponent` container walk goes through the defaulted
//!   [`Component::shared_children`] extension instead of `instanceof`.
//! - Re-entrancy: upstream components call back into the TUI synchronously
//!   from within `handleInput` (e.g. `tui.setFocus(...)` on Esc). Rust cannot
//!   re-borrow the TUI mid-dispatch, so `Tui` is a clonable handle around
//!   `Arc<Mutex<TuiInner>>`; mutating calls made while the inner lock is held
//!   (from inside component `handle_input` / `render`, or from another thread)
//!   are queued and drained right after the in-progress dispatch/render
//!   completes, preserving upstream's observable ordering. Read-only queries
//!   (`is_focused`, `has_overlay`, ...) return a default on lock contention.
//! - Timers: upstream's `process.nextTick` + 16ms `setTimeout` render throttle
//!   and the introspection query timeouts become explicit deadlines, matching
//!   `terminal.rs` / `stdin_buffer.rs`: `request_render` only records intent,
//!   and the event loop drives [`Tui::tick`] / [`Tui::pump`]
//!   ([`Tui::next_deadline`] yields the wait timeout). Multiple
//!   `request_render(true)` calls before the next tick coalesce into one
//!   render (upstream would run `doRender` once per nextTick callback).
//! - Input delivery: the `Terminal::start` callbacks push raw input into an
//!   inbox which [`Tui::tick`] drains through the upstream `handleInput` flow.
//!   Same thread, same order — but delivery never happens inside
//!   `Terminal::pump`. [`Tui::pump`] waits on the terminal's lock-free event
//!   source ([`Terminal::event_source`]), so neither the inner lock nor the
//!   terminal lock is held across the (possibly indefinite) event wait:
//!   cross-thread [`Tui::stop`] / [`Tui::with_terminal`] never block behind a
//!   parked driver. (Terminals without an event source — virtual test
//!   terminals — fall back to the in-lock [`Terminal::pump`].)
//! - `Component::handle_input` is a defaulted trait method, so the upstream
//!   `focusedComponent?.handleInput` existence check always passes: a focused
//!   component without input handling still gets a no-op call plus the
//!   trailing `requestRender`.
//! - Stopped render timers: upstream leaks `renderRequested === true` when a
//!   throttled render is pending at `stop()`, swallowing post-restart renders
//!   until a forced one; here the pending deadline survives `stop()` and fires
//!   on the first `tick` after `start()`. While stopped, the deferred render
//!   is not reported by [`Tui::next_deadline`] / [`Tui::has_pending_work`],
//!   so the host loop can sleep until the restart.
//! - Env vars renamed per ADR-0001: `PI_HARDWARE_CURSOR` →
//!   `PIR_HARDWARE_CURSOR`, `PI_CLEAR_ON_SHRINK` → `PIR_CLEAR_ON_SHRINK`,
//!   `PI_DEBUG_REDRAW` → `PIR_DEBUG_REDRAW`, `PI_TUI_DEBUG` →
//!   `PIR_TUI_DEBUG`, `PI_CODING_AGENT_DIR` → `PIR_CODING_AGENT_DIR`; the
//!   default log directory is `~/.pir/agent` (upstream `~/.pi/agent`). Log
//!   files: `pi-debug.log` → `pir-debug.log`, `pi-crash.log` →
//!   `pir-crash.log`; the `PIR_TUI_DEBUG` dump directory is `/tmp/pir-tui`
//!   (upstream `/tmp/tui`).
//! - `SizeValue` is an enum; upstream's invalid-percentage-string fallback
//!   (`"abc%"` → anchor center) applies to negative/NaN percent values.
//! - `add_input_listener` / `on_terminal_color_scheme_change` return numeric
//!   ids paired with `remove_*` methods instead of unsubscribe closures.
//! - `query_terminal_background_color` / `query_terminal_color_scheme` return
//!   `oneshot::Receiver`s instead of Promises; their timeouts fire from
//!   [`Tui::tick`].
//! - The width-overflow path writes the crash log, stops the TUI and then
//!   panics with the upstream error message (upstream throws an uncaught
//!   Error). Redraw/crash log write failures are ignored (upstream would
//!   throw).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::oneshot;

use crate::keys::{is_key_release, matches_key};
use crate::terminal::{InputHandler, ResizeHandler, Terminal};
use crate::terminal_colors::{
    is_osc11_background_color_response, parse_osc11_background_color,
    parse_terminal_color_scheme_report, RgbColor, TerminalColorScheme,
};
use crate::terminal_image::{
    delete_kitty_image, get_capabilities, is_image_line, set_cell_dimensions, CellDimensions,
};
use crate::utils::{
    extract_segments, normalize_terminal_output, slice_by_column, slice_with_width, visible_width,
};

/// Cursor position marker - APC (Application Program Command) sequence.
/// Zero-width escape sequence terminals ignore. Focused components emit this
/// at the cursor position; TUI finds and strips it, then positions the
/// hardware cursor there (IME candidate window positioning).
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

/// Component interface - all components must implement this
/// (upstream `Component`, tui.ts:64).
///
/// Lock contract: component methods (`render`, `handle_input`, `invalidate`,
/// `wants_key_release`) are invoked while the component's own
/// [`SharedComponent`] mutex is held (see the header note on re-entrancy).
/// Implementations must not re-enter the component tree from within these
/// methods — neither by locking other [`SharedComponent`]s directly
/// (deadlock) nor synchronously through TUI callbacks; mutating `Tui` calls
/// made mid-dispatch are queued and run after the dispatch completes.
pub trait Component: Send {
    /// Render the component to ANSI lines for the given viewport width.
    /// Each returned string is one line; visible width must not exceed `width`.
    fn render(&self, width: usize) -> Vec<String>;

    /// Optional handler for keyboard input when the component has focus
    /// (upstream optional `handleInput`, tui.ts:75).
    fn handle_input(&mut self, _data: &str) {}

    /// If true, the component receives key release events (Kitty protocol).
    /// Default false — release events are filtered out (tui.ts:81).
    fn wants_key_release(&self) -> bool {
        false
    }

    /// Invalidate any cached rendering state (theme change / full re-render).
    fn invalidate(&mut self) {}

    /// Mirror of upstream `isFocusable` (`"focused" in component`, tui.ts:110).
    fn as_focusable(&self) -> Option<&dyn Focusable> {
        None
    }

    /// Mutable counterpart of [`Component::as_focusable`].
    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        None
    }

    /// Mirror of the upstream `root instanceof Container` check in
    /// `containsComponent` (tui.ts:485-489): components that own shared child
    /// references override this so the TUI can walk the tree for
    /// `isComponentMounted`. Default: not a container. (T11 extension to the
    /// frozen contract; the frozen `Container` owns boxed children and does
    /// not participate.)
    fn shared_children(&self) -> Option<Vec<SharedComponent>> {
        None
    }
}

/// Components that can receive focus and display a hardware cursor.
/// When focused, the component should emit [`CURSOR_MARKER`] at the cursor
/// position in its render output (upstream `Focusable`, tui.ts:104).
pub trait Focusable: Component {
    /// Whether the component currently has focus (upstream `focused` property).
    fn focused(&self) -> bool;

    /// Set by TUI when focus changes (upstream `component.focused = ...`).
    fn set_focused(&mut self, focused: bool);
}

/// Type guard equivalent of upstream `isFocusable` (tui.ts:110).
pub fn is_focusable(component: &dyn Component) -> bool {
    component.as_focusable().is_some()
}

/// Cloneable handle components use to request a re-render.
/// Upstream passes the whole `TUI` instance (Loader calls
/// `ui.requestRender()`, loader.ts:88-89); Rust passes just that capability.
/// The closure holds the render schedule weakly: components that store the
/// handle (Loader) are themselves owned by the TUI's child list, so a strong
/// capture would be a reference cycle.
#[derive(Clone)]
pub struct RenderHandle(Arc<dyn Fn() + Send + Sync>);

impl RenderHandle {
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// Request a re-render (upstream `TUI.requestRender()`).
    pub fn request_render(&self) {
        (self.0)();
    }
}

impl std::fmt::Debug for RenderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderHandle").finish_non_exhaustive()
    }
}

/// Container - a component that contains other components
/// (upstream `Container`, tui.ts:256).
#[derive(Default)]
pub struct Container {
    pub children: Vec<Box<dyn Component>>,
}

impl Container {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_child(&mut self, component: Box<dyn Component>) {
        self.children.push(component);
    }

    /// Remove by identity (upstream uses `indexOf` reference equality).
    pub fn remove_child(&mut self, component: &dyn Component) {
        let target = component as *const dyn Component as *const ();
        if let Some(index) = self
            .children
            .iter()
            .position(|child| &**child as *const dyn Component as *const () == target)
        {
            self.children.remove(index);
        }
    }

    pub fn clear(&mut self) {
        self.children.clear();
    }
}

impl Component for Container {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for child in &self.children {
            lines.extend(child.render(width));
        }
        lines
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}

// =============================================================================
// Shared components and identity (see header note on ownership)
// =============================================================================

/// Shared, interior-mutable component reference. Replaces upstream's JS
/// object references; identity comparisons (`===` upstream) use
/// [`Arc::ptr_eq`].
pub type SharedComponent = Arc<Mutex<Box<dyn Component>>>;

/// Wrap a component into a [`SharedComponent`].
pub fn shared_component(component: impl Component + 'static) -> SharedComponent {
    Arc::new(Mutex::new(Box::new(component)))
}

/// Wrap an already-boxed component into a [`SharedComponent`].
pub fn shared_component_from_boxed(component: Box<dyn Component>) -> SharedComponent {
    Arc::new(Mutex::new(component))
}

/// Lock a shared component, recovering from poisoning (a panicking component
/// must not wedge the whole TUI).
fn lock_component(component: &SharedComponent) -> MutexGuard<'_, Box<dyn Component>> {
    component
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Upstream `componentA === componentB`.
fn same_component(a: &SharedComponent, b: &SharedComponent) -> bool {
    Arc::ptr_eq(a, b)
}

// =============================================================================
// Input listeners (tui.ts:90-91)
// =============================================================================

/// `InputListenerResult` (tui.ts:90): `{ consume?: boolean; data?: string }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputListenerResult {
    /// Stop routing this input (upstream `consume`).
    pub consume: bool,
    /// Replace the input for subsequent listeners and the focused component
    /// (upstream `data`).
    pub data: Option<String>,
}

/// `InputListener` (tui.ts:91).
pub type InputListener = Box<dyn FnMut(&str) -> Option<InputListenerResult> + Send>;

/// Color scheme change listener (tui.ts:320).
pub type TerminalColorSchemeListener = Box<dyn FnMut(TerminalColorScheme) + Send>;

// =============================================================================
// Overlay types (tui.ts:124-251)
// =============================================================================

/// `OverlayAnchor` (tui.ts:127-136).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayAnchor {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

/// `OverlayMargin` (tui.ts:141-146). Unset sides default to 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayMargin {
    pub top: Option<i32>,
    pub right: Option<i32>,
    pub bottom: Option<i32>,
    pub left: Option<i32>,
}

/// `SizeValue` (tui.ts:149): absolute columns/rows or a percentage of the
/// terminal dimension (upstream `"50%"` string).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeValue {
    Absolute(i32),
    Percent(f64),
}

/// `OverlayOptions["margin"]` (tui.ts:196): a single number for all sides or
/// per-side edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayMarginSpec {
    Uniform(i32),
    Edges(OverlayMargin),
}

/// `OverlayOptions` (tui.ts:171-207). All fields optional, mirroring upstream.
#[derive(Default)]
pub struct OverlayOptions {
    /// Width in columns, or percentage of terminal width.
    pub width: Option<SizeValue>,
    /// Minimum width in columns.
    pub min_width: Option<i32>,
    /// Maximum height in rows, or percentage of terminal height.
    pub max_height: Option<SizeValue>,
    /// Anchor point for positioning (default: center).
    pub anchor: Option<OverlayAnchor>,
    /// Horizontal offset from anchor position (positive = right).
    pub offset_x: Option<i32>,
    /// Vertical offset from anchor position (positive = down).
    pub offset_y: Option<i32>,
    /// Row position: absolute, or percentage (25% = 25% from top).
    pub row: Option<SizeValue>,
    /// Column position: absolute, or percentage.
    pub col: Option<SizeValue>,
    /// Margin from terminal edges.
    pub margin: Option<OverlayMarginSpec>,
    /// Controls overlay visibility based on terminal dimensions; called each
    /// render cycle with `(term_width, term_height)`.
    pub visible: Option<Box<dyn Fn(i32, i32) -> bool + Send>>,
    /// If true, don't capture keyboard focus when shown.
    pub non_capturing: bool,
}

/// `OverlayUnfocusOptions` (tui.ts:210-213).
pub struct OverlayUnfocusOptions {
    /// Explicit target to focus after releasing this overlay.
    pub target: Option<SharedComponent>,
}

/// `parseSizeValue` (tui.ts:152-161). Percent values floor like upstream
/// (`Math.floor((referenceSize * pct) / 100)`).
fn parse_size_value(value: Option<&SizeValue>, reference_size: i32) -> Option<i32> {
    match value {
        None => None,
        Some(SizeValue::Absolute(value)) => Some(*value),
        Some(SizeValue::Percent(percent)) => {
            Some(((reference_size as f64 * percent) / 100.0).floor() as i32)
        }
    }
}

/// `isTermuxSession` (tui.ts:163-165). `Boolean(process.env.TERMUX_VERSION)`:
/// an empty value is falsy in JS.
fn is_termux_session() -> bool {
    std::env::var("TERMUX_VERSION").is_ok_and(|value| !value.is_empty())
}

/// `OverlayStackEntry` (tui.ts:233-239). The `id` replaces upstream's entry
/// object identity.
struct OverlayStackEntry {
    id: u64,
    component: SharedComponent,
    options: Option<OverlayOptions>,
    pre_focus: Option<SharedComponent>,
    hidden: bool,
    focus_order: u64,
}

/// `OverlayBlockedFocusResume` (tui.ts:241).
#[derive(Clone)]
enum OverlayBlockedFocusResume {
    RestoreOverlay,
    FocusTarget(Option<SharedComponent>),
}

/// `OverlayFocusRestoreState` (tui.ts:242-250). `Eligible` / `Blocked` carry
/// the entry id plus the entry's component (upstream stores the live entry
/// object; the component reference is what `resolve` paths actually read).
#[derive(Clone)]
enum OverlayFocusRestoreState {
    Inactive,
    Eligible {
        overlay_id: u64,
        component: SharedComponent,
    },
    Blocked {
        overlay_id: u64,
        component: SharedComponent,
        blocked_by: SharedComponent,
        resume: OverlayBlockedFocusResume,
    },
}

impl OverlayFocusRestoreState {
    fn overlay_id(&self) -> Option<u64> {
        match self {
            OverlayFocusRestoreState::Inactive => None,
            OverlayFocusRestoreState::Eligible { overlay_id, .. }
            | OverlayFocusRestoreState::Blocked { overlay_id, .. } => Some(*overlay_id),
        }
    }
}

/// `OverlayFocusRestorePolicy` (tui.ts:251).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayFocusRestorePolicy {
    Clear,
    Preserve,
}

/// `PendingOsc11BackgroundQuery` (tui.ts:92-96); the `setTimeout` timer is an
/// explicit deadline fired by [`Tui::tick`] (see header note). Settled
/// entries are reaped (upstream relies on GC).
struct PendingOsc11BackgroundQuery {
    settled: bool,
    sender: Option<oneshot::Sender<Option<RgbColor>>>,
    deadline: Option<Instant>,
}

/// Pending `query_terminal_color_scheme` (tui.ts:1698-1718); same explicit
/// deadline treatment, same reaping of settled entries.
struct PendingTerminalColorSchemeQuery {
    settled: bool,
    sender: Option<oneshot::Sender<Option<TerminalColorScheme>>>,
    deadline: Option<Instant>,
}

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

// =============================================================================
// Environment / logging helpers
// =============================================================================

/// `TUI.MIN_RENDER_INTERVAL_MS` (tui.ts:309).
const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(16);

/// `PIR_DEBUG_REDRAW` (ADR-0001 rename of `PI_DEBUG_REDRAW`, tui.ts:1331).
const ENV_DEBUG_REDRAW: &str = "PIR_DEBUG_REDRAW";
/// `PIR_TUI_DEBUG` (ADR-0001 rename of `PI_TUI_DEBUG`, tui.ts:1577).
const ENV_TUI_DEBUG: &str = "PIR_TUI_DEBUG";
/// `PIR_HARDWARE_CURSOR` (ADR-0001 rename of `PI_HARDWARE_CURSOR`, tui.ts:312).
const ENV_HARDWARE_CURSOR: &str = "PIR_HARDWARE_CURSOR";
/// `PIR_CLEAR_ON_SHRINK` (ADR-0001 rename of `PI_CLEAR_ON_SHRINK`, tui.ts:313).
const ENV_CLEAR_ON_SHRINK: &str = "PIR_CLEAR_ON_SHRINK";
/// `PIR_CODING_AGENT_DIR` (ADR-0001 rename of `PI_CODING_AGENT_DIR`,
/// tui.ts:332).
const ENV_CODING_AGENT_DIR: &str = "PIR_CODING_AGENT_DIR";

/// Upstream `process.env.X === "1"` checks.
fn env_flag_is_1(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}

/// Default log directory: `~/.pir/agent` (upstream `~/.pi/agent`, tui.ts:332).
fn default_log_directory() -> PathBuf {
    home_dir().join(".pir").join("agent")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

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

// =============================================================================
// Render scheduling (explicit-deadline form of upstream's nextTick/setTimeout)
// =============================================================================

/// Upstream `renderRequested` / `renderTimer` / `lastRenderAt` (tui.ts:306-308)
/// as an explicit deadline driven by [`Tui::tick`].
struct RenderSchedule {
    requested: bool,
    deadline: Option<Instant>,
    last_render_at: Option<Instant>,
}

/// Raw terminal events queued by the `Terminal::start` callbacks and drained
/// by [`Tui::tick`] (see header note on input delivery).
enum InboxEvent {
    Input(String),
    Resize,
}

/// Mutation queued while the inner lock is held (see header note on
/// re-entrancy).
type PendingOp = Box<dyn FnOnce(&mut TuiInner) + Send>;

fn lock_shared<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// =============================================================================
// Tui (upstream `TUI`, tui.ts:295)
// =============================================================================

/// Minimal TUI with differential rendering (upstream `TUI`, tui.ts:295).
///
/// Clonable handle around shared state; all clones refer to the same TUI.
/// Single-threaded event-loop semantics mirror upstream: drive rendering with
/// [`Tui::tick`] / [`Tui::pump`] from the loop thread.
#[derive(Clone)]
pub struct Tui {
    inner: Arc<Mutex<TuiInner>>,
    /// Shared with `TuiInner::terminal`; used directly by `pump` /
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

/// Lock-free terminal size cache shared between `Tui` and `TuiInner`.
#[derive(Clone, Default)]
struct TerminalSizeCache {
    rows: Arc<AtomicU16>,
    columns: Arc<AtomicU16>,
}

/// The terminal behind its own mutex, shared between `Tui` (blocking `pump`
/// waits and `with_terminal`) and `TuiInner` (writes during render/tick).
/// Lock order is always `inner` → `terminal`; the terminal's input callbacks
/// only touch the inbox mutex, so no cycle is possible. The blocking wait in
/// `Terminal::pump` must never hold the inner lock: the driver parks there
/// between frames, and `std::sync::Mutex` is not FIFO-fair, so a thread that
/// re-locks in a tight loop starves any parked `lock_inner` waiter (e.g.
/// `with_terminal` from the run loop) for an unbounded time.
type SharedTerminal = Arc<Mutex<Box<dyn Terminal + Send>>>;

struct TuiInner {
    terminal: SharedTerminal,
    children: Vec<SharedComponent>,
    previous_lines: Vec<String>,
    /// JS `Set<number>`: deduplicated, first-seen insertion order.
    previous_kitty_image_ids: Vec<u32>,
    /// 0 = no previous render; -1 after `requestRender(true)` (tui.ts:719).
    previous_width: i32,
    previous_height: i32,
    focused_component: Option<SharedComponent>,
    /// JS `Set<InputListener>`: insertion-ordered, deduplicated by id.
    input_listeners: Vec<(u64, InputListener)>,
    on_debug: Option<Box<dyn FnMut() + Send>>,
    cursor_row: i32,
    hardware_cursor_row: i32,
    show_hardware_cursor: bool,
    clear_on_shrink: bool,
    max_lines_rendered: usize,
    previous_viewport_top: i32,
    full_redraw_count: u64,
    stopped: bool,
    pending_osc11_background_replies: usize,
    pending_osc11_background_queries: VecDeque<PendingOsc11BackgroundQuery>,
    terminal_color_scheme_listeners: Vec<(u64, TerminalColorSchemeListener)>,
    terminal_color_scheme_notifications_enabled: bool,
    pending_terminal_color_scheme_queries: Vec<PendingTerminalColorSchemeQuery>,
    log_directory: PathBuf,
    focus_order_counter: u64,
    overlay_stack: Vec<OverlayStackEntry>,
    overlay_focus_restore: OverlayFocusRestoreState,
    schedule: Arc<Mutex<RenderSchedule>>,
    size_cache: TerminalSizeCache,
}

impl Tui {
    /// Upstream `new TUI(terminal)` (tui.ts:329).
    pub fn new(terminal: Box<dyn Terminal + Send>) -> Tui {
        Self::with_options(terminal, None, None)
    }

    /// Upstream `new TUI(terminal, showHardwareCursor?, logDirectory?)`
    /// (tui.ts:329-336).
    pub fn with_options(
        terminal: Box<dyn Terminal + Send>,
        show_hardware_cursor: Option<bool>,
        log_directory: Option<PathBuf>,
    ) -> Tui {
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
        let inner = TuiInner {
            terminal: Arc::clone(&terminal),
            children: Vec::new(),
            previous_lines: Vec::new(),
            previous_kitty_image_ids: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            focused_component: None,
            input_listeners: Vec::new(),
            on_debug: None,
            cursor_row: 0,
            hardware_cursor_row: 0,
            show_hardware_cursor: show_hardware_cursor
                .unwrap_or_else(|| env_flag_is_1(ENV_HARDWARE_CURSOR)),
            clear_on_shrink: env_flag_is_1(ENV_CLEAR_ON_SHRINK),
            max_lines_rendered: 0,
            previous_viewport_top: 0,
            full_redraw_count: 0,
            stopped: false,
            pending_osc11_background_replies: 0,
            pending_osc11_background_queries: VecDeque::new(),
            terminal_color_scheme_listeners: Vec::new(),
            terminal_color_scheme_notifications_enabled: false,
            pending_terminal_color_scheme_queries: Vec::new(),
            log_directory: log_directory
                .or_else(|| std::env::var_os(ENV_CODING_AGENT_DIR).map(PathBuf::from))
                .unwrap_or_else(default_log_directory),
            focus_order_counter: 0,
            overlay_stack: Vec::new(),
            overlay_focus_restore: OverlayFocusRestoreState::Inactive,
            schedule: Arc::clone(&schedule),
            size_cache: size_cache.clone(),
        };
        Tui {
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

    fn lock_inner(&self) -> MutexGuard<'_, TuiInner> {
        lock_shared(&self.inner)
    }

    /// Run `op` against the inner state. When the inner lock is already held
    /// (re-entrant call from a component mid-dispatch, or from another
    /// thread), the op is queued and runs after the in-progress work
    /// completes (see header note on re-entrancy).
    fn run_or_queue(&self, op: impl FnOnce(&mut TuiInner) + Send + 'static) {
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
    fn try_read<R>(&self, read: impl FnOnce(&TuiInner) -> R) -> Option<R> {
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
        OverlayHandle {
            tui: self.clone(),
            entry_id,
        }
    }

    /// Upstream `hideOverlay`: hide the topmost overlay and restore previous
    /// focus (tui.ts:591).
    pub fn hide_overlay(&self) {
        self.run_or_queue(TuiInner::hide_overlay);
    }

    /// Upstream `hasOverlay` (tui.ts:607). Returns false on lock contention.
    pub fn has_overlay(&self) -> bool {
        self.try_read(|inner| inner.has_overlay()).unwrap_or(false)
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
    /// [`Tui::remove_input_listener`] (upstream returns an unsubscribe
    /// closure).
    pub fn add_input_listener(&self, listener: InputListener) -> u64 {
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
    /// [`Tui::remove_terminal_color_scheme_listener`].
    pub fn on_terminal_color_scheme_change(&self, listener: TerminalColorSchemeListener) -> u64 {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.run_or_queue(move |inner| inner.terminal_color_scheme_listeners.push((id, listener)));
        id
    }

    /// Remove a color scheme listener registered with
    /// [`Tui::on_terminal_color_scheme_change`].
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
    /// timeout / parse failure. The timeout is fired by [`Tui::tick`].
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

    /// Upstream `invalidate` override (tui.ts:632): children plus overlays.
    pub fn invalidate(&self) {
        self.run_or_queue(TuiInner::invalidate);
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

    /// Upstream `start` (tui.ts:637).
    pub fn start(&self) {
        let mut inner = self.lock_inner();
        inner.stopped = false;
        let input_inbox = Arc::clone(&self.inbox);
        let on_input: InputHandler = Box::new(move |data: &str| {
            lock_shared(&input_inbox).push_back(InboxEvent::Input(data.to_string()));
        });
        let resize_inbox = Arc::clone(&self.inbox);
        let on_resize: ResizeHandler = Box::new(move || {
            lock_shared(&resize_inbox).push_back(InboxEvent::Resize);
        });
        inner.terminal().start(on_input, on_resize);
        inner.terminal().hide_cursor();
        if inner.terminal_color_scheme_notifications_enabled {
            inner.terminal().write("\x1b[?2031h");
        }
        inner.query_cell_size();
        inner.request_render(false);
    }

    /// Upstream `stop` (tui.ts:689).
    pub fn stop(&self) {
        self.lock_inner().stop_internal();
    }

    /// Non-blocking [`Tui::stop`] for the panic/signal recovery path
    /// (`recovery.rs`): returns `false` without restoring when the inner lock
    /// is held (e.g. the panicking thread holds it mid-render), so the caller
    /// can fall back to a fixed restore sequence instead of deadlocking.
    /// Poisoning is recovered the same way as [`lock_shared`].
    pub(crate) fn try_stop(&self) -> bool {
        use std::sync::TryLockError;
        let mut inner = match self.inner.try_lock() {
            Ok(inner) => inner,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        inner.stop_internal();
        true
    }

    /// Enqueue a full [`Tui::stop`] for the recovery fallback (`recovery.rs`)
    /// used when the inner lock cannot be taken: the op runs on the next
    /// [`Tui::tick`] pending-op drain, writing the complete restore sequence
    /// and setting `stopped` so later renders short-circuit. When the panic
    /// happened on the event-loop thread itself nothing drains the op — no
    /// renders follow either, so the minimal fallback sequence stands.
    pub(crate) fn queue_stop(&self) {
        lock_shared(&self.pending).push(Box::new(TuiInner::stop_internal));
    }

    /// Upstream `requestRender(force)` (tui.ts:716).
    ///
    /// Records render intent; the actual render happens on [`Tui::tick`] once
    /// the deadline is reached (immediate for `force`, 16ms-throttled
    /// otherwise). With `force`, the previous frame state is reset so the
    /// next render takes the full-clear path (tui.ts:717-738).
    pub fn request_render(&self, force: bool) {
        if force {
            self.run_or_queue(TuiInner::reset_render_state_for_force);
        }
        schedule_render(&self.schedule, force, Instant::now());
    }

    /// The next instant at which [`Tui::tick`] has work to do: a pending
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
    /// unexpired query timeouts do not count either — [`Tui::next_deadline`]
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
    /// then drive the TUI like [`Tui::tick`]. Mirrors the upstream event
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

    // --- overlay layout / compositing (associated, upstream private) -------

    /// Upstream `compositeLineAt` (tui.ts:1185-1228). Associated function:
    /// upstream's instance method uses no instance state (the upstream
    /// regression test reaches it through a TUI instance).
    pub fn composite_line_at(
        base_line: &str,
        overlay_line: &str,
        start_col: i32,
        overlay_width: i32,
        total_width: i32,
    ) -> String {
        if is_image_line(base_line) {
            return base_line.to_string();
        }

        // Single pass through base_line extracts both before and after segments.
        let after_start = start_col + overlay_width;
        let base = extract_segments(
            base_line,
            start_col.max(0) as usize,
            after_start.max(0) as usize,
            (total_width - after_start).max(0) as usize,
            true,
        );

        // Extract overlay with width tracking (strict=true to exclude wide
        // chars at boundary).
        let (overlay_text, overlay_actual_width) =
            slice_with_width(overlay_line, 0, overlay_width.max(0) as usize, true);

        // Pad segments to target widths.
        let before_pad = 0.max(start_col - base.before_width as i32);
        let overlay_pad = 0.max(overlay_width - overlay_actual_width as i32);
        let actual_before_width = start_col.max(base.before_width as i32);
        let actual_overlay_width = overlay_width.max(overlay_actual_width as i32);
        let after_target = 0.max(total_width - actual_before_width - actual_overlay_width);
        let after_pad = 0.max(after_target - base.after_width as i32);

        let reset = SEGMENT_RESET;
        let result = format!(
            "{}{}{}{}{}{}{}{}",
            base.before,
            " ".repeat(before_pad as usize),
            reset,
            overlay_text,
            " ".repeat(overlay_pad as usize),
            reset,
            base.after,
            " ".repeat(after_pad as usize)
        );

        // CRITICAL: Always verify and truncate to terminal width (final
        // safeguard against width overflow which would crash the TUI).
        if visible_width(&result) <= total_width.max(0) as usize {
            return result;
        }
        slice_by_column(&result, 0, total_width.max(0) as usize, true)
    }
}

/// `TUI.SEGMENT_RESET` (tui.ts:1097): SGR reset + OSC 8 hyperlink terminator.
const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// Handle returned by [`Tui::show_overlay`] for controlling the overlay
/// (upstream `OverlayHandle`, tui.ts:218-231).
#[derive(Clone)]
pub struct OverlayHandle {
    tui: Tui,
    entry_id: u64,
}

impl OverlayHandle {
    /// Permanently remove the overlay (cannot be shown again) (tui.ts:513).
    pub fn hide(&self) {
        let entry_id = self.entry_id;
        self.tui
            .run_or_queue(move |inner| inner.overlay_hide(entry_id));
    }

    /// Temporarily hide or show the overlay (tui.ts:528).
    pub fn set_hidden(&self, hidden: bool) {
        let entry_id = self.entry_id;
        self.tui
            .run_or_queue(move |inner| inner.overlay_set_hidden(entry_id, hidden));
    }

    /// Check if the overlay is temporarily hidden (tui.ts:548). Returns false
    /// on lock contention.
    pub fn is_hidden(&self) -> bool {
        let entry_id = self.entry_id;
        self.tui
            .try_read(move |inner| {
                inner
                    .overlay_stack
                    .iter()
                    .find(|entry| entry.id == entry_id)
                    .is_some_and(|entry| entry.hidden)
            })
            .unwrap_or(false)
    }

    /// Focus this overlay and bring it to the visual front (tui.ts:549).
    pub fn focus(&self) {
        let entry_id = self.entry_id;
        self.tui
            .run_or_queue(move |inner| inner.overlay_focus(entry_id));
    }

    /// Release focus to the next visible capturing overlay or previous
    /// target, or to an explicit target when provided (tui.ts:555).
    pub fn unfocus(&self, options: Option<OverlayUnfocusOptions>) {
        let entry_id = self.entry_id;
        self.tui
            .run_or_queue(move |inner| inner.overlay_unfocus(entry_id, options));
    }

    /// Check if this overlay currently has focus (tui.ts:586). Returns false
    /// on lock contention.
    pub fn is_focused(&self) -> bool {
        let entry_id = self.entry_id;
        self.tui
            .try_read(move |inner| {
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

/// Shared render-schedule update (upstream `requestRender` /
/// `scheduleRender`, tui.ts:716-763).
fn schedule_render(schedule: &Arc<Mutex<RenderSchedule>>, force: bool, now: Instant) {
    let mut schedule = lock_shared(schedule);
    if force {
        // Upstream clears the pending timer and renders on the next tick,
        // bypassing the throttle.
        schedule.requested = true;
        schedule.deadline = Some(now);
        return;
    }
    if schedule.requested {
        return;
    }
    schedule.requested = true;
    let delay = match schedule.last_render_at {
        None => Duration::ZERO,
        Some(last) => {
            if now >= last {
                MIN_RENDER_INTERVAL.saturating_sub(now.duration_since(last))
            } else {
                // A future `last_render_at` (tests drive `tick` with synthetic
                // times) behaves like a negative JS elapsed: delay grows.
                MIN_RENDER_INTERVAL + last.duration_since(now)
            }
        }
    };
    schedule.deadline = Some(now + delay);
}

impl TuiInner {
    /// Lock the shared terminal. Never held across a blocking wait; see
    /// [`SharedTerminal`] for the lock-ordering rules.
    fn terminal(&self) -> MutexGuard<'_, Box<dyn Terminal + Send>> {
        lock_shared(&self.terminal)
    }

    /// `requestRender(true)` state reset (tui.ts:717-724); -1 sentinels
    /// trigger `widthChanged` / `heightChanged`, forcing a full clear.
    fn reset_render_state_for_force(&mut self) {
        self.previous_lines = Vec::new();
        self.previous_width = -1;
        self.previous_height = -1;
        self.cursor_row = 0;
        self.hardware_cursor_row = 0;
        self.max_lines_rendered = 0;
        self.previous_viewport_top = 0;
    }

    /// `requestRender` from inside the engine (tui.ts:716).
    fn request_render(&self, force: bool) {
        schedule_render(&self.schedule, force, Instant::now());
    }

    /// `setFocus` (tui.ts:368-370).
    fn set_focus(&mut self, component: Option<SharedComponent>) {
        self.set_focus_internal(component, OverlayFocusRestorePolicy::Clear);
    }

    /// `setFocusInternal` (tui.ts:372-435).
    fn set_focus_internal(
        &mut self,
        component: Option<SharedComponent>,
        overlay_focus_restore: OverlayFocusRestorePolicy,
    ) {
        let previous_focus = self.focused_component.clone();
        let mut next_focus = component;
        let previous_focused_overlay = previous_focus.as_ref().and_then(|previous| {
            self.overlay_stack
                .iter()
                .find(|entry| {
                    same_component(&entry.component, previous) && self.is_overlay_visible(entry)
                })
                .map(|entry| entry.id)
        });
        let next_focus_is_overlay = next_focus.as_ref().is_some_and(|next| {
            self.overlay_stack
                .iter()
                .any(|entry| same_component(&entry.component, next))
        });
        let restore_state = self.get_visible_overlay_focus_restore();

        if next_focus.is_some() && !next_focus_is_overlay {
            match &restore_state {
                OverlayFocusRestoreState::Blocked {
                    overlay_id,
                    component: overlay_component,
                    blocked_by,
                    resume,
                } if previous_focus
                    .as_ref()
                    .is_some_and(|previous| same_component(blocked_by, previous)) =>
                {
                    if matches!(resume, OverlayBlockedFocusResume::FocusTarget(_))
                        || !self.is_component_mounted(blocked_by)
                    {
                        next_focus = self.resolve_blocked_overlay_focus_resume(&restore_state);
                    } else if let Some(next) = next_focus.clone() {
                        self.overlay_focus_restore = OverlayFocusRestoreState::Blocked {
                            overlay_id: *overlay_id,
                            component: overlay_component.clone(),
                            blocked_by: next,
                            resume: resume.clone(),
                        };
                    }
                }
                _ => {
                    let restore_overlay_id = restore_state.overlay_id();
                    if let (Some(prev_overlay_id), Some(next)) =
                        (previous_focused_overlay, next_focus.clone())
                    {
                        if restore_overlay_id == Some(prev_overlay_id)
                            && !self.is_overlay_focus_ancestor(prev_overlay_id, &next)
                        {
                            if let Some(overlay_component) = self
                                .overlay_stack
                                .iter()
                                .find(|entry| entry.id == prev_overlay_id)
                                .map(|entry| entry.component.clone())
                            {
                                self.overlay_focus_restore = OverlayFocusRestoreState::Blocked {
                                    overlay_id: prev_overlay_id,
                                    component: overlay_component,
                                    blocked_by: next,
                                    resume: OverlayBlockedFocusResume::RestoreOverlay,
                                };
                            }
                        }
                    }
                }
            }
        } else if next_focus.is_none() {
            let blocked_resume = match &restore_state {
                OverlayFocusRestoreState::Blocked { blocked_by, .. }
                    if previous_focus
                        .as_ref()
                        .is_some_and(|previous| same_component(blocked_by, previous)) =>
                {
                    Some(self.resolve_blocked_overlay_focus_resume(&restore_state))
                }
                _ => None,
            };
            match blocked_resume {
                Some(target) => next_focus = target,
                None => {
                    if matches!(overlay_focus_restore, OverlayFocusRestorePolicy::Clear) {
                        self.clear_overlay_focus_restore();
                    }
                }
            }
        }

        if let Some(previous) = &self.focused_component {
            if let Some(focusable) = lock_component(previous).as_focusable_mut() {
                focusable.set_focused(false);
            }
        }

        self.focused_component = next_focus;

        if let Some(next) = &self.focused_component {
            if let Some(focusable) = lock_component(next).as_focusable_mut() {
                focusable.set_focused(true);
            }
        }

        let focused_overlay = self.focused_component.clone().and_then(|focused| {
            self.overlay_stack
                .iter()
                .find(|entry| {
                    same_component(&entry.component, &focused) && self.is_overlay_visible(entry)
                })
                .map(|entry| (entry.id, entry.component.clone()))
        });
        if let Some((overlay_id, component)) = focused_overlay {
            self.overlay_focus_restore = OverlayFocusRestoreState::Eligible {
                overlay_id,
                component,
            };
        }
    }

    /// `clearOverlayFocusRestore` (tui.ts:437-439).
    fn clear_overlay_focus_restore(&mut self) {
        self.overlay_focus_restore = OverlayFocusRestoreState::Inactive;
    }

    /// `clearOverlayFocusRestoreFor` (tui.ts:441-445).
    fn clear_overlay_focus_restore_for(&mut self, overlay_id: u64) {
        if self.overlay_focus_restore.overlay_id() == Some(overlay_id) {
            self.clear_overlay_focus_restore();
        }
    }

    /// `resolveBlockedOverlayFocusResume` (tui.ts:447-451).
    fn resolve_blocked_overlay_focus_resume(
        &mut self,
        restore_state: &OverlayFocusRestoreState,
    ) -> Option<SharedComponent> {
        if let OverlayFocusRestoreState::Blocked {
            component, resume, ..
        } = restore_state
        {
            match resume {
                OverlayBlockedFocusResume::RestoreOverlay => return Some(component.clone()),
                OverlayBlockedFocusResume::FocusTarget(target) => {
                    self.clear_overlay_focus_restore();
                    return target.clone();
                }
            }
        }
        None
    }

    /// `getVisibleOverlayFocusRestore` (tui.ts:453-460).
    fn get_visible_overlay_focus_restore(&self) -> OverlayFocusRestoreState {
        let Some(overlay_id) = self.overlay_focus_restore.overlay_id() else {
            return OverlayFocusRestoreState::Inactive;
        };
        match self
            .overlay_stack
            .iter()
            .find(|entry| entry.id == overlay_id)
        {
            Some(entry) if self.is_overlay_visible(entry) => self.overlay_focus_restore.clone(),
            _ => OverlayFocusRestoreState::Inactive,
        }
    }

    /// `isOverlayFocusAncestor` (tui.ts:462-471): walk the preFocus chain,
    /// cycle-safe.
    fn is_overlay_focus_ancestor(&self, entry_id: u64, component: &SharedComponent) -> bool {
        let mut visited: Vec<*const Mutex<Box<dyn Component>>> = Vec::new();
        let mut current = self
            .overlay_stack
            .iter()
            .find(|entry| entry.id == entry_id)
            .and_then(|entry| entry.pre_focus.clone());
        while let Some(current_component) = current {
            let ptr = Arc::as_ptr(&current_component);
            if visited.contains(&ptr) {
                return false;
            }
            visited.push(ptr);
            if same_component(&current_component, component) {
                return true;
            }
            current = self
                .overlay_stack
                .iter()
                .find(|entry| same_component(&entry.component, &current_component))
                .and_then(|entry| entry.pre_focus.clone());
        }
        false
    }

    /// `retargetOverlayPreFocus` (tui.ts:473-479).
    fn retarget_overlay_pre_focus(&mut self, removed_id: u64) {
        let Some(removed) = self
            .overlay_stack
            .iter()
            .find(|entry| entry.id == removed_id)
            .map(|entry| (entry.component.clone(), entry.pre_focus.clone()))
        else {
            return;
        };
        for entry in &mut self.overlay_stack {
            if entry.id != removed_id
                && entry
                    .pre_focus
                    .as_ref()
                    .is_some_and(|pre_focus| same_component(pre_focus, &removed.0))
            {
                entry.pre_focus = removed.1.clone();
            }
        }
    }

    /// `isComponentMounted` (tui.ts:481-483).
    fn is_component_mounted(&self, component: &SharedComponent) -> bool {
        self.children
            .iter()
            .any(|child| Self::contains_component(child, component))
    }

    /// `containsComponent` (tui.ts:485-489); the container walk goes through
    /// [`Component::shared_children`] (see header note). Cycle-safe like
    /// [`TuiInner::is_overlay_focus_ancestor`]: shared-child graphs can cycle.
    fn contains_component(root: &SharedComponent, target: &SharedComponent) -> bool {
        let mut visited: Vec<*const Mutex<Box<dyn Component>>> = Vec::new();
        Self::contains_component_walk(root, target, &mut visited)
    }

    fn contains_component_walk(
        root: &SharedComponent,
        target: &SharedComponent,
        visited: &mut Vec<*const Mutex<Box<dyn Component>>>,
    ) -> bool {
        if same_component(root, target) {
            return true;
        }
        let ptr = Arc::as_ptr(root);
        if visited.contains(&ptr) {
            return false;
        }
        visited.push(ptr);
        // Collect the children and release the component lock before
        // recursing: on a cyclic graph, recursing with the guard held would
        // re-lock the same component's Mutex and deadlock.
        let Some(children) = lock_component(root).shared_children() else {
            return false;
        };
        children
            .iter()
            .any(|child| Self::contains_component_walk(child, target, visited))
    }

    /// `showOverlay` body (tui.ts:496-510); the returned handle is built by
    /// the caller.
    fn show_overlay(
        &mut self,
        entry_id: u64,
        component: SharedComponent,
        options: Option<OverlayOptions>,
    ) {
        self.focus_order_counter += 1;
        let focus_order = self.focus_order_counter;
        let entry = OverlayStackEntry {
            id: entry_id,
            component: component.clone(),
            options,
            pre_focus: self.focused_component.clone(),
            hidden: false,
            focus_order,
        };
        self.overlay_stack.push(entry);
        // Only focus if overlay is actually visible.
        let entry = self.overlay_stack.last();
        let captures = entry.is_some_and(|entry| {
            !entry
                .options
                .as_ref()
                .is_some_and(|options| options.non_capturing)
                && self.is_overlay_visible(entry)
        });
        if captures {
            self.set_focus(Some(component));
        }
        self.terminal().hide_cursor();
        self.request_render(false);
    }

    /// `OverlayHandle.hide` body (tui.ts:513-527).
    fn overlay_hide(&mut self, entry_id: u64) {
        let Some(index) = self
            .overlay_stack
            .iter()
            .position(|entry| entry.id == entry_id)
        else {
            return;
        };
        self.clear_overlay_focus_restore_for(entry_id);
        self.retarget_overlay_pre_focus(entry_id);
        let entry = self.overlay_stack.remove(index);
        // Restore focus if this overlay had focus.
        if self
            .focused_component
            .as_ref()
            .is_some_and(|focused| same_component(focused, &entry.component))
        {
            let top_visible = self
                .get_topmost_visible_overlay()
                .map(|index| self.overlay_stack[index].component.clone());
            self.set_focus(top_visible.or(entry.pre_focus.clone()));
        }
        if self.overlay_stack.is_empty() {
            self.terminal().hide_cursor();
        }
        self.request_render(false);
    }

    /// `OverlayHandle.setHidden` body (tui.ts:528-547).
    fn overlay_set_hidden(&mut self, entry_id: u64, hidden: bool) {
        let Some(index) = self
            .overlay_stack
            .iter()
            .position(|entry| entry.id == entry_id)
        else {
            // Upstream mutates the detached entry (kept alive by the closure)
            // and still requests a render; the only observable effect is the
            // render request.
            self.request_render(false);
            return;
        };
        if self.overlay_stack[index].hidden == hidden {
            return;
        }
        self.overlay_stack[index].hidden = hidden;
        let (component, pre_focus, non_capturing) = {
            let entry = &self.overlay_stack[index];
            (
                entry.component.clone(),
                entry.pre_focus.clone(),
                entry
                    .options
                    .as_ref()
                    .is_some_and(|options| options.non_capturing),
            )
        };
        if hidden {
            self.clear_overlay_focus_restore_for(entry_id);
            // If this overlay had focus, move focus to next visible or preFocus.
            if self
                .focused_component
                .as_ref()
                .is_some_and(|focused| same_component(focused, &component))
            {
                let top_visible = self
                    .get_topmost_visible_overlay()
                    .map(|index| self.overlay_stack[index].component.clone());
                self.set_focus(top_visible.or(pre_focus));
            }
        } else {
            // Restore focus to this overlay when showing (if actually visible).
            let visible = self.is_overlay_visible(&self.overlay_stack[index]);
            if !non_capturing && visible {
                self.focus_order_counter += 1;
                self.overlay_stack[index].focus_order = self.focus_order_counter;
                self.set_focus(Some(component));
            }
        }
        self.request_render(false);
    }

    /// `OverlayHandle.focus` body (tui.ts:549-554).
    fn overlay_focus(&mut self, entry_id: u64) {
        let Some(index) = self
            .overlay_stack
            .iter()
            .position(|entry| entry.id == entry_id)
        else {
            return;
        };
        if !self.is_overlay_visible(&self.overlay_stack[index]) {
            return;
        }
        self.focus_order_counter += 1;
        self.overlay_stack[index].focus_order = self.focus_order_counter;
        let component = self.overlay_stack[index].component.clone();
        self.set_focus(Some(component));
        self.request_render(false);
    }

    /// `OverlayHandle.unfocus` body (tui.ts:555-585).
    fn overlay_unfocus(&mut self, entry_id: u64, options: Option<OverlayUnfocusOptions>) {
        let Some(entry_index) = self
            .overlay_stack
            .iter()
            .position(|entry| entry.id == entry_id)
        else {
            return;
        };
        let component = self.overlay_stack[entry_index].component.clone();
        let is_focused = self
            .focused_component
            .as_ref()
            .is_some_and(|focused| same_component(focused, &component));
        // Upstream reads the raw (unvalidated) restore state here.
        let restore_state = self.overlay_focus_restore.clone();
        let has_pending_restore = restore_state.overlay_id() == Some(entry_id);
        if !is_focused && !has_pending_restore {
            return;
        }
        if let OverlayFocusRestoreState::Blocked {
            overlay_id,
            component: overlay_component,
            blocked_by,
            ..
        } = &restore_state
        {
            if *overlay_id == entry_id
                && self
                    .focused_component
                    .as_ref()
                    .is_some_and(|focused| same_component(focused, blocked_by))
            {
                if let Some(options) = options {
                    self.overlay_focus_restore = OverlayFocusRestoreState::Blocked {
                        overlay_id: entry_id,
                        component: overlay_component.clone(),
                        blocked_by: blocked_by.clone(),
                        resume: OverlayBlockedFocusResume::FocusTarget(options.target),
                    };
                } else {
                    self.clear_overlay_focus_restore();
                }
                self.request_render(false);
                return;
            }
        }
        self.clear_overlay_focus_restore_for(entry_id);
        if is_focused || options.is_some() {
            let top_visible = self.get_topmost_visible_overlay();
            let fallback_target = match top_visible {
                Some(index) if self.overlay_stack[index].id != entry_id => {
                    Some(self.overlay_stack[index].component.clone())
                }
                _ => self.overlay_stack[entry_index].pre_focus.clone(),
            };
            let target = match options {
                Some(options) => options.target,
                None => fallback_target,
            };
            self.set_focus(target);
        }
        self.request_render(false);
    }

    /// `hideOverlay` (tui.ts:590-604): hide the topmost overlay and restore
    /// previous focus.
    fn hide_overlay(&mut self) {
        let Some(entry_id) = self.overlay_stack.last().map(|entry| entry.id) else {
            return;
        };
        self.clear_overlay_focus_restore_for(entry_id);
        self.retarget_overlay_pre_focus(entry_id);
        let entry = self.overlay_stack.pop();
        let Some(entry) = entry else { return };
        if self
            .focused_component
            .as_ref()
            .is_some_and(|focused| same_component(focused, &entry.component))
        {
            let top_visible = self
                .get_topmost_visible_overlay()
                .map(|index| self.overlay_stack[index].component.clone());
            self.set_focus(top_visible.or(entry.pre_focus.clone()));
        }
        if self.overlay_stack.is_empty() {
            self.terminal().hide_cursor();
        }
        self.request_render(false);
    }

    /// `hasOverlay` (tui.ts:607-609).
    fn has_overlay(&self) -> bool {
        self.overlay_stack
            .iter()
            .any(|entry| self.is_overlay_visible(entry))
    }

    /// `isOverlayVisible` (tui.ts:612-618).
    fn is_overlay_visible(&self, entry: &OverlayStackEntry) -> bool {
        if entry.hidden {
            return false;
        }
        if let Some(visible) = entry
            .options
            .as_ref()
            .and_then(|options| options.visible.as_ref())
        {
            // Separate statements: each `terminal()` guard must drop before
            // the next acquisition (std::sync::Mutex is not reentrant).
            let columns = i32::from(self.terminal().columns());
            let rows = i32::from(self.terminal().rows());
            return visible(columns, rows);
        }
        true
    }

    /// `getTopmostVisibleOverlay` (tui.ts:621-630): the visual-frontmost
    /// visible capturing overlay, if any.
    fn get_topmost_visible_overlay(&self) -> Option<usize> {
        let mut topmost: Option<usize> = None;
        for (index, overlay) in self.overlay_stack.iter().enumerate() {
            if overlay
                .options
                .as_ref()
                .is_some_and(|options| options.non_capturing)
                || !self.is_overlay_visible(overlay)
            {
                continue;
            }
            if topmost.is_none_or(|top| overlay.focus_order > self.overlay_stack[top].focus_order) {
                topmost = Some(index);
            }
        }
        topmost
    }

    /// `invalidate` override (tui.ts:632-635).
    fn invalidate(&mut self) {
        for child in &self.children {
            lock_component(child).invalidate();
        }
        for overlay in &self.overlay_stack {
            lock_component(&overlay.component).invalidate();
        }
    }

    /// `setShowHardwareCursor` (tui.ts:346-353).
    fn set_show_hardware_cursor(&mut self, enabled: bool) {
        if self.show_hardware_cursor == enabled {
            return;
        }
        self.show_hardware_cursor = enabled;
        if !enabled {
            self.terminal().hide_cursor();
        }
        self.request_render(false);
    }

    /// `setTerminalColorSchemeNotifications` (tui.ts:669-677).
    fn set_terminal_color_scheme_notifications(&mut self, enabled: bool) {
        if self.terminal_color_scheme_notifications_enabled == enabled {
            return;
        }
        self.terminal_color_scheme_notifications_enabled = enabled;
        if !self.stopped {
            self.terminal().write(if enabled {
                "\x1b[?2031h"
            } else {
                "\x1b[?2031l"
            });
        }
    }

    /// `queryCellSize` (tui.ts:679-687): only queried when the terminal
    /// supports images (cell size is only used for image rendering).
    fn query_cell_size(&mut self) {
        if get_capabilities().images.is_none() {
            return;
        }
        self.terminal().write("\x1b[16t");
    }

    /// `stop` (tui.ts:689-714). A pending render deadline deliberately
    /// survives (upstream leaks it as `renderRequested`; see header note) and
    /// fires on the first `tick` after `start()`.
    fn stop_internal(&mut self) {
        self.stopped = true;
        if self.terminal_color_scheme_notifications_enabled {
            self.terminal().write("\x1b[?2031l");
        }
        // Move cursor to the end of the content to prevent
        // overwriting/artifacts on exit.
        if !self.previous_lines.is_empty() {
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

        self.terminal().show_cursor();
        self.terminal().stop();
    }

    /// `handleInput` (tui.ts:765-839).
    fn handle_input(&mut self, data: &str) {
        if self.consume_osc11_background_response(data) {
            return;
        }
        if self.consume_terminal_color_scheme_report(data) {
            return;
        }

        let listener_data;
        let mut data = data;
        if !self.input_listeners.is_empty() {
            let mut current = data.to_string();
            for (_, listener) in &mut self.input_listeners {
                if let Some(result) = listener(&current) {
                    if result.consume {
                        return;
                    }
                    if let Some(replacement) = result.data {
                        current = replacement;
                    }
                }
            }
            if current.is_empty() {
                return;
            }
            listener_data = current;
            data = &listener_data;
        }

        // Consume terminal cell size responses without blocking unrelated input.
        if self.consume_cell_size_response(data) {
            return;
        }

        // Global debug key handler (Shift+Ctrl+D).
        if matches_key(data, "shift+ctrl+d") && self.on_debug.is_some() {
            if let Some(on_debug) = self.on_debug.as_mut() {
                on_debug();
            }
            return;
        }

        // If the focused component is an overlay, verify it's still visible
        // (visibility can change due to terminal resize or visible() callback).
        let focused_overlay_index = self.focused_component.as_ref().and_then(|focused| {
            self.overlay_stack
                .iter()
                .position(|entry| same_component(&entry.component, focused))
        });
        if let Some(index) = focused_overlay_index {
            if !self.is_overlay_visible(&self.overlay_stack[index]) {
                // Focused overlay is no longer visible, redirect to the
                // topmost visible overlay.
                let top_visible = self.get_topmost_visible_overlay();
                if let Some(top_index) = top_visible {
                    let component = self.overlay_stack[top_index].component.clone();
                    self.set_focus(Some(component));
                } else {
                    let pre_focus = self.overlay_stack[index].pre_focus.clone();
                    self.set_focus_internal(pre_focus, OverlayFocusRestorePolicy::Preserve);
                }
            }
        }

        let focus_is_overlay = self.focused_component.as_ref().is_some_and(|focused| {
            self.overlay_stack
                .iter()
                .any(|entry| same_component(&entry.component, focused))
        });
        if !focus_is_overlay {
            match self.get_visible_overlay_focus_restore() {
                OverlayFocusRestoreState::Eligible { component, .. } => {
                    self.set_focus(Some(component));
                }
                OverlayFocusRestoreState::Blocked {
                    component,
                    blocked_by,
                    resume,
                    ..
                } if self
                    .focused_component
                    .as_ref()
                    .is_none_or(|focused| !same_component(&blocked_by, focused)) =>
                {
                    match resume {
                        OverlayBlockedFocusResume::RestoreOverlay => {
                            self.set_focus(Some(component));
                        }
                        OverlayBlockedFocusResume::FocusTarget(target) => {
                            self.clear_overlay_focus_restore();
                            self.set_focus(target);
                        }
                    }
                }
                _ => {}
            }
        }

        // Pass input to the focused component (including Ctrl+C); the focused
        // component decides how to handle it. Upstream skips components
        // without a `handleInput` method; here the defaulted trait method is
        // always present (see header note).
        let Some(focused) = self.focused_component.clone() else {
            return;
        };
        // Filter out key release events unless the component opts in.
        let wants_release = lock_component(&focused).wants_key_release();
        if is_key_release(data) && !wants_release {
            return;
        }
        lock_component(&focused).handle_input(data);
        self.request_render(false);
    }

    /// `consumeOsc11BackgroundResponse` (tui.ts:841-863).
    fn consume_osc11_background_response(&mut self, data: &str) -> bool {
        if self.pending_osc11_background_replies == 0 {
            return false;
        }
        if !is_osc11_background_color_response(data) {
            return false;
        }

        let rgb = parse_osc11_background_color(data);
        self.pending_osc11_background_replies -= 1;
        if let Some(mut query) = self.pending_osc11_background_queries.pop_front() {
            if !query.settled {
                query.settled = true;
                query.deadline = None;
                if let Some(sender) = query.sender.take() {
                    let _ = sender.send(rgb);
                }
            }
        }
        true
    }

    /// `consumeTerminalColorSchemeReport` (tui.ts:865-875).
    fn consume_terminal_color_scheme_report(&mut self, data: &str) -> bool {
        let Some(scheme) = parse_terminal_color_scheme_report(data) else {
            return false;
        };
        for (_, listener) in &mut self.terminal_color_scheme_listeners {
            listener(scheme);
        }
        // Query promises resolve through a registered listener upstream; the
        // net effect is that every unsettled query resolves with the report.
        for query in &mut self.pending_terminal_color_scheme_queries {
            if !query.settled {
                query.settled = true;
                query.deadline = None;
                if let Some(sender) = query.sender.take() {
                    let _ = sender.send(Some(scheme));
                }
            }
        }
        // Reap settled queries (upstream's promises are GC'd once the
        // notification listener unsubscribes, tui.ts:1698-1718).
        self.pending_terminal_color_scheme_queries
            .retain(|query| !query.settled);
        true
    }

    /// `consumeCellSizeResponse` (tui.ts:877-895). Response format:
    /// `ESC [ 6 ; height ; width t`.
    fn consume_cell_size_response(&mut self, data: &str) -> bool {
        let Some(middle) = data
            .strip_prefix("\x1b[6;")
            .and_then(|rest| rest.strip_suffix('t'))
        else {
            return false;
        };
        let mut parts = middle.split(';');
        let (Some(height), Some(width), None) = (parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        let (Ok(height_px), Ok(width_px)) = (height.parse::<u32>(), width.parse::<u32>()) else {
            return false;
        };
        if height_px == 0 || width_px == 0 {
            return true;
        }

        set_cell_dimensions(CellDimensions {
            width_px: f64::from(width_px),
            height_px: f64::from(height_px),
        });
        // Invalidate all components so images re-render with correct dimensions.
        self.invalidate();
        self.request_render(false);
        true
    }

    /// `resolveOverlayLayout` (tui.ts:901-999).
    fn resolve_overlay_layout(
        options: Option<&OverlayOptions>,
        overlay_height: i32,
        term_width: i32,
        term_height: i32,
    ) -> OverlayLayout {
        // Parse margin (clamp to non-negative).
        let margin = match options.and_then(|options| options.margin) {
            Some(OverlayMarginSpec::Uniform(uniform)) => OverlayMargin {
                top: Some(uniform),
                right: Some(uniform),
                bottom: Some(uniform),
                left: Some(uniform),
            },
            Some(OverlayMarginSpec::Edges(edges)) => edges,
            None => OverlayMargin::default(),
        };
        let margin_top = margin.top.unwrap_or(0).max(0);
        let margin_right = margin.right.unwrap_or(0).max(0);
        let margin_bottom = margin.bottom.unwrap_or(0).max(0);
        let margin_left = margin.left.unwrap_or(0).max(0);

        // Available space after margins.
        let avail_width = 1.max(term_width - margin_left - margin_right);
        let avail_height = 1.max(term_height - margin_top - margin_bottom);

        // === Resolve width ===
        let mut width = parse_size_value(
            options.and_then(|options| options.width.as_ref()),
            term_width,
        )
        .unwrap_or(80.min(avail_width));
        if let Some(min_width) = options.and_then(|options| options.min_width) {
            width = width.max(min_width);
        }
        width = 1.max(width.min(avail_width));

        // === Resolve maxHeight ===
        let mut max_height = parse_size_value(
            options.and_then(|options| options.max_height.as_ref()),
            term_height,
        );
        if let Some(max_height_value) = max_height.as_mut() {
            *max_height_value = 1.max((*max_height_value).min(avail_height));
        }

        // Effective overlay height (may be clamped by maxHeight).
        let effective_height = match max_height {
            Some(max_height) => overlay_height.min(max_height),
            None => overlay_height,
        };

        // === Resolve position ===
        let anchor = options
            .and_then(|options| options.anchor)
            .unwrap_or_default();
        let row = match options.and_then(|options| options.row.as_ref()) {
            // Percentage: 0% = top, 100% = bottom (overlay stays within bounds).
            Some(SizeValue::Percent(percent)) if *percent >= 0.0 && percent.is_finite() => {
                let max_row = 0.max(avail_height - effective_height);
                margin_top + (max_row as f64 * (*percent / 100.0)).floor() as i32
            }
            // Absolute row position.
            Some(SizeValue::Absolute(row)) => *row,
            // Invalid percentage (upstream regex mismatch) falls back to center.
            Some(SizeValue::Percent(_)) => Self::resolve_anchor_row(
                OverlayAnchor::Center,
                effective_height,
                avail_height,
                margin_top,
            ),
            // Anchor-based (default: center).
            None => Self::resolve_anchor_row(anchor, effective_height, avail_height, margin_top),
        };
        let col = match options.and_then(|options| options.col.as_ref()) {
            // Percentage: 0% = left, 100% = right (overlay stays within bounds).
            Some(SizeValue::Percent(percent)) if *percent >= 0.0 && percent.is_finite() => {
                let max_col = 0.max(avail_width - width);
                margin_left + (max_col as f64 * (*percent / 100.0)).floor() as i32
            }
            // Absolute column position.
            Some(SizeValue::Absolute(col)) => *col,
            // Invalid percentage (upstream regex mismatch) falls back to center.
            Some(SizeValue::Percent(_)) => {
                Self::resolve_anchor_col(OverlayAnchor::Center, width, avail_width, margin_left)
            }
            // Anchor-based (default: center).
            None => Self::resolve_anchor_col(anchor, width, avail_width, margin_left),
        };

        // Apply offsets.
        let mut row = row;
        let mut col = col;
        if let Some(offset_y) = options.and_then(|options| options.offset_y) {
            row += offset_y;
        }
        if let Some(offset_x) = options.and_then(|options| options.offset_x) {
            col += offset_x;
        }

        // Clamp to terminal bounds (respecting margins).
        let row = margin_top.max(row.min(term_height - margin_bottom - effective_height));
        let col = margin_left.max(col.min(term_width - margin_right - width));

        OverlayLayout {
            width,
            row,
            col,
            max_height,
        }
    }

    /// `resolveAnchorRow` (tui.ts:1001-1016). `div_euclid` matches JS
    /// `Math.floor((availHeight - height) / 2)` for negative numerators.
    fn resolve_anchor_row(
        anchor: OverlayAnchor,
        height: i32,
        avail_height: i32,
        margin_top: i32,
    ) -> i32 {
        match anchor {
            OverlayAnchor::TopLeft | OverlayAnchor::TopCenter | OverlayAnchor::TopRight => {
                margin_top
            }
            OverlayAnchor::BottomLeft
            | OverlayAnchor::BottomCenter
            | OverlayAnchor::BottomRight => margin_top + avail_height - height,
            OverlayAnchor::LeftCenter | OverlayAnchor::Center | OverlayAnchor::RightCenter => {
                margin_top + (avail_height - height).div_euclid(2)
            }
        }
    }

    /// `resolveAnchorCol` (tui.ts:1018-1033).
    fn resolve_anchor_col(
        anchor: OverlayAnchor,
        width: i32,
        avail_width: i32,
        margin_left: i32,
    ) -> i32 {
        match anchor {
            OverlayAnchor::TopLeft | OverlayAnchor::LeftCenter | OverlayAnchor::BottomLeft => {
                margin_left
            }
            OverlayAnchor::TopRight | OverlayAnchor::RightCenter | OverlayAnchor::BottomRight => {
                margin_left + avail_width - width
            }
            OverlayAnchor::TopCenter | OverlayAnchor::Center | OverlayAnchor::BottomCenter => {
                margin_left + (avail_width - width).div_euclid(2)
            }
        }
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
                Self::resolve_overlay_layout(entry.options.as_ref(), 0, term_width, term_height);

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
            let layout = Self::resolve_overlay_layout(
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
                    result[index as usize] = Tui::composite_line_at(
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

    /// `logRedraw` (tui.ts:1331-1338); env var renamed to `PIR_DEBUG_REDRAW`
    /// (ADR-0001) and the log file to `pir-debug.log`.
    fn log_redraw(&self, reason: &str, new_lines_len: usize, height: i32) {
        if !env_flag_is_1(ENV_DEBUG_REDRAW) {
            return;
        }
        let log_path = self.log_directory.join("pir-debug.log");
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
                let crash_log_path = self.log_directory.join("pir-crash.log");
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
                self.stop_internal();

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
            let debug_dir = PathBuf::from("/tmp/pir-tui");
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

    /// Earliest deadline among unsettled introspection queries. Their
    /// timeouts fire from [`TuiInner::tick`] regardless of stopped, like
    /// upstream's `setTimeout` (tui.ts:1678-1686, 1715).
    fn next_query_deadline(&self) -> Option<Instant> {
        self.pending_osc11_background_queries
            .iter()
            .filter(|query| !query.settled)
            .filter_map(|query| query.deadline)
            .chain(
                self.pending_terminal_color_scheme_queries
                    .iter()
                    .filter(|query| !query.settled)
                    .filter_map(|query| query.deadline),
            )
            .min()
    }

    /// Whether any unsettled introspection query's deadline expired by `now`
    /// (an expired timeout is pending work; future deadlines are not).
    fn has_expired_query(&self, now: Instant) -> bool {
        let expired = |deadline: Option<Instant>| deadline.is_some_and(|deadline| now >= deadline);
        let osc11_expired = self
            .pending_osc11_background_queries
            .iter()
            .any(|query| !query.settled && expired(query.deadline));
        let scheme_expired = self
            .pending_terminal_color_scheme_queries
            .iter()
            .any(|query| !query.settled && expired(query.deadline));
        osc11_expired || scheme_expired
    }

    /// Fire expired deadlines (render throttle, introspection query timeouts).
    /// Called by [`Tui::tick`].
    fn tick(&mut self, now: Instant) {
        let should_render = {
            let mut schedule = lock_shared(&self.schedule);
            if schedule.requested && schedule.deadline.is_some_and(|deadline| now >= deadline) {
                if self.stopped {
                    // Upstream leaves renderRequested set when the timer fires
                    // after stop() (tui.ts:753); the deadline persists here and
                    // fires after a restart instead (see header note).
                    false
                } else {
                    schedule.requested = false;
                    schedule.deadline = None;
                    schedule.last_render_at = Some(now);
                    true
                }
            } else {
                false
            }
        };
        if should_render {
            self.do_render();
        }

        // Query timeouts fire regardless of stopped, like upstream's
        // setTimeout (tui.ts:1678-1686, 1715).
        for query in &mut self.pending_osc11_background_queries {
            if !query.settled && query.deadline.is_some_and(|deadline| now >= deadline) {
                query.settled = true;
                query.deadline = None;
                if let Some(sender) = query.sender.take() {
                    let _ = sender.send(None);
                }
            }
        }
        for query in &mut self.pending_terminal_color_scheme_queries {
            if !query.settled && query.deadline.is_some_and(|deadline| now >= deadline) {
                query.settled = true;
                query.deadline = None;
                if let Some(sender) = query.sender.take() {
                    let _ = sender.send(None);
                }
            }
        }
        // Reap settled entries (upstream relies on GC, tui.ts:1678-1718). The
        // osc11 reply match pops the front FIFO, so settled fronts must not
        // linger; the color-scheme Vec is only ever appended to otherwise.
        while self
            .pending_osc11_background_queries
            .front()
            .is_some_and(|query| query.settled)
        {
            self.pending_osc11_background_queries.pop_front();
        }
        self.pending_terminal_color_scheme_queries
            .retain(|query| !query.settled);
    }
}

/// `OverlayLayout` result (tui.ts:906): `{ width, row, col, maxHeight }`.
struct OverlayLayout {
    width: i32,
    row: i32,
    col: i32,
    max_height: Option<i32>,
}

// =============================================================================
// Tests
// =============================================================================

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
    //! the explicit `settle` helper driving `Tui::tick` with synthetic instants.

    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;

    use unicode_width::UnicodeWidthChar;

    use crate::terminal_image::{reset_capabilities_cache, set_capabilities, TerminalCapabilities};

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
    fn settle(tui: &Tui) -> Instant {
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
    fn render_and_flush(tui: &Tui) {
        tui.request_render(true);
        settle(tui);
    }

    /// `sendInput` + processing tick (input is queued to the inbox and drained
    /// by the tick; see header note on input delivery).
    fn send_input(terminal: &VirtualTerminal, tui: &Tui, data: &str) {
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
    /// touch the real `~/.pir/agent`, even if an env flag leaks across tests.
    fn temp_log_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "pir-tui-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn new_tui(terminal: &VirtualTerminal) -> Tui {
        Tui::with_options(Box::new(terminal.clone()), None, Some(temp_log_dir()))
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
        let tui = Tui::with_options(Box::new(terminal.clone()), None, Some(log_dir.clone()));
        let (component, lines) = test_component(&["test"]);
        let _ = lines;
        tui.add_child(component);
        tui.start();
        settle(&tui);

        let log = std::fs::read_to_string(log_dir.join("pir-debug.log"))
            .unwrap_or_else(|err| panic!("missing pir-debug.log: {err}"));
        assert!(
            log.contains("fullRender: first render"),
            "expected redraw log, got: {log}"
        );
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
    }

    // -------------------------------------------------------------------------
    // overlay-options.test.ts
    // -------------------------------------------------------------------------

    fn overlay_viewport(terminal: &VirtualTerminal, tui: &Tui) -> Vec<String> {
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
    }

    // -------------------------------------------------------------------------
    // regression-overlay-cjk-boundary.test.ts (TUI-level composite cases; the
    // extract_segments cases live in utils.rs tests)
    // -------------------------------------------------------------------------

    #[test]
    fn cjk_boundary_composites_overlay_at_requested_column_starting_inside_wide_grapheme() {
        let out = Tui::composite_line_at("abcd让EFGH", "│XX│", 5, 4, 20);
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
        let out = Tui::composite_line_at("abcd让EFGH", "│XX│", 4, 4, 20);
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
        tui.stop();
    }

    // -------------------------------------------------------------------------
    // terminal-colors.test.ts: "TUI.queryTerminalBackgroundColor"
    // -------------------------------------------------------------------------

    fn colors_test_setup() -> (
        VirtualTerminal,
        Tui,
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
        tui.stop();
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
        tui.stop();
    }

    #[test]
    fn osc11_consumes_unparseable_strict_replies_and_resolves_none() {
        let (terminal, tui, component_h, listener_inputs) = colors_test_setup();
        let query = tui.query_terminal_background_color(Duration::from_millis(1000));

        send_input(&terminal, &tui, "\x1b]11;not-a-color\x07");

        assert_eq!(query.blocking_recv().ok().flatten(), None);
        assert_eq!(*lock_shared(&listener_inputs), Vec::<String>::new());
        assert_eq!(component_h.inputs(), Vec::<String>::new());
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
        tui.start();
        terminal.clear_writes();
        tui.tick(rendered_at + Duration::from_millis(20));
        assert!(
            terminal.get_writes().contains('b'),
            "pending render fires on the first tick after restart"
        );
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
        tui.stop();
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
            "RenderHandle must not close a Tui -> children -> component -> Tui cycle"
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
        assert!(TuiInner::contains_component(&a, &a));
        assert!(
            !TuiInner::contains_component(&a, &target),
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
            tui: Tui,
        }

        impl Component for RecoveryComponent {
            fn render(&self, _width: usize) -> Vec<String> {
                vec!["x".to_string()]
            }

            // handle_input runs with the inner lock held, so the recovery
            // path takes its locked-Tui fallback.
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
        tui.stop();

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
        tui.stop();
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
        tui.stop();
    }

    #[test]
    fn show_hardware_cursor_makes_cursor_visible() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = Tui::with_options(Box::new(terminal.clone()), Some(true), Some(temp_log_dir()));
        let (editor, _marker_col) = cursor_editor();
        tui.add_child(editor.clone());
        tui.set_focus(Some(editor.clone()));
        tui.start();
        settle(&tui);

        assert!(!terminal.cursor_hidden(), "cursor visible when enabled");
        assert!(terminal.get_writes().contains("\x1b[?25h"));
        tui.stop();
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
        tui.stop();
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
                Some(InputListenerResult {
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
                Some(InputListenerResult {
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
        tui.stop();

        let terminal = VirtualTerminal::new(80, 24);
        let tui = new_tui(&terminal);
        let (component, component_h) = input_recorder();
        tui.add_child(component.clone());
        tui.set_focus(Some(component));
        tui.add_input_listener(Box::new(|_data: &str| {
            Some(InputListenerResult {
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
        tui.stop();
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
        tui.stop();
    }

    #[test]
    fn stop_moves_cursor_below_content_and_shows_it() {
        let terminal = VirtualTerminal::new(40, 10);
        let tui = new_tui(&terminal);
        let (component, _lines) = test_component(&["one", "two"]);
        tui.add_child(component);
        tui.start();
        settle(&tui);

        tui.stop();
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
}
