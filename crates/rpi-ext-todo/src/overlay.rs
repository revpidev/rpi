//! The persistent todo widget above the editor.
//!
//! Port of upstream `packages/rpiv-todo/todo-overlay.ts` @ `0fdf4f8` plus
//! the `updateTodoOverlay` closure of `index.ts`.
//!
//! ## Shape mapping (design §4.1): factory + requestRender → declarative
//! setWidget re-send
//!
//! Upstream registers a render **factory** (`setWidget(key, factory,
//! {placement:"aboveEditor"})`) and refreshes by asking the host to
//! re-render (`tui.requestRender([forced])`) — the host then calls
//! `render(width)` per frame. rpi's `set_widget(key, content, options)` is
//! declarative (`WidgetContent::Lines`), so the rpi port keeps an overlay
//! state machine (registered / collapsed / fade-out sets / fingerprint)
//! and **re-sends the assembled line set** on every refresh:
//!
//! - **Dirty-mark throttling (FR-H)**: when the assembled line set is
//!   byte-identical to the last sent set, the re-send is skipped — the
//!   output is unchanged (equivalent to the host deduping an equal-value
//!   `setWidget`), only the wasted host re-render is eliminated. The
//!   collapsed↔expanded height step **bypasses** the fingerprint
//!   (upstream `requestRender(true)`).
//! - **Width**: the assembly function is width-parameterized exactly like
//!   upstream `renderWidget(theme, width)` (golden frames snapshot it at
//!   widths 60/100/200). The event-driven re-send path has no render
//!   width of its own (the declarative ABI carries none), so it assembles
//!   at [`RENDER_WIDTH_UNBOUNDED`] — truncation is a no-op there and
//!   over-wide lines wrap at the host's `Text` component (the established
//!   behavior for every `Lines` widget; task-file §7.3 ruling 3).
//! - **Theme**: re-read from `ctx.ui.theme` on every assembly (never
//!   cached across frames — upstream invalidate-on-theme-change has the
//!   same effect: the next assembly reads the new theme, and the changed
//!   line set defeats the fingerprint, forcing the re-send).
//! - **Lazy overlay module pre-warm** (`PREWARM_DELAY_MS`) is a jiti/TS
//!   dynamic-import cost optimization with no rpi counterpart ([N/A],
//!   requirements §4.4); the construction stays task-gated
//!   (`update_todo_overlay`'s `!overlay && !has_visible` guard).

use std::collections::HashSet;

use serde_json::{json, Value};

use crate::config::{self, COLLAPSE_KEY_OFF};
use crate::i18n::I18n;
use crate::state::selectors::{
    select_has_active, select_overlay_layout, select_show_task_ids, select_todo_counts,
};
use crate::state::TaskState;
use crate::tool::types::{Task, TaskStatus};
use crate::view::{format_overlay_task_line, AnsiTheme, TodoTheme};
use crate::HostCall;

/// Widget key — verbatim (`WIDGET_KEY`).
pub const WIDGET_KEY: &str = "rpiv-todos";

/// The event-path assembly width: no truncation (`usize::MAX` makes
/// `truncate_to_width` a pass-through); over-wide lines wrap at the host.
pub const RENDER_WIDTH_UNBOUNDED: usize = usize::MAX;

/// The trailing-row connector replaced onto the last task row when
/// nothing overflows (`├─` → `└─`).
const ROW_PREFIX: &str = "├─";
const LAST_ROW_PREFIX: &str = "└─";

// ---------------------------------------------------------------------------
// Host plumbing
// ---------------------------------------------------------------------------

/// Push (or remove) the widget through `ui.setWidget`. `None` removes.
fn push_widget(host: &dyn HostCall, lines: Option<&[String]>) -> Result<(), crate::HostError> {
    host.call(
        "ui.setWidget",
        json!({
            "key": WIDGET_KEY,
            "content": lines.map(|lines| Value::Array(lines.iter().map(|line| json!(line)).collect())),
            "placement": "aboveEditor",
        }),
    )
    .map(|_| ())
}

/// Read the current theme (`ctx.ui.theme`), re-read on every assembly;
/// transport failures degrade to an unstyled theme.
fn read_theme(host: &dyn HostCall) -> AnsiTheme {
    match host.call("ui.theme", json!({})) {
        Ok(theme) => AnsiTheme::from_theme_json(&theme),
        Err(_) => AnsiTheme::from_theme_json(&Value::Null),
    }
}

/// `ctx.ui.getToolsExpanded()` — transport failures (or hosts without the
/// API) answer `None` so the configured budget applies (upstream optional
/// chaining `this.uiCtx?.getToolsExpanded?.() === true`).
fn get_tools_expanded(host: &dyn HostCall) -> Option<bool> {
    host.call("ui.getToolsExpanded", json!({}))
        .ok()
        .and_then(|value| value.as_bool())
}

// ---------------------------------------------------------------------------
// TodoOverlay
// ---------------------------------------------------------------------------

/// The overlay state machine (upstream `TodoOverlay`). Host interaction is
/// funneled through a [`HostCall`] reference so the pure display-state
/// transitions stay unit-testable against a scripted host.
pub struct TodoOverlay {
    /// Whether a UI context is bound (upstream `this.uiCtx`). The
    /// controller drops the overlay wholesale on foreground teardown, so
    /// this only flips false via [`TodoOverlay::dispose`].
    ui_bound: bool,
    /// Widget registration state (upstream `widgetRegistered`).
    widget_registered: bool,
    /// Completed tasks displayed this turn, to hide on the next
    /// `agent_start` (upstream `completedTaskIdsPendingHide`).
    completed_pending_hide: HashSet<i64>,
    /// Completed tasks hidden by a previous turn (upstream
    /// `hiddenCompletedTaskIds`).
    hidden_completed: HashSet<i64>,
    /// Last seen `nextId` — a decrease (e.g. `clear` resetting it to 1)
    /// resets the completed-display state (upstream `lastNextId`).
    last_next_id: Option<i64>,
    /// Collapsed two-line form toggle (upstream `collapsed`).
    collapsed: bool,
    /// Last sent line set — the dirty-mark fingerprint (FR-H; upstream
    /// has no equivalent because `requestRender` is idempotent).
    last_lines: Option<Vec<String>>,
}

impl Default for TodoOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoOverlay {
    pub fn new() -> Self {
        TodoOverlay {
            ui_bound: false,
            widget_registered: false,
            completed_pending_hide: HashSet::new(),
            hidden_completed: HashSet::new(),
            last_next_id: None,
            collapsed: false,
            last_lines: None,
        }
    }

    /// Bind the UI context (upstream `setUICtx`'s identity comparison):
    /// the controller constructs a FRESH overlay per foreground claim
    /// (teardown drops it wholesale), so "a different ctx" maps to
    /// construction and this bind only flips an unbound overlay; repeat
    /// binds are idempotent.
    pub fn set_ui_ctx(&mut self) {
        if !self.ui_bound {
            self.ui_bound = true;
            self.widget_registered = false;
            self.last_lines = None;
        }
    }

    /// Whether the widget is currently registered (upstream
    /// `isRegistered` — the shortcut handler's guard).
    pub fn is_registered(&self) -> bool {
        self.widget_registered
    }

