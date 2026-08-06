//! Port of `session-selector.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - Theme and TUI are injected explicitly (`Arc<Theme>` / [`Tui`]) instead
//!   of the upstream global `theme` getter and `requestRender` callback
//!   (theme.ts:799-816). Render requests use [`Tui::request_render`]
//!   (non-forced, matching the upstream `requestRender()` calls).
//! - Loading is synchronous: upstream loads current/all sessions
//!   asynchronously with `(loaded, total)` progress callbacks
//!   (session-selector.ts:922-982); the port calls the synchronous
//!   [`SessionManager::list`] / [`SessionManager::list_all`] once and reports
//!   the completed progress (`total/total`) before clearing the loading
//!   state. The `allLoadSeq` staleness guard and the `scope !== this.scope`
//!   post-load checks become unnecessary (a scope toggle cannot interleave
//!   with a synchronous load).
//! - The header status-message auto-hide timer (`setTimeout`, 116-126) is not
//!   ported: messages persist until the next status change (unassigned — no
//!   v0.1 task claims the timer).
//! - Actual session deletion and renaming are delegated: the component only
//!   implements the confirmation state machine and exposes `on_delete`
//!   (path) / `on_rename` (path, new name) hooks — the local
//!   [`SessionManager`] has no `deleteSession`/`renameSession` (it only
//!   lists). Upstream performs the delete itself (`deleteSessionFile`,
//!   session-selector.ts:645-680, trash CLI + unlink fallback) and shows a
//!   method-dependent status message; the port optimistically removes the
//!   session from the loaded lists and shows "Session deleted". With no
//!   `on_delete` hook, confirming the deletion is a no-op. Rename: upstream
//!   passes `(path, newName)` to the callback and refreshes afterwards
//!   (interactive-mode.ts:4787-4792); the port calls `on_rename(path,
//!   new_name)` and refreshes.
//! - Upstream `SessionList` owns public callback properties wired by the
//!   outer component (session-selector.ts:792-829); Rust ownership prevents
//!   wiring closures into the outer during its own construction, so the list
//!   returns [`SessionListEvent`]s from its input handling and the outer
//!   applies them. The public callback surface (constructor parameters) is
//!   unchanged.
//! - The outer component renders its `buildBaseLayout` (spacer / border /
//!   header / content / border, session-selector.ts:735-747) directly instead
//!   of extending [`Container`]: the session list is owned by the component
//!   and cannot be re-added to a fresh container per render. Line output is
//!   identical.
//! - `searchInput.onSubmit` / `onEscape` (session-selector.ts:341-348) are
//!   not wired: Enter and Escape are intercepted by the list's own
//!   `tui.select.confirm` / `tui.select.cancel` handling before they reach
//!   the input (upstream dead paths). Rename-mode Enter is intercepted by
//!   the outer for the same reason.
//! - Escape in list mode calls `on_cancel` directly; upstream does not clear
//!   a non-empty search first (session-selector.ts:627-631).
//! - The `on_exit` callback is stored but never invoked — upstream assigns
//!   it to `sessionList.onExit` which nothing calls (session-selector.ts:800-803).
//! - Up/down selection clamps at the list bounds (session-selector.ts:604-610),
//!   it does not wrap.
//! - `KeybindingsManager` injection (upstream `options.keybindings`) is
//!   dropped: all matches go through the pir-tui global registry
//!   ([`get_keybindings`]), which the interactive mode installs with the
//!   full 73-entry table including `app.*` ids (interactive-mode.ts:163-204).
//! - `getSelectedSessionPath` returns `Option<String>` (upstream
//!   `string | undefined`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use pir_tui::components::input::Input;
use pir_tui::components::text::Text;
use pir_tui::keybindings::get_keybindings;
use pir_tui::tui::{Component, Focusable, Tui};
use pir_tui::utils::{truncate_to_width, visible_width};

use crate::core::session_manager::{SessionInfo, SessionManager};
use crate::core::themes::Theme;

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::{key_hint, key_text};
use super::session_selector_search::{
    filter_and_sort_sessions, has_session_name, NameFilter, SortMode,
};

/// `SessionScope` (session-selector.ts:23).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionScope {
    Current,
    All,
}

/// `StatusMessage.type` (session-selector.ts:65).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Info,
    Error,
}

/// Home directory (`os.homedir()`, session-selector.ts:26-28).
fn home_dir() -> Option<String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|v| v.to_string_lossy().to_string())
}

