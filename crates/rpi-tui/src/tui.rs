//! Port of `packages/tui/src/tui.ts` @ pi 0.82.1 (2efa728); the module
//! structure follows the upstream renderer split @ 4181f66 (T28):
//!
//! - `tui.rs` (this file): the public type surface — the component contract,
//!   overlay option types, input listener types — plus the object-safe
//!   [`Tui`] trait (upstream `TUI` interface, tui.ts:291-318), [`TuiMode`]
//!   (tui.ts:284), [`TuiStopOptions`] (tui.ts:286-289), the [`ViewportTui`]
//!   trait (tui.ts:322-325) and [`composite_tui_line`] (upstream
//!   `compositeTuiLine`, tui.ts:253-282).
//! - `tui_base.rs`: `TuiBase` (upstream `TuiBase`, tui.ts:331-1256) — the
//!   shared state and logic composed by renderer implementations: input
//!   dispatch, overlay stack, render scheduling, terminal introspection
//!   queries, start/stop common segments.
//! - `tui_main_screen.rs`: [`TuiMainScreen`]
//!   (upstream `TuiMainScreen`, tui-main-screen.ts:57) — the main-screen
//!   differential renderer.
//! - `tui_alt_screen.rs`: [`TuiAltScreen`](crate::tui_alt_screen::TuiAltScreen)
//!   (upstream `TuiAltScreen`, tui-alt-screen.ts:129) — the fullscreen
//!   alternate-screen renderer with an application-owned viewport (T31).
//!
//! Intentional differences: see below.
//!
//! The component contract section (Component / Focusable traits, is_focusable,
//! CURSOR_MARKER, RenderHandle, Container) is a FROZEN contract for components
//! and must only be extended, not broken (components are implemented against
//! it in parallel). Everything below it is the T11 engine port (T28: split
//! into the trait + base + main-screen renderer): the differential render
//! pipeline, overlay stack, focus-restore state machine, hardware cursor
//! positioning and terminal introspection queries.
//!
//! Differences from the upstream type layout (behavior unchanged):
//! - Upstream `Component` is a TS interface with optional `handleInput` /
//!   `wantsKeyRelease` members; here they are defaulted trait methods.
//! - Upstream `Focusable` is a structural `{ focused: boolean }` check
//!   (`isFocusable` = `"focused" in component`); here it is a sub-trait
//!   reached via `Component::as_focusable{,_mut}`.
//! - Components that re-render on a timer (Loader) hold a `RenderHandle`
//!   instead of a `&TUI` reference (upstream passes the TUI instance).
//! - Upstream `TUI extends Container`; here the [`Tui`] trait holds its own
//!   child list of [`SharedComponent`]s and mirrors the Container API
//!   (`add_child` / `remove_child` / `clear`). The frozen `Container`
//!   (Box-owned children) stays as-is for component composition.
//! - Upstream `TuiBase` uses class inheritance (`TuiMainScreen extends
//!   TuiBase`); Rust composes: `TuiMainScreenInner` holds a `TuiBase` and
//!   dereferences to it, and the upstream template hooks (`do_render`,
//!   `reset_render_state`, `before/after_terminal_stop`) are orchestrated by
//!   the subclass by hand.
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
//!   re-borrow the TUI mid-dispatch, so `TuiMainScreen` is a clonable handle
//!   around `Arc<Mutex<TuiMainScreenInner>>`; mutating calls made while the
//!   inner lock is held (from inside component `handle_input` / `render`, or
//!   from another thread) are queued and drained right after the in-progress
//!   dispatch/render completes, preserving upstream's observable ordering.
//!   Read-only queries (`is_focused`, `has_overlay`, ...) return a default on
//!   lock contention.
//! - Timers: upstream's `process.nextTick` + 16ms `setTimeout` render throttle
//!   and the introspection query timeouts become explicit deadlines, matching
//!   `terminal.rs` / `stdin_buffer.rs`: `request_render` only records intent,
//!   and the event loop drives `TuiMainScreen::tick` / `TuiMainScreen::pump`
//!   (`TuiMainScreen::next_deadline` yields the wait timeout). Multiple
//!   `request_render(true)` calls before the next tick coalesce into one
//!   render (upstream would run `doRender` once per nextTick callback).
//! - Input delivery: the `Terminal::start` callbacks push raw input into an
//!   inbox which `TuiMainScreen::tick` drains through the upstream
//!   `handleInput` flow. Same thread, same order — but delivery never happens
//!   inside `Terminal::pump`. `TuiMainScreen::pump` waits on the terminal's
//!   lock-free event source ([`Terminal::event_source`]), so neither the inner
//!   lock nor the terminal lock is held across the (possibly indefinite)
//!   event wait: cross-thread `TuiMainScreen::stop` /
//!   `TuiMainScreen::with_terminal` never block behind a parked driver.
//!   (Terminals without an event source — virtual test terminals — fall back
//!   to the in-lock [`Terminal::pump`].)
//! - `Component::handle_input` is a defaulted trait method, so the upstream
//!   `focusedComponent?.handleInput` existence check always passes: a focused
//!   component without input handling still gets a no-op call plus the
//!   trailing `requestRender`.
//! - Stopped render timers: upstream leaks `renderRequested === true` when a
//!   throttled render is pending at `stop()`, swallowing post-restart renders
//!   until a forced one; here the pending deadline survives `stop()` and fires
//!   on the first `tick` after `start()`. While stopped, the deferred render
//!   is not reported by `TuiMainScreen::next_deadline` /
//!   `TuiMainScreen::has_pending_work`, so the host loop can sleep until the
//!   restart.
//! - Env vars renamed per ADR-0001: `PI_HARDWARE_CURSOR` →
//!   `RPI_HARDWARE_CURSOR`, `PI_CLEAR_ON_SHRINK` → `RPI_CLEAR_ON_SHRINK`,
//!   `PI_DEBUG_REDRAW` → `RPI_DEBUG_REDRAW`, `PI_TUI_DEBUG` →
//!   `RPI_TUI_DEBUG`, `PI_CODING_AGENT_DIR` → `RPI_CODING_AGENT_DIR`; the
//!   default log directory is `~/.rpi/agent` (upstream `~/.pi/agent`). Log
//!   files: `pi-debug.log` → `rpi-debug.log`, `pi-crash.log` →
//!   `rpi-crash.log`; the `RPI_TUI_DEBUG` dump directory is `/tmp/rpi-tui`
//!   (upstream `/tmp/tui`).
//! - `SizeValue` is an enum; upstream's invalid-percentage-string fallback
//!   (`"abc%"` → anchor center) applies to negative/NaN percent values.
//! - `add_input_listener` / `on_terminal_color_scheme_change` return numeric
//!   ids paired with `remove_*` methods instead of unsubscribe closures.
//! - `query_terminal_background_color` / `query_terminal_color_scheme` return
//!   `oneshot::Receiver`s instead of Promises; their timeouts fire from
//!   `TuiMainScreen::tick`.
//! - The width-overflow path writes the crash log, stops the TUI and then
//!   panics with the upstream error message (upstream throws an uncaught
//!   Error). Redraw/crash log write failures are ignored (upstream would
//!   throw).
//! - T30 extension of the frozen component contract: `Component` gains two
//!   defaulted methods — `layout_node` (replaces upstream's `LAYOUT_NODE`
//!   symbol protocol, layout-node.ts:48-51) and `as_scroll_view` (scroll
//!   dispatch after hit-testing, same precedent as `as_focusable`). Both
//!   default to `None`, so existing components are unaffected.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::components::scroll_view::ScrollView;
use crate::layout_node::LayoutNode;
use crate::terminal::Terminal;
use crate::terminal_colors::{RgbColor, TerminalColorScheme};
use crate::terminal_image::is_image_line;
use crate::tui_main_screen::TuiMainScreen;
use crate::utils::{extract_segments, slice_by_column, slice_with_width, visible_width};

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

    /// Expandable-state toggle, mirroring upstream's `isExpandable`
    /// duck-typing (`"setExpanded" in component`, interactive-mode.ts:
    /// 176-178): the mode walks the loaded-resources and chat containers for
    /// children with expansion state (`setToolsExpanded`,
    /// interactive-mode.ts:4033-4048). Default: no expansion state — a no-op,
    /// exactly like upstream components without a `setExpanded` method.
    /// (T17 extension to the frozen contract; components with expansion state
    /// override this and forward to their inherent `set_expanded`.)
    fn set_expanded(&mut self, _expanded: bool) {}

    /// Layout node for the T30 layout engine: replaces upstream's
    /// `LAYOUT_NODE` symbol protocol (`getLayoutNode`, layout-node.ts:48-51).
    /// Stack/scroll containers override this; the returned node borrows the
    /// component, so the engine clones what it needs and releases the
    /// component lock before recursing into children. Default: not a layout
    /// container. (T30 extension to the frozen contract.)
    fn layout_node(&self) -> Option<LayoutNode<'_>> {
        None
    }

    /// Downcast-style accessor for scroll views, same precedent as
    /// [`Component::as_focusable`]: the layout engine (T31) calls scroll
    /// methods through this after hit-testing. Default: not a scroll view.
    /// (T30 extension to the frozen contract.)
    fn as_scroll_view(&self) -> Option<&ScrollView> {
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
pub(crate) fn lock_component(component: &SharedComponent) -> MutexGuard<'_, Box<dyn Component>> {
    component
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Upstream `componentA === componentB`.
pub(crate) fn same_component(a: &SharedComponent, b: &SharedComponent) -> bool {
    Arc::ptr_eq(a, b)
}

// =============================================================================
// Input listeners (tui.ts:76-77 @ 4181f66)
// =============================================================================

/// `TuiInputListenerResult` (tui.ts:76 @ 4181f66):
/// `{ consume?: boolean; data?: string }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiInputListenerResult {
    /// Stop routing this input (upstream `consume`).
    pub consume: bool,
    /// Replace the input for subsequent listeners and the focused component
    /// (upstream `data`).
    pub data: Option<String>,
}