    /// The snapshot read every assembly (upstream `getSnapshot`): the
    /// foreground slot plus the completed-display bookkeeping — a
    /// `nextId` decrease resets it, and ids that are no longer completed
    /// (reverted or tombstoned) leave both sets.
    fn get_snapshot(&mut self) -> TaskState {
        let state = crate::state::store::store().render_state();
        if self.last_next_id.is_some_and(|last| state.next_id < last) {
            self.reset_completed_display_state();
        }
        self.last_next_id = Some(state.next_id);
        let completed: HashSet<i64> = state
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Completed)
            .map(|task| task.id)
            .collect();
        self.completed_pending_hide
            .retain(|id| completed.contains(id));
        self.hidden_completed.retain(|id| completed.contains(id));
        state
    }

    /// Visible overlay tasks: non-deleted and not hidden-completed
    /// (upstream `selectOverlayTasks`).
    fn select_overlay_tasks(&self, snapshot: &TaskState) -> Vec<Task> {
        snapshot
            .tasks
            .iter()
            .filter(|task| {
                task.status != TaskStatus::Deleted
                    && !(task.status == TaskStatus::Completed
                        && self.hidden_completed.contains(&task.id))
            })
            .cloned()
            .collect()
    }

    /// Assemble the widget line set (upstream `renderWidget(theme, width)`
    /// — the `truncate` closure applies `truncateToWidth(line, width, "…")`).
    /// Also performs the completed-display tracking (skipped when the
    /// collapsed short-circuit fires — skipping the tracking when nothing
    /// is rendered is correctness, not optimization, upstream comment).
    fn render_widget_lines(
        &mut self,
        host: &dyn HostCall,
        i18n: &I18n,
        width: usize,
    ) -> Vec<String> {
        let snapshot = self.get_snapshot();
        let overlay_tasks = self.select_overlay_tasks(&snapshot);
        if overlay_tasks.is_empty() {
            return Vec::new();
        }
        let overlay_state = TaskState {
            tasks: overlay_tasks.clone(),
            next_id: snapshot.next_id,
        };
        let theme = read_theme(host);
        let truncate = |line: String| rpi_tui::utils::truncate_to_width(&line, width, "…", false);

        let counts = select_todo_counts(&overlay_state);
        let has_active = select_has_active(&overlay_state);
        let show_ids = select_show_task_ids(&overlay_state);

        let heading_color = if has_active { "accent" } else { "dim" };
        let heading_icon = if has_active { "●" } else { "○" };
        let heading_text = format!(
            "{} ({}/{})",
            i18n.t("overlay.heading", "Todos"),
            counts.completed,
            counts.total
        );
        let heading = truncate(format!(
            "{} {}",
            theme.fg(heading_color, heading_icon),
            theme.fg(heading_color, &heading_text)
        ));

        // Collapsed view: heading + dim expand hint + trailing spacer —
        // short-circuits before the budget math and the completed-display
        // tracking. The hint splices the resolved key into the {key}
        // placeholder (per-render, like the row budget); the "off"
        // sentinel (reachable mid-session after a config edit) renders a
        // static collapsed label instead.
        if self.collapsed {
            let key = config::resolve_collapse_key();
            let hint = if key == COLLAPSE_KEY_OFF {
                i18n.t("overlay.collapsed", "collapsed").to_owned()
            } else {
                i18n.t("overlay.expandHint", "{key} to expand")
                    .replace("{key}", &key)
            };
            let lines = vec![
                heading,
                truncate(format!(
                    "{} {}",
                    theme.fg("dim", LAST_ROW_PREFIX),
                    theme.fg("dim", &hint)
                )),
            ];
            return with_trailing_spacer(lines);
        }

        let mut lines = vec![heading];
        // Budget for content rows (heading + tasks/summary); the rendered
        // widget is one line taller — withTrailingSpacer appends a blank
        // row below the panel. The tool-output expansion mode is read on
        // every render so its shortcut also expands this live widget.
        let body_budget = if get_tools_expanded(host) == Some(true) {
            overlay_tasks.len()
        } else {
            config::get_max_widget_lines().saturating_sub(1)
        };
        let layout = select_overlay_layout(&overlay_state, body_budget);
        for task in &layout.visible {
            lines.push(truncate(format!(
                "{} {}",
                theme.fg("dim", ROW_PREFIX),
                format_overlay_task_line(task, &theme, show_ids)
            )));
        }

        let newly_displayed: Vec<i64> = overlay_tasks
            .iter()
            .filter(|task| {
                task.status == TaskStatus::Completed
                    && !self.completed_pending_hide.contains(&task.id)
                    && !self.hidden_completed.contains(&task.id)
            })
            .map(|task| task.id)
            .collect();
        for id in newly_displayed {
            self.completed_pending_hide.insert(id);
        }

        if layout.hidden_completed == 0 && layout.truncated_tail == 0 {
            let last = lines.len() - 1;
            lines[last] = lines[last].replacen(ROW_PREFIX, LAST_ROW_PREFIX, 1);
            return with_trailing_spacer(lines);
        }

        let total_hidden = layout.hidden_completed + layout.truncated_tail;
        let mut overflow_parts: Vec<String> = Vec::new();
        if layout.hidden_completed > 0 {
            overflow_parts.push(format!(
                "{} {}",
                layout.hidden_completed,
                i18n.format_status_label(TaskStatus::Completed)
            ));
        }
        if layout.truncated_tail > 0 {
            overflow_parts.push(format!(
                "{} {}",
                layout.truncated_tail,
                i18n.format_status_label(TaskStatus::Pending)
            ));
        }
        let more = i18n.t("overlay.more", "more");
        let summary = if overflow_parts.is_empty() {
            format!("+{total_hidden} {more}")
        } else {
            format!("+{total_hidden} {more} ({})", overflow_parts.join(", "))
        };
        lines.push(truncate(format!(
            "{} {}",
            theme.fg("dim", LAST_ROW_PREFIX),
            theme.fg("dim", &summary)
        )));
        with_trailing_spacer(lines)
    }

    /// Refresh entry (upstream `update()`): unregister when nothing is
    /// visible, otherwise assemble + throttled re-send (registering on
    /// the first send). Same-value re-sends are skipped (FR-H); the
    /// collapsed toggle bypasses the fingerprint through
    /// [`TodoOverlay::toggle_collapse`].
    pub fn update(&mut self, host: &dyn HostCall, i18n: &I18n) {
        if !self.ui_bound {
            return;
        }
        let snapshot = self.get_snapshot();
        let visible = self.select_overlay_tasks(&snapshot);
        if visible.is_empty() {
            if self.widget_registered {
                if let Err(error) = push_widget(host, None) {
                    tracing::warn!(%error, "rpiv-todo: widget removal rejected");
                }
                self.widget_registered = false;
                self.last_lines = None;
            }
            return;
        }
        self.send_lines(host, i18n, RENDER_WIDTH_UNBOUNDED, false);
    }

    /// Assemble at `width` and push unless the fingerprint matches (or
    /// `force` bypasses it). Registration follows a successful push.
    fn send_lines(&mut self, host: &dyn HostCall, i18n: &I18n, width: usize, force: bool) {
        let lines = self.render_widget_lines(host, i18n, width);
        if !force && self.last_lines.as_deref() == Some(lines.as_slice()) {
            self.widget_registered = true;
            return;
        }
        match push_widget(host, Some(&lines)) {
            Ok(()) => {
                self.widget_registered = true;
                self.last_lines = Some(lines);
            }
            Err(error) => {
                // A rejected push leaves the registration state alone: the
                // upstream factory registration cannot "fail" after
                // acceptance, and the next refresh retries the send.
                tracing::warn!(%error, "rpiv-todo: setWidget rejected");
            }
        }
    }

    /// Move tasks displayed in previous turns into the hidden set
    /// (upstream `hideCompletedTasksFromPreviousTurn`, fired on
    /// `agent_start`). No-op when nothing is pending hide; otherwise the
    /// migration re-renders — the re-assembled line set re-sends while the
    /// widget stays registered (an emptied panel renders `[]`, exactly
    /// like upstream's `render()` → `[]` under a still-registered widget;
    /// the unregister only happens on the NEXT `update()`).
    pub fn hide_completed_tasks_from_previous_turn(&mut self, host: &dyn HostCall, i18n: &I18n) {
        if self.completed_pending_hide.is_empty() {
            return;
        }
        for id in self.completed_pending_hide.drain() {
            self.hidden_completed.insert(id);
        }
        if !self.widget_registered {
            // Upstream `this.tui?.requestRender()` — a no-op before the
            // factory has been instantiated.
            return;
        }
        self.send_lines(host, i18n, RENDER_WIDTH_UNBOUNDED, false);
    }

    /// Toggle the collapsed two-line form (upstream `toggleCollapse`).
    /// The height step bypasses the dirty-mark fingerprint — the
    /// declarative counterpart of `requestRender(true)`.
    pub fn toggle_collapse(&mut self, host: &dyn HostCall, i18n: &I18n) {
        self.collapsed = !self.collapsed;
        if !self.widget_registered {
            return;
        }
        self.send_lines(host, i18n, RENDER_WIDTH_UNBOUNDED, true);
    }

    /// Clear the completed-display bookkeeping (upstream
    /// `resetCompletedDisplayState`); fires on foreground replay refreshes
    /// and on `agent_start` migration resets. Does NOT reset `collapsed`.
    pub fn reset_completed_display_state(&mut self) {
        self.completed_pending_hide.clear();
        self.hidden_completed.clear();
        self.last_next_id = None;
    }

    /// The collapsed state (observable for tests).
    pub fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    /// Tear the overlay down (upstream `dispose`): remove the widget,
    /// clear the ctx, reset the collapse flag and the display state.
    pub fn dispose(&mut self, host: &dyn HostCall) {
        if self.ui_bound {
            if let Err(error) = push_widget(host, None) {
                tracing::warn!(%error, "rpiv-todo: dispose setWidget rejected");
            }
        }
        self.widget_registered = false;
        self.ui_bound = false;
        self.collapsed = false;
        self.last_lines = None;
        self.reset_completed_display_state();
    }
}