/// `shortenPath` (session-selector.ts:26-33): replace the home prefix with
/// `~`.
fn shorten_path(path: &str) -> String {
    if path.is_empty() {
        return path.to_string();
    }
    if let Some(home) = home_dir() {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// `formatSessionDate` (session-selector.ts:35-49): relative age label.
fn format_session_date(modified_ms: i64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let diff_ms = now_ms.saturating_sub(modified_ms).max(0);
    let diff_mins = diff_ms / 60000;
    let diff_hours = diff_ms / 3_600_000;
    let diff_days = diff_ms / 86_400_000;

    if diff_mins < 1 {
        "now".to_string()
    } else if diff_mins < 60 {
        format!("{diff_mins}m")
    } else if diff_hours < 24 {
        format!("{diff_hours}h")
    } else if diff_days < 7 {
        format!("{diff_days}d")
    } else if diff_days < 30 {
        format!("{}w", diff_days / 7)
    } else if diff_days < 365 {
        format!("{}mo", diff_days / 30)
    } else {
        format!("{}y", diff_days / 365)
    }
}

/// `canonicalizePath` (session-selector.ts:51-54, utils/paths.ts:28-34):
/// `realpathSync` with a raw-path fallback.
fn canonicalize_path(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// A session tree node for hierarchical display (session-selector.ts:190-194).
struct SessionTreeNode {
    session: SessionInfo,
    children: Vec<SessionTreeNode>,
    latest_activity: i64,
}

/// Flattened node for display with tree structure info
/// (session-selector.ts:197-203).
struct FlatSessionNode {
    session: SessionInfo,
    depth: usize,
    is_last: bool,
    /// For each ancestor level, whether there are more siblings after it.
    ancestor_continues: Vec<bool>,
}

/// `buildSessionTree` (session-selector.ts:209-254): build a tree from
/// `parentSessionPath`, return root nodes sorted by latest activity
/// (descending) in each subtree.
fn build_session_tree(sessions: Vec<SessionInfo>) -> Vec<SessionTreeNode> {
    let mut by_path: HashMap<String, usize> = HashMap::new();
    let mut nodes: Vec<SessionTreeNode> = Vec::with_capacity(sessions.len());
    for session in sessions {
        let session_path = canonicalize_path(&session.path.to_string_lossy());
        nodes.push(SessionTreeNode {
            session,
            children: Vec::new(),
            latest_activity: 0,
        });
        by_path.insert(session_path, nodes.len() - 1);
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        let parent_path = node
            .session
            .parent_session_path
            .as_deref()
            .map(canonicalize_path);
        match parent_path.and_then(|p| by_path.get(&p).copied()) {
            Some(parent) => children[parent].push(i),
            None => roots.push(i),
        }
    }

    // `updateLatestActivity` (session-selector.ts:231-238): the latest
    // activity of a subtree is the max of all descendants' modified times.
    fn update_latest_activity(
        i: usize,
        nodes: &mut [SessionTreeNode],
        children: &[Vec<usize>],
    ) -> i64 {
        let mut latest = nodes[i].session.modified_ms;
        for &child in &children[i] {
            latest = latest.max(update_latest_activity(child, nodes, children));
        }
        nodes[i].latest_activity = latest;
        latest
    }
    for &root in &roots {
        update_latest_activity(root, &mut nodes, &children);
    }

    // `sortNodes` (session-selector.ts:245-250): sort each subtree by latest
    // activity descending.
    fn sort_nodes(i: usize, nodes: &[SessionTreeNode], children: &mut [Vec<usize>]) {
        children[i].sort_by(|a, b| nodes[*b].latest_activity.cmp(&nodes[*a].latest_activity));
        let kids = children[i].clone();
        for child in kids {
            sort_nodes(child, nodes, children);
        }
    }
    roots.sort_by(|a, b| nodes[*b].latest_activity.cmp(&nodes[*a].latest_activity));
    for &root in roots.clone().iter() {
        sort_nodes(root, &nodes, &mut children);
    }

    // Materialize the tree in sorted order.
    fn materialize(
        i: usize,
        nodes: &[SessionTreeNode],
        children: &[Vec<usize>],
    ) -> SessionTreeNode {
        SessionTreeNode {
            session: nodes[i].session.clone(),
            latest_activity: nodes[i].latest_activity,
            children: children[i]
                .iter()
                .map(|&child| materialize(child, nodes, children))
                .collect(),
        }
    }
    roots
        .into_iter()
        .map(|r| materialize(r, &nodes, &children))
        .collect()
}

/// `flattenSessionTree` (session-selector.ts:259-278): flatten tree into a
/// display list with tree structure metadata.
fn flatten_session_tree(roots: Vec<SessionTreeNode>) -> Vec<FlatSessionNode> {
    let mut result: Vec<FlatSessionNode> = Vec::new();

    fn walk(
        node: &SessionTreeNode,
        depth: usize,
        ancestor_continues: Vec<bool>,
        is_last: bool,
        result: &mut Vec<FlatSessionNode>,
    ) {
        result.push(FlatSessionNode {
            session: node.session.clone(),
            depth,
            is_last,
            ancestor_continues: ancestor_continues.clone(),
        });

        for (i, child) in node.children.iter().enumerate() {
            let child_is_last = i == node.children.len() - 1;
            // Only show continuation lines for non-root ancestors
            // (session-selector.ts:268-269).
            let continues = if depth > 0 { !is_last } else { false };
            let mut next_ancestors = ancestor_continues.clone();
            next_ancestors.push(continues);
            walk(child, depth + 1, next_ancestors, child_is_last, result);
        }
    }

    for (i, root) in roots.iter().enumerate() {
        walk(root, 0, Vec::new(), i == roots.len() - 1, &mut result);
    }

    result
}

/// `SessionSelectorHeader` (session-selector.ts:56-187): the top status
/// block with scope/sort/name state, loading progress and hint lines.
struct SessionSelectorHeader {
    scope: SessionScope,
    sort_mode: SortMode,
    name_filter: NameFilter,
    loading: bool,
    load_progress: Option<(usize, usize)>,
    show_path: bool,
    confirming_delete_path: Option<String>,
    status_message: Option<(StatusKind, String)>,
    show_rename_hint: bool,
    theme: Arc<Theme>,
}

impl SessionSelectorHeader {
    fn new(
        scope: SessionScope,
        sort_mode: SortMode,
        name_filter: NameFilter,
        theme: Arc<Theme>,
    ) -> Self {
        Self {
            scope,
            sort_mode,
            name_filter,
            loading: false,
            load_progress: None,
            show_path: false,
            confirming_delete_path: None,
            status_message: None,
            show_rename_hint: false,
            theme,
        }
    }

    fn set_scope(&mut self, scope: SessionScope) {
        self.scope = scope;
    }

    fn set_sort_mode(&mut self, sort_mode: SortMode) {
        self.sort_mode = sort_mode;
    }

    fn set_name_filter(&mut self, name_filter: NameFilter) {
        self.name_filter = name_filter;
    }

    /// `setLoading` (session-selector.ts:88-92): progress is scoped to the
    /// current load; clear whenever the loading state is set.
    fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
        self.load_progress = None;
    }

    fn set_progress(&mut self, loaded: usize, total: usize) {
        self.load_progress = Some((loaded, total));
    }

    fn set_show_path(&mut self, show_path: bool) {
        self.show_path = show_path;
    }

    fn set_show_rename_hint(&mut self, show: bool) {
        self.show_rename_hint = show;
    }

    fn set_confirming_delete_path(&mut self, path: Option<String>) {
        self.confirming_delete_path = path;
    }

    /// `setStatusMessage` (session-selector.ts:116-126). The upstream
    /// `autoHideMs` timer is not ported (see module header).
    fn set_status_message(&mut self, message: Option<(StatusKind, String)>) {
        self.status_message = message;
    }

    fn render(&self, width: usize) -> Vec<String> {
        let title = if self.scope == SessionScope::Current {
            "Resume Session (Current Folder)"
        } else {
            "Resume Session (All)"
        };
        let left_text = Theme::bold(title);

        let sort_label = match self.sort_mode {
            SortMode::Threaded => "Threaded",
            SortMode::Recent => "Recent",
            SortMode::Relevance => "Fuzzy",
        };
        let sort_text = format!(
            "{}{}",
            self.theme.fg("muted", "Sort: "),
            self.theme.fg("accent", sort_label)
        );

        let name_label = if self.name_filter == NameFilter::All {
            "All"
        } else {
            "Named"
        };
        let name_text = format!(
            "{}{}",
            self.theme.fg("muted", "Name: "),
            self.theme.fg("accent", name_label)
        );

        let scope_text: String = if self.loading {
            let progress_text = match self.load_progress {
                Some((loaded, total)) => format!("{loaded}/{total}"),
                None => "...".to_string(),
            };
            format!(
                "{}{}",
                self.theme.fg("muted", "○ Current Folder | "),
                self.theme.fg("accent", &format!("Loading {progress_text}"))
            )
        } else if self.scope == SessionScope::Current {
            format!(
                "{}{}",
                self.theme.fg("accent", "◉ Current Folder"),
                self.theme.fg("muted", " | ○ All")
            )
        } else {
            format!(
                "{}{}",
                self.theme.fg("muted", "○ Current Folder | "),
                self.theme.fg("accent", "◉ All")
            )
        };

        let right_text = truncate_to_width(
            &format!("{scope_text}  {name_text}  {sort_text}"),
            width,
            "",
            false,
        );
        let available_left = width
            .saturating_sub(visible_width(&right_text))
            .saturating_sub(1);
        let left = truncate_to_width(&left_text, available_left, "", false);
        let spacing = width.saturating_sub(visible_width(&left) + visible_width(&right_text));

        // Hint lines - change based on state (all branches truncate to width)
        // (session-selector.ts:155-183).
        let (hint_line1, hint_line2): (String, String) = if self.confirming_delete_path.is_some() {
            let confirm_hint = format!(
                "Delete session? {} · {}",
                key_hint(&self.theme, "tui.select.confirm", "confirm"),
                key_hint(&self.theme, "tui.select.cancel", "cancel")
            );
            (
                self.theme.fg(
                    "error",
                    &truncate_to_width(&confirm_hint, width, "…", false),
                ),
                String::new(),
            )
        } else if let Some((kind, message)) = &self.status_message {
            let color = if *kind == StatusKind::Error {
                "error"
            } else {
                "accent"
            };
            (
                self.theme
                    .fg(color, &truncate_to_width(message, width, "…", false)),
                String::new(),
            )
        } else {
            let path_state = if self.show_path { "(on)" } else { "(off)" };
            let sep = self.theme.fg("muted", " · ");
            let hint1 = format!(
                "{}{}{}",
                key_hint(&self.theme, "tui.input.tab", "scope"),
                sep,
                self.theme
                    .fg("muted", "re:<pattern> regex · \"phrase\" exact")
            );
            let mut hint2_parts = vec![
                key_hint(&self.theme, "app.session.toggleSort", "sort"),
                key_hint(&self.theme, "app.session.toggleNamedFilter", "named"),
                key_hint(&self.theme, "app.session.delete", "delete"),
                key_hint(
                    &self.theme,
                    "app.session.togglePath",
                    &format!("path {path_state}"),
                ),
            ];
            if self.show_rename_hint {
                hint2_parts.push(key_hint(&self.theme, "app.session.rename", "rename"));
            }
            let hint2 = hint2_parts.join(&sep);
            (
                truncate_to_width(&hint1, width, "…", false),
                truncate_to_width(&hint2, width, "…", false),
            )
        };

        vec![
            format!("{left}{}{right_text}", " ".repeat(spacing)),
            hint_line1,
            hint_line2,
        ]
    }
}

/// Events the [`SessionList`] reports to the outer component — replaces the
/// upstream `onSelect` / `onToggleScope` / ... callback properties
/// (session-selector.ts:299-309, see module header).
enum SessionListEvent {
    Select(String),
    Cancel,
    ToggleScope,
    ToggleSort,
    ToggleNameFilter,
    TogglePath(bool),
    DeleteConfirmChange(Option<String>),
    DeleteConfirm(String),
    Rename(String),
    Error(String),
}

/// Custom session list component with multi-line items and search
/// (`SessionList`, session-selector.ts:283-638). Upstream exposes it through
/// `SessionSelectorComponent.getSessionList()` (session-selector.ts:1028-1030);
/// the port mirrors that with public methods but keeps the fields private.
pub struct SessionList {
    all_sessions: Vec<SessionInfo>,
    filtered_sessions: Vec<FlatSessionNode>,
    selected_index: usize,
    search_input: Input,
    show_cwd: bool,
    sort_mode: SortMode,
    name_filter: NameFilter,
    show_path: bool,
    confirming_delete_path: Option<String>,
    current_session_canonical_path: Option<String>,
    max_visible: usize,
    focused: bool,
    theme: Arc<Theme>,
}

impl SessionList {
    fn new(
        sessions: Vec<SessionInfo>,
        show_cwd: bool,
        sort_mode: SortMode,
        name_filter: NameFilter,
        current_session_file_path: Option<String>,
        theme: Arc<Theme>,
    ) -> Self {
        let mut list = Self {
            all_sessions: sessions,
            filtered_sessions: Vec::new(),
            selected_index: 0,
            search_input: Input::new(),
            show_cwd,
            sort_mode,
            name_filter,
            show_path: false,
            confirming_delete_path: None,
            current_session_canonical_path: current_session_file_path
                .map(|p| canonicalize_path(&p)),
            max_visible: 10,
            focused: false,
            theme,
        };
        list.filter_sessions("");
        list
    }

    fn set_sort_mode(&mut self, sort_mode: SortMode) {
        self.sort_mode = sort_mode;
        let query = self.search_input.get_value().to_string();
        self.filter_sessions(&query);
    }

    fn set_name_filter(&mut self, name_filter: NameFilter) {
        self.name_filter = name_filter;
        let query = self.search_input.get_value().to_string();
        self.filter_sessions(&query);
    }

    /// `setSessions` (session-selector.ts:361-365).
    pub(crate) fn set_sessions(&mut self, sessions: Vec<SessionInfo>, show_cwd: bool) {
        self.all_sessions = sessions;
        self.show_cwd = show_cwd;
        let query = self.search_input.get_value().to_string();
        self.filter_sessions(&query);
    }

    /// `getSelectedSessionPath` (session-selector.ts:284-287).
    pub fn get_selected_session_path(&self) -> Option<String> {
        self.filtered_sessions
            .get(self.selected_index)
            .map(|node| node.session.path.to_string_lossy().to_string())
    }

    /// `filterSessions` (session-selector.ts:367-387).
    fn filter_sessions(&mut self, query: &str) {
        let trimmed = query.trim();
        let name_filtered: Vec<SessionInfo> = if self.name_filter == NameFilter::Named {
            self.all_sessions
                .iter()
                .filter(|session| has_session_name(session))
                .cloned()
                .collect()
        } else {
            self.all_sessions.clone()
        };

        if self.sort_mode == SortMode::Threaded && trimmed.is_empty() {
            // Threaded mode without search: show tree structure
            // (session-selector.ts:372-375).
            let roots = build_session_tree(name_filtered);
            self.filtered_sessions = flatten_session_tree(roots);
        } else {
            // Other modes or with search: flat list (session-selector.ts:377-384).
            let filtered =
                filter_and_sort_sessions(name_filtered, query, self.sort_mode, NameFilter::All);
            self.filtered_sessions = filtered
                .into_iter()
                .map(|session| FlatSessionNode {
                    session,
                    depth: 0,
                    is_last: true,
                    ancestor_continues: Vec::new(),
                })
                .collect();
        }
        self.selected_index = self
            .selected_index
            .min(self.filtered_sessions.len().saturating_sub(1));
    }

    /// `setConfirmingDeletePath` (session-selector.ts:389-392).
    fn set_confirming_delete_path(&mut self, path: Option<String>) {
        self.confirming_delete_path = path;
    }

    /// `startDeleteConfirmationForSelectedSession`
    /// (session-selector.ts:394-405).
    fn start_delete_confirmation_for_selected_session(&mut self) -> Option<SessionListEvent> {
        let selected = self.filtered_sessions.get(self.selected_index)?;

        // Prevent deleting the current session (session-selector.ts:398-402).
        if self.is_current_session_path(&selected.session.path) {
            return Some(SessionListEvent::Error(
                "Cannot delete the currently active session".to_string(),
            ));
        }

        let path = selected.session.path.to_string_lossy().to_string();
        self.set_confirming_delete_path(Some(path.clone()));
        Some(SessionListEvent::DeleteConfirmChange(Some(path)))
    }

    /// `isCurrentSessionPath` (session-selector.ts:407-410).
    fn is_current_session_path(&self, path: &Path) -> bool {
        self.current_session_canonical_path
            .as_deref()
            .is_some_and(|current| canonicalize_path(&path.to_string_lossy()) == *current)
    }

    /// `handleInput` (session-selector.ts:532-637). Returns the event the
    /// outer component must apply (see module header).
    fn process_input(&mut self, data: &str) -> Option<SessionListEvent> {
        let kb = get_keybindings();
        let kb = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());

        // Handle delete confirmation state first - intercept all keys
        // (session-selector.ts:536-549).
        if self.confirming_delete_path.is_some() {
            if kb.matches_id(data, "tui.select.confirm") {
                let path = self.confirming_delete_path.take().expect("checked some");
                return Some(SessionListEvent::DeleteConfirm(path));
            }
            if kb.matches_id(data, "tui.select.cancel") {
                self.set_confirming_delete_path(None);
                return Some(SessionListEvent::DeleteConfirmChange(None));
            }
            // Ignore all other keys while confirming.
            return None;
        }

        if kb.matches_id(data, "tui.input.tab") {
            return Some(SessionListEvent::ToggleScope);
        }

        if kb.matches_id(data, "app.session.toggleSort") {
            return Some(SessionListEvent::ToggleSort);
        }

        if kb.matches_id(data, "app.session.toggleNamedFilter") {
            return Some(SessionListEvent::ToggleNameFilter);
        }

        // Ctrl+P: toggle path display (session-selector.ts:569-573).
        if kb.matches_id(data, "app.session.togglePath") {
            self.show_path = !self.show_path;
            return Some(SessionListEvent::TogglePath(self.show_path));
        }

        // Ctrl+D: initiate delete confirmation (session-selector.ts:576-579).
        if kb.matches_id(data, "app.session.delete") {
            return self.start_delete_confirmation_for_selected_session();
        }

        // Rename selected session (session-selector.ts:582-588).
        if kb.matches_id(data, "app.session.rename") {
            let selected = self.filtered_sessions.get(self.selected_index);
            return selected.map(|node| {
                SessionListEvent::Rename(node.session.path.to_string_lossy().to_string())
            });
        }

        // Ctrl+Backspace: non-invasive convenience alias for delete
        // (session-selector.ts:591-601). Only triggers deletion when the
        // query is empty; otherwise it is forwarded to the input.
        if kb.matches_id(data, "app.session.deleteNoninvasive") {
            if !self.search_input.get_value().is_empty() {
                self.search_input.handle_input(data);
                let query = self.search_input.get_value().to_string();
                self.filter_sessions(&query);
                return None;
            }
            return self.start_delete_confirmation_for_selected_session();
        }

        // Up arrow (session-selector.ts:604-610).
        if kb.matches_id(data, "tui.select.up") {
            self.selected_index = self.selected_index.saturating_sub(1);
        }
        // Down arrow (session-selector.ts:607-610).
        else if kb.matches_id(data, "tui.select.down") {
            self.selected_index =
                (self.selected_index + 1).min(self.filtered_sessions.len().saturating_sub(1));
        }
        // Page up - jump up by maxVisible items (session-selector.ts:612-614).
        else if kb.matches_id(data, "tui.select.pageUp") {
            self.selected_index = self.selected_index.saturating_sub(self.max_visible);
        }
        // Page down - jump down by maxVisible items (session-selector.ts:616-618).
        else if kb.matches_id(data, "tui.select.pageDown") {
            self.selected_index = self
                .selected_index
                .saturating_add(self.max_visible)
                .min(self.filtered_sessions.len().saturating_sub(1));
        }
        // Enter (session-selector.ts:620-625).
        else if kb.matches_id(data, "tui.select.confirm") {
            let selected = self.filtered_sessions.get(self.selected_index);
            return selected.map(|node| {
                SessionListEvent::Select(node.session.path.to_string_lossy().to_string())
            });
        }
        // Escape - cancel (session-selector.ts:627-631).
        else if kb.matches_id(data, "tui.select.cancel") {
            return Some(SessionListEvent::Cancel);
        }
        // Pass everything else to the search input (session-selector.ts:633-636).
        else {
            self.search_input.handle_input(data);
            let query = self.search_input.get_value().to_string();
            self.filter_sessions(&query);
        }
        None
    }

    /// `buildTreePrefix` (session-selector.ts:522-530).
    fn build_tree_prefix(node: &FlatSessionNode) -> String {
        if node.depth == 0 {
            return String::new();
        }
        let parts: String = node
            .ancestor_continues
            .iter()
            .map(|continues| if *continues { "│  " } else { "   " })
            .collect::<Vec<_>>()
            .join("");
        let branch = if node.is_last { "└─ " } else { "├─ " };
        format!("{parts}{branch}")
    }

    fn render_list(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        // Render search input (session-selector.ts:418).
        lines.extend(self.search_input.render(width));
        lines.push(String::new()); // Blank line after search

        if self.filtered_sessions.is_empty() {
            // Empty states (session-selector.ts:421-438).
            let empty_message: String = if self.name_filter == NameFilter::Named {
                let toggle_key = key_text("app.session.toggleNamedFilter");
                if self.show_cwd {
                    format!("  No named sessions found. Press {toggle_key} to show all.")
                } else {
                    format!(
                        "  No named sessions in current folder. Press {toggle_key} to show all, or Tab to view all."
                    )
                }
            } else if self.show_cwd {
                // "All" scope - no sessions anywhere that match the filter.
                "  No sessions found".to_string()
            } else {
                // "Current folder" scope - hint to try "all".
                "  No sessions in current folder. Press Tab to view all.".to_string()
            };
            lines.push(self.theme.fg(
                "muted",
                &truncate_to_width(&empty_message, width, "…", false),
            ));
            return lines;
        }

        // Calculate visible range with scrolling (session-selector.ts:442-446).
        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(
                self.filtered_sessions
                    .len()
                    .saturating_sub(self.max_visible),
            );
        let end_index = (start_index + self.max_visible).min(self.filtered_sessions.len());

        // Render visible sessions (one line each with tree structure)
        // (session-selector.ts:449-510).
        for i in start_index..end_index {
            let node = &self.filtered_sessions[i];
            let session = &node.session;
            let is_selected = i == self.selected_index;
            let is_confirming_delete = session.path.to_string_lossy()
                == self.confirming_delete_path.as_deref().unwrap_or("");
            let is_current = self.is_current_session_path(&session.path);

            // Build tree prefix.
            let prefix = Self::build_tree_prefix(node);

            // Session display text (name or first message)
            // (session-selector.ts:459-463).
            let has_name = session.name.is_some();
            let display_text = session.name.as_deref().unwrap_or(&session.first_message);
            let normalized_message: String = display_text
                .chars()
                .map(|c| if c <= '\x1f' || c == '\x7f' { ' ' } else { c })
                .collect::<String>()
                .trim()
                .to_string();

            // Right side: message count and age (session-selector.ts:465-473).
            let age = format_session_date(session.modified_ms);
            let msg_count = session.message_count.to_string();
            let mut right_part = format!("{msg_count} {age}");
            if self.show_cwd && !session.cwd.is_empty() {
                right_part = format!("{} {right_part}", shorten_path(&session.cwd));
            }
            if self.show_path {
                right_part = format!(
                    "{} {right_part}",
                    shorten_path(&session.path.to_string_lossy())
                );
            }

            // Cursor (session-selector.ts:476).
            let cursor = if is_selected {
                self.theme.fg("accent", "› ")
            } else {
                "  ".to_string()
            };

            // Calculate available width for the message
            // (session-selector.ts:479-483).
            let prefix_width = visible_width(&prefix);
            let right_width = visible_width(&right_part) + 2; // +2 for spacing
            let available_for_msg = width
                .saturating_sub(2)
                .saturating_sub(prefix_width)
                .saturating_sub(right_width); // -2 for cursor
            let truncated_msg =
                truncate_to_width(&normalized_message, available_for_msg.max(10), "…", false);

            // Style the message (session-selector.ts:486-497).
            let message_color: Option<&str> = if is_confirming_delete {
                Some("error")
            } else if is_current {
                Some("accent")
            } else if has_name {
                Some("warning")
            } else {
                None
            };
            let styled_msg = match message_color {
                Some(color) => self.theme.fg(color, &truncated_msg),
                None => truncated_msg,
            };
            let styled_msg = if is_selected {
                Theme::bold(&styled_msg)
            } else {
                styled_msg
            };

            // Build the line (session-selector.ts:500-509).
            let left_part = format!("{cursor}{}{styled_msg}", self.theme.fg("dim", &prefix));
            let left_width = visible_width(&left_part);
            let spacing = width
                .saturating_sub(left_width + visible_width(&right_part))
                .max(1);
            let styled_right = self.theme.fg(
                if is_confirming_delete { "error" } else { "dim" },
                &right_part,
            );

            let mut line = format!("{left_part}{}{styled_right}", " ".repeat(spacing));
            if is_selected {
                line = self.theme.bg("selectedBg", &line);
            }
            lines.push(truncate_to_width(&line, width, "", false));
        }

        // Add scroll indicator if needed (session-selector.ts:513-517).
        if start_index > 0 || end_index < self.filtered_sessions.len() {
            let scroll_text = format!(
                "  ({}/{})",
                self.selected_index + 1,
                self.filtered_sessions.len()
            );
            let scroll_info = self
                .theme
                .fg("muted", &truncate_to_width(&scroll_text, width, "", false));
            lines.push(scroll_info);
        }

        lines
    }
}

impl Component for SessionList {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_list(width)
    }

    fn handle_input(&mut self, data: &str) {
        let _ = self.process_input(data);
    }

    fn invalidate(&mut self) {}

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for SessionList {
    /// `focused` setter propagates to the search input for IME cursor
    /// positioning (session-selector.ts:313-320).
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.search_input.set_focused(focused);
    }
}