/// `TuiInputListener` (tui.ts:77 @ 4181f66).
pub type TuiInputListener = Box<dyn FnMut(&str) -> Option<TuiInputListenerResult> + Send>;

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
pub(crate) fn parse_size_value(value: Option<&SizeValue>, reference_size: i32) -> Option<i32> {
    match value {
        None => None,
        Some(SizeValue::Absolute(value)) => Some(*value),
        Some(SizeValue::Percent(percent)) => {
            Some(((reference_size as f64 * percent) / 100.0).floor() as i32)
        }
    }
}

pub(crate) fn lock_shared<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `TUI.SEGMENT_RESET` (tui.ts:1097): SGR reset + OSC 8 hyperlink terminator.
pub(crate) const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// Upstream `compositeTuiLine` (tui.ts:253-282 @ 4181f66): composite overlay
/// content into a terminal line at a fixed column. Module-level public
/// function like upstream (the pre-split port was the associated function
/// `Tui::composite_line_at`, tui.ts:1185-1228 @ 2efa728 — same body; the
/// upstream regression test reached it through a TUI instance because it was
/// a private method there).
pub fn composite_tui_line(
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

// =============================================================================
// Tui trait (upstream `TUI` interface, tui.ts:291-318 @ 4181f66)
// =============================================================================

/// `TuiMode` (tui.ts:284 @ 4181f66): `"regular" | "fullscreen"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TuiMode {
    #[default]
    Regular,
    Fullscreen,
}

