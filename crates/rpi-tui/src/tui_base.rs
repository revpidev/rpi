//! Port of the `TuiBase` abstract class in `packages/tui/src/tui.ts`
//! (tui.ts:331-1256 @ 4181f66): the shared state and behavior of every TUI
//! renderer — child list, focus and overlay stack (incl. the focus-restore
//! state machine), input dispatch, render scheduling, terminal introspection
//! queries (OSC 11 / color scheme / cell size) and the start/stop common
//! segments.
//!
//! Rust has no inheritance, so the base is composed: renderer inner states
//! (currently only [`TuiMainScreenInner`](crate::tui_main_screen::TuiMainScreenInner))
//! hold a `TuiBase` and dereference to it. The upstream template hooks
//! (`do_render`, `reset_render_state`, `before/after_terminal_stop`,
//! tui.ts:372-382) are not virtual methods here; each renderer orchestrates
//! them by hand around the base's common segments.
//!
//! Upstream's `getMountedRoots` indirection (tui.ts:531-534, overridden by
//! the alt-screen renderer) is not ported: the only renderer so far walks
//! `children` directly, matching the pre-split behavior.
//!
//! Intentional differences (in addition to the `tui.rs` header notes):
//! - Upstream `stop()` cancels the pending render timer; here the deadline
//!   deliberately survives `stop` and fires after a restart (see the `tui.rs`
//!   header note on stopped render timers).
//! - `OverlayHandle`'s entry operations live here as `overlay_*` methods; the
//!   public handle type stays in `tui.rs` next to the [`Tui`](crate::tui::Tui)
//!   trait.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::AtomicU16;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use crate::keys::{is_key_release, matches_key};
use crate::terminal::{InputHandler, ResizeHandler, Terminal};
use crate::terminal_colors::{
    is_osc11_background_color_response, parse_osc11_background_color,
    parse_terminal_color_scheme_report, RgbColor, TerminalColorScheme,
};
use crate::terminal_image::{get_capabilities, set_cell_dimensions, CellDimensions};
use crate::tui::{
    lock_component, lock_shared, parse_size_value, same_component, Component, OverlayAnchor,
    OverlayMargin, OverlayMarginSpec, OverlayOptions, OverlayUnfocusOptions, SharedComponent,
    SharedTerminal, SizeValue, TerminalColorSchemeListener, TuiInputListener,
};

/// `OverlayStackEntry` (tui.ts:233-239). The `id` replaces upstream's entry
/// object identity.
pub(crate) struct OverlayStackEntry {
    pub(crate) id: u64,
    pub(crate) component: SharedComponent,
    pub(crate) options: Option<OverlayOptions>,
    pub(crate) pre_focus: Option<SharedComponent>,
    pub(crate) hidden: bool,
    pub(crate) focus_order: u64,
}

/// `OverlayBlockedFocusResume` (tui.ts:241).
#[derive(Clone)]
pub(crate) enum OverlayBlockedFocusResume {
    RestoreOverlay,
    FocusTarget(Option<SharedComponent>),
}