/// Component mode of [`SessionSelectorComponent`] (upstream `mode`,
/// session-selector.ts:717).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectorMode {
    List,
    Rename,
}

/// Component that renders a session selector
/// (`SessionSelectorComponent`, session-selector.ts:685-1031).
pub struct SessionSelectorComponent {
    session_list: SessionList,
    header: SessionSelectorHeader,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,
    theme: Arc<Theme>,
    tui: Tui,
    scope: SessionScope,
    sort_mode: SortMode,
    name_filter: NameFilter,
    current_sessions: Vec<SessionInfo>,
    all_sessions: Option<Vec<SessionInfo>>,
    current_loading: bool,
    all_loading: bool,
    mode: SelectorMode,
    rename_input: Input,
    rename_target_path: Option<String>,
    can_rename: bool,
    cwd: PathBuf,
    session_dir: Option<PathBuf>,
    on_select: Box<dyn FnMut(&str) + Send>,
    on_cancel: Box<dyn FnMut() + Send>,
    /// Stored for constructor parity; never invoked — upstream wires it to
    /// `sessionList.onExit`, which nothing calls (session-selector.ts:800-803).
    #[allow(dead_code)]
    _on_exit: Box<dyn FnMut() + Send>,
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    on_delete: Option<Box<dyn FnMut(&str) + Send>>,
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    on_rename: Option<Box<dyn FnMut(&str, &str) + Send>>,
    focused: bool,
}