/// `TuiStopOptions` (tui.ts:286-289 @ 4181f66): `{ preserveScreen?: boolean }`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TuiStopOptions {
    /// Leave renderer output in place for another TUI taking over the same
    /// terminal (upstream `preserveScreen`).
    pub preserve_screen: bool,
}

/// The terminal behind its own mutex, shared between the TUI handle (blocking
/// `pump` waits and `with_terminal`) and the inner state (writes during
/// render/tick). Lock order is always `inner` → `terminal`; the terminal's
/// input callbacks only touch the inbox mutex, so no cycle is possible. The
/// blocking wait in `Terminal::pump` must never hold the inner lock: the
/// driver parks there between frames, and `std::sync::Mutex` is not FIFO-fair,
/// so a thread that re-locks in a tight loop starves any parked `lock_inner`
/// waiter (e.g. `with_terminal` from the run loop) for an unbounded time.
///
/// Replaces the upstream `TUI.terminal` property (tui.ts:294).
pub type SharedTerminal = Arc<Mutex<Box<dyn Terminal + Send>>>;

/// Object-safe TUI interface (upstream `TUI`, tui.ts:291-318 @ 4181f66).
///
/// Implemented by [`TuiMainScreen`] (`mode() == TuiMode::Regular`) and by
/// [`TuiAltScreen`](crate::tui_alt_screen::TuiAltScreen)
/// (`mode() == TuiMode::Fullscreen`, T31). The generic `with_terminal` accessor
/// stays an inherent method on the implementation (it is not object-safe).
/// Upstream `TUI extends Component` (render/handleInput members); here the
/// engine was never a `Component` (see the header note on the Container API),
/// so the trait carries only the engine surface. The Rust-specific event-loop
/// driving methods (`tick` / `pump` / `next_deadline` / `has_pending_work`)
/// and the `remove_*` counterparts of the id-returning listener registrations
/// stay inherent on the implementation, like `with_terminal`.
pub trait Tui: Send + Sync {
    /// Upstream `readonly mode` (tui.ts:292).
    fn mode(&self) -> TuiMode;