/// Append the trailing blank line so the overlay isn't flush against the
/// editor box (upstream `withTrailingSpacer` — empty line sets pass
/// through unchanged).
fn with_trailing_spacer(mut lines: Vec<String>) -> Vec<String> {
    if lines.is_empty() {
        return lines;
    }
    lines.push(String::new());
    lines
}

// ---------------------------------------------------------------------------
// Controller (upstream `updateTodoOverlay` closure + the todoOverlay /
// uiCtx closure variables of index.ts)
// ---------------------------------------------------------------------------

/// Foreground overlay controller: the lazily-constructed overlay plus the
/// UI binding state (upstream closure variables `todoOverlay` / `uiCtx`).
#[derive(Default)]
pub struct OverlayController {
    overlay: Option<TodoOverlay>,
    /// The bound UI context generation (`Some` = bound).
    ui_generation: Option<u64>,
}

impl OverlayController {
    /// Bind the foreground UI context (upstream `uiCtx = ctx.ui` on the
    /// foreground `session_start` claim).
    pub fn bind_ui(&mut self, generation: u64) {
        self.ui_generation = Some(generation);
    }

    /// Drop the UI binding and dispose the overlay (upstream teardown on
    /// foreground `session_shutdown` — try/finally: the pointer clears
    /// even when the dispose push fails).
    pub fn teardown(&mut self, host: &dyn HostCall) {
        self.ui_generation = None;
        if let Some(mut overlay) = self.overlay.take() {
            overlay.dispose(host);
        }
    }

    /// The refresh entry (upstream `updateTodoOverlay(reset, generation)`):
    /// no-op without a UI binding or when the overlay does not exist yet
    /// and there is nothing visible (construction stays task-gated);
    /// otherwise construct/bind, optionally reset the completed-display
    /// state (foreground replays), and update.
    pub fn update_todo_overlay(&mut self, host: &dyn HostCall, i18n: &I18n, reset: bool) {
        let has_visible_tasks = crate::state::store::store()
            .render_state()
            .tasks
            .iter()
            .any(|task| task.status != TaskStatus::Deleted);
        if self.ui_generation.is_none() {
            return;
        }
        if self.overlay.is_none() && !has_visible_tasks {
            return;
        }
        let overlay = self.overlay.get_or_insert_with(TodoOverlay::new);
        overlay.set_ui_ctx();
        if reset {
            overlay.reset_completed_display_state();
        }
        overlay.update(host, i18n);
    }

    /// `agent_start`: migrate the previous turn's completed tasks into the
    /// hidden set (upstream `todoOverlay?.hideCompletedTasksFromPreviousTurn()`).
    pub fn on_agent_start(&mut self, host: &dyn HostCall, i18n: &I18n) {
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.hide_completed_tasks_from_previous_turn(host, i18n);
        }
    }

    /// The collapse shortcut handler (upstream: `if (!ctx.hasUI ||
    /// !todoOverlay?.isRegistered()) return; todoOverlay.toggleCollapse()`).
    pub fn handle_shortcut(&mut self, host: &dyn HostCall, i18n: &I18n) {
        if !crate::has_ui(host) {
            return;
        }
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        if !overlay.is_registered() {
            return;
        }
        overlay.toggle_collapse(host, i18n);
    }

    /// Direct overlay access for wiring tests.
    pub fn overlay(&self) -> Option<&TodoOverlay> {
        self.overlay.as_ref()
    }
}

/// Test-only access to the width-parameterized assembly (the golden-frame
/// seam — production paths go through [`TodoOverlay::update`] with
/// [`RENDER_WIDTH_UNBOUNDED`]).
impl TodoOverlay {
    /// Render at an explicit width without pushing (upstream
    /// `widget.render(width)`).
    #[cfg(test)]
    pub(crate) fn test_render(
        &mut self,
        host: &dyn HostCall,
        i18n: &I18n,
        width: usize,
    ) -> Vec<String> {
        self.render_widget_lines(host, i18n, width)
            .iter()
            .map(|line| strip_reset(line))
            .collect()
    }

    /// Assemble + return the unbounded line set (no push) — used by the
    /// empty-panel assertion.
    #[cfg(test)]
    pub(crate) fn send_and_capture(&mut self, host: &dyn HostCall, i18n: &I18n) -> Vec<String> {
        self.render_widget_lines(host, i18n, RENDER_WIDTH_UNBOUNDED)
    }
}

/// Strip the reset suffixes (test seam shared with the mock assertions).
#[cfg(test)]
fn strip_reset(line: &str) -> String {
    line.replace("\x1b[39m", "").replace("\x1b[0m", "")
}

#[cfg(test)]
mod tests {
    //! Ports of upstream `todo-overlay.render.test.ts`,
    //! `todo-overlay.lifecycle.test.ts`, the behavioral assertions of
    //! `lazy-overlay.regression.test.ts`, and the fade-out cases of
    //! `todo.invalidation.test.ts` @ `0fdf4f8` — all against a scripted
    //! host (identity theme via an empty `ui.theme` reply: the AnsiTheme
    //! over `{}` renders unstyled, byte-equal to the upstream
    //! `identityTheme` output modulo the bare `\x1b[39m` resets, which
    //! the recorded setWidget assertions strip — see [MockHost::widgets]).