impl SessionSelectorComponent {
    /// `constructor` (session-selector.ts:749-860). Upstream takes two async
    /// loaders (`SessionsLoader`) and a `requestRender` callback; the port
    /// takes the `cwd` / optional `session_dir` used by
    /// [`SessionManager::list`] / [`SessionManager::list_all`] and loads
    /// synchronously. `current_session_file_path` is the running session's
    /// file, whose deletion is prevented (upstream
    /// `currentSessionFilePath`, session-selector.ts:777-781).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub fn new(
        cwd: PathBuf,
        session_dir: Option<PathBuf>,
        theme: Arc<Theme>,
        tui: Tui,
        on_select: Box<dyn FnMut(&str) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
        on_exit: Box<dyn FnMut() + Send>,
        #[allow(clippy::type_complexity)] // mirrors the upstream callback type
        on_delete: Option<Box<dyn FnMut(&str) + Send>>,
        on_rename: Option<Box<dyn FnMut(&str, &str) + Send>>,
        current_session_file_path: Option<String>,
    ) -> Self {
        let can_rename = on_rename.is_some();

        let mut header = SessionSelectorHeader::new(
            SessionScope::Current,
            SortMode::Threaded,
            NameFilter::All,
            Arc::clone(&theme),
        );
        // `options.showRenameHint ?? this.canRename` (session-selector.ts:772);
        // the hint is derived from the rename hook being present.
        header.set_show_rename_hint(can_rename);

        let border_color = {
            let theme = Arc::clone(&theme);
            Box::new(move |s: &str| theme.fg("accent", s))
        };
        let top_border = DynamicBorder::new(border_color.clone());
        let bottom_border = DynamicBorder::new(border_color);

        let session_list = SessionList::new(
            Vec::new(),
            false,
            SortMode::Threaded,
            NameFilter::All,
            current_session_file_path,
            Arc::clone(&theme),
        );

        let mut component = Self {
            session_list,
            header,
            top_border,
            bottom_border,
            theme,
            tui,
            scope: SessionScope::Current,
            sort_mode: SortMode::Threaded,
            name_filter: NameFilter::All,
            current_sessions: Vec::new(),
            all_sessions: None,
            current_loading: false,
            all_loading: false,
            mode: SelectorMode::List,
            rename_input: Input::new(),
            rename_target_path: None,
            can_rename,
            cwd,
            session_dir,
            on_select,
            on_cancel,
            _on_exit: on_exit,
            on_delete,
            on_rename,
            focused: false,
        };

        // Start loading current sessions immediately
        // (session-selector.ts:859-860).
        component.load_scope(SessionScope::Current);
        component
    }

    /// `getSessionList` (session-selector.ts:1028-1030).
    pub fn get_session_list(&self) -> &SessionList {
        &self.session_list
    }

    /// `loadScope` (session-selector.ts:922-982), synchronous — see module
    /// header. Load failures are impossible: the local list APIs return
    /// `Vec<SessionInfo>` and never error.
    fn load_scope(&mut self, scope: SessionScope) {
        let show_cwd = scope == SessionScope::All;

        // Mark loading (session-selector.ts:926-935).
        if scope == SessionScope::Current {
            self.current_loading = true;
        } else {
            self.all_loading = true;
        }
        self.header.set_scope(scope);
        self.header.set_loading(true);

        let (sessions, total) = if scope == SessionScope::Current {
            let sessions = SessionManager::list(&self.cwd, self.session_dir.as_deref());
            let total = sessions.len();
            self.current_sessions = sessions.clone();
            self.current_loading = false;
            (sessions, total)
        } else {
            let sessions = SessionManager::list_all(self.session_dir.as_deref());
            let total = sessions.len();
            self.all_sessions = Some(sessions.clone());
            self.all_loading = false;
            (sessions, total)
        };

        // Upstream fires onProgress(loaded, total) during the async load
        // (session-selector.ts:937-942); the sync port reports the completed
        // count once before clearing the loading state.
        self.header.set_progress(total, total);
        self.header.set_loading(false);
        self.session_list.set_sessions(sessions, show_cwd);
        self.tui.request_render(false);
    }

    /// `toggleScope` (session-selector.ts:1003-1026).
    fn toggle_scope(&mut self) {
        if self.scope == SessionScope::Current {
            self.scope = SessionScope::All;
            self.header.set_scope(self.scope);

            if let Some(all_sessions) = &self.all_sessions {
                self.header.set_loading(false);
                self.session_list.set_sessions(all_sessions.clone(), true);
                self.tui.request_render(false);
                return;
            }

            if !self.all_loading {
                self.load_scope(SessionScope::All);
            }
            return;
        }

        self.scope = SessionScope::Current;
        self.header.set_scope(self.scope);
        self.header.set_loading(self.current_loading);
        self.session_list
            .set_sessions(self.current_sessions.clone(), false);
        self.tui.request_render(false);
    }

    /// `toggleSortMode` (session-selector.ts:984-990): cycle threaded →
    /// recent → relevance → threaded.
    fn toggle_sort_mode(&mut self) {
        self.sort_mode = match self.sort_mode {
            SortMode::Threaded => SortMode::Recent,
            SortMode::Recent => SortMode::Relevance,
            SortMode::Relevance => SortMode::Threaded,
        };
        self.header.set_sort_mode(self.sort_mode);
        self.session_list.set_sort_mode(self.sort_mode);
        self.tui.request_render(false);
    }

    /// `toggleNameFilter` (session-selector.ts:992-997).
    fn toggle_name_filter(&mut self) {
        self.name_filter = if self.name_filter == NameFilter::All {
            NameFilter::Named
        } else {
            NameFilter::All
        };
        self.header.set_name_filter(self.name_filter);
        self.session_list.set_name_filter(self.name_filter);
        self.tui.request_render(false);
    }

    /// `onDeleteSession` (session-selector.ts:832-856), synchronous: invoke
    /// the `on_delete` hook, drop the session from the loaded lists, show the
    /// status and refresh from disk. Upstream awaits the actual file
    /// deletion (`deleteSessionFile`) and shows a trash/unlink-dependent
    /// message; the port delegates the deletion (see module header).
    fn delete_session(&mut self, path: &str) {
        let Some(on_delete) = self.on_delete.as_mut() else {
            // No delete hook wired by the caller: confirming is a no-op —
            // the session file is not removed.
            self.refresh_sessions_after_mutation();
            return;
        };
        on_delete(path);

        self.current_sessions
            .retain(|s| s.path.to_string_lossy() != path);
        if let Some(all_sessions) = self.all_sessions.as_mut() {
            all_sessions.retain(|s| s.path.to_string_lossy() != path);
        }

        let sessions = if self.scope == SessionScope::All {
            self.all_sessions.clone().unwrap_or_default()
        } else {
            self.current_sessions.clone()
        };
        let show_cwd = self.scope == SessionScope::All;
        self.session_list.set_sessions(sessions, show_cwd);

        self.header
            .set_status_message(Some((StatusKind::Info, "Session deleted".to_string())));
        self.refresh_sessions_after_mutation();
        self.tui.request_render(false);
    }

    /// `refreshSessionsAfterMutation` (session-selector.ts:999-1001).
    fn refresh_sessions_after_mutation(&mut self) {
        self.load_scope(self.scope);
    }

    /// `onRenameSession` + `enterRenameMode` (session-selector.ts:807-815,
    /// 866-887).
    fn enter_rename_mode(&mut self, session_path: &str) {
        if !self.can_rename {
            return;
        }
        if self.scope == SessionScope::Current && self.current_loading {
            return;
        }
        if self.scope == SessionScope::All && self.all_loading {
            return;
        }

        let sessions = if self.scope == SessionScope::All {
            self.all_sessions.clone().unwrap_or_default()
        } else {
            self.current_sessions.clone()
        };
        let name = sessions
            .iter()
            .find(|s| s.path.to_string_lossy() == session_path)
            .and_then(|s| s.name.clone());

        self.mode = SelectorMode::Rename;
        self.rename_target_path = Some(session_path.to_string());
        self.rename_input.set_value(name.as_deref().unwrap_or(""));
        self.rename_input.set_focused(true);
        self.tui.request_render(false);
    }

    /// `exitRenameMode` (session-selector.ts:889-896).
    fn exit_rename_mode(&mut self) {
        self.mode = SelectorMode::List;
        self.rename_target_path = None;
        self.tui.request_render(false);
    }

    /// `confirmRename` (session-selector.ts:898-920), synchronous. Upstream
    /// awaits `renameSession(target, next)` and refreshes in a `finally`;
    /// the port invokes the `on_rename` hook and refreshes.
    fn confirm_rename(&mut self, value: &str) {
        let next = value.trim();
        if next.is_empty() {
            return;
        }
        let Some(target) = self.rename_target_path.clone() else {
            self.exit_rename_mode();
            return;
        };
        if self.on_rename.is_none() {
            self.exit_rename_mode();
            return;
        }

        if let Some(on_rename) = self.on_rename.as_mut() {
            on_rename(&target, next);
        }
        self.refresh_sessions_after_mutation();
        self.exit_rename_mode();
    }

    /// Apply a [`SessionListEvent`] — the outer half of the upstream
    /// callback wiring (session-selector.ts:792-829).
    fn apply_event(&mut self, event: SessionListEvent) {
        match event {
            SessionListEvent::Select(path) => {
                self.header.set_status_message(None);
                (self.on_select)(&path);
            }
            SessionListEvent::Cancel => {
                self.header.set_status_message(None);
                (self.on_cancel)();
            }
            SessionListEvent::ToggleScope => self.toggle_scope(),
            SessionListEvent::ToggleSort => self.toggle_sort_mode(),
            SessionListEvent::ToggleNameFilter => self.toggle_name_filter(),
            SessionListEvent::TogglePath(show) => {
                self.header.set_show_path(show);
                self.tui.request_render(false);
            }
            SessionListEvent::DeleteConfirmChange(path) => {
                self.header.set_confirming_delete_path(path);
                self.tui.request_render(false);
            }
            SessionListEvent::DeleteConfirm(path) => {
                self.header.set_confirming_delete_path(None);
                self.delete_session(&path);
            }
            SessionListEvent::Rename(path) => self.enter_rename_mode(&path),
            SessionListEvent::Error(message) => {
                self.header
                    .set_status_message(Some((StatusKind::Error, message)));
                self.tui.request_render(false);
            }
        }
    }

    /// `buildBaseLayout` content (session-selector.ts:735-747) rendered
    /// directly (see module header): spacer, border, header, content, spacer,
    /// border. The rename panel content (session-selector.ts:872-883)
    /// replaces the list when renaming.
    fn render_layout(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(String::new()); // Spacer(1)
        lines.extend(self.top_border.render(width));
        lines.push(String::new()); // Spacer(1)
        if self.mode == SelectorMode::List {
            lines.extend(self.header.render(width));
            lines.push(String::new()); // Spacer(1)
            lines.extend(self.session_list.render(width));
        } else {
            // Rename panel (session-selector.ts:872-883).
            lines.extend(Text::new(Theme::bold("Rename Session"), 1, 0, None).render(width));
            lines.push(String::new()); // Spacer(1)
            lines.extend(self.rename_input.render(width));
            lines.push(String::new()); // Spacer(1)
            let hint = format!(
                "{} to save · {} to cancel",
                key_text("tui.select.confirm"),
                key_text("tui.select.cancel")
            );
            lines.extend(Text::new(self.theme.fg("muted", &hint), 1, 0, None).render(width));
        }
        lines.push(String::new()); // Spacer(1)
        lines.extend(self.bottom_border.render(width));
        lines
    }
}