    /// Upstream `terminal` property (tui.ts:294), returning the shared
    /// terminal handle (see [`SharedTerminal`] for the lock-ordering rules).
    fn terminal(&self) -> SharedTerminal;

    /// Upstream `get fullRedraws` (tui.ts:296).
    fn full_redraws(&self) -> u64;

    // --- container API (upstream `TUI extends Container`) -----------------

    /// Upstream `addChild` (tui.ts:297).
    fn add_child(&self, component: SharedComponent);

    /// Upstream `removeChild` (identity comparison, tui.ts:298).
    fn remove_child(&self, component: &SharedComponent);

    /// Upstream `clear` (tui.ts:299).
    fn clear(&self);

    // --- settings ----------------------------------------------------------

    /// Upstream `getShowHardwareCursor` (tui.ts:300).
    fn get_show_hardware_cursor(&self) -> bool;

    /// Upstream `setShowHardwareCursor` (tui.ts:301).
    fn set_show_hardware_cursor(&self, enabled: bool);

    /// Upstream `getClearOnShrink` (tui.ts:302).
    fn get_clear_on_shrink(&self) -> bool;

    /// Upstream `setClearOnShrink` (tui.ts:303).
    fn set_clear_on_shrink(&self, enabled: bool);

    // --- focus -------------------------------------------------------------

    /// Upstream `setFocus` (tui.ts:304).
    fn set_focus(&self, component: Option<SharedComponent>);

    /// Upstream `getFocusedComponent` (tui.ts:414-416, made public in
    /// b103937d3). Returns `None` on lock contention.
    fn get_focused_component(&self) -> Option<SharedComponent>;

    // --- overlay API -------------------------------------------------------

