//! `TuiHandle` — a stable-reference proxy over the live TUI renderer, the
//! Rust equivalent of upstream's `createInteractiveTuiReference` `Proxy`
//! (interactive-mode.ts:355-383 @ b103937d3).
//!
//! Components that hold a long-lived TUI reference (the [`crate::components::editor::Editor`],
//! selectors) cannot re-bind their field when `switch_tui_mode` replaces the
//! renderer (interactive-mode.ts:788-837). Upstream solves this with a JS
//! `Proxy` that lazily forwards every property access to the current
//! renderer; [`TuiHandle`] does the same with an `Arc<Mutex<Renderer>>`.
//!
//! [`Renderer`] is a public two-variant enum wrapping [`TuiMainScreen`] or
//! [`TuiAltScreen`]. All forwarded methods match by variant and call the
//! corresponding inherent method. `swap_renderer` replaces the inner value
//! without changing the handle's identity, so every holder sees the new
//! renderer after a switch.
//!
//! Intentional differences: none beyond the general `tui.rs` / `tui-main-screen.rs`
//! / `tui_alt_screen.rs` header notes. The `Proxy`'s `set` / `has` / `getPrototypeOf`
//! traps have no Rust equivalent — all state mutation goes through methods.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use crate::components::scroll_view::ScrollbarMode;
use crate::terminal_colors::{RgbColor, TerminalColorScheme};
use crate::tui::{
    OverlayHandle, OverlayOptions, RenderHandle, SharedComponent, SharedTerminal,
    TerminalColorSchemeListener, Tui, TuiInputListener, TuiMode, TuiStopOptions,
};
use crate::tui_alt_screen::TuiAltScreen;
use crate::tui_main_screen::{TuiMainScreen, TuiMainScreenRenderState};

/// The live renderer behind a [`TuiHandle`] — one of the two concrete
/// implementations (tui.rs:565).
pub enum Renderer {
    Main(TuiMainScreen),
    Alt(TuiAltScreen),
}

/// A snapshot of the current renderer (cloned handle). Used internally to
/// dispatch method calls without holding the [`TuiHandle`] lock (avoids
/// re-entrancy deadlocks — both handles are `Arc`-based so clone is cheap).
enum RendererClone {
    Main(TuiMainScreen),
    Alt(TuiAltScreen),
}

/// Stable-reference proxy over the live TUI renderer
/// (interactive-mode.ts:355-383). Cloning a `TuiHandle` clones the inner
/// `Arc`, so all clones share the same renderer and all see a
/// [`TuiHandle::swap_renderer`] call.
#[derive(Clone)]
pub struct TuiHandle {
    inner: Arc<Mutex<Renderer>>,
}

impl TuiHandle {
    /// Wrap a [`TuiMainScreen`] (Regular mode).
    pub fn from_main(tui: TuiMainScreen) -> Self {
        TuiHandle {
            inner: Arc::new(Mutex::new(Renderer::Main(tui))),
        }
    }

    /// Wrap a [`TuiAltScreen`] (Fullscreen mode).
    pub fn from_alt(tui: TuiAltScreen) -> Self {
        TuiHandle {
            inner: Arc::new(Mutex::new(Renderer::Alt(tui))),
        }
    }

    /// Replace the inner renderer (interactive-mode.ts:820 `this.renderer = nextUi`).
    /// Used by `switch_tui_mode`; every clone of this handle sees the new
    /// renderer after this call.
    pub fn swap_renderer(&self, renderer: Renderer) {
        *lock_inner(&self.inner) = renderer;
    }

    /// Expose the constructor for `switch_tui_mode` (the enum is private, so
    /// callers build variants via these helpers).
    pub fn renderer_from_main(tui: TuiMainScreen) -> Renderer {
        Renderer::Main(tui)
    }

    pub fn renderer_from_alt(tui: TuiAltScreen) -> Renderer {
        Renderer::Alt(tui)
    }