/// `OverlayFocusRestoreState` (tui.ts:242-250). `Eligible` / `Blocked` carry
/// the entry id plus the entry's component (upstream stores the live entry
/// object; the component reference is what `resolve` paths actually read).
#[derive(Clone)]
pub(crate) enum OverlayFocusRestoreState {
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
pub(crate) enum OverlayFocusRestorePolicy {
    Clear,
    Preserve,
}

/// `PendingOsc11BackgroundQuery` (tui.ts:92-96); the `setTimeout` timer is an
/// explicit deadline fired by [`TuiMainScreen::tick`] (see header note). Settled
/// entries are reaped (upstream relies on GC).
pub(crate) struct PendingOsc11BackgroundQuery {
    pub(crate) settled: bool,
    pub(crate) sender: Option<oneshot::Sender<Option<RgbColor>>>,
    pub(crate) deadline: Option<Instant>,
}

/// Pending `query_terminal_color_scheme` (tui.ts:1698-1718); same explicit
/// deadline treatment, same reaping of settled entries.
pub(crate) struct PendingTerminalColorSchemeQuery {
    pub(crate) settled: bool,
    pub(crate) sender: Option<oneshot::Sender<Option<TerminalColorScheme>>>,
    pub(crate) deadline: Option<Instant>,
}

/// `OverlayLayout` result (tui.ts:906): `{ width, row, col, maxHeight }`.
pub(crate) struct OverlayLayout {
    pub(crate) width: i32,
    pub(crate) row: i32,
    pub(crate) col: i32,
    pub(crate) max_height: Option<i32>,
}

// =============================================================================
// Environment / logging helpers
// =============================================================================

/// `TUI.MIN_RENDER_INTERVAL_MS` (tui.ts:309).
const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(16);

/// `RPI_HARDWARE_CURSOR` (ADR-0001 rename of `PI_HARDWARE_CURSOR`, tui.ts:312).
const ENV_HARDWARE_CURSOR: &str = "RPI_HARDWARE_CURSOR";
/// `RPI_CLEAR_ON_SHRINK` (ADR-0001 rename of `PI_CLEAR_ON_SHRINK`, tui.ts:313).
const ENV_CLEAR_ON_SHRINK: &str = "RPI_CLEAR_ON_SHRINK";
/// `RPI_CODING_AGENT_DIR` (ADR-0001 rename of `PI_CODING_AGENT_DIR`,
/// tui.ts:332).
const ENV_CODING_AGENT_DIR: &str = "RPI_CODING_AGENT_DIR";

/// Upstream `process.env.X === "1"` checks.
pub(crate) fn env_flag_is_1(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}

/// Default log directory: `~/.rpi/agent` (upstream `~/.pi/agent`, tui.ts:332).
fn default_log_directory() -> PathBuf {
    home_dir().join(".rpi").join("agent")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Upstream `renderRequested` / `renderTimer` / `lastRenderAt` (tui.ts:306-308)
/// as an explicit deadline driven by [`TuiMainScreen::tick`].
pub(crate) struct RenderSchedule {
    pub(crate) requested: bool,
    pub(crate) deadline: Option<Instant>,
    pub(crate) last_render_at: Option<Instant>,
}

/// Lock-free terminal size cache shared between `Tui` and `TuiInner`.
#[derive(Clone, Default)]
pub(crate) struct TerminalSizeCache {
    pub(crate) rows: Arc<AtomicU16>,
    pub(crate) columns: Arc<AtomicU16>,
}

/// Shared render-schedule update (upstream `requestRender` /
/// `scheduleRender`, tui.ts:765-817 @ 4181f66). The `force` path is upstream
/// `requestRender(true)` = `resetRenderState` (caller's side) +
/// `requestImmediateRender` (29d9f087c): reset happens in
/// `TuiMainScreen::request_render`, the immediate arm is the deadline set
/// here.
pub(crate) fn schedule_render(schedule: &Arc<Mutex<RenderSchedule>>, force: bool, now: Instant) {
    let mut schedule = lock_shared(schedule);
    if force {
        // Upstream `requestImmediateRender` cancels the pending timer and
        // renders on the next tick, bypassing the throttle (tui.ts:776-779).
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

/// Shared TUI state and behavior (upstream `TuiBase`, tui.ts:331-1256
/// @ 4181f66). Composed by renderer inner states; see the module header.
pub(crate) struct TuiBase {
    pub(crate) terminal: SharedTerminal,
    pub(crate) children: Vec<SharedComponent>,
    pub(crate) focused_component: Option<SharedComponent>,
    /// JS `Set<TuiInputListener>`: insertion-ordered, deduplicated by id.
    pub(crate) input_listeners: Vec<(u64, TuiInputListener)>,
    pub(crate) on_debug: Option<Box<dyn FnMut() + Send>>,
    pub(crate) show_hardware_cursor: bool,
    pub(crate) clear_on_shrink: bool,
    pub(crate) full_redraw_count: u64,
    pub(crate) stopped: bool,
    pub(crate) pending_osc11_background_replies: usize,
    pub(crate) pending_osc11_background_queries: VecDeque<PendingOsc11BackgroundQuery>,
    pub(crate) terminal_color_scheme_listeners: Vec<(u64, TerminalColorSchemeListener)>,
    pub(crate) terminal_color_scheme_notifications_enabled: bool,
    pub(crate) pending_terminal_color_scheme_queries: Vec<PendingTerminalColorSchemeQuery>,
    pub(crate) log_directory: PathBuf,
    pub(crate) focus_order_counter: u64,
    pub(crate) overlay_stack: Vec<OverlayStackEntry>,
    pub(crate) overlay_focus_restore: OverlayFocusRestoreState,
    pub(crate) schedule: Arc<Mutex<RenderSchedule>>,
    pub(crate) size_cache: TerminalSizeCache,
}

impl TuiBase {
    /// Upstream `TuiBase` constructor (tui.ts:363-370). `terminal`,
    /// `schedule` and `size_cache` are created by the renderer handle first
    /// (they are shared with it) and passed in here.
    pub(crate) fn new(
        terminal: SharedTerminal,
        show_hardware_cursor: Option<bool>,
        log_directory: Option<PathBuf>,
        schedule: Arc<Mutex<RenderSchedule>>,
        size_cache: TerminalSizeCache,
    ) -> TuiBase {
        TuiBase {
            terminal,
            children: Vec::new(),
            focused_component: None,
            input_listeners: Vec::new(),
            on_debug: None,
            show_hardware_cursor: show_hardware_cursor
                .unwrap_or_else(|| env_flag_is_1(ENV_HARDWARE_CURSOR)),
            clear_on_shrink: env_flag_is_1(ENV_CLEAR_ON_SHRINK),
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
            schedule,
            size_cache,
        }
    }
    /// Lock the shared terminal. Never held across a blocking wait; see
    /// [`SharedTerminal`] for the lock-ordering rules.
    pub(crate) fn terminal(&self) -> MutexGuard<'_, Box<dyn Terminal + Send>> {
        lock_shared(&self.terminal)
    }

    /// `requestRender` from inside the engine (tui.ts:716).
    pub(crate) fn request_render(&self, force: bool) {
        schedule_render(&self.schedule, force, Instant::now());
    }

    /// `requestImmediateRender` (tui.ts:776-791 @ 4181f66, 29d9f087c): cancel
    /// any queued throttled deadline and arm the render for the current tick.
    /// The upstream `process.nextTick` dedup/preemption details
    /// (`immediateRenderScheduled`, the second `cancelRenderTimer` inside the
    /// callback) are covered by the tick model: a tick drains all queued input
    /// before firing deadlines, so several inputs in one tick share this single
    /// render, and `deadline = now` preempts a previously queued throttled
    /// frame (tui.ts:784-786).
    fn request_immediate_render(&self) {
        let mut schedule = lock_shared(&self.schedule);
        // Upstream `cancelRenderTimer` (tui.ts:777).
        schedule.deadline = None;
        schedule.requested = true;
        schedule.deadline = Some(Instant::now());
    }

    /// `setFocus` (tui.ts:368-370).
    pub(crate) fn set_focus(&mut self, component: Option<SharedComponent>) {
        self.set_focus_internal(component, OverlayFocusRestorePolicy::Clear);
    }

    /// `setFocusInternal` (tui.ts:372-435).
    pub(crate) fn set_focus_internal(
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
    pub(crate) fn clear_overlay_focus_restore(&mut self) {
        self.overlay_focus_restore = OverlayFocusRestoreState::Inactive;
    }

    /// `clearOverlayFocusRestoreFor` (tui.ts:441-445).
    pub(crate) fn clear_overlay_focus_restore_for(&mut self, overlay_id: u64) {
        if self.overlay_focus_restore.overlay_id() == Some(overlay_id) {
            self.clear_overlay_focus_restore();
        }
    }

    /// `resolveBlockedOverlayFocusResume` (tui.ts:447-451).
    pub(crate) fn resolve_blocked_overlay_focus_resume(
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
    pub(crate) fn get_visible_overlay_focus_restore(&self) -> OverlayFocusRestoreState {
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
    pub(crate) fn is_overlay_focus_ancestor(
        &self,
        entry_id: u64,
        component: &SharedComponent,
    ) -> bool {
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
    pub(crate) fn retarget_overlay_pre_focus(&mut self, removed_id: u64) {
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
    pub(crate) fn is_component_mounted(&self, component: &SharedComponent) -> bool {
        self.children
            .iter()
            .any(|child| Self::contains_component(child, component))
    }

    /// `containsComponent` (tui.ts:485-489); the container walk goes through
    /// [`Component::shared_children`] (see header note). Cycle-safe like
    /// [`TuiInner::is_overlay_focus_ancestor`]: shared-child graphs can cycle.
    pub(crate) fn contains_component(root: &SharedComponent, target: &SharedComponent) -> bool {
        let mut visited: Vec<*const Mutex<Box<dyn Component>>> = Vec::new();
        Self::contains_component_walk(root, target, &mut visited)
    }

    pub(crate) fn contains_component_walk(
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
    pub(crate) fn show_overlay(
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
    pub(crate) fn overlay_hide(&mut self, entry_id: u64) {
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
    pub(crate) fn overlay_set_hidden(&mut self, entry_id: u64, hidden: bool) {
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
    pub(crate) fn overlay_focus(&mut self, entry_id: u64) {
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
    pub(crate) fn overlay_unfocus(
        &mut self,
        entry_id: u64,
        options: Option<OverlayUnfocusOptions>,
    ) {
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
    pub(crate) fn hide_overlay(&mut self) {
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
    pub(crate) fn has_overlay(&self) -> bool {
        self.overlay_stack
            .iter()
            .any(|entry| self.is_overlay_visible(entry))
    }

    /// `isOverlayVisible` (tui.ts:612-618).
    pub(crate) fn is_overlay_visible(&self, entry: &OverlayStackEntry) -> bool {
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
    pub(crate) fn get_topmost_visible_overlay(&self) -> Option<usize> {
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
    pub(crate) fn invalidate(&mut self) {
        for child in &self.children {
            lock_component(child).invalidate();
        }
        for overlay in &self.overlay_stack {
            lock_component(&overlay.component).invalidate();
        }
    }

    /// `setShowHardwareCursor` (tui.ts:346-353).
    pub(crate) fn set_show_hardware_cursor(&mut self, enabled: bool) {
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
    pub(crate) fn set_terminal_color_scheme_notifications(&mut self, enabled: bool) {
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
    pub(crate) fn query_cell_size(&mut self) {
        if get_capabilities().images.is_none() {
            return;
        }
        self.terminal().write("\x1b[16t");
    }

    /// `get hasOverlayEntries` (tui.ts:358-360 @ 4181f66; made public in
    /// b103937d3): any overlay entries, visible or not.
    pub(crate) fn has_overlay_entries(&self) -> bool {
        !self.overlay_stack.is_empty()
    }

    /// `TuiBase.start` (tui.ts:691-705 @ 4181f66). The subclass hooks
    /// `beforeTerminalStart` / `afterTerminalStart` (tui.ts:376-378) are
    /// no-ops for the main screen, so the handle calls this directly after
    /// building the terminal callbacks.
    pub(crate) fn start_common(&mut self, on_input: InputHandler, on_resize: ResizeHandler) {
        self.stopped = false;
        self.terminal().start(on_input, on_resize);
        self.terminal().hide_cursor();
        if self.terminal_color_scheme_notifications_enabled {
            self.terminal().write("\x1b[?2031h");
        }
        self.query_cell_size();
        self.request_render(false);
    }

    /// `TuiBase.stop` common prefix (tui.ts:746-750 @ 4181f66): mark stopped
    /// and disable color scheme notifications. Upstream also cancels the
    /// render timer here; the pending deadline deliberately survives `stop`
    /// instead (see the `tui.rs` header note on stopped render timers).
    pub(crate) fn begin_stop(&mut self) {
        self.stopped = true;
        if self.terminal_color_scheme_notifications_enabled {
            self.terminal().write("\x1b[?2031l");
        }
    }

    /// `TuiBase.stop` common tail (tui.ts:752-753 @ 4181f66), run after the
    /// subclass `beforeTerminalStop` hook. The upstream `afterTerminalStop`
    /// hook (tui.ts:382) is a no-op for the main screen.
    pub(crate) fn end_stop(&mut self) {
        self.terminal().show_cursor();
        self.terminal().stop();
    }

    /// Render-deadline check of the throttle schedule (upstream
    /// `scheduleRender` timer body, tui.ts:805-812 @ 4181f66). The caller
    /// runs its `do_render` when this returns true.
    pub(crate) fn take_render_due(&mut self, now: Instant) -> bool {
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
    }

    /// Fire expired introspection query timeouts and reap settled entries.
    /// Timeouts fire regardless of stopped, like upstream's `setTimeout`
    /// (tui.ts:1678-1686, 1715).
    pub(crate) fn fire_expired_queries(&mut self, now: Instant) {
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

    /// `handleInput` (tui.ts:765-839).
    pub(crate) fn handle_input(&mut self, data: &str) {
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
        // Keyboard input is latency-sensitive. Avoid the throttled path,
        // where even a zero delay can take a full 16ms tick on Windows
        // (tui.ts:891-893 @ 4181f66, 29d9f087c).
        self.request_immediate_render();
    }

    /// `consumeOsc11BackgroundResponse` (tui.ts:841-863).
    pub(crate) fn consume_osc11_background_response(&mut self, data: &str) -> bool {
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
    pub(crate) fn consume_terminal_color_scheme_report(&mut self, data: &str) -> bool {
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
    pub(crate) fn consume_cell_size_response(&mut self, data: &str) -> bool {
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
    pub(crate) fn resolve_overlay_layout(
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
    pub(crate) fn resolve_anchor_row(
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
    pub(crate) fn resolve_anchor_col(
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

    /// Earliest deadline among unsettled introspection queries. Their
    /// timeouts fire from [`TuiInner::tick`] regardless of stopped, like
    /// upstream's `setTimeout` (tui.ts:1678-1686, 1715).
    pub(crate) fn next_query_deadline(&self) -> Option<Instant> {
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
    pub(crate) fn has_expired_query(&self, now: Instant) -> bool {
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
}