    /// Upstream `showOverlay` (tui.ts:305). Returns a handle to control the
    /// overlay's visibility and focus.
    fn show_overlay(
        &self,
        component: SharedComponent,
        options: Option<OverlayOptions>,
    ) -> OverlayHandle;

    /// Upstream `hideOverlay`: hide the topmost overlay and restore previous
    /// focus (tui.ts:306).
    fn hide_overlay(&self);

    /// Upstream `hasOverlay` (tui.ts:307). Returns false on lock contention.
    fn has_overlay(&self) -> bool;

    /// Upstream `get hasOverlayEntries` (tui.ts:358-360, made public in
    /// b103937d3): any overlay entries, visible or not. Returns false on lock
    /// contention.
    fn has_overlay_entries(&self) -> bool;

    // --- lifecycle ---------------------------------------------------------

    /// Upstream `start` (tui.ts:308).
    fn start(&self);

    /// Upstream `stop(options)` (tui.ts:309). `preserve_screen` skips the
    /// cursor-to-content-end sequence so a taking-over TUI can reuse the
    /// screen (upstream `beforeTerminalStop`, tui-main-screen.ts:101-109).
    fn stop(&self, options: TuiStopOptions);

    /// Upstream `renderNow(force)` (tui.ts:310, 757-763): synchronous render
    /// bypassing the throttle; `force` resets the render state first. No-op
    /// while stopped (the render short-circuits in `do_render`).
    fn render_now(&self, force: bool);

    /// Upstream `requestRender(force)` (tui.ts:311): records render intent;
    /// the actual render happens on the next tick (immediate for `force`,
    /// 16ms-throttled otherwise).
    fn request_render(&self, force: bool);

    // --- listeners ----------------------------------------------------------

    /// Upstream `addInputListener` (tui.ts:312). Returns an id for
    /// `remove_input_listener` (upstream returns an unsubscribe closure).
    fn add_input_listener(&self, listener: TuiInputListener) -> u64;

    /// Upstream `removeInputListener` (tui.ts:313); by id (see above).
    fn remove_input_listener(&self, id: u64);

    /// Upstream `onTerminalColorSchemeChange` (tui.ts:314). Returns an id
    /// paired with the implementation's `remove_*` method (upstream returns
    /// an unsubscribe closure).
    fn on_terminal_color_scheme_change(&self, listener: TerminalColorSchemeListener) -> u64;

    /// Upstream `setTerminalColorSchemeNotifications` (tui.ts:315).
    fn set_terminal_color_scheme_notifications(&self, enabled: bool);

    // --- terminal introspection queries -------------------------------------

    /// Upstream `queryTerminalBackgroundColor` (tui.ts:316); resolves with
    /// the parsed RGB color, or `None` on timeout / parse failure.
    fn query_terminal_background_color(
        &self,
        timeout: Duration,
    ) -> oneshot::Receiver<Option<RgbColor>>;

    /// Upstream `queryTerminalColorScheme` (tui.ts:317).
    fn query_terminal_color_scheme(
        &self,
        timeout: Duration,
    ) -> oneshot::Receiver<Option<TerminalColorScheme>>;

    /// Upstream `invalidate` override (tui.ts:686-689 via `Component`):
    /// children plus overlays.
    fn invalidate(&self);
}

/// `ViewportTUI` (tui.ts:322-325 @ 4181f66): a [`Tui`] that renders into a
/// managed viewport instead of the main screen. Implemented by
/// [`TuiAltScreen`](crate::tui_alt_screen::TuiAltScreen) (T31). Upstream only
/// declares `setLayoutRoot` on the interface; the remaining members are the
/// public `TuiAltScreen` viewport API (tui-alt-screen.ts:185-191, 351-364,
/// 380-383), exposed here so `dyn ViewportTui` consumers can drive the
/// viewport. (Upstream brands implementors with the `VIEWPORT_TUI` symbol +
/// `isViewportTUI` guard, tui.ts:320/327-329; a Rust `dyn Tui` can be
/// downcast or the trait used directly, so no brand here.)
pub trait ViewportTui: Tui {
    /// Upstream `setLayoutRoot` (tui.ts:324).
    fn set_layout_root(&self, root: Option<SharedComponent>);