impl Component for SessionSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_layout(width)
    }

    /// `handleInput` (session-selector.ts:686-698): rename mode intercepts
    /// cancel/confirm and forwards everything else to the rename input; list
    /// mode forwards to the session list.
    fn handle_input(&mut self, data: &str) {
        if self.mode == SelectorMode::Rename {
            let kb = get_keybindings();
            let kb = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());
            if kb.matches_id(data, "tui.select.cancel") {
                self.exit_rename_mode();
                return;
            }
            // Enter confirms the rename (replaces the unwired
            // `renameInput.onSubmit`, session-selector.ts:786-788 — see
            // module header).
            if kb.matches_id(data, "tui.select.confirm") {
                let value = self.rename_input.get_value().to_string();
                self.confirm_rename(&value);
                return;
            }
            // Drop the keybinding read guard before forwarding:
            // `Input::handle_input` takes its own read lock on the same
            // global (std RwLock is not reentrant against a queued writer).
            drop(kb);
            self.rename_input.handle_input(data);
            return;
        }

        if let Some(event) = self.session_list.process_input(data) {
            self.apply_event(event);
        }
    }

    fn invalidate(&mut self) {}

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for SessionSelectorComponent {
    /// `focused` setter propagates to the session list and rename input
    /// (session-selector.ts:723-733).
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.session_list.set_focused(focused);
        self.rename_input.set_focused(focused);
        if focused && self.mode == SelectorMode::Rename {
            self.rename_input.set_focused(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::interactive_mode::install_global_keybindings;
    use crate::modes::interactive::test_support::{TempDir, TestTerminal};

    /// ISO-8601 UTC timestamp for the given epoch ms (same civil-from-days
    /// algorithm as `tui.rs`).
    fn iso_timestamp(ms: i64) -> String {
        let secs = ms.div_euclid(1000);
        let millis = ms.rem_euclid(1000);
        let days = secs.div_euclid(86_400);
        let secs_of_day = secs.rem_euclid(86_400);
        let (hour, minute, second) = (
            secs_of_day / 3600,
            secs_of_day % 3600 / 60,
            secs_of_day % 60,
        );
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let day_of_era = z.rem_euclid(146_097);
        let year_of_era =
            (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let year = year_of_era + era * 400;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_prime = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
        let month = if month_prime < 10 {
            month_prime + 3
        } else {
            month_prime - 9
        };
        let year = if month <= 2 { year + 1 } else { year };
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
    }

    /// Write a real session file parseable by `SessionManager::list`:
    /// header line + optional session_info (name) + optional user message.
    /// `modified_ms` becomes the user-message activity time, or the header
    /// timestamp when no message is written.
    fn write_session(
        dir: &Path,
        id: &str,
        cwd: &str,
        name: Option<&str>,
        parent: Option<&str>,
        first_message: Option<&str>,
        modified_ms: i64,
    ) -> PathBuf {
        let path = dir.join(format!("{id}.jsonl"));
        let mut lines = String::new();
        let mut header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": id,
            "timestamp": iso_timestamp(modified_ms),
            "cwd": cwd,
        });
        if let Some(parent) = parent {
            header["parentSession"] = serde_json::Value::String(parent.to_string());
        }
        lines.push_str(&header.to_string());
        lines.push('\n');
        if let Some(name) = name {
            let info = serde_json::json!({
                "type": "session_info",
                "id": format!("si-{id}"),
                "parentId": null,
                "timestamp": iso_timestamp(modified_ms),
                "name": name,
            });
            lines.push_str(&info.to_string());
            lines.push('\n');
        }
        if let Some(message) = first_message {
            let entry = serde_json::json!({
                "type": "message",
                "id": format!("m-{id}"),
                "parentId": null,
                "timestamp": iso_timestamp(modified_ms),
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": message}],
                    "timestamp": modified_ms,
                },
            });
            lines.push_str(&entry.to_string());
            lines.push('\n');
        }
        std::fs::write(&path, lines).expect("write session file");
        path
    }

    /// Test harness: temp session dir + cwd, plus the written session paths.
    struct Harness {
        _tmp: TempDir,
        cwd: PathBuf,
        session_dir: PathBuf,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = TempDir::new();
            let cwd = tmp.path().join("cwd");
            std::fs::create_dir_all(&cwd).expect("cwd dir");
            let session_dir = tmp.path().join("sessions");
            std::fs::create_dir_all(&session_dir).expect("sessions dir");
            Self {
                _tmp: tmp,
                cwd,
                session_dir,
            }
        }

        fn cwd_str(&self) -> String {
            self.cwd.to_string_lossy().to_string()
        }
    }

    fn theme() -> Arc<Theme> {
        Arc::new(crate::core::themes::load_theme("dark", None).expect("builtin dark theme"))
    }

    fn tui() -> Tui {
        Tui::new(Box::new(TestTerminal::new()))
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn rendered(component: &SessionSelectorComponent, width: usize) -> Vec<String> {
        component
            .render(width)
            .iter()
            .map(|line| strip_ansi(line))
            .collect()
    }

    struct Callbacks {
        selected: Arc<std::sync::Mutex<Vec<String>>>,
        cancelled: Arc<std::sync::Mutex<usize>>,
        deleted: Arc<std::sync::Mutex<Vec<String>>>,
        renamed: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    impl Callbacks {
        fn new() -> Self {
            Self {
                selected: Arc::new(std::sync::Mutex::new(Vec::new())),
                cancelled: Arc::new(std::sync::Mutex::new(0)),
                deleted: Arc::new(std::sync::Mutex::new(Vec::new())),
                renamed: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
        fn on_select(&self) -> Box<dyn FnMut(&str) + Send> {
            let captured = Arc::clone(&self.selected);
            Box::new(move |path: &str| {
                captured.lock().unwrap().push(path.to_string());
            })
        }
        fn on_cancel(&self) -> Box<dyn FnMut() + Send> {
            let captured = Arc::clone(&self.cancelled);
            Box::new(move || {
                *captured.lock().unwrap() += 1;
            })
        }
        #[allow(clippy::type_complexity)] // mirrors the upstream callback type
        fn on_delete(&self) -> Box<dyn FnMut(&str) + Send> {
            let captured = Arc::clone(&self.deleted);
            Box::new(move |path: &str| {
                captured.lock().unwrap().push(path.to_string());
            })
        }
        #[allow(clippy::too_many_arguments)] // mirrors the upstream constructor
        #[allow(clippy::type_complexity)] // mirrors the upstream callback type
        fn on_rename(&self) -> Box<dyn FnMut(&str, &str) + Send> {
            let captured = Arc::clone(&self.renamed);
            Box::new(move |path: &str, name: &str| {
                captured
                    .lock()
                    .unwrap()
                    .push((path.to_string(), name.to_string()));
            })
        }
    }

    fn build(
        harness: &Harness,
        callbacks: &Callbacks,
        on_delete: bool,
        on_rename: bool,
        current_session_file: Option<&Path>,
    ) -> SessionSelectorComponent {
        SessionSelectorComponent::new(
            harness.cwd.clone(),
            Some(harness.session_dir.clone()),
            theme(),
            tui(),
            callbacks.on_select(),
            callbacks.on_cancel(),
            Box::new(|| {}),
            on_delete.then(|| callbacks.on_delete()),
            on_rename.then(|| callbacks.on_rename()),
            current_session_file.map(|p| p.to_string_lossy().to_string()),
        )
    }

    /// Session file body strings stripped of ANSI codes, one entry per
    /// rendered line.
    fn body_lines(component: &SessionSelectorComponent, width: usize) -> Vec<String> {
        rendered(component, width)
    }

    #[test]
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn renders_header_and_sessions() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("fix the build"),
            1_000,
        );
        write_session(
            &harness.session_dir,
            "s2",
            &cwd,
            Some("deploy"),
            None,
            Some("ship it"),
            2_000,
        );
        let callbacks = Callbacks::new();
        let component = build(&harness, &callbacks, false, false, None);

        // At width 80 the right-side status block (~53 cols) truncates the
        // title like upstream (`availableLeft = width - rightWidth - 1`,
        // session-selector.ts:151-153); at width 200 the full title fits.
        let lines = rendered(&component, 80);
        let joined = lines.join("\n");
        assert!(joined.contains("Resume Session"));
        assert!(joined.contains("◉ Current Folder"));
        assert!(joined.contains("Sort: Threaded"));
        let wide = rendered(&component, 200).join("\n");
        assert!(wide.contains("Resume Session (Current Folder)"));
        // Named sessions display the name; unnamed ones the first message.
        assert!(wide.contains("deploy"));
        assert!(wide.contains("fix the build"));
        // The selected line (s2, sorted first by modified desc) gets the
        // selectedBg background color.
        let raw = component.render(200);
        let selected_line = raw.iter().find(|l| l.contains("deploy")).unwrap();
        assert!(
            selected_line.contains("\u{1b}[48"),
            "selected line has bg color"
        );
        // Every visible line stays within the width budget.
        for line in &raw {
            assert!(visible_width(line) <= 200, "overflow: {line:?}");
        }
    }

    #[test]
    fn scope_switch_toggles_current_and_all() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        let other_cwd = harness
            .cwd
            .parent()
            .unwrap()
            .join("other")
            .to_string_lossy()
            .to_string();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("in cwd"),
            1_000,
        );
        write_session(
            &harness.session_dir,
            "s2",
            &other_cwd,
            None,
            None,
            Some("elsewhere"),
            2_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, false, None);

        // Current scope: only the matching-cwd session.
        let lines = body_lines(&component, 80);
        assert!(lines.join("\n").contains("in cwd"));
        assert!(!lines.join("\n").contains("elsewhere"));
        assert!(lines.join("\n").contains("◉ Current Folder"));

        // Tab → all scope: both sessions, header flips to ◉ All.
        component.handle_input("\t");
        let lines = body_lines(&component, 80);
        let joined = lines.join("\n");
        assert!(joined.contains("in cwd"));
        assert!(joined.contains("elsewhere"));
        assert!(joined.contains("◉ All"));

        // Tab → back to current.
        component.handle_input("\t");
        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("◉ Current Folder"));
        assert!(!joined.contains("elsewhere"));
    }

    #[test]
    fn sort_mode_cycles_threaded_recent_relevance() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("older"),
            1_000,
        );
        write_session(
            &harness.session_dir,
            "s2",
            &cwd,
            None,
            None,
            Some("newer"),
            2_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, false, None);

        assert!(body_lines(&component, 80)
            .join("\n")
            .contains("Sort: Threaded"));

        component.handle_input("\x13"); // Ctrl+S
        assert!(body_lines(&component, 80)
            .join("\n")
            .contains("Sort: Recent"));

        component.handle_input("\x13"); // Ctrl+S
        assert!(body_lines(&component, 80)
            .join("\n")
            .contains("Sort: Fuzzy"));

        component.handle_input("\x13"); // Ctrl+S
        assert!(body_lines(&component, 80)
            .join("\n")
            .contains("Sort: Threaded"));
    }

    #[test]
    fn selection_moves_and_enter_selects() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        let p1 = write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("first"),
            1_000,
        );
        let p2 = write_session(
            &harness.session_dir,
            "s2",
            &cwd,
            None,
            None,
            Some("second"),
            2_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, false, None);

        // s2 is newest → selected by default.
        component.handle_input("\r"); // Enter
        let selected = callbacks.selected.lock().unwrap().clone();
        assert_eq!(selected[0], p2.to_string_lossy().to_string());

        component.handle_input("\x1b[B"); // Down → s1
        component.handle_input("\r");
        let selected = callbacks.selected.lock().unwrap().clone();
        assert_eq!(selected[1], p1.to_string_lossy().to_string());

        component.handle_input("\x1b[A"); // Up → back to s2
        component.handle_input("\r");
        let selected = callbacks.selected.lock().unwrap().clone();
        assert_eq!(selected[2], p2.to_string_lossy().to_string());
    }

    #[test]
    fn escape_cancels() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("first"),
            1_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, false, None);

        component.handle_input("\x1b"); // Escape
        assert_eq!(*callbacks.cancelled.lock().unwrap(), 1);
        // Ctrl+C also matches tui.select.cancel.
        component.handle_input("\x03");
        assert_eq!(*callbacks.cancelled.lock().unwrap(), 2);
    }

    #[test]
    fn search_filters_with_tokens_regex_and_phrase() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("fix node cve"),
            1_000,
        );
        write_session(
            &harness.session_dir,
            "s2",
            &cwd,
            None,
            None,
            Some("deploy to prod"),
            2_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, false, None);

        // Fuzzy tokens: "fix cve" matches s1 only.
        for ch in "fix cve".chars() {
            component.handle_input(&ch.to_string());
        }
        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("fix node cve"));
        assert!(!joined.contains("deploy to prod"));

        // Clear and use re: regex (case-insensitive).
        for _ in 0..8 {
            component.handle_input("\x7f"); // Backspace
        }
        for ch in "re:PROD".chars() {
            component.handle_input(&ch.to_string());
        }
        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("deploy to prod"));
        assert!(!joined.contains("fix node cve"));

        // Clear and use a quoted phrase.
        for _ in 0..8 {
            component.handle_input("\x7f");
        }
        for ch in "\"node cve\"".chars() {
            component.handle_input(&ch.to_string());
        }
        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("fix node cve"));
        assert!(!joined.contains("deploy to prod"));

        // No match: empty-state message.
        for _ in 0..10 {
            component.handle_input("\x7f");
        }
        for ch in "zzz".chars() {
            component.handle_input(&ch.to_string());
        }
        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("No sessions in current folder"));
    }

    #[test]
    fn invalid_regex_shows_empty_state() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("anything"),
            1_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, false, None);

        for ch in "re:[".chars() {
            component.handle_input(&ch.to_string());
        }
        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("No sessions in current folder"));
    }

    #[test]
    fn delete_confirmation_intercepts_then_invokes_hook() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        // s1 is the newest session (sorted first → selected).
        let target = write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("to delete"),
            2_000,
        );
        write_session(
            &harness.session_dir,
            "s2",
            &cwd,
            None,
            None,
            Some("kept"),
            1_000,
        );
        let deleted: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let target_arc = Arc::new(target.clone());
        let mut component = SessionSelectorComponent::new(
            harness.cwd.clone(),
            Some(harness.session_dir.clone()),
            theme(),
            tui(),
            Box::new(|_| {}),
            Box::new(|| {}),
            Box::new(|| {}),
            Some(Box::new({
                // The integration layer performs the actual file deletion;
                // the refresh after the mutation then drops the session from
                // the list.
                let deleted = Arc::clone(&deleted);
                let target = Arc::clone(&target_arc);
                move |path: &str| {
                    if path == target.to_string_lossy() {
                        std::fs::remove_file(&*target).expect("remove session file");
                    }
                    deleted.lock().unwrap().push(path.to_string());
                }
            })),
            None,
            None,
        );

        // Ctrl+D enters the confirmation state: header shows the confirm hint.
        component.handle_input("\x04");
        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("Delete session?"));
        assert!(deleted.lock().unwrap().is_empty());

        // Arrow keys are ignored while confirming.
        component.handle_input("\x1b[B");
        assert!(deleted.lock().unwrap().is_empty());

        // Enter confirms: the hook fires with the selected path and the
        // session disappears from the list.
        component.handle_input("\r");
        assert_eq!(
            deleted.lock().unwrap().clone(),
            vec![target.to_string_lossy().to_string()]
        );
        let joined = body_lines(&component, 80).join("\n");
        assert!(!joined.contains("to delete"));
        assert!(joined.contains("kept"));
        assert!(joined.contains("Session deleted"));
    }

    #[test]
    fn delete_confirmation_escape_aborts() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("to delete"),
            1_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, true, false, None);

        component.handle_input("\x04"); // Enter confirmation
        assert!(body_lines(&component, 80)
            .join("\n")
            .contains("Delete session?"));
        component.handle_input("\x1b"); // Escape aborts
        assert!(callbacks.deleted.lock().unwrap().is_empty());
        let joined = body_lines(&component, 80).join("\n");
        assert!(!joined.contains("Delete session?"));
        assert!(joined.contains("to delete"));
    }

    #[test]
    fn delete_confirmation_ctrl_c_aborts() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("to delete"),
            1_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, true, false, None);

        component.handle_input("\x04");
        component.handle_input("\x03"); // Ctrl+C = tui.select.cancel
        assert!(callbacks.deleted.lock().unwrap().is_empty());
        assert!(!body_lines(&component, 80)
            .join("\n")
            .contains("Delete session?"));
    }

    #[test]
    fn current_session_cannot_be_deleted() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        // The current session is the newest → selected by default.
        let current = write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("current"),
            2_000,
        );
        write_session(
            &harness.session_dir,
            "s2",
            &cwd,
            None,
            None,
            Some("other"),
            1_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, true, false, Some(&current));

        // Deleting the current session shows an error status instead of the
        // confirmation state.
        component.handle_input("\x04");
        assert!(callbacks.deleted.lock().unwrap().is_empty());
        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("Cannot delete the currently active session"));
        assert!(!joined.contains("Delete session?"));
    }

    #[test]
    fn delete_without_hook_is_noop() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("stays"),
            1_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, false, None);

        component.handle_input("\x04");
        component.handle_input("\r");
        // No hook was invoked; the session stays listed.
        assert!(callbacks.deleted.lock().unwrap().is_empty());
        assert!(body_lines(&component, 80).join("\n").contains("stays"));
    }

    #[test]
    fn rename_mode_edits_and_confirms() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        // s2 is newest → selected first; s1 (named) is second.
        write_session(
            &harness.session_dir,
            "s2",
            &cwd,
            None,
            None,
            Some("other"),
            2_000,
        );
        let target = write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            Some("old name"),
            None,
            Some("msg"),
            1_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, true, None);

        // Ctrl+R on the selected (unnamed) session: rename panel with an
        // empty pre-filled name.
        component.handle_input("\x12");
        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("Rename Session"));
        assert!(joined.contains("to save"));

        // Escape, move down to the named session and rename it.
        component.handle_input("\x1b");
        component.handle_input("\x1b[B");
        component.handle_input("\x12");
        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("old name")); // pre-filled with the current name

        // Ctrl+A (line start) + Ctrl+K (delete to end), then type the new name.
        component.handle_input("\x01");
        component.handle_input("\x0b");
        for ch in "new name".chars() {
            component.handle_input(&ch.to_string());
        }
        component.handle_input("\r");
        assert_eq!(
            callbacks.renamed.lock().unwrap().clone(),
            vec![(target.to_string_lossy().to_string(), "new name".to_string())]
        );
        // Back in list mode after confirming.
        let joined = body_lines(&component, 80).join("\n");
        assert!(!joined.contains("Rename Session"));
        assert!(joined.contains("other"));
    }

    #[test]
    fn rename_mode_escape_cancels_without_callback() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            Some("old name"),
            None,
            Some("msg"),
            1_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, true, None);

        component.handle_input("\x12"); // Ctrl+R
        component.handle_input("\x1b"); // Escape cancels rename mode
        assert!(callbacks.renamed.lock().unwrap().is_empty());
        let joined = body_lines(&component, 80).join("\n");
        assert!(!joined.contains("Rename Session"));
        // Back in list mode: the session line shows its name.
        assert!(joined.contains("old name"));
    }

    #[test]
    fn rename_empty_value_does_not_confirm() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            Some("old name"),
            None,
            Some("msg"),
            1_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, true, None);

        component.handle_input("\x12"); // Ctrl+R
        component.handle_input("\x01"); // Ctrl+A
        component.handle_input("\x0b"); // Ctrl+K deletes the rest
        component.handle_input("\r");
        assert!(callbacks.renamed.lock().unwrap().is_empty());
        // Still in rename mode (upstream returns early on empty input).
        assert!(body_lines(&component, 80)
            .join("\n")
            .contains("Rename Session"));
    }

    #[test]
    fn named_filter_toggles() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("unnamed"),
            1_000,
        );
        write_session(
            &harness.session_dir,
            "s2",
            &cwd,
            Some("named"),
            None,
            Some("named msg"),
            2_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, false, None);

        assert!(body_lines(&component, 80).join("\n").contains("Name: All"));
        component.handle_input("\x0e"); // Ctrl+N
        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("Name: Named"));
        // Only the named session stays; its line shows the name.
        assert!(joined.contains("named"));
        assert!(!joined.contains("unnamed"));
        // Back to all.
        component.handle_input("\x0e"); // Ctrl+N
        assert!(body_lines(&component, 80).join("\n").contains("unnamed"));
    }

    #[test]
    fn toggle_path_shows_paths() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        let path = write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("msg"),
            1_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, false, None);

        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("path (off)"));
        assert!(!joined.contains(&path.to_string_lossy().to_string()));
        component.handle_input("\x10"); // Ctrl+P
        let joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("path (on)"));
        assert!(joined.contains(&path.to_string_lossy().to_string()));
    }

    #[test]
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn threaded_tree_renders_hierarchy() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        let parent = write_session(
            &harness.session_dir,
            "p",
            &cwd,
            None,
            None,
            Some("parent"),
            1_000,
        );
        let parent_str = parent.to_string_lossy().to_string();
        write_session(
            &harness.session_dir,
            "c",
            &cwd,
            None,
            Some(&parent_str),
            Some("child"),
            2_000,
        );
        let callbacks = Callbacks::new();
        let component = build(&harness, &callbacks, false, false, None);

        let joined = body_lines(&component, 80).join("\n");
        // Both sessions visible in threaded mode with a tree branch marker.
        assert!(joined.contains("parent"));
        assert!(joined.contains("child"));
        assert!(joined.contains("└─") || joined.contains("├─"));
        // The child (newest activity) bubbles up: parent sorts first.
        let parent_pos = joined.find("parent").unwrap();
        let child_pos = joined.find("child").unwrap();
        assert!(parent_pos < child_pos);
    }

    #[test]
    fn scrolling_shows_position_indicator() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        for i in 0..15 {
            write_session(
                &harness.session_dir,
                &format!("s{i:02}"),
                &cwd,
                None,
                None,
                Some(&format!("session {i}")),
                i * 1000,
            );
        }
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, false, None);

        // Move far down; the scroll indicator is present as soon as the
        // window end drops below the list length (start=0, end=10 < 15).
        let mut joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("(1/15)"));
        assert!(joined.contains("session 14"));
        for _ in 0..14 {
            component.handle_input("\x1b[B");
        }
        joined = body_lines(&component, 80).join("\n");
        assert!(joined.contains("(15/15)"));
        assert!(joined.contains("session 0"));
    }

    #[test]
    fn focus_propagates_to_search_input() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("msg"),
            1_000,
        );
        let callbacks = Callbacks::new();
        let mut component = build(&harness, &callbacks, false, false, None);

        component.set_focused(true);
        // The focused search input emits the cursor marker in its line; the
        // reverse-video fake cursor (`\x1b[7m`) is unique to the input line.
        let raw = component.render(80);
        let input_line = raw
            .iter()
            .find(|l| l.contains("\x1b[7m"))
            .expect("input line");
        assert!(input_line.contains("\u{1b}_pi:c\u{07}"));
        component.set_focused(false);
        let raw = component.render(80);
        let input_line = raw
            .iter()
            .find(|l| l.contains("\x1b[7m"))
            .expect("input line");
        assert!(!input_line.contains("\u{1b}_pi:c\u{07}"));
    }

    #[test]
    fn session_list_standalone_filters_and_selects() {
        install_global_keybindings();
        let harness = Harness::new();
        let cwd = harness.cwd_str();
        let s1 = write_session(
            &harness.session_dir,
            "s1",
            &cwd,
            None,
            None,
            Some("alpha"),
            1_000,
        );
        let s2 = write_session(
            &harness.session_dir,
            "s2",
            &cwd,
            None,
            None,
            Some("beta"),
            2_000,
        );
        let sessions = SessionManager::list(&harness.cwd, Some(&harness.session_dir));
        assert_eq!(sessions.len(), 2);
        let mut list = SessionList::new(
            sessions,
            false,
            SortMode::Threaded,
            NameFilter::All,
            None,
            theme(),
        );

        assert_eq!(
            list.get_selected_session_path().as_deref(),
            Some(s2.to_string_lossy().as_ref())
        );
        list.handle_input("alpha");
        // Typing "alpha" keeps s1; the selected path re-clamps to s1.
        assert_eq!(
            list.get_selected_session_path().as_deref(),
            Some(s1.to_string_lossy().as_ref())
        );
        assert!(strip_ansi(&list.render(80).join("\n")).contains("alpha"));
        assert!(!strip_ansi(&list.render(80).join("\n")).contains("beta"));
    }
}