    use super::*;
    use serde_json::json;

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Scripted host: session id, recorded `ui.setWidget` calls (with the
    /// ANSI reset suffixes stripped so assertions compare the identity
    /// theme's plain lines), and optional tools-expanded state.
    struct MockHost {
        tools_expanded: Option<bool>,
        theme: Value,
        widgets: std::sync::Mutex<Vec<Option<Vec<String>>>>,
    }

    impl MockHost {
        fn new() -> Self {
            MockHost {
                tools_expanded: None,
                theme: json!({}),
                widgets: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// Recorded widget pushes (None = removal).
        fn widgets(&self) -> Vec<Option<Vec<String>>> {
            self.widgets
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }

        fn last_lines(&self) -> Option<Vec<String>> {
            self.widgets()
                .last()
                .cloned()
                .flatten()
                .map(|lines| lines.iter().map(|line| strip_resets(line)).collect())
        }

        fn push_count(&self) -> usize {
            self.widgets().len()
        }
    }

    /// Strip the reset suffixes so identity-theme lines compare cleanly
    /// (the AnsiTheme over an empty theme still appends `\x1b[39m` per
    /// fg wrap — the host `Theme::fg` shape — and `truncate_to_width`
    /// closes a dangling prefix with `\x1b[0m`; both are invisible to
    /// the terminal and orthogonal to the pinned content/layout bytes).
    fn strip_resets(line: &str) -> String {
        line.replace("\x1b[39m", "").replace("\x1b[0m", "")
    }

    impl HostCall for MockHost {
        fn call(&self, method: &str, _args: Value) -> Result<Value, crate::HostError> {
            match method {
                "ui.theme" => Ok(self.theme.clone()),
                "ui.getToolsExpanded" => {
                    Ok(self.tools_expanded.map(Value::Bool).unwrap_or(Value::Null))
                }
                "ctx.hasUI" => Ok(json!(true)),
                "ui.setWidget" => {
                    let lines = _args.get("content").and_then(Value::as_array).map(|items| {
                        items
                            .iter()
                            .map(|item| item.as_str().unwrap_or_default().to_owned())
                            .collect::<Vec<_>>()
                    });
                    self.widgets
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(lines);
                    Ok(Value::Null)
                }
                _ => Ok(Value::Null),
            }
        }
    }

    /// Seed the FOREGROUND slot through the real reducer + store (the
    /// production mutation path, upstream `setup()`).
    fn seed(actions: &[Value]) {
        crate::state::store::store().set_active_render_session("test-session");
        let host = SeedHost;
        for params in actions {
            let _ = crate::tool::execute(&host, params);
        }
    }

    struct SeedHost;

    impl HostCall for SeedHost {
        fn call(&self, method: &str, _args: Value) -> Result<Value, crate::HostError> {
            match method {
                "ctx.sessionFile" => Ok(json!({"path": null, "id": "test-session"})),
                _ => Ok(Value::Null),
            }
        }
    }

    fn overlay_with(host: &MockHost) -> TodoOverlay {
        let mut overlay = TodoOverlay::new();
        overlay.set_ui_ctx();
        let i18n = I18n::for_locale("en");
        overlay.update(host, &i18n);
        overlay
    }

    /// Drive one update against a fresh controller (the production path).
    fn controller_update(controller: &mut OverlayController, host: &dyn HostCall) {
        let i18n = I18n::for_locale("en");
        controller.update_todo_overlay(host, &i18n, false);
    }

    // ------------------------------------------------------------------
    // Heading (todo-overlay.render.test.ts — heading describe)
    // ------------------------------------------------------------------

    #[test]
    fn heading_includes_the_completed_total_count() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "a"}),
            json!({"action": "create", "subject": "b"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let lines = overlay.test_render(&host, &i18n, 200);
        assert!(lines[0].contains("Todos (1/2)"), "{lines:?}");
    }

    #[test]
    fn heading_uses_the_filled_icon_when_any_task_is_active() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "a"})]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        assert!(overlay.test_render(&host, &i18n, 200)[0].contains("●"));
    }

    #[test]
    fn heading_uses_the_hollow_icon_when_all_tasks_are_completed() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "a"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        assert!(overlay.test_render(&host, &i18n, 200)[0].contains("○"));
    }

    // ------------------------------------------------------------------
    // Natural-order rendering (no overflow)
    // ------------------------------------------------------------------

    #[test]
    fn renders_one_line_per_task_plus_heading_with_last_row_connector() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "a"}),
            json!({"action": "create", "subject": "b"}),
            json!({"action": "create", "subject": "c"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let lines = overlay.test_render(&host, &i18n, 200);
        assert_eq!(lines.len(), 5, "heading + 3 + trailing spacer");
        assert!(lines[1].contains("├─"));
        assert!(lines[2].contains("├─"));
        assert!(lines[3].contains("└─"));
        assert_eq!(lines[4], "");
    }

    #[test]
    fn omits_deleted_tasks_from_the_rendered_output() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "visible"}),
            json!({"action": "create", "subject": "gone"}),
            json!({"action": "update", "id": 2, "status": "deleted"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let out = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(out.contains("visible"));
        assert!(!out.contains("gone"));
    }

    // ------------------------------------------------------------------
    // Per-task formatting
    // ------------------------------------------------------------------

    #[test]
    fn pending_task_uses_the_hollow_glyph() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "pending-task"})]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let lines = overlay.test_render(&host, &i18n, 200);
        assert!(lines[1].contains("○"));
        assert!(lines[1].contains("pending-task"));
    }

    #[test]
    fn in_progress_task_uses_the_half_glyph_and_active_form() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "do it", "activeForm": "Doing it"}),
            json!({"action": "update", "id": 1, "status": "in_progress"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let line = overlay.test_render(&host, &i18n, 200)[1].clone();
        assert!(line.contains("◐"));
        assert!(line.contains("do it"));
        assert!(line.contains("(Doing it)"));
    }

    #[test]
    fn completed_task_stays_visible_until_the_next_agent_turn() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "done"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let first = overlay.test_render(&host, &i18n, 200);
        assert!(first[1].contains("✓"));
        assert!(first[1].contains("done"));
        // Repeated renders keep showing it (displayed-once tracking is
        // idempotent).
        assert!(overlay.test_render(&host, &i18n, 200)[1].contains("done"));
        // agent_start migration → the panel renders an empty line set
        // under the still-registered widget (the unregister only happens
        // on the NEXT update(), upstream `render()` → `[]` shape).
        overlay.hide_completed_tasks_from_previous_turn(&host, &i18n);
        assert_eq!(
            overlay.send_and_capture(&host, &i18n),
            Vec::<String>::new(),
            "emptied panel renders an empty line set"
        );
        // And the following update() unregisters.
        overlay.update(&host, &i18n);
        assert!(!overlay.is_registered());
        assert_eq!(host.widgets().last(), Some(&None));
    }

    // ------------------------------------------------------------------
    // showIds gate
    // ------------------------------------------------------------------

    #[test]
    fn does_not_show_id_prefixes_without_blocked_by() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "a"}),
            json!({"action": "create", "subject": "b"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let out = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(!out.contains("#1"), "{out}");
        assert!(!out.contains("#2"), "{out}");
    }

    #[test]
    fn shows_id_prefixes_and_chain_suffix_with_blocked_by() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "base"}),
            json!({"action": "create", "subject": "follow-up", "blockedBy": [1]}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let out = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(out.contains("#1"));
        assert!(out.contains("#2"));
        assert!(out.contains("⛓"));
    }

    // ------------------------------------------------------------------
    // Overflow collapse (the budget matrix)
    // ------------------------------------------------------------------

    fn seed_overflow(pending: usize, completed_from: usize, completed_to: usize) {
        let mut actions = Vec::new();
        for i in 1..=pending {
            actions.push(json!({"action": "create", "subject": format!("p{i}")}));
        }
        if completed_from > 0 && completed_to >= completed_from {
            for i in completed_from..=completed_to {
                actions.push(json!({"action": "create", "subject": format!("c{i}")}));
                actions.push(json!({"action": "update", "id": i, "status": "completed"}));
            }
        }
        seed(&actions);
    }

    #[test]
    fn drops_completed_first_when_dropping_is_enough() {
        // 12 total = 8 pending + 4 completed, budget = 11 → all pending
        // plus 2 of the 4 completed; 2 completed hidden.
        let _guard = serialized();
        crate::__reset_state();
        seed_overflow(8, 9, 12);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let lines = overlay.test_render(&host, &i18n, 200);
        // heading + 10 visible + 1 summary + trailing spacer = 13
        assert_eq!(lines.len(), 13);
        let joined = lines.join("\n");
        for i in 1..=8 {
            assert!(joined.contains(&format!("p{i}")), "missing p{i}");
        }
        assert_eq!(lines[lines.len() - 1], "");
        assert!(lines[lines.len() - 2].contains("+2 more"));
        assert!(lines[lines.len() - 2].contains("2 completed"));
    }

    #[test]
    fn truncates_the_pending_tail_when_dropping_completed_is_not_enough() {
        // 12 pending, budget 11 → first 10 visible, 2 pending truncated.
        let _guard = serialized();
        crate::__reset_state();
        seed_overflow(12, 0, 0);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let lines = overlay.test_render(&host, &i18n, 200);
        assert_eq!(lines.len(), 13);
        assert_eq!(lines[lines.len() - 1], "");
        let summary = &lines[lines.len() - 2];
        assert!(summary.contains("+2 more"));
        assert!(summary.contains("2 pending"));
        assert!(!summary.contains("completed"));
    }

    #[test]
    fn summary_contains_both_parts_on_mixed_overflow() {
        // 12 pending + 3 completed = 15, budget 11 → inner 10: first 10
        // pending, 2 pending truncated, 3 completed hidden → "+5 more
        // (3 completed, 2 pending)".
        let _guard = serialized();
        crate::__reset_state();
        seed_overflow(12, 13, 15);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let lines = overlay.test_render(&host, &i18n, 200);
        let summary = &lines[lines.len() - 2];
        assert!(summary.contains("+5 more"));
        assert!(summary.contains("3 completed"));
        assert!(summary.contains("2 pending"));
    }

    #[test]
    fn hides_overflowed_completed_tasks_on_the_next_agent_turn_too() {
        let _guard = serialized();
        crate::__reset_state();
        seed_overflow(11, 12, 16);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let before = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(before.contains("Todos (5/16)"));
        assert!(before.contains("+6 more"));
        assert!(before.contains("5 completed"));
        overlay.hide_completed_tasks_from_previous_turn(&host, &i18n);
        let after = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(after.contains("Todos (0/11)"));
        assert!(after.contains("p11"));
        assert!(!after.contains("+1 more"));
        assert!(!after.contains("completed"));
    }

    #[test]
    fn does_not_engage_overflow_at_exactly_11_visible_tasks() {
        let _guard = serialized();
        crate::__reset_state();
        seed_overflow(11, 0, 0);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let lines = overlay.test_render(&host, &i18n, 200);
        assert_eq!(lines.len(), 13);
        assert_eq!(lines[lines.len() - 1], "");
        assert!(!lines[lines.len() - 2].contains('+'));
        assert!(lines[lines.len() - 2].contains("└─"));
    }

    #[test]
    fn follows_the_tool_output_expansion_mode() {
        let _guard = serialized();
        crate::__reset_state();
        seed_overflow(17, 0, 0);
        let mut host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");

        let collapsed = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(collapsed.contains("+7 more"));
        assert!(!collapsed.contains("p17"));

        host.tools_expanded = Some(true);
        let expanded = overlay.test_render(&host, &i18n, 200);
        assert_eq!(expanded.len(), 19, "heading + 17 tasks + spacer");
        let joined = expanded.join("\n");
        assert!(joined.contains("p17"));
        assert!(!joined.contains(" more"));
        assert!(expanded[expanded.len() - 2].contains("└─"));

        host.tools_expanded = Some(false);
        assert!(overlay
            .test_render(&host, &i18n, 200)
            .join("\n")
            .contains("+7 more"));
    }

    #[test]
    fn keeps_the_configured_budget_without_an_expansion_api() {
        // tools_expanded = None (host without the API / transport error)
        // → the budget applies (upstream optional chaining).
        let _guard = serialized();
        crate::__reset_state();
        seed_overflow(17, 0, 0);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        assert!(overlay
            .test_render(&host, &i18n, 200)
            .join("\n")
            .contains("+7 more"));
    }

    // ------------------------------------------------------------------
    // Collapse/expand render
    // ------------------------------------------------------------------

    #[test]
    fn collapsed_view_returns_exactly_three_lines() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "a"}),
            json!({"action": "create", "subject": "b"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        overlay.toggle_collapse(&host, &i18n);
        let lines = overlay.test_render(&host, &i18n, 200);
        assert_eq!(lines.len(), 3, "heading + hint + spacer");
        assert!(lines[0].contains("Todos (1/2)"));
        assert!(lines[1].contains("└─"));
        assert!(lines[1].contains("ctrl+shift+t to expand"));
        assert_eq!(lines[2], "");
    }

    #[test]
    fn uncollapsed_default_renders_the_full_view() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "a"}),
            json!({"action": "create", "subject": "b"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let lines = overlay.test_render(&host, &i18n, 200);
        assert_eq!(lines.len(), 4);
        assert!(lines.iter().any(|line| line.contains('a')));
        assert!(lines.iter().any(|line| line.contains('b')));
    }

    #[test]
    fn collapsed_render_short_circuits_before_completed_display_tracking() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "done"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
        ]);
        let host = MockHost::new();
        // Bind WITHOUT update()'s assembly (upstream registers the factory
        // without rendering; the rpi update() = register + render in one,
        // so the tracking-free path starts from a bound-but-unrendered
        // overlay).
        let mut overlay = TodoOverlay::new();
        overlay.set_ui_ctx();
        let i18n = I18n::for_locale("en");
        overlay.toggle_collapse(&host, &i18n);
        // Collapsed render — must NOT queue the completed task.
        let _ = overlay.test_render(&host, &i18n, 200);
        // Draining the pending-hide set is a no-op (nothing was queued).
        overlay.hide_completed_tasks_from_previous_turn(&host, &i18n);
        overlay.toggle_collapse(&host, &i18n);
        let expanded = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(expanded.contains("done"));
        assert!(expanded.contains("✓"));
    }

    // ------------------------------------------------------------------
    // Collapse hint resolves the key from config per render
    // (todo-overlay.render.test.ts — collapse hint describe; review P1-1)
    // ------------------------------------------------------------------

    fn collapsed_hint_with(config: &serde_json::Value) -> String {
        crate::config::set_test_config(Some(config.clone()));
        seed(&[json!({"action": "create", "subject": "a"})]);
        let host = MockHost::new();
        let mut overlay = TodoOverlay::new();
        overlay.set_ui_ctx();
        let i18n = I18n::for_locale("en");
        overlay.toggle_collapse(&host, &i18n);
        let lines = overlay.test_render(&host, &i18n, 200);
        assert_eq!(lines.len(), 3);
        lines[1].clone()
    }

    #[test]
    fn hint_renders_the_configured_key_and_never_leaks_the_placeholder() {
        let _guard = serialized();
        crate::__reset_state();
        let hint = collapsed_hint_with(&json!({"collapseKey": "alt+o"}));
        assert!(hint.contains("alt+o to expand"), "{hint}");
        assert!(!hint.contains("{key}"), "{hint}");
        assert!(!hint.contains("ctrl+shift+t"), "{hint}");
    }

    #[test]
    fn hint_renders_the_default_key_when_config_is_missing() {
        let _guard = serialized();
        crate::__reset_state();
        let hint = collapsed_hint_with(&json!({}));
        assert!(hint.contains("ctrl+shift+t to expand"), "{hint}");
        assert!(!hint.contains("{key}"), "{hint}");
    }

    #[test]
    fn hint_renders_the_default_key_when_the_configured_spec_is_invalid() {
        let _guard = serialized();
        crate::__reset_state();
        let hint = collapsed_hint_with(&json!({"collapseKey": "ctr+t"}));
        assert!(hint.contains("ctrl+shift+t to expand"), "{hint}");
    }

    #[test]
    fn hint_renders_a_static_collapsed_label_for_the_off_sentinel() {
        // Reachable mid-session: collapse with a bound key, then edit the
        // config to "off" — the per-render resolver returns the sentinel;
        // the hint must not splice it into the {key} placeholder.
        let _guard = serialized();
        crate::__reset_state();
        let hint = collapsed_hint_with(&json!({"collapseKey": "off"}));
        assert!(hint.contains("collapsed"), "{hint}");
        assert!(!hint.contains("off to expand"), "{hint}");
        assert!(!hint.contains("{key}"), "{hint}");
    }

    // ------------------------------------------------------------------
    // Theme invalidation (todo-overlay.lifecycle.test.ts — "uses the
    // current UI theme after invalidation without re-registering";
    // review P1-1: a theme change must defeat the fingerprint and
    // re-send the re-themed lines)
    // ------------------------------------------------------------------

    #[test]
    fn a_theme_change_defeats_the_fingerprint_and_resends_the_new_colors() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "a"})]);
        let mut host = MockHost::new();
        host.theme = json!({
            "vars": {"a-color": "#8abeb7"},
            "colors": {"accent": "a-color"}
        });
        let mut overlay = TodoOverlay::new();
        overlay.set_ui_ctx();
        let i18n = I18n::for_locale("en");
        overlay.update(&host, &i18n);
        assert_eq!(host.push_count(), 1);
        let first = host.last_lines().expect("initial push");
        assert!(
            first
                .iter()
                .any(|line| line.contains("\x1b[38;2;138;190;183m")),
            "initial heading carries theme A's accent: {first:?}"
        );
        // Theme switch: the next assembly re-reads ui.theme, the changed
        // ANSI defeats the equal-value skip, and the re-send carries the
        // new accent — without any re-registration bookkeeping beyond the
        // same setWidget channel.
        host.theme = json!({
            "vars": {"a-color": "#ff0000"},
            "colors": {"accent": "a-color"}
        });
        overlay.update(&host, &i18n);
        assert_eq!(host.push_count(), 2, "theme change forces a re-send");
        let second = host.last_lines().expect("re-themed push");
        assert!(
            second
                .iter()
                .any(|line| line.contains("\x1b[38;2;255;0;0m")),
            "re-themed heading carries theme B's accent: {second:?}"
        );
        assert!(
            !second
                .iter()
                .any(|line| line.contains("\x1b[38;2;138;190;183m")),
            "stale accent must be gone: {second:?}"
        );
        assert!(overlay.is_registered());
    }

    // ------------------------------------------------------------------
    // Width truncation
    // ------------------------------------------------------------------

    #[test]
    fn renders_without_panicking_at_small_widths() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({
            "action": "create",
            "subject": "a very long subject that would overflow a narrow column"
        })]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let lines = overlay.test_render(&host, &i18n, 20);
        assert!(lines
            .iter()
            .all(|line| rpi_tui::utils::visible_width(line) <= 20));
        assert!(lines[1].contains('…'));
    }

    #[test]
    fn drops_completed_tasks_from_counts_after_the_next_agent_turn() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "done"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
            json!({"action": "create", "subject": "next"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        assert!(overlay
            .test_render(&host, &i18n, 200)
            .join("\n")
            .contains("Todos (1/2)"));
        let second = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(second.contains("Todos (1/2)"));
        assert!(second.contains("next"));
        assert!(second.contains("done"));
        overlay.hide_completed_tasks_from_previous_turn(&host, &i18n);
        let hidden = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(hidden.contains("Todos (0/1)"));
        assert!(hidden.contains("next"));
        assert!(!hidden.contains("done"));
    }

    #[test]
    fn re_renders_reflect_live_state_changes_without_re_registering() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "first"})]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let out1 = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(out1.contains("first"));
        // A second task lands through the real mutation path.
        seed(&[json!({"action": "create", "subject": "second"})]);
        let out2 = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(out2.contains("first"));
        assert!(out2.contains("second"));
    }

    // ------------------------------------------------------------------
    // Lifecycle (todo-overlay.lifecycle.test.ts)
    // ------------------------------------------------------------------

    #[test]
    fn update_without_a_ui_ctx_binding_is_a_no_op() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "a"})]);
        let host = MockHost::new();
        let mut overlay = TodoOverlay::new();
        let i18n = I18n::for_locale("en");
        overlay.update(&host, &i18n);
        assert_eq!(host.push_count(), 0);
    }

    #[test]
    fn update_with_empty_todos_does_not_send_a_widget() {
        let _guard = serialized();
        crate::__reset_state();
        crate::state::store::store().set_active_render_session("test-session");
        let host = MockHost::new();
        let mut overlay = TodoOverlay::new();
        overlay.set_ui_ctx();
        let i18n = I18n::for_locale("en");
        overlay.update(&host, &i18n);
        assert_eq!(host.push_count(), 0);
    }

    #[test]
    fn first_update_with_tasks_sends_the_widget_once() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "a"})]);
        let host = MockHost::new();
        let mut overlay = TodoOverlay::new();
        overlay.set_ui_ctx();
        let i18n = I18n::for_locale("en");
        overlay.update(&host, &i18n);
        assert_eq!(host.push_count(), 1);
        assert!(overlay.is_registered());
        // The pushed set is the aboveEditor lines payload.
        let lines = host.last_lines().expect("one push");
        assert!(lines[0].contains("Todos (0/1)"));
    }

    #[test]
    fn second_update_with_unchanged_lines_is_throttled() {
        // FR-H: the dirty-mark fingerprint skips the equal-value re-send
        // (output-equivalent to the upstream requestRender dedupe — the
        // panel content is identical).
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "a"})]);
        let host = MockHost::new();
        let mut overlay = TodoOverlay::new();
        overlay.set_ui_ctx();
        let i18n = I18n::for_locale("en");
        overlay.update(&host, &i18n);
        overlay.update(&host, &i18n);
        assert_eq!(host.push_count(), 1, "unchanged line set is not re-sent");
        // A changed line set re-sends.
        seed(&[json!({"action": "update", "id": 1, "status": "completed"})]);
        overlay.update(&host, &i18n);
        assert_eq!(host.push_count(), 2);
    }

    #[test]
    fn transition_non_empty_to_empty_unregisters_the_widget() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "a"})]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        seed(&[json!({"action": "clear"})]);
        overlay.update(&host, &i18n);
        let widgets = host.widgets();
        assert_eq!(widgets.len(), 2);
        assert_eq!(widgets[1], None, "second push removes the widget");
        assert!(!overlay.is_registered());
    }

    #[test]
    fn empty_to_non_empty_after_the_empty_transition_re_registers() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "a"})]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        seed(&[json!({"action": "clear"})]);
        overlay.update(&host, &i18n);
        seed(&[json!({"action": "create", "subject": "b"})]);
        overlay.update(&host, &i18n);
        let widgets = host.widgets();
        assert_eq!(widgets.len(), 3, "register, unregister, re-register");
        assert!(widgets[2].is_some());
        assert!(overlay.is_registered());
    }

    #[test]
    fn set_ui_ctx_with_the_same_generation_is_idempotent() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "a"})]);
        let host = MockHost::new();
        let mut overlay = TodoOverlay::new();
        let i18n = I18n::for_locale("en");
        overlay.set_ui_ctx();
        overlay.update(&host, &i18n);
        overlay.set_ui_ctx();
        overlay.update(&host, &i18n);
        assert_eq!(host.push_count(), 1);
    }

    #[test]
    fn dispose_unregisters_and_clears_the_binding() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "a"})]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        overlay.dispose(&host);
        let widgets = host.widgets();
        assert_eq!(widgets.len(), 2);
        assert_eq!(widgets[1], None);
        // Further updates without rebinding do not touch the host.
        let i18n = I18n::for_locale("en");
        overlay.update(&host, &i18n);
        assert_eq!(host.push_count(), 2);
    }

    #[test]
    fn reset_completed_display_state_lets_replayed_completed_tasks_reshow() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "done"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        assert!(overlay
            .test_render(&host, &i18n, 200)
            .join("\n")
            .contains("done"));
        overlay.hide_completed_tasks_from_previous_turn(&host, &i18n);
        assert!(!overlay
            .test_render(&host, &i18n, 200)
            .join("\n")
            .contains("done"));
        overlay.reset_completed_display_state();
        assert!(overlay
            .test_render(&host, &i18n, 200)
            .join("\n")
            .contains("done"));
    }

    #[test]
    fn hide_completed_is_a_no_op_when_nothing_is_pending_hide() {
        let mut overlay = TodoOverlay::new();
        let host = MockHost::new();
        let i18n = I18n::for_locale("en");
        overlay.hide_completed_tasks_from_previous_turn(&host, &i18n);
        assert_eq!(host.push_count(), 0);
    }

    #[test]
    fn all_deleted_todos_count_as_empty() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "a"}),
            json!({"action": "update", "id": 1, "status": "deleted"}),
        ]);
        let host = MockHost::new();
        let mut overlay = TodoOverlay::new();
        overlay.set_ui_ctx();
        let i18n = I18n::for_locale("en");
        overlay.update(&host, &i18n);
        assert_eq!(host.push_count(), 0, "no widget for an all-deleted list");
    }

    // ------------------------------------------------------------------
    // Collapse state (lifecycle.test.ts — collapse/expand describe)
    // ------------------------------------------------------------------

    #[test]
    fn a_new_overlay_starts_expanded() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "a"}),
            json!({"action": "create", "subject": "b"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        let out = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(!out.contains("ctrl+shift+t to expand"));
    }

    #[test]
    fn toggle_collapse_flips_the_state_and_forces_a_resend() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "a"}),
            json!({"action": "create", "subject": "b"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        assert_eq!(host.push_count(), 1);
        // Collapse → collapsed render shape + forced re-send (bypasses
        // the fingerprint — upstream requestRender(true)).
        overlay.toggle_collapse(&host, &i18n);
        assert!(overlay.is_collapsed());
        let lines = host.last_lines().expect("forced re-send");
        assert!(lines.len() == 3 && lines[1].contains("to expand"));
        // Toggle back → expanded, forced again.
        overlay.toggle_collapse(&host, &i18n);
        assert!(!overlay.is_collapsed());
        let lines = host.last_lines().expect("forced re-send");
        assert!(!lines.iter().any(|line| line.contains("to expand")));
    }

    #[test]
    fn is_registered_reflects_the_registration_state() {
        let _guard = serialized();
        crate::__reset_state();
        crate::state::store::store().set_active_render_session("test-session");
        let host = MockHost::new();
        let mut overlay = TodoOverlay::new();
        assert!(!overlay.is_registered());
        seed(&[json!({"action": "create", "subject": "a"})]);
        overlay.set_ui_ctx();
        let i18n = I18n::for_locale("en");
        overlay.update(&host, &i18n);
        assert!(overlay.is_registered());
        overlay.dispose(&host);
        assert!(!overlay.is_registered());
    }

    #[test]
    fn reset_completed_display_state_does_not_reset_collapsed() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "a"}),
            json!({"action": "create", "subject": "b"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        overlay.toggle_collapse(&host, &i18n);
        overlay.reset_completed_display_state();
        assert!(overlay.is_collapsed());
        assert!(overlay.test_render(&host, &i18n, 200)[1].contains("to expand"));
    }

    // ------------------------------------------------------------------
    // Controller (index.ts updateTodoOverlay closure + shortcut guard)
    // ------------------------------------------------------------------

    #[test]
    fn controller_construction_stays_task_gated() {
        let _guard = serialized();
        crate::__reset_state();
        crate::state::store::store().set_active_render_session("test-session");
        let host = MockHost::new();
        let mut controller = OverlayController::default();
        controller.bind_ui(1);
        // Empty foreground → no overlay constructed, no widget sent.
        controller_update(&mut controller, &host);
        assert!(controller.overlay().is_none());
        assert_eq!(host.push_count(), 0);
        // First visible task constructs + registers.
        seed(&[json!({"action": "create", "subject": "a"})]);
        controller_update(&mut controller, &host);
        assert!(controller.overlay().is_some());
        assert_eq!(host.push_count(), 1);
        // Later updates reuse the constructed overlay.
        seed(&[json!({"action": "create", "subject": "b"})]);
        controller_update(&mut controller, &host);
        assert_eq!(host.push_count(), 2);
    }

    #[test]
    fn controller_update_without_a_ui_binding_is_a_no_op() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "a"})]);
        let host = MockHost::new();
        let mut controller = OverlayController::default();
        controller_update(&mut controller, &host);
        assert!(controller.overlay().is_none());
        assert_eq!(host.push_count(), 0);
    }

    #[test]
    fn controller_teardown_disposes_the_overlay_even_when_the_push_fails() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "a"})]);
        let host = FailingWidgetHost;
        let mut controller = OverlayController::default();
        controller.bind_ui(1);
        let i18n = I18n::for_locale("en");
        controller.update_todo_overlay(&host, &i18n, false);
        // teardown must drop the overlay regardless of the failing push
        // (upstream try/finally).
        controller.teardown(&host);
        assert!(controller.overlay().is_none());
    }

    struct FailingWidgetHost;

    impl HostCall for FailingWidgetHost {
        fn call(&self, method: &str, _args: Value) -> Result<Value, crate::HostError> {
            match method {
                "ui.setWidget" => Err(crate::HostError {
                    kind: "stale".to_owned(),
                    message: "ctx gone".to_owned(),
                }),
                "ctx.sessionFile" => Ok(json!({"path": null, "id": "test-session"})),
                _ => Ok(Value::Null),
            }
        }
    }

    #[test]
    fn controller_shortcut_guard_ladder() {
        let _guard = serialized();
        crate::__reset_state();
        let i18n = I18n::for_locale("en");
        // Registered widget → a headless ctx (!hasUI) does not toggle it.
        seed(&[json!({"action": "create", "subject": "a"})]);
        let host = MockHost::new();
        let mut controller = OverlayController::default();
        controller.bind_ui(1);
        controller.update_todo_overlay(&host, &i18n, false);
        assert_eq!(host.push_count(), 1, "registered exactly once");
        controller.handle_shortcut(&HeadlessHost, &i18n);
        assert_eq!(host.push_count(), 1, "headless ctx does not toggle");
        // No overlay constructed yet → no-op.
        let mut controller = OverlayController::default();
        controller.handle_shortcut(&host, &i18n);
        assert_eq!(host.push_count(), 1);
        // Bound but empty list → overlay not constructed → no-op.
        crate::__reset_state();
        crate::state::store::store().set_active_render_session("test-session");
        let mut controller = OverlayController::default();
        controller.bind_ui(1);
        controller.update_todo_overlay(&host, &i18n, false);
        assert!(controller.overlay().is_none());
        controller.handle_shortcut(&host, &i18n);
        assert_eq!(host.push_count(), 1);
        // Registered → toggles (forced re-send flips to collapsed).
        seed(&[json!({"action": "create", "subject": "a"})]);
        controller.update_todo_overlay(&host, &i18n, false);
        let before = host.push_count();
        controller.handle_shortcut(&host, &i18n);
        assert!(host.push_count() > before);
        assert!(controller
            .overlay()
            .is_some_and(|overlay| overlay.is_collapsed()));
    }

    struct HeadlessHost;

    impl HostCall for HeadlessHost {
        fn call(&self, method: &str, _args: Value) -> Result<Value, crate::HostError> {
            match method {
                "ctx.sessionFile" => Ok(json!({"path": null, "id": "test-session"})),
                "ctx.hasUI" => Ok(json!(false)),
                _ => Ok(Value::Null),
            }
        }
    }

    // ------------------------------------------------------------------
    // lazy-overlay.regression.test.ts behavioral assertions (the jiti
    // loading semantics are [N/A]; these two behaviors are real):
    // a repeated tool_execution_end does NOT replay (the branch is stale
    // there — message_end runs after), and a transient widget failure
    // does not permanently poison the refresh path.
    // ------------------------------------------------------------------

    #[test]
    fn repeated_tool_end_refreshes_without_replaying() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "first"})]);
        let host = MockHost::new();
        let mut controller = OverlayController::default();
        controller.bind_ui(1);
        let i18n = I18n::for_locale("en");
        controller.update_todo_overlay(&host, &i18n, false);
        // A live mutation lands via the tool; the tool_execution_end
        // refresh renders from the STORE (no replay — the branch is
        // stale at that point, upstream comment).
        seed(&[json!({"action": "create", "subject": "second"})]);
        controller.update_todo_overlay(&host, &i18n, false);
        let lines = host.last_lines().expect("refresh pushed");
        let joined = lines.join("\n");
        assert!(joined.contains("first") && joined.contains("second"));
    }

    #[test]
    fn a_failed_push_does_not_permanently_poison_refreshes() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[json!({"action": "create", "subject": "a"})]);
        let failing = FailingWidgetHost;
        let mut controller = OverlayController::default();
        controller.bind_ui(1);
        let i18n = I18n::for_locale("en");
        controller.update_todo_overlay(&failing, &i18n, false);
        assert!(controller.overlay().is_some(), "overlay constructed anyway");
        // The next refresh against a healthy host succeeds.
        let host = MockHost::new();
        seed(&[json!({"action": "create", "subject": "b"})]);
        controller.update_todo_overlay(&host, &i18n, false);
        let lines = host.last_lines().expect("recovered push");
        assert!(lines.join("\n").contains('b'));
    }

    // ------------------------------------------------------------------
    // Fade-out bookkeeping edges (getSnapshot invariants)
    // ------------------------------------------------------------------

    #[test]
    fn a_next_id_decrease_resets_the_completed_display_state() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "done"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
            json!({"action": "create", "subject": "filler"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        overlay.hide_completed_tasks_from_previous_turn(&host, &i18n);
        assert!(!overlay
            .test_render(&host, &i18n, 200)
            .join("\n")
            .contains("done"));
        // clear resets nextId (3 -> 1; the re-created task brings it to 2)
        // — the decrease resets the completed-display state, so the
        // re-created completed task shows again.
        seed(&[
            json!({"action": "clear"}),
            json!({"action": "create", "subject": "done"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
        ]);
        assert!(overlay
            .test_render(&host, &i18n, 200)
            .join("\n")
            .contains("done"));
    }

    // ------------------------------------------------------------------
    // Golden frames: the full line set per scenario × width (design §7 —
    // the WidgetContent::Lines payloads pinned byte-for-byte; identity
    // theme, i.e. the reset-stripped form of the runtime output)
    // ------------------------------------------------------------------

    /// Scenario states seeded through the real reducer; returns (label,
    /// overlay modifications to apply before snapshotting).
    fn golden_scenario_lines(
        scenario: &str,
        width: usize,
        fade_previous_turn: bool,
    ) -> Vec<String> {
        let actions: Vec<Value> = match scenario {
            // deps: a pending base + an in_progress follow-up with
            // activeForm and blockedBy.
            "deps" => vec![
                json!({"action": "create", "subject": "Create DemoTodo domain entity"}),
                json!({"action": "create", "subject": "Create repository", "activeForm": "creating the repository", "blockedBy": [1]}),
                json!({"action": "update", "id": 2, "status": "in_progress"}),
                json!({"action": "create", "subject": "Register DI bindings"}),
            ],
            // all-done: every task completed → hollow dim heading.
            "all-done" => vec![
                json!({"action": "create", "subject": "Research"}),
                json!({"action": "update", "id": 1, "status": "completed"}),
            ],
            // overflow: 14 pending → 10 visible + "+4 more (4 pending)".
            "overflow" => {
                let mut actions = Vec::new();
                for i in 1..=14 {
                    actions.push(json!({"action": "create", "subject": format!("task {i}")}));
                }
                actions
            }
            // wide: a single very long pending subject (truncation
            // differences across the width ladder).
            "wide" => vec![json!({
                "action": "create",
                "subject": "A very long task subject that will overflow the sixty column golden frame width"
            })],
            // faded: one completed + one pending; the completed was
            // displayed and the agent turn rolled over.
            "faded" => vec![
                json!({"action": "create", "subject": "Done work"}),
                json!({"action": "update", "id": 1, "status": "completed"}),
                json!({"action": "create", "subject": "Remaining work"}),
            ],
            _ => Vec::new(),
        };
        seed(&actions);
        let host = MockHost::new();
        let mut overlay = TodoOverlay::new();
        overlay.set_ui_ctx();
        let i18n = I18n::for_locale("en");
        overlay.update(&host, &i18n);
        if fade_previous_turn {
            overlay.hide_completed_tasks_from_previous_turn(&host, &i18n);
        }
        overlay.test_render(&host, &i18n, width)
    }

    #[test]
    fn golden_frames_scenarios_times_widths() {
        // The pinned expectations are transcribed from the verified
        // behavior (each checked against the upstream render semantics:
        // heading counts, connector prefixes, id/chain gates, budget
        // summary, fade-out, collapse hint, and the `…` truncation) —
        // reset-suffixes stripped (see strip_resets).
        let _guard = serialized();
        // deps × all widths (longest row is 55 columns — intact at 60).
        for width in [60, 100, 200] {
            crate::__reset_state();
            assert_eq!(
                golden_scenario_lines("deps", width, false),
                vec![
                    "● Todos (0/3)".to_owned(),
                    "├─ ○ #1 Create DemoTodo domain entity".to_owned(),
                    "├─ ◐ #2 Create repository (creating the repository) ⛓ #1".to_owned(),
                    "└─ ○ #3 Register DI bindings".to_owned(),
                    String::new(),
                ],
                "deps @ {width}"
            );
        }
        // wide: 60 truncates with the ellipsis; 100/200 keep the full row.
        for (width, expected_row) in [
            (
                60,
                "└─ ○ A very long task subject that will overflow the sixty …",
            ),
            (
                100,
                "└─ ○ A very long task subject that will overflow the sixty column golden frame width",
            ),
            (
                200,
                "└─ ○ A very long task subject that will overflow the sixty column golden frame width",
            ),
        ] {
            crate::__reset_state();
            assert_eq!(
                golden_scenario_lines("wide", width, false),
                vec!["● Todos (0/1)".to_owned(), expected_row.to_owned(), String::new()],
                "wide @ {width}"
            );
        }
        // overflow: 14 pending, budget 11 → 10 rows + "+4 more (4 pending)".
        let overflow_expected = || {
            let mut lines = vec!["● Todos (0/14)".to_owned()];
            for i in 1..=10 {
                lines.push(format!("├─ ○ task {i}"));
            }
            lines.push("└─ +4 more (4 pending)".to_owned());
            lines.push(String::new());
            lines
        };
        for width in [60, 100, 200] {
            crate::__reset_state();
            assert_eq!(
                golden_scenario_lines("overflow", width, false),
                overflow_expected(),
                "overflow @ {width}"
            );
        }
        // all-done: hollow dim heading + struck completed subject
        // (strikethrough wraps are real style, not resets — kept).
        for width in [60, 100, 200] {
            crate::__reset_state();
            assert_eq!(
                golden_scenario_lines("all-done", width, false),
                vec![
                    "○ Todos (1/1)".to_owned(),
                    "└─ ✓ \u{1b}[9mResearch\u{1b}[29m".to_owned(),
                    String::new(),
                ],
                "all-done @ {width}"
            );
        }
        // faded: after the agent turn rolls over, the displayed completed
        // task leaves the panel (counts drop with it).
        for width in [60, 100, 200] {
            crate::__reset_state();
            assert_eq!(
                golden_scenario_lines("faded", width, true),
                vec![
                    "● Todos (0/1)".to_owned(),
                    "└─ ○ Remaining work".to_owned(),
                    String::new(),
                ],
                "faded @ {width}"
            );
        }
        // collapsed: two-line form + spacer, hint carries the current key.
        for width in [60, 100, 200] {
            crate::__reset_state();
            seed_deps_for_collapse();
            assert_eq!(
                collapsed_lines(width),
                vec![
                    "● Todos (0/2)".to_owned(),
                    "└─ ctrl+shift+t to expand".to_owned(),
                    String::new(),
                ],
                "collapsed @ {width}"
            );
        }
    }

    fn seed_deps_for_collapse() {
        seed(&[
            json!({"action": "create", "subject": "Create DemoTodo domain entity"}),
            json!({"action": "create", "subject": "Create repository", "activeForm": "creating the repository", "blockedBy": [1]}),
            json!({"action": "update", "id": 2, "status": "in_progress"}),
        ]);
    }

    fn collapsed_lines(width: usize) -> Vec<String> {
        let host = MockHost::new();
        let mut overlay = TodoOverlay::new();
        overlay.set_ui_ctx();
        let i18n = I18n::for_locale("en");
        overlay.update(&host, &i18n);
        overlay.toggle_collapse(&host, &i18n);
        overlay.test_render(&host, &i18n, width)
    }

    #[test]
    fn a_reverted_task_leaves_the_hide_sets() {
        let _guard = serialized();
        crate::__reset_state();
        seed(&[
            json!({"action": "create", "subject": "done"}),
            json!({"action": "update", "id": 1, "status": "completed"}),
        ]);
        let host = MockHost::new();
        let mut overlay = overlay_with(&host);
        let i18n = I18n::for_locale("en");
        overlay.hide_completed_tasks_from_previous_turn(&host, &i18n);
        // A completed task cannot legally revert through the reducer
        // (completed is one-way) — the revert arrives via a wholesale
        // slot replacement (the replay seam), and the hidden id leaves
        // the set in getSnapshot's retain (getSnapshot invariants).
        crate::state::store::store().replace_state(
            "test-session",
            crate::state::TaskState {
                tasks: vec![crate::tool::types::Task {
                    id: 1,
                    subject: "done".to_owned(),
                    status: crate::tool::types::TaskStatus::Pending,
                    description: None,
                    active_form: None,
                    blocked_by: None,
                    owner: None,
                    metadata: None,
                }],
                next_id: 2,
            },
        );
        let joined = overlay.test_render(&host, &i18n, 200).join("\n");
        assert!(joined.contains("○"), "{joined}");
        assert!(joined.contains("done"));
    }
}