    /// Upstream `get viewportTop` (tui-alt-screen.ts:185-187).
    fn viewport_top(&self) -> usize;

    /// Upstream `get isFollowingOutput` (tui-alt-screen.ts:189-191).
    fn is_following_output(&self) -> bool;

    /// Upstream `scrollBy` (tui-alt-screen.ts:351-354).
    fn scroll_by(&self, lines: i64);

    /// Upstream `scrollToTop` (tui-alt-screen.ts:356-359).
    fn scroll_to_top(&self);

    /// Upstream `scrollToBottom` (tui-alt-screen.ts:361-364).
    fn scroll_to_bottom(&self);

    /// Upstream `flash` (tui-alt-screen.ts:380-383): show a transient message
    /// in the alternate-screen flash stack. `duration_ms` mirrors the
    /// `durationMs` parameter; `None` uses the upstream default
    /// ([`DEFAULT_DURATION_MS`](crate::components::alt_screen_flash::DEFAULT_DURATION_MS)).
    fn flash(&self, message: &str, duration_ms: Option<u64>);
}

/// Handle returned by `show_overlay` for controlling the overlay
/// (upstream `OverlayHandle`, tui.ts:218-231).
///
/// The entry operations go through a per-renderer ops trait so the same
/// handle type works for every [`Tui`] implementation (T31 added the
/// alternate-screen renderer; upstream's handle closes over the live TUI
/// object).
#[derive(Clone)]
pub struct OverlayHandle {
    ops: Arc<dyn OverlayHandleOps>,
    entry_id: u64,
}

/// Per-renderer backing operations for [`OverlayHandle`] (see its doc).
pub(crate) trait OverlayHandleOps: Send + Sync {
    fn hide(&self, entry_id: u64);
    fn set_hidden(&self, entry_id: u64, hidden: bool);
    fn is_hidden(&self, entry_id: u64) -> bool;
    fn focus(&self, entry_id: u64);
    fn unfocus(&self, entry_id: u64, options: Option<OverlayUnfocusOptions>);
    fn is_focused(&self, entry_id: u64) -> bool;
}

impl OverlayHandle {
    /// Constructed by the renderer's `show_overlay` implementation.
    pub(crate) fn new(ops: Arc<dyn OverlayHandleOps>, entry_id: u64) -> Self {
        Self { ops, entry_id }
    }

    /// Permanently remove the overlay (cannot be shown again) (tui.ts:513).
    pub fn hide(&self) {
        self.ops.hide(self.entry_id);
    }

    /// Temporarily hide or show the overlay (tui.ts:528).
    pub fn set_hidden(&self, hidden: bool) {
        self.ops.set_hidden(self.entry_id, hidden);
    }

    /// Check if the overlay is temporarily hidden (tui.ts:548). Returns false
    /// on lock contention.
    pub fn is_hidden(&self) -> bool {
        self.ops.is_hidden(self.entry_id)
    }

    /// Focus this overlay and bring it to the visual front (tui.ts:549).
    pub fn focus(&self) {
        self.ops.focus(self.entry_id);
    }

    /// Release focus to the next visible capturing overlay or previous
    /// target, or to an explicit target when provided (tui.ts:555).
    pub fn unfocus(&self, options: Option<OverlayUnfocusOptions>) {
        self.ops.unfocus(self.entry_id, options);
    }

    /// Check if this overlay currently has focus (tui.ts:586). Returns false
    /// on lock contention.
    pub fn is_focused(&self) -> bool {
        self.ops.is_focused(self.entry_id)
    }
}

impl OverlayHandleOps for TuiMainScreen {
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