    /// Upstream `readonly mode` (tui.ts:292).
    pub fn mode(&self) -> TuiMode {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.mode(),
            RendererClone::Alt(tui) => tui.mode(),
        }
    }

    /// Snapshot the current renderer as a clonable enum (lock + clone +
    /// release). Callers dispatch on the variant and invoke methods on the
    /// cloned handle **without holding the TuiHandle lock** — this avoids
    /// re-entrancy deadlocks when a renderer method (render, tick, etc.)
    /// triggers a component callback that re-enters the TuiHandle.
    fn renderer_clone(&self) -> RendererClone {
        match &*lock_inner(&self.inner) {
            Renderer::Main(tui) => RendererClone::Main(tui.clone()),
            Renderer::Alt(tui) => RendererClone::Alt(tui.clone()),
        }
    }

    /// Upstream `terminal` property (tui.ts:294).
    pub fn terminal(&self) -> SharedTerminal {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.terminal(),
            RendererClone::Alt(tui) => tui.terminal(),
        }
    }

    // --- container API ----------------------------------------------------

    /// Upstream `addChild` (tui.ts:297).
    pub fn add_child(&self, component: SharedComponent) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.add_child(component),
            RendererClone::Alt(tui) => tui.add_child(component),
        }
    }

    /// Upstream `removeChild` (tui.ts:298).
    pub fn remove_child(&self, component: &SharedComponent) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.remove_child(component),
            RendererClone::Alt(tui) => tui.remove_child(component),
        }
    }

    /// Upstream `clear` (tui.ts:299).
    pub fn clear(&self) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.clear(),
            RendererClone::Alt(tui) => tui.clear(),
        }
    }

    /// Rust addition: swap `old` for `new` preserving position
    /// ([`TuiMainScreen::swap_child`]).
    pub fn swap_child(&self, old: &SharedComponent, new: &SharedComponent) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.swap_child(old, new),
            RendererClone::Alt(tui) => tui.swap_child(old, new),
        }
    }

    /// Rust addition: insert child at index ([`TuiMainScreen::insert_child_at`]).
    pub fn insert_child_at(&self, index: usize, component: SharedComponent) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.insert_child_at(index, component),
            RendererClone::Alt(tui) => tui.insert_child_at(index, component),
        }
    }

    /// Rust addition: child position ([`TuiMainScreen::child_position`]).
    pub fn child_position(&self, component: &SharedComponent) -> Option<usize> {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.child_position(component),
            RendererClone::Alt(tui) => tui.child_position(component),
        }
    }

    /// Rust addition: child count ([`TuiMainScreen::children_len`]).
    pub fn children_len(&self) -> usize {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.children_len(),
            RendererClone::Alt(tui) => tui.children_len(),
        }
    }

    /// Rust addition (T32): snapshot the child list for re-mounting after a
    /// renderer swap (interactive-mode.ts:793).
    pub fn children(&self) -> Vec<SharedComponent> {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.children(),
            RendererClone::Alt(tui) => tui.children(),
        }
    }

    // --- focus ------------------------------------------------------------

    /// Upstream `setFocus` (tui.ts:304).
    pub fn set_focus(&self, component: Option<SharedComponent>) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.set_focus(component),
            RendererClone::Alt(tui) => tui.set_focus(component),
        }
    }

    /// Never-blocking `setFocus` variant — safe to call while holding a
    /// component-container lock (V13-05 FR-B R1): dispatches to the active
    /// renderer's nonblocking focus path, which never drains pending ops
    /// under the inner lock.
    pub fn set_focus_nonblocking(&self, component: Option<SharedComponent>) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.set_focus_nonblocking(component),
            RendererClone::Alt(tui) => tui.set_focus_nonblocking(component),
        }
    }

    /// Upstream `getFocusedComponent` (tui.ts:414-416 @ 4181f66).
    pub fn get_focused_component(&self) -> Option<SharedComponent> {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.get_focused_component(),
            RendererClone::Alt(tui) => tui.get_focused_component(),
        }
    }

    // --- settings ---------------------------------------------------------

    /// Upstream `getShowHardwareCursor` (tui.ts:300).
    pub fn get_show_hardware_cursor(&self) -> bool {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.get_show_hardware_cursor(),
            RendererClone::Alt(tui) => tui.get_show_hardware_cursor(),
        }
    }

    /// Upstream `setShowHardwareCursor` (tui.ts:301).
    pub fn set_show_hardware_cursor(&self, enabled: bool) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.set_show_hardware_cursor(enabled),
            RendererClone::Alt(tui) => tui.set_show_hardware_cursor(enabled),
        }
    }

    /// Upstream `getClearOnShrink` (tui.ts:302).
    pub fn get_clear_on_shrink(&self) -> bool {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.get_clear_on_shrink(),
            RendererClone::Alt(tui) => tui.get_clear_on_shrink(),
        }
    }

    /// Upstream `setClearOnShrink` (tui.ts:303).
    pub fn set_clear_on_shrink(&self, enabled: bool) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.set_clear_on_shrink(enabled),
            RendererClone::Alt(tui) => tui.set_clear_on_shrink(enabled),
        }
    }

    /// Set the debug callback (upstream `onDebug`, tui.ts:305).
    pub fn set_on_debug(&self, on_debug: Option<Box<dyn FnMut() + Send>>) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.set_on_debug(on_debug),
            RendererClone::Alt(tui) => tui.set_on_debug(on_debug),
        }
    }

    /// Take the debug callback off the current renderer so
    /// `switch_tui_mode` can move it to the new one
    /// (interactive-mode.ts:798, 816).
    pub fn take_on_debug(&self) -> Option<Box<dyn FnMut() + Send>> {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.take_on_debug(),
            RendererClone::Alt(tui) => tui.take_on_debug(),
        }
    }

    // --- overlay API ------------------------------------------------------

    /// Upstream `showOverlay` (tui.ts:305).
    pub fn show_overlay(
        &self,
        component: SharedComponent,
        options: Option<OverlayOptions>,
    ) -> OverlayHandle {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.show_overlay(component, options),
            RendererClone::Alt(tui) => tui.show_overlay(component, options),
        }
    }

    /// Upstream `hideOverlay` (tui.ts:306).
    pub fn hide_overlay(&self) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.hide_overlay(),
            RendererClone::Alt(tui) => tui.hide_overlay(),
        }
    }

    /// Upstream `hasOverlay` (tui.ts:307).
    pub fn has_overlay(&self) -> bool {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.has_overlay(),
            RendererClone::Alt(tui) => tui.has_overlay(),
        }
    }

    /// Upstream `get hasOverlayEntries` (tui.ts:358-360).
    pub fn has_overlay_entries(&self) -> bool {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.has_overlay_entries(),
            RendererClone::Alt(tui) => tui.has_overlay_entries(),
        }
    }

    // --- lifecycle --------------------------------------------------------

    /// Upstream `start` (tui.ts:308).
    pub fn start(&self) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.start(),
            RendererClone::Alt(tui) => tui.start(),
        }
    }

    /// Upstream `stop(options)` (tui.ts:309).
    pub fn stop(&self, options: TuiStopOptions) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.stop(options),
            RendererClone::Alt(tui) => tui.stop(options),
        }
    }

    /// Upstream `renderNow(force)` (tui.ts:310).
    pub fn render_now(&self, force: bool) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.render_now(force),
            RendererClone::Alt(tui) => tui.render_now(force),
        }
    }

    /// Upstream `requestRender(force)` (tui.ts:311).
    pub fn request_render(&self, force: bool) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.request_render(force),
            RendererClone::Alt(tui) => tui.request_render(force),
        }
    }

    /// Upstream `invalidate` override (tui.ts:686-689).
    pub fn invalidate(&self) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.invalidate(),
            RendererClone::Alt(tui) => tui.invalidate(),
        }
    }

    // --- listeners --------------------------------------------------------

    /// Upstream `addInputListener` (tui.ts:312).
    pub fn add_input_listener(&self, listener: TuiInputListener) -> u64 {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.add_input_listener(listener),
            RendererClone::Alt(tui) => tui.add_input_listener(listener),
        }
    }

    /// Upstream `removeInputListener` (tui.ts:313).
    pub fn remove_input_listener(&self, id: u64) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.remove_input_listener(id),
            RendererClone::Alt(tui) => tui.remove_input_listener(id),
        }
    }

    /// Upstream `onTerminalColorSchemeChange` (tui.ts:314).
    pub fn on_terminal_color_scheme_change(&self, listener: TerminalColorSchemeListener) -> u64 {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.on_terminal_color_scheme_change(listener),
            RendererClone::Alt(tui) => tui.on_terminal_color_scheme_change(listener),
        }
    }

    /// Upstream `setTerminalColorSchemeNotifications` (tui.ts:315).
    pub fn set_terminal_color_scheme_notifications(&self, enabled: bool) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.set_terminal_color_scheme_notifications(enabled),
            RendererClone::Alt(tui) => tui.set_terminal_color_scheme_notifications(enabled),
        }
    }

    /// Number of registered terminal color-scheme listeners (observability
    /// for the `switch_tui_mode` theme-listener rebind,
    /// interactive-mode.ts:827).
    pub fn terminal_color_scheme_listener_count(&self) -> usize {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.terminal_color_scheme_listener_count(),
            RendererClone::Alt(tui) => tui.terminal_color_scheme_listener_count(),
        }
    }

    // --- terminal introspection queries -----------------------------------

    /// Upstream `queryTerminalBackgroundColor` (tui.ts:316).
    pub fn query_terminal_background_color(
        &self,
        timeout: Duration,
    ) -> oneshot::Receiver<Option<RgbColor>> {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.query_terminal_background_color(timeout),
            RendererClone::Alt(tui) => tui.query_terminal_background_color(timeout),
        }
    }

    /// Upstream `queryTerminalColorScheme` (tui.ts:317).
    pub fn query_terminal_color_scheme(
        &self,
        timeout: Duration,
    ) -> oneshot::Receiver<Option<TerminalColorScheme>> {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.query_terminal_color_scheme(timeout),
            RendererClone::Alt(tui) => tui.query_terminal_color_scheme(timeout),
        }
    }

    // --- handle / introspection (Rust additions) --------------------------

    /// Access the terminal (upstream `public terminal`, tui.ts:296).
    pub fn with_terminal<R>(&self, f: impl FnOnce(&mut dyn crate::terminal::Terminal) -> R) -> R {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.with_terminal(f),
            RendererClone::Alt(tui) => tui.with_terminal(f),
        }
    }

    /// Cloneable capability handle for timer-driven components.
    pub fn render_handle(&self) -> RenderHandle {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.render_handle(),
            RendererClone::Alt(tui) => tui.render_handle(),
        }
    }

    /// Terminal row count (lock-free cache read).
    pub fn terminal_rows(&self) -> u16 {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.terminal_rows(),
            RendererClone::Alt(tui) => tui.terminal_rows(),
        }
    }

    // --- event-loop driving (Rust additions) ------------------------------

    /// Drive the TUI ([`TuiMainScreen::tick`] / [`TuiAltScreen::tick`]).
    pub fn tick(&self, now: Instant) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.tick(now),
            RendererClone::Alt(tui) => tui.tick(now),
        }
    }

    /// Blocking event pump ([`TuiMainScreen::pump`] / [`TuiAltScreen::pump`]).
    pub fn pump(&self, timeout: Option<Duration>) -> bool {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.pump(timeout),
            RendererClone::Alt(tui) => tui.pump(timeout),
        }
    }

    /// Next deadline ([`TuiMainScreen::next_deadline`] / [`TuiAltScreen::next_deadline`]).
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.next_deadline(),
            RendererClone::Alt(tui) => tui.next_deadline(),
        }
    }

    /// Pending work check ([`TuiMainScreen::has_pending_work`] /
    /// [`TuiAltScreen::has_pending_work`]).
    pub fn has_pending_work(&self) -> bool {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.has_pending_work(),
            RendererClone::Alt(tui) => tui.has_pending_work(),
        }
    }

    // --- mode-specific (T32) ----------------------------------------------

    /// Capture the main-screen render state for a renderer swap
    /// (tui-main-screen.ts:68-77 @ b103937d3). `None` when the current
    /// renderer is the alt screen (no capture/restore).
    pub fn capture_render_state(&self) -> Option<TuiMainScreenRenderState> {
        match self.renderer_clone() {
            RendererClone::Main(tui) => Some(tui.capture_render_state()),
            RendererClone::Alt(_) => None,
        }
    }

    /// Restore a previously captured main-screen render state
    /// (tui-main-screen.ts:80-89). No-op when the current renderer is the
    /// alt screen.
    pub fn restore_render_state(&self, state: TuiMainScreenRenderState) {
        match self.renderer_clone() {
            RendererClone::Main(tui) => tui.restore_render_state(state),
            RendererClone::Alt(_) => {}
        }
    }

    /// Set the viewport layout root (tui-alt-screen.ts:193-198). No-op when
    /// the current renderer is the main screen.
    pub fn set_layout_root(&self, root: Option<SharedComponent>) {
        match self.renderer_clone() {
            RendererClone::Main(_) => {}
            RendererClone::Alt(tui) => tui.set_layout_root(root),
        }
    }

    /// The primary scroll view of the alt-screen renderer
    /// (tui-alt-screen.ts:185). `None` on the main screen.
    pub fn get_primary_scroll_view(&self) -> Option<SharedComponent> {
        match self.renderer_clone() {
            RendererClone::Main(_) => None,
            RendererClone::Alt(tui) => Some(tui.get_primary_scroll_view()),
        }
    }

    /// Apply the fullscreen scrollbar setting to the alt screen's primary
    /// scroll view (interactive-mode.ts:1894-1896 @ 6129a353b). No-op on the
    /// main screen.
    pub fn set_fullscreen_scrollbar(&self, mode: ScrollbarMode) {
        if let RendererClone::Alt(tui) = self.renderer_clone() {
            tui.set_fullscreen_scrollbar(mode);
        }
    }
}

/// Lock the inner renderer (poisoning recovered like `lock_shared`).
fn lock_inner(inner: &Mutex<Renderer>) -> std::sync::MutexGuard<'_, Renderer> {
    inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
