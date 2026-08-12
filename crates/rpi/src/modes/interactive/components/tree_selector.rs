//! Tree selector — port of
//! `packages/coding-agent/src/modes/interactive/components/tree-selector.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816); the dynamic border color is
//!   derived from the same theme.
//! - Upstream `TreeSelectorComponent` composes its layout through `Container`
//!   children; the port renders the same line sequence directly (spacer,
//!   border, title, help, search line, border, spacer, list / label input,
//!   spacer, border) — identical output contract.
//! - Upstream `SearchLine` reads the search query live off the `TreeList`
//!   (search-line.ts-style `getSearchQuery()`); the port caches the query in
//!   `SearchLine` (synced after every input dispatch) because Rust
//!   `Component::render` takes `&self`.
//! - Upstream wires `treeList.onLabelEdit` / `treeList.onCopy` /
//!   `labelInput.onSubmit` / `labelInput.onCancel` to the outer component
//!   with closures capturing `this` (tree-selector.ts:1365-1368, 1394-1399);
//!   the port forwards these through shared slots (`Arc<Mutex<...>>`) drained
//!   after each dispatch.
//! - Upstream schedules `onCancel` 100ms after construction for empty trees
//!   (tree-selector.ts:1387-1389); the port does not fire it — the
//!   integration layer decides when to dismiss an empty tree.
//! - `formatLabelTimestamp` uses JS `Date` local time upstream; the port
//!   converts via `libc::localtime_r` on unix and falls back to UTC
//!   (civil-from-days) elsewhere. Same `HH:MM` / `M/D HH:MM` /
//!   `YY/M/D HH:MM` output shapes.
//! - `JSON.stringify` argument previews in `formatToolCall` preserve
//!   insertion order upstream; serde_json maps order keys by their default
//!   (BTreeMap) ordering.
//! - Raw/unknown persisted entries (`StoredEntry::Raw`) render as an empty
//!   string and are visible in the "default" filter view; upstream only has
//!   typed entries.
//! - `updateNodeLabel` timestamps use the rpi `now_iso8601()` helper
//!   (`new Date().toISOString()` upstream).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use rpi_agent::messages::AgentMessage;
use rpi_agent::session::{parse_iso8601_ms, SessionEntry};
use rpi_ai::types::{AssistantContent, StopReason, ToolResultContent, UserContent};
use rpi_ai::utils::text::{content_text_assistant, content_text_tool_result, content_text_user};
use rpi_tui::components::input::Input;
use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::text::Text;
use rpi_tui::keybindings::get_keybindings;
use rpi_tui::tui::{Component, Focusable};
use rpi_tui::tui_main_screen::TuiMainScreen;
use rpi_tui::utils::{slice_by_column, truncate_to_width, visible_width, wrap_text_with_ansi};
use serde_json::{Map, Value};

use crate::core::session_manager::{now_iso8601, SessionTreeNode};
use crate::core::settings_manager::TreeFilterMode;
use crate::core::themes::Theme;

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::{format_key_text, key_hint, KeyTextFormatOptions};

/// Gutter info: position (displayIndent where connector was shown) and
/// whether to show `│` (tree-selector.ts:20-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GutterInfo {
    /// displayIndent level where the connector was shown.
    position: usize,
    /// true = show `│`, false = show spaces.
    show: bool,
}

/// Flattened tree node for navigation (tree-selector.ts:26-39).
#[derive(Debug, Clone)]
struct FlatNode {
    node: SessionTreeNode,
    /// Indentation level (each level = 3 chars).
    indent: usize,
    /// Whether to show connector (`├─` or `└─`) — true if parent has
    /// multiple children.
    show_connector: bool,
    /// If show_connector, true = last sibling (`└─`), false = not last (`├─`).
    is_last: bool,
    /// Gutter info for each ancestor branch point.
    gutters: Vec<GutterInfo>,
    /// True if this node is a root under a virtual branching root
    /// (multiple roots).
    is_virtual_root_child: bool,
}

struct HorizontalViewportRow {
    gutter: String,
    body: String,
    anchor_col: usize,
    body_width: usize,
    is_selected: bool,
}

const TREE_GUTTER_WIDTH: usize = 2;
const MIN_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 4;
const MAX_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 20;
const MIN_ANCHOR_CONTEXT_WIDTH: usize = 2;
const MAX_ANCHOR_CONTEXT_WIDTH: usize = 12;

/// Render tree rows into a horizontally clipped viewport
/// (tree-selector.ts:62-92).
///
/// The tree gutter is always kept visible. The row bodies are shifted left
/// only when the selected row's anchor (the start of its entry text after
/// tree indentation/markers) would otherwise be too far right to see useful
/// content.
fn render_horizontal_viewport(rows: &[HorizontalViewportRow], width: usize) -> Vec<String> {
    let viewport_width = width.saturating_sub(TREE_GUTTER_WIDTH);
    let max_body_width = rows.iter().map(|row| row.body_width).max().unwrap_or(0);
    let max_horizontal_scroll = max_body_width.saturating_sub(viewport_width);
    let selected_row = rows.iter().find(|row| row.is_selected);

    // Only pan horizontally when needed to keep enough selected-row content
    // visible after its anchor.
    let mut horizontal_scroll = 0;
    if let Some(selected_row) = selected_row {
        if max_horizontal_scroll > 0 {
            let min_visible_anchor_content_width = MAX_VISIBLE_ANCHOR_CONTENT_WIDTH
                .min(MIN_VISIBLE_ANCHOR_CONTENT_WIDTH.max(viewport_width / 3));
            if selected_row.anchor_col
                > viewport_width.saturating_sub(min_visible_anchor_content_width)
            {
                let anchor_context_width =
                    MAX_ANCHOR_CONTEXT_WIDTH.min(MIN_ANCHOR_CONTEXT_WIDTH.max(viewport_width / 4));
                horizontal_scroll = max_horizontal_scroll
                    .min(selected_row.anchor_col.saturating_sub(anchor_context_width));
            }
        }
    }

    // Clip only the body; the fixed-width gutter remains visible as
    // navigation context.
    rows.iter()
        .map(|row| {
            let line = if horizontal_scroll > 0 {
                format!(
                    "{}{}\x1b[0m",
                    row.gutter,
                    slice_by_column(&row.body, horizontal_scroll, viewport_width, true)
                )
            } else {
                format!("{}{}", row.gutter, row.body)
            };
            truncate_to_width(&line, width, "", false)
        })
        .collect()
}

/// Tool call info for lookup (tree-selector.ts:100-104).
struct ToolCallInfo {
    name: String,
    arguments: Map<String, Value>,
}

/// Tree list component with selection and ASCII-art visualization
/// (tree-selector.ts:106-1154).
pub(crate) struct TreeList {
    flat_nodes: Vec<FlatNode>,
    filtered_nodes: Vec<FlatNode>,
    selected_index: usize,
    current_leaf_id: Option<String>,
    max_visible_lines: usize,
    filter_mode: TreeFilterMode,
    search_query: String,
    tool_call_map: HashMap<String, ToolCallInfo>,
    multiple_roots: bool,
    show_label_timestamps: bool,
    active_path_ids: HashSet<String>,
    visible_parent_map: HashMap<String, Option<String>>,
    visible_children_map: HashMap<Option<String>, Vec<String>>,
    last_selected_id: Option<String>,
    folded_nodes: HashSet<String>,

    theme: Arc<Theme>,

    /// `onSelect` (tree-selector.ts:123).
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub on_select: Option<Box<dyn FnMut(&str) + Send>>,
    /// `onCancel` (tree-selector.ts:124).
    pub on_cancel: Option<Box<dyn FnMut() + Send>>,
    /// `onCopy` (tree-selector.ts:125).
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub on_copy: Option<Box<dyn FnMut(Option<&str>) + Send>>,
    /// `onLabelEdit` (tree-selector.ts:126).
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub on_label_edit: Option<Box<dyn FnMut(&str, Option<&str>) + Send>>,
}

/// Branch-walk direction for `findBranchSegmentStart`
/// (tree-selector.ts:1125).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
}

impl TreeList {
    /// `constructor` (tree-selector.ts:128-147).
    fn new(
        tree: Vec<SessionTreeNode>,
        current_leaf_id: Option<String>,
        max_visible_lines: usize,
        initial_selected_id: Option<String>,
        initial_filter_mode: TreeFilterMode,
        theme: Arc<Theme>,
    ) -> Self {
        let mut list = Self {
            flat_nodes: Vec::new(),
            filtered_nodes: Vec::new(),
            selected_index: 0,
            current_leaf_id,
            max_visible_lines,
            filter_mode: initial_filter_mode,
            search_query: String::new(),
            tool_call_map: HashMap::new(),
            multiple_roots: tree.len() > 1,
            show_label_timestamps: false,
            active_path_ids: HashSet::new(),
            visible_parent_map: HashMap::new(),
            visible_children_map: HashMap::new(),
            last_selected_id: None,
            folded_nodes: HashSet::new(),
            theme,
            on_select: None,
            on_cancel: None,
            on_copy: None,
            on_label_edit: None,
        };
        list.flat_nodes = list.flatten_tree(&tree);
        list.build_active_path();
        list.apply_filter();

        // Start with initialSelectedId if provided, otherwise current leaf.
        let target_id = initial_selected_id.or_else(|| list.current_leaf_id.clone());
        list.selected_index = list.find_nearest_visible_index(target_id.as_deref());
        list.last_selected_id = list
            .filtered_nodes
            .get(list.selected_index)
            .map(|node| node.node.entry.id().to_string());
        list
    }

    /// Find the index of the nearest visible entry, walking up the parent
    /// chain if needed. Returns the index in filtered_nodes, or the last
    /// index as fallback (tree-selector.ts:153-177).
    fn find_nearest_visible_index(&self, entry_id: Option<&str>) -> usize {
        if self.filtered_nodes.is_empty() {
            return 0;
        }

        // Build a map for parent lookup.
        let entry_map: HashMap<&str, &FlatNode> = self
            .flat_nodes
            .iter()
            .map(|f| (f.node.entry.id(), f))
            .collect();
        // Build a map of visible entry IDs to their indices in filteredNodes.
        let visible_id_to_index: HashMap<&str, usize> = self
            .filtered_nodes
            .iter()
            .enumerate()
            .map(|(i, node)| (node.node.entry.id(), i))
            .collect();

        // Walk from entryId up to root, looking for a visible entry.
        let mut current_id = entry_id;
        while let Some(id) = current_id {
            if let Some(&index) = visible_id_to_index.get(id) {
                return index;
            }
            let Some(node) = entry_map.get(id) else { break };
            current_id = node.node.entry.parent_id();
        }

        // Fallback: last visible entry.
        self.filtered_nodes.len() - 1
    }

    /// Build the set of entry IDs on the path from root to current leaf
    /// (tree-selector.ts:180-198).
    fn build_active_path(&mut self) {
        self.active_path_ids.clear();
        let Some(leaf_id) = self.current_leaf_id.clone() else {
            return;
        };
        let entry_map: HashMap<&str, &FlatNode> = self
            .flat_nodes
            .iter()
            .map(|f| (f.node.entry.id(), f))
            .collect();

        // Walk from leaf to root.
        let mut current_id: Option<String> = Some(leaf_id);
        while let Some(id) = current_id {
            self.active_path_ids.insert(id.clone());
            let Some(node) = entry_map.get(id.as_str()) else {
                break;
            };
            current_id = node.node.entry.parent_id().map(str::to_string);
        }
    }

    /// `flattenTree` (tree-selector.ts:200-328).
    ///
    /// Indentation rules:
    /// - At indent 0: stay at 0 unless parent has >1 children (then +1)
    /// - At indent 1: children always go to indent 2 (visual grouping of
    ///   subtree)
    /// - At indent 2+: stay flat for single-child chains, +1 only if parent
    ///   branches
    fn flatten_tree(&mut self, roots: &[SessionTreeNode]) -> Vec<FlatNode> {
        let mut result: Vec<FlatNode> = Vec::new();
        self.tool_call_map.clear();

        // Determine which subtrees contain the active leaf (to sort current
        // branch first). Iterative post-order traversal to avoid stack
        // overflow. Upstream keys by object identity; the port keys by entry
        // id (unique per tree).
        let mut contains_active: HashMap<String, bool> = HashMap::new();
        let leaf_id = self.current_leaf_id.as_deref();
        {
            // Build list in pre-order, then process in reverse for
            // post-order effect.
            let mut all_nodes: Vec<&SessionTreeNode> = Vec::new();
            let mut pre_order_stack: Vec<&SessionTreeNode> = roots.iter().collect();
            while let Some(node) = pre_order_stack.pop() {
                all_nodes.push(node);
                // Push children in reverse so they're processed left-to-right.
                for child in node.children.iter().rev() {
                    pre_order_stack.push(child);
                }
            }
            // Process in reverse (post-order): children before parents.
            for node in all_nodes.iter().rev() {
                let mut has = leaf_id.is_some_and(|lid| node.entry.id() == lid);
                for child in &node.children {
                    if contains_active
                        .get(child.entry.id())
                        .copied()
                        .unwrap_or(false)
                    {
                        has = true;
                    }
                }
                contains_active.insert(node.entry.id().to_string(), has);
            }
        }

        // Add roots in reverse order, prioritizing the one containing the
        // active leaf. If multiple roots, treat them as children of a
        // virtual root that branches.
        let multiple_roots = roots.len() > 1;
        let mut ordered_roots: Vec<&SessionTreeNode> = roots.iter().collect();
        ordered_roots.sort_by_key(|node| {
            !contains_active
                .get(node.entry.id())
                .copied()
                .unwrap_or(false)
        });

        // Stack items: [node, indent, justBranched, showConnector, isLast,
        // gutters, isVirtualRootChild].
        type StackItem = (
            SessionTreeNode,
            usize,
            bool,
            bool,
            bool,
            Vec<GutterInfo>,
            bool,
        );
        let mut stack: Vec<StackItem> = Vec::new();
        for i in (0..ordered_roots.len()).rev() {
            let is_last = i == ordered_roots.len() - 1;
            stack.push((
                ordered_roots[i].clone(),
                if multiple_roots { 1 } else { 0 },
                multiple_roots,
                multiple_roots,
                is_last,
                Vec::new(),
                multiple_roots,
            ));
        }

        while let Some((
            node,
            indent,
            just_branched,
            show_connector,
            is_last,
            gutters,
            is_virtual_root_child,
        )) = stack.pop()
        {
            // Extract tool calls from assistant messages for later lookup.
            if let Some(SessionEntry::Message(message_entry)) = node.entry.known() {
                if let AgentMessage::Assistant(assistant) = &message_entry.message {
                    for block in &assistant.content {
                        if let AssistantContent::ToolCall(tool_call) = block {
                            self.tool_call_map.insert(
                                tool_call.id.clone(),
                                ToolCallInfo {
                                    name: tool_call.name.clone(),
                                    arguments: tool_call.arguments.clone(),
                                },
                            );
                        }
                    }
                }
            }

            result.push(FlatNode {
                node: node.clone(),
                indent,
                show_connector,
                is_last,
                gutters: gutters.clone(),
                is_virtual_root_child,
            });

            let children = &node.children;
            let multiple_children = children.len() > 1;

            // Order children so the branch containing the active leaf comes
            // first.
            let mut prioritized: Vec<&SessionTreeNode> = Vec::new();
            let mut rest: Vec<&SessionTreeNode> = Vec::new();
            for child in children {
                if contains_active
                    .get(child.entry.id())
                    .copied()
                    .unwrap_or(false)
                {
                    prioritized.push(child);
                } else {
                    rest.push(child);
                }
            }
            let ordered_children: Vec<&SessionTreeNode> =
                prioritized.into_iter().chain(rest).collect();

            // Calculate child indent.
            let child_indent = if multiple_children {
                // Parent branches: children get +1.
                indent + 1
            } else if just_branched && indent > 0 {
                // First generation after a branch: +1 for visual grouping.
                indent + 1
            } else {
                // Single-child chain: stay flat.
                indent
            };

            // Build gutters for children. If this node showed a connector,
            // add a gutter entry for descendants. Only add the gutter if the
            // connector is actually displayed (not suppressed for virtual
            // root children). Connector is at position
            // (displayIndent - 1), so the gutter should be there too.
            let connector_displayed = show_connector && !is_virtual_root_child;
            let current_display_indent = if self.multiple_roots {
                indent.saturating_sub(1)
            } else {
                indent
            };
            let connector_position = current_display_indent.saturating_sub(1);
            let child_gutters = if connector_displayed {
                let mut gutters = gutters.clone();
                gutters.push(GutterInfo {
                    position: connector_position,
                    show: !is_last,
                });
                gutters
            } else {
                gutters.clone()
            };

            // Add children in reverse order.
            for i in (0..ordered_children.len()).rev() {
                let child_is_last = i == ordered_children.len() - 1;
                stack.push((
                    ordered_children[i].clone(),
                    child_indent,
                    multiple_children,
                    multiple_children,
                    child_is_last,
                    child_gutters.clone(),
                    false,
                ));
            }
        }

        result
    }

    /// `applyFilter` (tree-selector.ts:330-426).
    fn apply_filter(&mut self) {
        // Update lastSelectedId only when we have a valid selection
        // (non-empty list). This preserves the selection when switching
        // through empty filter results.
        if !self.filtered_nodes.is_empty() {
            if let Some(id) = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|node| node.node.entry.id().to_string())
            {
                self.last_selected_id = Some(id);
            }
        }

        let search_tokens: Vec<String> = self
            .search_query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let current_leaf_id = self.current_leaf_id.clone();

        let filtered: Vec<FlatNode> = self
            .flat_nodes
            .iter()
            .filter(|flat_node| {
                let entry = &flat_node.node.entry;
                let is_current_leaf = current_leaf_id
                    .as_deref()
                    .is_some_and(|lid| entry.id() == lid);

                // Skip assistant messages with only tool calls (no text)
                // unless error/aborted. Always show current leaf so the
                // active position is visible.
                if let Some(SessionEntry::Message(message_entry)) = entry.known() {
                    if let AgentMessage::Assistant(assistant) = &message_entry.message {
                        if !is_current_leaf {
                            let has_text = has_text_content(&assistant.content);
                            let is_error_or_aborted = assistant.stop_reason != StopReason::Stop
                                && assistant.stop_reason != StopReason::ToolUse;
                            // Only hide if no text AND not an error/aborted
                            // message.
                            if !has_text && !is_error_or_aborted {
                                return false;
                            }
                        }
                    }
                }

                // Apply filter mode. Entry types hidden in the default view
                // (settings/bookkeeping).
                let is_settings_entry = matches!(
                    entry.known(),
                    Some(
                        SessionEntry::Label(_)
                            | SessionEntry::Custom(_)
                            | SessionEntry::ModelChange(_)
                            | SessionEntry::ThinkingLevelChange(_)
                            | SessionEntry::SessionInfo(_)
                    )
                );
                let passes_filter = match self.filter_mode {
                    // Just user messages.
                    TreeFilterMode::UserOnly => matches!(
                        entry.known(),
                        Some(SessionEntry::Message(message_entry))
                            if matches!(message_entry.message, AgentMessage::User(_))
                    ),
                    // Default minus tool results.
                    TreeFilterMode::NoTools => {
                        !is_settings_entry
                            && !matches!(
                                entry.known(),
                                Some(SessionEntry::Message(message_entry))
                                    if matches!(message_entry.message, AgentMessage::ToolResult(_))
                            )
                    }
                    // Just labeled entries.
                    TreeFilterMode::LabeledOnly => flat_node.node.label.is_some(),
                    // Show everything.
                    TreeFilterMode::All => true,
                    // Default mode: hide settings/bookkeeping entries.
                    TreeFilterMode::Default => !is_settings_entry,
                };
                if !passes_filter {
                    return false;
                }

                // Apply search filter.
                if !search_tokens.is_empty() {
                    let node_text = self.get_searchable_text(&flat_node.node).to_lowercase();
                    return search_tokens.iter().all(|token| node_text.contains(token));
                }

                true
            })
            .cloned()
            .collect();
        self.filtered_nodes = filtered;

        // Filter out descendants of folded nodes.
        if !self.folded_nodes.is_empty() {
            let mut skip_set: HashSet<String> = HashSet::new();
            for flat_node in &self.flat_nodes {
                let id = flat_node.node.entry.id().to_string();
                if let Some(parent_id) = flat_node.node.entry.parent_id() {
                    if self.folded_nodes.contains(parent_id) || skip_set.contains(parent_id) {
                        skip_set.insert(id);
                    }
                }
            }
            self.filtered_nodes
                .retain(|flat_node| !skip_set.contains(flat_node.node.entry.id()));
        }

        // Recalculate visual structure (indent, connectors, gutters) based
        // on the visible tree.
        self.recalculate_visual_structure();

        // Try to preserve cursor on the same node, or find nearest visible
        // ancestor.
        if let Some(last_selected_id) = self.last_selected_id.clone() {
            self.selected_index = self.find_nearest_visible_index(Some(&last_selected_id));
        } else if self.selected_index >= self.filtered_nodes.len() {
            // Clamp index if out of bounds.
            self.selected_index = self.filtered_nodes.len().saturating_sub(1);
        }

        // Update lastSelectedId to the actual selection (may have changed
        // due to parent walk).
        if !self.filtered_nodes.is_empty() {
            if let Some(id) = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|node| node.node.entry.id().to_string())
            {
                self.last_selected_id = Some(id);
            }
        }
    }

    /// Recompute indentation/connectors for the filtered view
    /// (tree-selector.ts:434-557).
    ///
    /// Filtering can hide intermediate entries; descendants attach to the
    /// nearest visible ancestor. Keep indentation semantics aligned with
    /// `flattenTree` so single-child chains don't drift right.
    fn recalculate_visual_structure(&mut self) {
        if self.filtered_nodes.is_empty() {
            return;
        }

        let visible_ids: HashSet<String> = self
            .filtered_nodes
            .iter()
            .map(|node| node.node.entry.id().to_string())
            .collect();

        // Build entry map for efficient parent lookup (using the full tree).
        let entry_map: HashMap<String, usize> = self
            .flat_nodes
            .iter()
            .enumerate()
            .map(|(i, flat_node)| (flat_node.node.entry.id().to_string(), i))
            .collect();

        // Find nearest visible ancestor for a node.
        let find_visible_ancestor = |node_id: &str| -> Option<String> {
            let &index = entry_map.get(node_id)?;
            let mut current_id = self.flat_nodes[index]
                .node
                .entry
                .parent_id()
                .map(str::to_string);
            while let Some(id) = current_id {
                if visible_ids.contains(&id) {
                    return Some(id);
                }
                let Some(&index) = entry_map.get(&id) else {
                    break;
                };
                current_id = self.flat_nodes[index]
                    .node
                    .entry
                    .parent_id()
                    .map(str::to_string);
            }
            None
        };

        // Build visible tree structure:
        // - visibleParent: nodeId → nearest visible ancestor (or None for
        //   roots)
        // - visibleChildren: parentId → list of visible children (in
        //   filteredNodes order)
        let mut visible_parent: HashMap<String, Option<String>> = HashMap::new();
        let mut visible_children: HashMap<Option<String>, Vec<String>> = HashMap::new();
        visible_children.insert(None, Vec::new()); // root-level nodes

        for flat_node in &self.filtered_nodes {
            let node_id = flat_node.node.entry.id().to_string();
            let ancestor_id = find_visible_ancestor(&node_id);
            visible_parent.insert(node_id.clone(), ancestor_id.clone());
            visible_children
                .entry(ancestor_id)
                .or_default()
                .push(node_id);
        }

        // Update multipleRoots based on visible roots.
        let visible_root_ids = visible_children.get(&None).cloned().unwrap_or_default();
        self.multiple_roots = visible_root_ids.len() > 1;

        // Build a map for quick lookup: nodeId → index in filteredNodes.
        let filtered_node_map: HashMap<String, usize> = self
            .filtered_nodes
            .iter()
            .enumerate()
            .map(|(i, flat_node)| (flat_node.node.entry.id().to_string(), i))
            .collect();

        // DFS over the visible tree using flattenTree indentation semantics.
        // Stack items: [nodeId, indent, justBranched, showConnector, isLast,
        // gutters, isVirtualRootChild].
        type StackItem = (String, usize, bool, bool, bool, Vec<GutterInfo>, bool);
        let mut stack: Vec<StackItem> = Vec::new();

        // Add visible roots in reverse order (to process in forward order
        // via the stack).
        for i in (0..visible_root_ids.len()).rev() {
            let is_last = i == visible_root_ids.len() - 1;
            stack.push((
                visible_root_ids[i].clone(),
                if self.multiple_roots { 1 } else { 0 },
                self.multiple_roots,
                self.multiple_roots,
                is_last,
                Vec::new(),
                self.multiple_roots,
            ));
        }

        while let Some((
            node_id,
            indent,
            just_branched,
            show_connector,
            is_last,
            gutters,
            is_virtual_root_child,
        )) = stack.pop()
        {
            let Some(&index) = filtered_node_map.get(&node_id) else {
                continue;
            };

            // Update this node's visual properties.
            let flat_node = &mut self.filtered_nodes[index];
            flat_node.indent = indent;
            flat_node.show_connector = show_connector;
            flat_node.is_last = is_last;
            flat_node.gutters = gutters.clone();
            flat_node.is_virtual_root_child = is_virtual_root_child;

            // Get visible children of this node.
            let children: Vec<String> = visible_children
                .get(&Some(node_id.clone()))
                .cloned()
                .unwrap_or_default();
            let multiple_children = children.len() > 1;

            // Child indent follows flattenTree: branch points (and first
            // generation after a branch) shift +1.
            let child_indent = if multiple_children || (just_branched && indent > 0) {
                indent + 1
            } else {
                indent
            };

            // Child gutters follow flattenTree connector/gutter rules.
            let connector_displayed = show_connector && !is_virtual_root_child;
            let current_display_indent = if self.multiple_roots {
                indent.saturating_sub(1)
            } else {
                indent
            };
            let connector_position = current_display_indent.saturating_sub(1);
            let child_gutters = if connector_displayed {
                let mut gutters = gutters.clone();
                gutters.push(GutterInfo {
                    position: connector_position,
                    show: !is_last,
                });
                gutters
            } else {
                gutters.clone()
            };

            // Add children in reverse order (to process in forward order
            // via the stack).
            for i in (0..children.len()).rev() {
                let child_is_last = i == children.len() - 1;
                stack.push((
                    children[i].clone(),
                    child_indent,
                    multiple_children,
                    multiple_children,
                    child_is_last,
                    child_gutters.clone(),
                    false,
                ));
            }
        }

        // Store visible tree maps for ancestor/descendant lookups in
        // navigation.
        self.visible_parent_map = visible_parent;
        self.visible_children_map = visible_children;
    }

    /// Get searchable text content from a node (tree-selector.ts:560-615).
    fn get_searchable_text(&self, node: &SessionTreeNode) -> String {
        let entry = &node.entry;
        let mut parts: Vec<String> = Vec::new();

        if let Some(label) = &node.label {
            parts.push(label.clone());
        }

        match entry.known() {
            Some(SessionEntry::Message(message_entry)) => match &message_entry.message {
                AgentMessage::User(user_message) => {
                    parts.push("user".to_string());
                    parts.push(extract_content_user(&user_message.content));
                }
                AgentMessage::Assistant(assistant) => {
                    parts.push("assistant".to_string());
                    parts.push(extract_content_assistant(&assistant.content));
                }
                AgentMessage::ToolResult(tool_result) => {
                    parts.push("toolResult".to_string());
                    parts.push(extract_content_tool_result(&tool_result.content));
                }
                AgentMessage::BashExecution(bash) => {
                    parts.push("bashExecution".to_string());
                    if !bash.command.is_empty() {
                        parts.push(bash.command.clone());
                    }
                }
                AgentMessage::Custom(custom) => {
                    parts.push("custom".to_string());
                    parts.push(extract_content_user(&custom.content));
                }
                AgentMessage::BranchSummary(_) => parts.push("branchSummary".to_string()),
                AgentMessage::CompactionSummary(_) => {
                    parts.push("compactionSummary".to_string());
                }
            },
            Some(SessionEntry::CustomMessage(custom_message)) => {
                parts.push(custom_message.custom_type.clone());
                match &custom_message.content {
                    UserContent::Text(text) => parts.push(text.clone()),
                    UserContent::Blocks(_) => {
                        parts.push(extract_content_user(&custom_message.content));
                    }
                }
            }
            Some(SessionEntry::Compaction(_)) => parts.push("compaction".to_string()),
            Some(SessionEntry::BranchSummary(branch_summary)) => {
                parts.push("branch summary".to_string());
                parts.push(branch_summary.summary.clone());
            }
            Some(SessionEntry::SessionInfo(session_info)) => {
                parts.push("title".to_string());
                if let Some(name) = &session_info.name {
                    parts.push(name.clone());
                }
            }
            Some(SessionEntry::ModelChange(model_change)) => {
                parts.push("model".to_string());
                parts.push(model_change.model_id.clone());
            }
            Some(SessionEntry::ThinkingLevelChange(thinking_level_change)) => {
                parts.push("thinking".to_string());
                parts.push(thinking_level_change.thinking_level.clone());
            }
            Some(SessionEntry::Custom(custom)) => {
                parts.push("custom".to_string());
                parts.push(custom.custom_type.clone());
            }
            Some(SessionEntry::Label(label)) => {
                parts.push("label".to_string());
                parts.push(label.label.clone().unwrap_or_default());
            }
            Some(_) | None => {}
        }

        parts.join(" ")
    }

    /// `getSearchQuery` (tree-selector.ts:619-621).
    fn get_search_query(&self) -> &str {
        &self.search_query
    }

    /// `copySelected` (tree-selector.ts:627-630).
    fn copy_selected(&mut self) {
        let text: Option<String> = self
            .filtered_nodes
            .get(self.selected_index)
            .and_then(|flat_node| get_entry_copy_text(&flat_node.node));
        if let Some(callback) = self.on_copy.as_mut() {
            callback(text.as_deref());
        }
    }

    /// `updateNodeLabel` (tree-selector.ts:632-640).
    fn update_node_label(
        &mut self,
        entry_id: &str,
        label: Option<&str>,
        label_timestamp: Option<&str>,
    ) {
        for flat_node in &mut self.flat_nodes {
            if flat_node.node.entry.id() == entry_id {
                flat_node.node.label = label.map(str::to_string);
                flat_node.node.label_timestamp = if label.is_some() {
                    Some(
                        label_timestamp
                            .map(str::to_string)
                            .unwrap_or_else(now_iso8601),
                    )
                } else {
                    None
                };
                break;
            }
        }
    }

    /// `getStatusLabels` (tree-selector.ts:642-662).
    fn get_status_labels(&self) -> String {
        let mut labels = String::new();
        match self.filter_mode {
            TreeFilterMode::NoTools => labels += " [no-tools]",
            TreeFilterMode::UserOnly => labels += " [user]",
            TreeFilterMode::LabeledOnly => labels += " [labeled]",
            TreeFilterMode::All => labels += " [all]",
            TreeFilterMode::Default => {}
        }
        if self.show_label_timestamps {
            labels += " [+label time]";
        }
        labels
    }

    /// `getEntryDisplayText` (tree-selector.ts:768-852).
    fn get_entry_display_text(&self, node: &SessionTreeNode, is_selected: bool) -> String {
        let entry = &node.entry;
        let normalize = |s: &str| s.replace(['\n', '\t'], " ").trim().to_string();

        let result: String = match entry.known() {
            Some(SessionEntry::Message(message_entry)) => match &message_entry.message {
                AgentMessage::User(user_message) => format!(
                    "{}{}",
                    self.theme.fg("accent", "user: "),
                    normalize(&extract_content_user(&user_message.content))
                ),
                AgentMessage::Assistant(assistant) => {
                    let text_content = normalize(&extract_content_assistant(&assistant.content));
                    if !text_content.is_empty() {
                        format!(
                            "{}{}",
                            self.theme.fg("success", "assistant: "),
                            text_content
                        )
                    } else if assistant.stop_reason == StopReason::Aborted {
                        format!(
                            "{}{}",
                            self.theme.fg("success", "assistant: "),
                            self.theme.fg("muted", "(aborted)")
                        )
                    } else if assistant
                        .error_message
                        .as_deref()
                        .is_some_and(|message| !message.is_empty())
                    {
                        let error: String =
                            normalize(assistant.error_message.as_deref().unwrap_or(""))
                                .chars()
                                .take(80)
                                .collect();
                        format!(
                            "{}{}",
                            self.theme.fg("success", "assistant: "),
                            self.theme.fg("error", &error)
                        )
                    } else {
                        format!(
                            "{}{}",
                            self.theme.fg("success", "assistant: "),
                            self.theme.fg("muted", "(no content)")
                        )
                    }
                }
                AgentMessage::ToolResult(tool_result) => {
                    if let Some(tool_call) = self.tool_call_map.get(&tool_result.tool_call_id) {
                        self.theme.fg(
                            "muted",
                            &format_tool_call(&tool_call.name, &tool_call.arguments),
                        )
                    } else {
                        self.theme
                            .fg("muted", &format!("[{}]", tool_result.tool_name))
                    }
                }
                AgentMessage::BashExecution(bash) => self
                    .theme
                    .fg("dim", &format!("[bash]: {}", normalize(&bash.command))),
                AgentMessage::Custom(_) => self.theme.fg("dim", "[custom]"),
                AgentMessage::BranchSummary(_) => self.theme.fg("dim", "[branchSummary]"),
                AgentMessage::CompactionSummary(_) => self.theme.fg("dim", "[compactionSummary]"),
            },
            Some(SessionEntry::CustomMessage(custom_message)) => {
                let content = match &custom_message.content {
                    UserContent::Text(text) => text.clone(),
                    UserContent::Blocks(_) => extract_full_content_user(&custom_message.content),
                };
                format!(
                    "{}{}",
                    self.theme.fg(
                        "customMessageLabel",
                        &format!("[{}]: ", custom_message.custom_type)
                    ),
                    normalize(&content)
                )
            }
            Some(SessionEntry::Compaction(compaction)) => {
                // Math.round(tokensBefore / 1000) — exact for u64.
                let tokens = (compaction.tokens_before + 500) / 1000;
                self.theme
                    .fg("borderAccent", &format!("[compaction: {tokens}k tokens]"))
            }
            Some(SessionEntry::BranchSummary(branch_summary)) => format!(
                "{}{}",
                self.theme.fg("warning", "[branch summary]: "),
                normalize(&branch_summary.summary)
            ),
            Some(SessionEntry::ModelChange(model_change)) => self
                .theme
                .fg("dim", &format!("[model: {}]", model_change.model_id)),
            Some(SessionEntry::ThinkingLevelChange(thinking_level_change)) => self.theme.fg(
                "dim",
                &format!("[thinking: {}]", thinking_level_change.thinking_level),
            ),
            Some(SessionEntry::Custom(custom)) => self
                .theme
                .fg("dim", &format!("[custom: {}]", custom.custom_type)),
            Some(SessionEntry::Label(label)) => self.theme.fg(
                "dim",
                &format!("[label: {}]", label.label.as_deref().unwrap_or("(cleared)")),
            ),
            Some(SessionEntry::SessionInfo(session_info)) => {
                if let Some(name) = &session_info.name {
                    format!(
                        "{}{}{}",
                        self.theme.fg("dim", "[title: "),
                        self.theme.fg("dim", name),
                        self.theme.fg("dim", "]")
                    )
                } else {
                    format!(
                        "{}{}{}",
                        self.theme.fg("dim", "[title: "),
                        Theme::italic(&self.theme.fg("dim", "empty")),
                        self.theme.fg("dim", "]")
                    )
                }
            }
            Some(_) | None => String::new(),
        };

        if is_selected {
            Theme::bold(&result)
        } else {
            result
        }
    }

    /// Whether a node can be folded: it has visible children and is either
    /// a root (no visible parent) or a segment start (visible parent has
    /// multiple visible children) (tree-selector.ts:1109-1116).
    fn is_foldable(&self, entry_id: &str) -> bool {
        let Some(children) = self.visible_children_map.get(&Some(entry_id.to_string())) else {
            return false;
        };
        if children.is_empty() {
            return false;
        }
        let parent_id = self.visible_parent_map.get(entry_id).cloned().flatten();
        let Some(parent_id) = parent_id else {
            return true;
        };
        let siblings = self.visible_children_map.get(&Some(parent_id));
        siblings.is_some_and(|siblings| siblings.len() > 1)
    }

    /// Find the index of the next branch segment start in the given
    /// direction. A segment start is the first child of a branch point.
    ///
    /// "up" walks the visible parent chain; "down" walks visible children
    /// (always following the first child) (tree-selector.ts:1125-1153).
    fn find_branch_segment_start(&self, direction: Direction) -> usize {
        let Some(selected_id) = self
            .filtered_nodes
            .get(self.selected_index)
            .map(|node| node.node.entry.id().to_string())
        else {
            return self.selected_index;
        };

        let index_by_entry_id: HashMap<&str, usize> = self
            .filtered_nodes
            .iter()
            .enumerate()
            .map(|(i, node)| (node.node.entry.id(), i))
            .collect();

        let mut current_id: String = selected_id;
        if direction == Direction::Down {
            loop {
                let children: Vec<String> = self
                    .visible_children_map
                    .get(&Some(current_id.clone()))
                    .cloned()
                    .unwrap_or_default();
                if children.is_empty() {
                    return index_by_entry_id
                        .get(current_id.as_str())
                        .copied()
                        .unwrap_or(self.selected_index);
                }
                if children.len() > 1 {
                    return index_by_entry_id
                        .get(children[0].as_str())
                        .copied()
                        .unwrap_or(self.selected_index);
                }
                current_id = children[0].clone();
            }
        }

        // direction === "up".
        loop {
            let parent_id = self.visible_parent_map.get(&current_id).cloned().flatten();
            let Some(parent_id) = parent_id else {
                return index_by_entry_id
                    .get(current_id.as_str())
                    .copied()
                    .unwrap_or(self.selected_index);
            };
            let children: Vec<String> = self
                .visible_children_map
                .get(&Some(parent_id.clone()))
                .cloned()
                .unwrap_or_default();
            if children.len() > 1 {
                let segment_start = index_by_entry_id.get(current_id.as_str()).copied();
                if segment_start.is_some_and(|start| start < self.selected_index) {
                    return segment_start.unwrap();
                }
            }
            current_id = parent_id;
        }
    }
}

impl Component for TreeList {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        if self.filtered_nodes.is_empty() {
            lines.push(truncate_to_width(
                &self.theme.fg("muted", "  No entries found"),
                width,
                "",
                false,
            ));
            lines.push(truncate_to_width(
                &self
                    .theme
                    .fg("muted", &format!("  (0/0){}", self.get_status_labels())),
                width,
                "",
                false,
            ));
            return lines;
        }

        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible_lines / 2)
            .min(
                self.filtered_nodes
                    .len()
                    .saturating_sub(self.max_visible_lines),
            );
        let end_index = (start_index + self.max_visible_lines).min(self.filtered_nodes.len());

        let mut rendered_rows: Vec<HorizontalViewportRow> = Vec::new();
        for i in start_index..end_index {
            let flat_node = &self.filtered_nodes[i];
            let entry = &flat_node.node.entry;
            let is_selected = i == self.selected_index;

            // Build line: cursor + prefix + path marker + label + content.
            let cursor = if is_selected {
                self.theme.fg("accent", "› ")
            } else {
                "  ".to_string()
            };

            // If multiple roots, shift display (roots at 0, not 1).
            let display_indent = if self.multiple_roots {
                flat_node.indent.saturating_sub(1)
            } else {
                flat_node.indent
            };

            // Build prefix with gutters at their correct positions. Each
            // gutter has a position (displayIndent where its connector was
            // shown).
            let connector = if flat_node.show_connector && !flat_node.is_virtual_root_child {
                if flat_node.is_last {
                    "└─ "
                } else {
                    "├─ "
                }
            } else {
                ""
            };
            let connector_position = if connector.is_empty() {
                None
            } else {
                display_indent.checked_sub(1)
            };

            // Build prefix char by char, placing gutters and connector at
            // their positions.
            let total_chars = display_indent * 3;
            let mut prefix_chars: Vec<char> = Vec::new();
            let is_folded = self.folded_nodes.contains(entry.id());
            for i in 0..total_chars {
                let level = i / 3;
                let pos_in_level = i % 3;

                // Check if there's a gutter at this level.
                let gutter = flat_node.gutters.iter().find(|g| g.position == level);
                if let Some(gutter) = gutter {
                    if pos_in_level == 0 {
                        prefix_chars.push(if gutter.show { '│' } else { ' ' });
                    } else {
                        prefix_chars.push(' ');
                    }
                } else if let Some(connector_position) = connector_position {
                    if level == connector_position {
                        // Connector at this level, with fold indicator.
                        if pos_in_level == 0 {
                            prefix_chars.push(if flat_node.is_last { '└' } else { '├' });
                        } else if pos_in_level == 1 {
                            let foldable = self.is_foldable(entry.id());
                            prefix_chars.push(if is_folded {
                                '⊞'
                            } else if foldable {
                                '⊟'
                            } else {
                                '─'
                            });
                        } else {
                            prefix_chars.push(' ');
                        }
                    } else {
                        prefix_chars.push(' ');
                    }
                } else {
                    prefix_chars.push(' ');
                }
            }
            let prefix: String = prefix_chars.into_iter().collect();

            // Fold marker for nodes without connectors (roots).
            let shows_fold_in_connector =
                flat_node.show_connector && !flat_node.is_virtual_root_child;
            let fold_marker = if is_folded && !shows_fold_in_connector {
                self.theme.fg("accent", "⊞ ")
            } else {
                String::new()
            };

            // Active path marker — shown right before the entry text.
            let is_on_active_path = self.active_path_ids.contains(entry.id());
            let path_marker = if is_on_active_path {
                self.theme.fg("accent", "• ")
            } else {
                String::new()
            };

            let label = if let Some(label) = &flat_node.node.label {
                self.theme.fg("warning", &format!("[{label}] "))
            } else {
                String::new()
            };
            let label_timestamp = if self.show_label_timestamps
                && flat_node.node.label.is_some()
                && flat_node.node.label_timestamp.is_some()
            {
                self.theme.fg(
                    "muted",
                    &format!(
                        "{} ",
                        format_label_timestamp(
                            flat_node.node.label_timestamp.as_deref().unwrap_or("")
                        )
                    ),
                )
            } else {
                String::new()
            };
            let content = self.get_entry_display_text(&flat_node.node, is_selected);
            let prefix_part = format!(
                "{}{}{}",
                self.theme.fg("dim", &prefix),
                fold_marker,
                path_marker
            );
            let anchor_col = visible_width(&prefix_part);
            let mut gutter = cursor;
            let mut body = format!("{prefix_part}{label}{label_timestamp}{content}");
            if is_selected {
                gutter = self.theme.bg("selectedBg", &gutter);
                body = self.theme.bg("selectedBg", &body);
            }
            let body_width = visible_width(&body);
            rendered_rows.push(HorizontalViewportRow {
                gutter,
                body,
                anchor_col,
                body_width,
                is_selected,
            });
        }

        lines.extend(render_horizontal_viewport(&rendered_rows, width));
        lines.push(truncate_to_width(
            &self.theme.fg(
                "muted",
                &format!(
                    "  ({}/{}){}",
                    self.selected_index + 1,
                    self.filtered_nodes.len(),
                    self.get_status_labels()
                ),
            ),
            width,
            "",
            false,
        ));

        lines
    }

    /// `handleInput` (tree-selector.ts:996-1102).
    fn handle_input(&mut self, data: &str) {
        let keybindings = get_keybindings();
        let read = keybindings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if read.matches_id(data, "tui.select.up") {
            self.selected_index = if self.selected_index == 0 {
                self.filtered_nodes.len().saturating_sub(1)
            } else {
                self.selected_index - 1
            };
        } else if read.matches_id(data, "tui.select.down") {
            let len = self.filtered_nodes.len();
            self.selected_index = if self.selected_index + 1 >= len {
                0
            } else {
                self.selected_index + 1
            };
        } else if read.matches_id(data, "app.tree.foldOrUp") {
            let current_id = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|node| node.node.entry.id().to_string());
            let should_fold = current_id
                .as_deref()
                .is_some_and(|id| self.is_foldable(id) && !self.folded_nodes.contains(id));
            if should_fold {
                let id = current_id.expect("checked above");
                self.folded_nodes.insert(id);
                self.apply_filter();
            } else {
                self.selected_index = self.find_branch_segment_start(Direction::Up);
            }
        } else if read.matches_id(data, "app.tree.unfoldOrDown") {
            let current_id = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|node| node.node.entry.id().to_string());
            let should_unfold = current_id
                .as_deref()
                .is_some_and(|id| self.folded_nodes.contains(id));
            if should_unfold {
                self.folded_nodes
                    .remove(current_id.as_deref().expect("checked above"));
                self.apply_filter();
            } else {
                self.selected_index = self.find_branch_segment_start(Direction::Down);
            }
        } else if read.matches_id(data, "tui.editor.cursorLeft")
            || read.matches_id(data, "tui.select.pageUp")
        {
            // Page up.
            self.selected_index = self.selected_index.saturating_sub(self.max_visible_lines);
        } else if read.matches_id(data, "tui.editor.cursorRight")
            || read.matches_id(data, "tui.select.pageDown")
        {
            // Page down.
            self.selected_index = (self.selected_index + self.max_visible_lines)
                .min(self.filtered_nodes.len().saturating_sub(1));
        } else if read.matches_id(data, "tui.select.confirm") {
            let selected_id = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|node| node.node.entry.id().to_string());
            if let (Some(id), Some(callback)) = (selected_id, self.on_select.as_mut()) {
                callback(&id);
            }
        } else if read.matches_id(data, "app.message.copy") {
            self.copy_selected();
        } else if read.matches_id(data, "tui.select.cancel") {
            if !self.search_query.is_empty() {
                self.search_query.clear();
                self.folded_nodes.clear();
                self.apply_filter();
            } else if let Some(callback) = self.on_cancel.as_mut() {
                callback();
            }
        } else if read.matches_id(data, "app.tree.filter.default") {
            // Direct filter: default.
            self.filter_mode = TreeFilterMode::Default;
            self.folded_nodes.clear();
            self.apply_filter();
        } else if read.matches_id(data, "app.tree.filter.noTools") {
            // Toggle filter: no-tools ↔ default.
            self.filter_mode = if self.filter_mode == TreeFilterMode::NoTools {
                TreeFilterMode::Default
            } else {
                TreeFilterMode::NoTools
            };
            self.folded_nodes.clear();
            self.apply_filter();
        } else if read.matches_id(data, "app.tree.filter.userOnly") {
            // Toggle filter: user-only ↔ default.
            self.filter_mode = if self.filter_mode == TreeFilterMode::UserOnly {
                TreeFilterMode::Default
            } else {
                TreeFilterMode::UserOnly
            };
            self.folded_nodes.clear();
            self.apply_filter();
        } else if read.matches_id(data, "app.tree.filter.labeledOnly") {
            // Toggle filter: labeled-only ↔ default.
            self.filter_mode = if self.filter_mode == TreeFilterMode::LabeledOnly {
                TreeFilterMode::Default
            } else {
                TreeFilterMode::LabeledOnly
            };
            self.folded_nodes.clear();
            self.apply_filter();
        } else if read.matches_id(data, "app.tree.filter.all") {
            // Toggle filter: all ↔ default.
            self.filter_mode = if self.filter_mode == TreeFilterMode::All {
                TreeFilterMode::Default
            } else {
                TreeFilterMode::All
            };
            self.folded_nodes.clear();
            self.apply_filter();
        } else if read.matches_id(data, "app.tree.filter.cycleBackward") {
            // Cycle filter backwards.
            self.filter_mode = cycle_filter_mode(self.filter_mode, false);
            self.folded_nodes.clear();
            self.apply_filter();
        } else if read.matches_id(data, "app.tree.filter.cycleForward") {
            // Cycle filter forwards: default → no-tools → user-only →
            // labeled-only → all → default.
            self.filter_mode = cycle_filter_mode(self.filter_mode, true);
            self.folded_nodes.clear();
            self.apply_filter();
        } else if read.matches_id(data, "tui.editor.deleteCharBackward") {
            if !self.search_query.is_empty() {
                self.search_query.pop();
                self.folded_nodes.clear();
                self.apply_filter();
            }
        } else if read.matches_id(data, "app.tree.editLabel") {
            let selected = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|node| (node.node.entry.id().to_string(), node.node.label.clone()));
            if let Some((id, label)) = selected {
                if let Some(callback) = self.on_label_edit.as_mut() {
                    callback(&id, label.as_deref());
                }
            }
        } else if read.matches_id(data, "app.tree.toggleLabelTimestamp") {
            self.show_label_timestamps = !self.show_label_timestamps;
        } else {
            let has_control_chars = data.chars().any(|ch| {
                let code = ch as u32;
                code < 32 || code == 0x7f || (0x80..=0x9f).contains(&code)
            });
            if !has_control_chars && !data.is_empty() {
                self.search_query.push_str(data);
                self.folded_nodes.clear();
                self.apply_filter();
            }
        }
    }

    fn invalidate(&mut self) {}
}

/// `formatToolCall` (tree-selector.ts:938-994): shortened `[name: ...]`
/// preview for tool results, with `$HOME` shortened to `~`.
fn format_tool_call(name: &str, arguments: &Map<String, Value>) -> String {
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok());
    let shorten_path = |path: String| -> String {
        if let Some(home) = &home {
            if !home.is_empty() && path.starts_with(home.as_str()) {
                let suffix: String = path.chars().skip(home.chars().count()).collect();
                return format!("~{suffix}");
            }
        }
        path
    };

    // `String(args.path || args.file_path || "")` — first non-empty arg.
    let path_arg = |args: &Map<String, Value>| -> String {
        let path = get_arg(args, "path");
        if path.is_empty() {
            get_arg(args, "file_path")
        } else {
            path
        }
    };

    match name {
        "read" => {
            let path = shorten_path(path_arg(arguments));
            let offset = arguments.get("offset").and_then(|value| value.as_i64());
            let limit = arguments.get("limit").and_then(|value| value.as_i64());
            let mut display = path;
            if offset.is_some() || limit.is_some() {
                let start = offset.unwrap_or(1);
                let end = limit.map(|limit| start + limit - 1);
                display = format!("{display}:{start}");
                if let Some(end) = end {
                    display = format!("{display}-{end}");
                }
            }
            format!("[read: {display}]")
        }
        "write" => {
            let path = shorten_path(path_arg(arguments));
            format!("[write: {path}]")
        }
        "edit" => {
            let path = shorten_path(path_arg(arguments));
            format!("[edit: {path}]")
        }
        "bash" => {
            let raw_cmd = get_arg(arguments, "command");
            let cmd: String = raw_cmd
                .replace(['\n', '\t'], " ")
                .trim()
                .chars()
                .take(50)
                .collect();
            format!(
                "[bash: {cmd}{}]",
                if raw_cmd.chars().count() > 50 {
                    "..."
                } else {
                    ""
                }
            )
        }
        "grep" => {
            let pattern = get_arg(arguments, "pattern");
            let path = get_arg(arguments, "path");
            let path = shorten_path(if path.is_empty() {
                ".".to_string()
            } else {
                path
            });
            format!("[grep: /{pattern}/ in {path}]")
        }
        "find" => {
            let pattern = get_arg(arguments, "pattern");
            let path = get_arg(arguments, "path");
            let path = shorten_path(if path.is_empty() {
                ".".to_string()
            } else {
                path
            });
            format!("[find: {pattern} in {path}]")
        }
        "ls" => {
            let path = get_arg(arguments, "path");
            let path = shorten_path(if path.is_empty() {
                ".".to_string()
            } else {
                path
            });
            format!("[ls: {path}]")
        }
        _ => {
            // Custom tool — show name and truncated JSON args.
            let args_str = serde_json::to_string(arguments).unwrap_or_default();
            let sliced: String = args_str.chars().take(40).collect();
            format!(
                "[{name}: {sliced}{}]",
                if args_str.chars().count() > 40 {
                    "..."
                } else {
                    ""
                }
            )
        }
    }
}

/// `String(args[key] ?? "")` — argument lookup with JS-string coercion for
/// non-string values.
fn get_arg(arguments: &Map<String, Value>, key: &str) -> String {
    match arguments.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(value)) => value.clone(),
        Some(other) => other.to_string(),
    }
}

/// Filter-mode cycle order (tree-selector.ts:1066-1068, 1073-1075).
const FILTER_CYCLE: [TreeFilterMode; 5] = [
    TreeFilterMode::Default,
    TreeFilterMode::NoTools,
    TreeFilterMode::UserOnly,
    TreeFilterMode::LabeledOnly,
    TreeFilterMode::All,
];

fn cycle_filter_mode(mode: TreeFilterMode, forward: bool) -> TreeFilterMode {
    let index = FILTER_CYCLE.iter().position(|m| *m == mode).unwrap_or(0);
    let step = if forward { 1 } else { FILTER_CYCLE.len() - 1 };
    FILTER_CYCLE[(index + step) % FILTER_CYCLE.len()]
}

// ---------------------------------------------------------------------------
// Content extraction helpers (tree-selector.ts:879-936)
// ---------------------------------------------------------------------------

/// `extractContent` for user/custom content — text blocks joined, capped at
/// 200 chars (tree-selector.ts:879-881).
fn extract_content_user(content: &UserContent) -> String {
    content_text_user(content, "").chars().take(200).collect()
}

/// `extractContent` for assistant content blocks.
fn extract_content_assistant(content: &[AssistantContent]) -> String {
    content_text_assistant(content, "")
        .chars()
        .take(200)
        .collect()
}

/// `extractContent` for tool result content blocks.
fn extract_content_tool_result(content: &[ToolResultContent]) -> String {
    content_text_tool_result(content, "")
        .chars()
        .take(200)
        .collect()
}

/// `extractFullContent` for user/custom content (tree-selector.ts:883-894).
fn extract_full_content_user(content: &UserContent) -> String {
    content_text_user(content, "")
}

/// `extractFullContent` for assistant content blocks.
fn extract_full_content_assistant(content: &[AssistantContent]) -> String {
    content_text_assistant(content, "")
}

/// `extractFullContent` for tool result content blocks.
fn extract_full_content_tool_result(content: &[ToolResultContent]) -> String {
    content_text_tool_result(content, "")
}

/// `hasTextContent` — whether any text block has non-whitespace content
/// (tree-selector.ts:925-936).
fn has_text_content(content: &[AssistantContent]) -> bool {
    content.iter().any(|block| match block {
        AssistantContent::Text(text) => !text.text.trim().is_empty(),
        AssistantContent::Thinking(_) | AssistantContent::ToolCall(_) => false,
    })
}

/// `getEntryCopyText` (tree-selector.ts:896-923): the copyable text of a
/// node, or `None` when empty after trimming (upstream `undefined`).
fn get_entry_copy_text(node: &SessionTreeNode) -> Option<String> {
    let entry = &node.entry;
    let text: Option<String> = match entry.known() {
        Some(SessionEntry::Message(message_entry)) => match &message_entry.message {
            AgentMessage::BashExecution(bash) => Some(bash.command.clone()),
            AgentMessage::User(user_message) => {
                Some(extract_full_content_user(&user_message.content))
            }
            AgentMessage::Assistant(assistant) => {
                let text = extract_full_content_assistant(&assistant.content);
                if text.is_empty() {
                    assistant.error_message.clone()
                } else {
                    Some(text)
                }
            }
            AgentMessage::ToolResult(tool_result) => {
                Some(extract_full_content_tool_result(&tool_result.content))
            }
            AgentMessage::Custom(custom) => Some(extract_full_content_user(&custom.content)),
            _ => None,
        },
        Some(SessionEntry::CustomMessage(custom_message)) => {
            Some(extract_full_content_user(&custom_message.content))
        }
        Some(SessionEntry::Compaction(compaction)) => Some(compaction.summary.clone()),
        Some(SessionEntry::BranchSummary(branch_summary)) => Some(branch_summary.summary.clone()),
        _ => None,
    };
    text.filter(|text| !text.trim().is_empty())
}

// ---------------------------------------------------------------------------
// Label timestamp formatting (tree-selector.ts:854-877)
// ---------------------------------------------------------------------------

/// `formatLabelTimestamp` — local-time `HH:MM`, `M/D HH:MM` (same year) or
/// `YY/M/D HH:MM` (tree-selector.ts:854-877).
fn format_label_timestamp(timestamp: &str) -> String {
    let Some(ms) = parse_iso8601_ms(timestamp) else {
        return String::new();
    };
    let (year, month, day, hour, minute) = local_time(ms);
    let (now_year, now_month, now_day, _, _) = local_time(current_ms());
    format_label_parts(
        (now_year, now_month, now_day),
        year,
        month,
        day,
        hour,
        minute,
    )
}

/// Pure formatting logic of `formatLabelTimestamp`, separated for tests.
fn format_label_parts(
    now: (i32, u32, u32),
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> String {
    let time = format!("{hour:02}:{minute:02}");
    if year == now.0 && month == now.1 && day == now.2 {
        return time;
    }
    if year == now.0 {
        return format!("{month}/{day} {time}");
    }
    format!("{:02}/{month}/{day} {time}", year.rem_euclid(100))
}

fn current_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Epoch ms → local wall-clock (year, month, day, hour, minute). Uses
/// `libc::localtime_r` on unix; falls back to UTC elsewhere or when the
/// conversion fails.
#[cfg(unix)]
fn local_time(ms: i64) -> (i32, u32, u32, u32, u32) {
    let secs = ms.div_euclid(1000);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs as libc::time_t;
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if ok {
        (
            tm.tm_year + 1900,
            (tm.tm_mon + 1) as u32,
            tm.tm_mday as u32,
            tm.tm_hour as u32,
            tm.tm_min as u32,
        )
    } else {
        epoch_ms_to_utc(ms)
    }
}

/// Non-unix fallback: UTC.
#[cfg(not(unix))]
fn local_time(ms: i64) -> (i32, u32, u32, u32, u32) {
    epoch_ms_to_utc(ms)
}

/// Civil date from days since epoch (Howard Hinnant's algorithm; same as
/// session_manager.rs:78-90).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

fn epoch_ms_to_utc(ms: i64) -> (i32, u32, u32, u32, u32) {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hour = (rem / 3_600_000) as u32;
    let minute = ((rem % 3_600_000) / 60_000) as u32;
    (year as i32, month, day, hour, minute)
}

// ---------------------------------------------------------------------------
// Search line (tree-selector.ts:1156-1175)
// ---------------------------------------------------------------------------

/// Component that displays the current search query.
struct SearchLine {
    /// Cached query from the [`TreeList`] (upstream reads it live — see the
    /// module header).
    query: String,
    theme: Arc<Theme>,
}

impl SearchLine {
    fn new(theme: Arc<Theme>) -> Self {
        Self {
            query: String::new(),
            theme,
        }
    }

    fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
    }
}

impl Component for SearchLine {
    fn render(&self, width: usize) -> Vec<String> {
        let line = if self.query.is_empty() {
            format!("  {}", self.theme.fg("muted", "Type to search:"))
        } else {
            format!(
                "  {} {}",
                self.theme.fg("muted", "Type to search:"),
                self.theme.fg("accent", &self.query)
            )
        };
        vec![truncate_to_width(&line, width, "", false)]
    }

    fn handle_input(&mut self, _data: &str) {}

    fn invalidate(&mut self) {}
}

// ---------------------------------------------------------------------------
// Tree help (tree-selector.ts:1177-1268)
// ---------------------------------------------------------------------------

/// Component that renders tree help as semantic rows with chunk-aware
/// wrapping.
struct TreeHelp {
    theme: Arc<Theme>,
}

impl TreeHelp {
    fn new(theme: Arc<Theme>) -> Self {
        Self { theme }
    }
}

impl Component for TreeHelp {
    fn render(&self, width: usize) -> Vec<String> {
        let items: Vec<String> = TREE_HELP_ITEMS
            .iter()
            .map(|(keys, label, label_first)| {
                let text = format_help_keys(keys);
                if text.is_empty() {
                    return (*label).to_string();
                }
                if *label_first {
                    format!("{label} {text}")
                } else {
                    format!("{text} {label}")
                }
            })
            .collect();

        let available_width = width.max(1);
        let indent = "  ";
        let separator = " · ";
        let mut lines: Vec<String> = Vec::new();
        let mut current_line = String::new();

        for item in items {
            let candidate = if current_line.is_empty() {
                if visible_width(&format!("{indent}{item}")) <= available_width {
                    format!("{indent}{item}")
                } else {
                    item.clone()
                }
            } else {
                format!("{current_line}{separator}{item}")
            };
            if current_line.is_empty() || visible_width(&candidate) <= available_width {
                current_line = candidate;
                continue;
            }

            lines.extend(wrap_text_with_ansi(
                current_line.trim_end(),
                available_width,
            ));
            current_line = if visible_width(&format!("{indent}{item}")) <= available_width {
                format!("{indent}{item}")
            } else {
                item
            };
        }

        if !current_line.is_empty() {
            lines.extend(wrap_text_with_ansi(
                current_line.trim_end(),
                available_width,
            ));
        }

        lines
            .into_iter()
            .map(|line| self.theme.fg("muted", &line))
            .collect()
    }

    fn handle_input(&mut self, _data: &str) {}

    fn invalidate(&mut self) {}
}

/// `TREE_HELP_ITEMS` (tree-selector.ts:1217-1236): (keybinding ids, label,
/// labelFirst).
const TREE_HELP_ITEMS: &[(&[&str], &str, bool)] = &[
    (&["tui.select.up", "tui.select.down"], "move", false),
    (
        &["tui.editor.cursorLeft", "tui.editor.cursorRight"],
        "page",
        false,
    ),
    (
        &["app.tree.foldOrUp", "app.tree.unfoldOrDown"],
        "branch",
        false,
    ),
    (&["app.message.copy"], "copy", false),
    (&["app.tree.editLabel"], "label", false),
    (&["app.tree.toggleLabelTimestamp"], "label time", false),
    (
        &[
            "app.tree.filter.default",
            "app.tree.filter.noTools",
            "app.tree.filter.userOnly",
            "app.tree.filter.labeledOnly",
            "app.tree.filter.all",
        ],
        "filters",
        true,
    ),
    (
        &[
            "app.tree.filter.cycleForward",
            "app.tree.filter.cycleBackward",
        ],
        "cycle",
        true,
    ),
];

/// `formatHelpKeys` (tree-selector.ts:1238-1253): the first resolved key of
/// each keybinding, compacted and with arrow-key replacements.
fn format_help_keys(keybindings: &[&str]) -> String {
    let read = get_keybindings()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let keys: Vec<String> = keybindings
        .iter()
        .filter_map(|id| read.get_keys_by_id(id).into_iter().next())
        .collect();
    if keys.is_empty() {
        return String::new();
    }

    replace_word(
        &replace_word(
            &replace_word(
                &replace_word(
                    &replace_word(
                        &replace_word(
                            &format_key_text(
                                &compact_raw_keys(&keys),
                                KeyTextFormatOptions::default(),
                            ),
                            "pageUp",
                            "pgup",
                        ),
                        "pageDown",
                        "pgdn",
                    ),
                    "up",
                    "↑",
                ),
                "down",
                "↓",
            ),
            "left",
            "←",
        ),
        "right",
        "→",
    )
}

/// `compactRawKeys` (tree-selector.ts:1255-1268): `ctrl+d/t/u/l/a` style
/// compaction when all keys share a prefix.
fn compact_raw_keys(keys: &[String]) -> String {
    if keys.len() == 1 {
        return keys[0].clone();
    }

    let parts: Vec<(String, String)> = keys
        .iter()
        .map(|key| match key.rfind('+') {
            Some(separator_index) => (
                key[..=separator_index].to_string(),
                key[separator_index + 1..].to_string(),
            ),
            None => (String::new(), key.clone()),
        })
        .collect();
    let prefix = &parts[0].0;
    if !prefix.is_empty() && parts.iter().all(|part| &part.0 == prefix) {
        format!(
            "{}{}",
            prefix,
            parts
                .iter()
                .map(|part| part.1.as_str())
                .collect::<Vec<_>>()
                .join("/")
        )
    } else {
        keys.join("/")
    }
}

/// JS `\bword\b` replacement (used for the arrow-key rewrites in
/// `formatHelpKeys`, tree-selector.ts:1247-1252).
fn replace_word(text: &str, word: &str, replacement: &str) -> String {
    let is_word_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(word) {
        let before = rest[..pos].chars().next_back();
        let after = rest[pos + word.len()..].chars().next();
        let at_boundary =
            before.is_none_or(|c| !is_word_char(c)) && after.is_none_or(|c| !is_word_char(c));
        out.push_str(&rest[..pos]);
        if at_boundary {
            out.push_str(replacement);
        } else {
            out.push_str(word);
        }
        rest = &rest[pos + word.len()..];
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Label input (tree-selector.ts:1270-1323)
// ---------------------------------------------------------------------------

/// Label input component shown when editing a label.
struct LabelInput {
    input: Input,
    entry_id: String,
    theme: Arc<Theme>,
    focused: bool,

    /// `onSubmit` (tree-selector.ts:1274).
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub on_submit: Option<Box<dyn FnMut(&str, Option<&str>) + Send>>,
    /// `onCancel` (tree-selector.ts:1275).
    pub on_cancel: Option<Box<dyn FnMut() + Send>>,
}

impl LabelInput {
    /// `constructor` (tree-selector.ts:1287-1293).
    fn new(entry_id: String, current_label: Option<&str>, theme: Arc<Theme>) -> Self {
        let mut input = Input::new();
        if let Some(label) = current_label {
            if !label.is_empty() {
                input.set_value(label);
            }
        }
        Self {
            input,
            entry_id,
            theme,
            focused: false,
            on_submit: None,
            on_cancel: None,
        }
    }
}

impl Component for LabelInput {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let indent = "  ";
        let available_width = width.saturating_sub(indent.chars().count());
        lines.push(truncate_to_width(
            &format!(
                "{indent}{}",
                self.theme.fg("muted", "Label (empty to remove):")
            ),
            width,
            "",
            false,
        ));
        for line in self.input.render(available_width) {
            lines.push(truncate_to_width(
                &format!("{indent}{line}"),
                width,
                "",
                false,
            ));
        }
        lines.push(truncate_to_width(
            &format!(
                "{indent}{}  {}",
                key_hint(&self.theme, "tui.select.confirm", "save"),
                key_hint(&self.theme, "tui.select.cancel", "cancel"),
            ),
            width,
            "",
            false,
        ));
        lines
    }

    /// `handleInput` (tree-selector.ts:1312-1322).
    fn handle_input(&mut self, data: &str) {
        let keybindings = get_keybindings();
        let read = keybindings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if read.matches_id(data, "tui.select.confirm") {
            let value = self.input.get_value().trim().to_string();
            let label = if value.is_empty() { None } else { Some(value) };
            if let Some(callback) = self.on_submit.as_mut() {
                callback(&self.entry_id, label.as_deref());
            }
        } else if read.matches_id(data, "tui.select.cancel") {
            if let Some(callback) = self.on_cancel.as_mut() {
                callback();
            }
        } else {
            drop(read);
            self.input.handle_input(data);
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

impl Focusable for LabelInput {
    fn focused(&self) -> bool {
        self.focused
    }

    /// Propagate to the inner input for IME cursor positioning
    /// (tree-selector.ts:1282-1285).
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }
}

// ---------------------------------------------------------------------------
// Tree selector component (tree-selector.ts:1325-1427)
// ---------------------------------------------------------------------------

/// Component that renders a session tree selector for navigation.
///
/// The entry-tree wrapping is handled by `interactive_mode.rs`; the
/// component itself only renders its own line stack (upstream
/// `Container` children, tree-selector.ts:1375-1385).
pub struct TreeSelectorComponent {
    tree_list: TreeList,
    label_input: Option<LabelInput>,
    theme: Arc<Theme>,
    /// Retained for the integration layer (render scheduling); upstream
    /// passes `this.ui` at the call site instead.
    #[allow(dead_code)]
    tui: TuiMainScreen,

    /// `onLabelChangeCallback` (tree-selector.ts:1333).
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    on_label_change: Option<Box<dyn FnMut(&str, Option<&str>) + Send>>,
    /// `onCopy` (tree-selector.ts:1334) — set by the mode layer.
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub on_copy: Option<Box<dyn FnMut(Option<&str>) + Send>>,

    // Shared slots forwarding the inner callbacks (see the module header).
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    label_edit_slot: Arc<Mutex<Option<(String, Option<String>)>>>,
    copy_slot: Arc<Mutex<Option<Option<String>>>>,
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    label_submit_slot: Arc<Mutex<Option<(String, Option<String>)>>>,
    label_cancel_slot: Arc<Mutex<bool>>,

    // Focusable implementation — propagate to labelInput when active for IME
    // cursor positioning (tree-selector.ts:1337-1347).
    focused: bool,

    // Static layout pieces (upstream Container children,
    // tree-selector.ts:1375-1385).
    border: DynamicBorder,
    title: Text,
    help: TreeHelp,
    search_line: SearchLine,
}

impl TreeSelectorComponent {
    /// `constructor` (tree-selector.ts:1349-1390) + explicit theme/tui
    /// injection (local convention).
    #[allow(clippy::too_many_arguments)] // mirrors the upstream constructor
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub fn new(
        tree: Vec<SessionTreeNode>,
        leaf_id: Option<String>,
        terminal_rows: u16,
        theme: Arc<Theme>,
        tui: TuiMainScreen,
        on_select: Box<dyn FnMut(&str) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
        on_label_change: Option<Box<dyn FnMut(&str, Option<&str>) + Send>>,
        initial_selected_id: Option<String>,
        initial_filter_mode: TreeFilterMode,
    ) -> Self {
        let max_visible_lines = (terminal_rows as usize / 2).max(5);

        let mut tree_list = TreeList::new(
            tree,
            leaf_id,
            max_visible_lines,
            initial_selected_id,
            initial_filter_mode,
            Arc::clone(&theme),
        );
        tree_list.on_select = Some(on_select);
        tree_list.on_cancel = Some(on_cancel);

        // `treeList.onLabelEdit = (entryId, currentLabel) =>
        // this.showLabelInput(...)` (tree-selector.ts:1368) — forwarded
        // through a slot drained after each dispatch.
        let label_edit_slot = Arc::new(Mutex::new(None::<(String, Option<String>)>));
        let slot = label_edit_slot.clone();
        tree_list.on_label_edit = Some(Box::new(move |entry_id, current_label| {
            *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some((entry_id.to_string(), current_label.map(str::to_string)));
        }));

        // `treeList.onCopy = (text) => this.onCopy?.(text)`
        // (tree-selector.ts:1367).
        let copy_slot = Arc::new(Mutex::new(None::<Option<String>>));
        let slot = copy_slot.clone();
        tree_list.on_copy = Some(Box::new(move |text| {
            *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(text.map(str::to_string));
        }));

        let border_color = {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str| theme.fg("border", text))
        };

        Self {
            tree_list,
            label_input: None,
            theme: Arc::clone(&theme),
            tui,
            on_label_change,
            on_copy: None,
            label_edit_slot,
            copy_slot,
            label_submit_slot: Arc::new(Mutex::new(None::<(String, Option<String>)>)),
            label_cancel_slot: Arc::new(Mutex::new(false)),
            focused: false,
            border: DynamicBorder::new(border_color),
            title: Text::new(Theme::bold("  Session Tree"), 1, 0, None),
            help: TreeHelp::new(Arc::clone(&theme)),
            search_line: SearchLine::new(theme),
        }
        // NOTE: upstream schedules `setTimeout(() => onCancel(), 100)` for
        // empty trees (tree-selector.ts:1387-1389); the port intentionally
        // does not fire it — the integration layer decides.
    }

    /// `showLabelInput` (tree-selector.ts:1392-1407).
    fn show_label_input(&mut self, entry_id: &str, current_label: Option<&str>) {
        let mut label_input =
            LabelInput::new(entry_id.to_string(), current_label, Arc::clone(&self.theme));

        // `labelInput.onSubmit = (id, label) => { updateNodeLabel;
        // onLabelChangeCallback?.; hideLabelInput }` (tree-selector.ts:1394-1398).
        let submit_slot = Arc::clone(&self.label_submit_slot);
        label_input.on_submit = Some(Box::new(move |entry_id, label| {
            *submit_slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some((entry_id.to_string(), label.map(str::to_string)));
        }));
        // `labelInput.onCancel = () => this.hideLabelInput()`
        // (tree-selector.ts:1399).
        let cancel_slot = Arc::clone(&self.label_cancel_slot);
        label_input.on_cancel = Some(Box::new(move || {
            *cancel_slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        }));

        // Propagate current focused state to the new labelInput
        // (tree-selector.ts:1402).
        label_input.set_focused(self.focused);

        self.label_input = Some(label_input);
    }

    /// `getTreeList` (tree-selector.ts:1424-1426).
    /// Reserved for the label-edit wiring (TODO(unassigned), see
    /// `interactive_mode.rs` `show_tree_selector`).
    #[allow(dead_code)]
    pub(crate) fn get_tree_list(&mut self) -> &mut TreeList {
        &mut self.tree_list
    }
}

impl Component for TreeSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        // Upstream Container children in order (tree-selector.ts:1375-1385).
        let mut lines: Vec<String> = Vec::new();
        lines.extend(Spacer::new(1).render(width));
        lines.extend(self.border.render(width));
        lines.extend(self.title.render(width));
        lines.extend(self.help.render(width));
        lines.extend(self.search_line.render(width));
        lines.extend(self.border.render(width));
        lines.push(String::new()); // Spacer(1).
        if let Some(label_input) = &self.label_input {
            lines.extend(label_input.render(width));
        } else {
            lines.extend(self.tree_list.render(width));
        }
        lines.push(String::new()); // Spacer(1).
        lines.extend(self.border.render(width));
        lines
    }

    /// `handleInput` (tree-selector.ts:1416-1422) + slot draining.
    fn handle_input(&mut self, data: &str) {
        if self.label_input.is_some() {
            if let Some(label_input) = self.label_input.as_mut() {
                label_input.handle_input(data);
            }
            // `labelInput.onSubmit` / `labelInput.onCancel` forwarding.
            if let Some((entry_id, label)) = self
                .label_submit_slot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
            {
                self.tree_list
                    .update_node_label(&entry_id, label.as_deref(), None);
                if let Some(callback) = self.on_label_change.as_mut() {
                    callback(&entry_id, label.as_deref());
                }
                self.label_input = None;
            } else if std::mem::take(
                &mut *self
                    .label_cancel_slot
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()),
            ) {
                self.label_input = None;
            }
        } else {
            self.tree_list.handle_input(data);
            self.search_line
                .set_query(self.tree_list.get_search_query());
            // `treeList.onLabelEdit` forwarding → showLabelInput. The slot
            // guard must drop before the &mut self call below.
            let label_edit_request = self
                .label_edit_slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some((entry_id, current_label)) = label_edit_request {
                self.show_label_input(&entry_id, current_label.as_deref());
            }
            // `treeList.onCopy` forwarding → this.onCopy.
            if let Some(text) = self
                .copy_slot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
            {
                if let Some(callback) = self.on_copy.as_mut() {
                    callback(text.as_deref());
                }
            }
        }
    }

    fn invalidate(&mut self) {
        self.tree_list.invalidate();
        if let Some(label_input) = self.label_input.as_mut() {
            label_input.invalidate();
        }
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for TreeSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    /// Propagate to labelInput when it's active (tree-selector.ts:1339-1346).
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if let Some(label_input) = self.label_input.as_mut() {
            label_input.set_focused(focused);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session_manager::StoredEntry;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::interactive_mode::install_global_keybindings;
    use rpi_agent::messages::{BashExecutionMessage, BashExecutionRole};
    use rpi_agent::session::{
        BranchSummaryEntry, CompactionEntry, CustomEntry, CustomMessageEntry, LabelEntry,
        MessageEntry, ModelChangeEntry, SessionInfoEntry, ThinkingLevelChangeEntry,
    };
    use rpi_ai::types::{
        AssistantContent as Ac, ImageContent, TextContent as Tc, ToolCall,
        ToolResultContent as Trc, UserContentBlock, UserRole,
    };

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    const TS: &str = "2025-01-01T00:00:00.000Z";

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
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

    fn known(entry: SessionEntry) -> StoredEntry {
        let raw = serde_json::to_value(&entry).expect("serialize SessionEntry");
        StoredEntry::Known {
            typed: Box::new(entry),
            raw,
        }
    }

    fn msg(id: &str, parent_id: Option<&str>, message: AgentMessage) -> StoredEntry {
        known(SessionEntry::Message(MessageEntry {
            id: id.to_owned(),
            parent_id: parent_id.map(str::to_owned),
            timestamp: TS.to_owned(),
            message,
        }))
    }

    fn user_msg(id: &str, parent_id: Option<&str>, text: &str) -> StoredEntry {
        msg(
            id,
            parent_id,
            AgentMessage::User(rpi_ai::types::UserMessage {
                role: UserRole::User,
                content: UserContent::Text(text.to_owned()),
                timestamp: 1,
            }),
        )
    }

    fn user_msg_blocks(id: &str, parent_id: Option<&str>, text: &str) -> StoredEntry {
        msg(
            id,
            parent_id,
            AgentMessage::User(rpi_ai::types::UserMessage {
                role: UserRole::User,
                content: UserContent::Blocks(vec![
                    UserContentBlock::Text(Tc {
                        text: text.to_owned(),
                        text_signature: None,
                    }),
                    UserContentBlock::Image(ImageContent {
                        data: "abc".into(),
                        mime_type: "image/png".into(),
                    }),
                ]),
                timestamp: 1,
            }),
        )
    }

    fn assistant_msg(
        id: &str,
        parent_id: Option<&str>,
        text: &str,
        stop_reason: StopReason,
        error_message: Option<&str>,
    ) -> StoredEntry {
        let content: Vec<Ac> = if text.is_empty() {
            Vec::new()
        } else {
            vec![Ac::Text(Tc {
                text: text.to_owned(),
                text_signature: None,
            })]
        };
        assistant_msg_with_content(id, parent_id, content, stop_reason, error_message)
    }

    fn assistant_msg_with_content(
        id: &str,
        parent_id: Option<&str>,
        content: Vec<Ac>,
        stop_reason: StopReason,
        error_message: Option<&str>,
    ) -> StoredEntry {
        msg(
            id,
            parent_id,
            AgentMessage::Assistant(rpi_ai::types::AssistantMessage {
                role: rpi_ai::types::AssistantRole::Assistant,
                content,
                api: "anthropic-messages".into(),
                provider: "anthropic".to_owned(),
                model: "claude-test".to_owned(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: rpi_ai::types::Usage {
                    input: 1,
                    output: 1,
                    cache_read: 0,
                    cache_write: 0,
                    cache_write1h: None,
                    reasoning: None,
                    total_tokens: 2,
                    cost: Default::default(),
                },
                stop_reason,
                error_message: error_message.map(str::to_owned),
                timestamp: 1,
                deferred: None,
                end_turn: None,
                raw_stop_reason: None,
            }),
        )
    }

    fn tool_call_block(id: &str, name: &str, arguments: serde_json::Value) -> Ac {
        Ac::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            thought_signature: None,
            namespace: None,
        })
    }

    fn tool_result_msg(
        id: &str,
        parent_id: Option<&str>,
        tool_call_id: &str,
        tool_name: &str,
        text: &str,
    ) -> StoredEntry {
        msg(
            id,
            parent_id,
            AgentMessage::ToolResult(rpi_ai::types::ToolResultMessage {
                role: rpi_ai::types::ToolResultRole::ToolResult,
                tool_call_id: tool_call_id.to_owned(),
                tool_name: tool_name.to_owned(),
                content: vec![Trc::Text(Tc {
                    text: text.to_owned(),
                    text_signature: None,
                })],
                details: None,
                usage: None,
                added_tool_names: None,
                is_error: false,
                timestamp: 1,
            }),
        )
    }

    fn bash_msg(id: &str, parent_id: Option<&str>, command: &str) -> StoredEntry {
        msg(
            id,
            parent_id,
            AgentMessage::BashExecution(BashExecutionMessage {
                role: BashExecutionRole::BashExecution,
                command: command.to_owned(),
                output: String::new(),
                exit_code: Some(0),
                cancelled: false,
                truncated: false,
                full_output_path: None,
                timestamp: 1,
                exclude_from_context: None,
            }),
        )
    }

    fn node(
        entry: StoredEntry,
        children: Vec<SessionTreeNode>,
        label: Option<&str>,
    ) -> SessionTreeNode {
        SessionTreeNode {
            entry,
            children,
            label: label.map(str::to_owned),
            label_timestamp: None,
        }
    }

    fn tree_list(
        tree: Vec<SessionTreeNode>,
        leaf_id: Option<&str>,
        max_visible_lines: usize,
        initial_selected_id: Option<&str>,
        filter_mode: TreeFilterMode,
    ) -> TreeList {
        TreeList::new(
            tree,
            leaf_id.map(str::to_owned),
            max_visible_lines,
            initial_selected_id.map(str::to_owned),
            filter_mode,
            theme(),
        )
    }

    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn component(
        tree: Vec<SessionTreeNode>,
        leaf_id: Option<&str>,
        on_label_change: Option<Box<dyn FnMut(&str, Option<&str>) + Send>>,
    ) -> TreeSelectorComponent {
        let tui = TuiMainScreen::new(Box::new(
            crate::modes::interactive::test_support::TestTerminal::new(),
        ));
        TreeSelectorComponent::new(
            tree,
            leaf_id.map(str::to_owned),
            24,
            theme(),
            tui,
            Box::new(|_| {}),
            Box::new(|| {}),
            on_label_change,
            None,
            TreeFilterMode::Default,
        )
    }

    /// Raw key sequences for the default bindings (keys.ts).
    const UP: &str = "\x1b[A";
    const DOWN: &str = "\x1b[B";
    const LEFT: &str = "\x1b[D";
    const RIGHT: &str = "\x1b[C";
    const CTRL_LEFT: &str = "\x1b[1;5D";
    const CTRL_RIGHT: &str = "\x1b[1;5C";
    const PAGE_UP: &str = "\x1b[5~";
    const PAGE_DOWN: &str = "\x1b[6~";
    const ENTER: &str = "\r";
    const ESCAPE: &str = "\x1b";
    const BACKSPACE: &str = "\x7f";
    const CTRL_X: &str = "\x18";
    const CTRL_O: &str = "\x0f";
    const CTRL_T: &str = "\x14";
    const CTRL_U: &str = "\x15";
    const CTRL_D: &str = "\x04";
    const CTRL_A: &str = "\x01";
    const CTRL_L: &str = "\x0c";
    const SHIFT_L: &str = "L";
    const SHIFT_T: &str = "T";

    /// Two roots with a branching active subtree:
    /// r1 (root one) → r1a → r1a1 (leaf), r1a → r1b; r2 (root two).
    fn branching_tree() -> Vec<SessionTreeNode> {
        let r1a1 = node(user_msg("r1a1", Some("r1a"), "grand"), vec![], None);
        let r1a = node(user_msg("r1a", Some("r1"), "child a"), vec![r1a1], None);
        let r1b = node(user_msg("r1b", Some("r1"), "child b"), vec![], None);
        let r1 = node(user_msg("r1", None, "root one"), vec![r1a, r1b], None);
        let r2 = node(user_msg("r2", None, "root two"), vec![], None);
        vec![r1, r2]
    }

    // -----------------------------------------------------------------------
    // Flattening
    // -----------------------------------------------------------------------

    #[test]
    fn flatten_tree_prioritizes_active_branch_and_marks_connectors() {
        let list = tree_list(
            branching_tree(),
            Some("r1a"),
            5,
            None,
            TreeFilterMode::Default,
        );

        let ids: Vec<&str> = list.flat_nodes.iter().map(|f| f.node.entry.id()).collect();
        // Active branch (r1, r1a) first; r1a1 only after its parent.
        assert_eq!(ids, vec!["r1", "r1a", "r1a1", "r1b", "r2"]);

        let r1 = &list.flat_nodes[0];
        assert_eq!(r1.indent, 1); // virtual-root child
        assert!(r1.show_connector);
        assert!(!r1.is_last); // r2 is last
        assert!(r1.gutters.is_empty());
        assert!(r1.is_virtual_root_child);

        let r1a = &list.flat_nodes[1];
        assert_eq!(r1a.indent, 2); // branch → +1
        assert!(r1a.show_connector);
        assert!(!r1a.is_last);
        assert!(r1a.gutters.is_empty()); // virtual-root children suppress gutters
        assert!(!r1a.is_virtual_root_child);

        let r1a1 = &list.flat_nodes[2];
        // First generation after a branch gets +1 (justBranched rule).
        assert_eq!(r1a1.indent, 3);
        assert!(!r1a1.show_connector);
        assert!(r1a1.is_last);
        // Connector of r1a (displayIndent 1 → position 0) propagates a gutter.
        assert_eq!(
            r1a1.gutters,
            vec![GutterInfo {
                position: 0,
                show: true
            }]
        );
        assert!(!r1a1.is_virtual_root_child);

        let r1b = &list.flat_nodes[3];
        assert_eq!(r1b.indent, 2);
        assert!(r1b.show_connector);
        assert!(r1b.is_last);
        assert!(!r1b.is_virtual_root_child);

        let r2 = &list.flat_nodes[4];
        assert_eq!(r2.indent, 1);
        assert!(r2.is_last);
        assert!(r2.is_virtual_root_child);
    }

    #[test]
    fn flatten_tree_single_root_stays_flat_for_chains() {
        let a1 = node(user_msg("a1", Some("a"), "a1"), vec![], None);
        let a = node(user_msg("a", Some("r1"), "a"), vec![a1], None);
        let r1 = node(user_msg("r1", None, "r1"), vec![a], None);

        let list = tree_list(vec![r1], None, 5, None, TreeFilterMode::Default);

        let r1n = &list.flat_nodes[0];
        assert_eq!(r1n.indent, 0);
        assert!(!r1n.show_connector); // single root: no connector

        // Single-child chain stays at indent 0.
        let a = &list.flat_nodes[1];
        assert_eq!(a.indent, 0);
        assert!(!a.show_connector);
        let a1 = &list.flat_nodes[2];
        assert_eq!(a1.indent, 0);
    }

    #[test]
    fn flatten_tree_branch_children_get_extra_indent() {
        let x = node(user_msg("x", Some("r2"), "x"), vec![], None);
        let y = node(user_msg("y", Some("r2"), "y"), vec![], None);
        let r2 = node(user_msg("r2", None, "r2"), vec![x, y], None);

        let list = tree_list(vec![r2.clone()], None, 5, None, TreeFilterMode::Default);

        // Single root: no virtual-root indent, branch children +1.
        let r2n = &list.flat_nodes[0];
        assert_eq!(r2n.indent, 0);
        assert!(!r2n.show_connector);
        let x = &list.flat_nodes[1];
        assert_eq!(x.indent, 1);
        assert!(x.show_connector);
        let y = &list.flat_nodes[2];
        assert_eq!(y.indent, 1);
        assert!(y.show_connector);
        assert!(y.is_last);

        // Two roots: treated as a virtual root that branches.
        let r2b = node(user_msg("r2b", None, "r2b"), vec![], None);
        let list = tree_list(
            vec![r2.clone(), r2b],
            None,
            5,
            None,
            TreeFilterMode::Default,
        );
        assert_eq!(list.flat_nodes[0].indent, 1);
        assert!(list.flat_nodes[0].is_virtual_root_child);
        assert_eq!(list.flat_nodes[1].indent, 2);
        assert!(list.flat_nodes[1].show_connector);
        assert!(!list.flat_nodes[1].is_virtual_root_child);
        assert_eq!(list.flat_nodes[2].indent, 2);
        assert_eq!(list.flat_nodes[3].indent, 1);
        assert!(list.flat_nodes[3].is_virtual_root_child);
        assert!(list.flat_nodes[3].is_last);
    }

    #[test]
    fn render_shows_connectors_fold_markers_and_active_path() {
        let list = tree_list(
            branching_tree(),
            Some("r1a"),
            5,
            None,
            TreeFilterMode::Default,
        );
        let lines: Vec<String> = list
            .render(60)
            .iter()
            .map(|line| strip_ansi(line))
            .collect();

        // r1a is on the active path with a foldable marker at the connector.
        assert!(
            lines.iter().any(|line| line.contains("├⊟ • user: child a")),
            "{lines:?}"
        );
        // r1a1 carries the gutter below r1a's connector (displayIndent 2).
        assert!(
            lines.iter().any(|line| line.contains("│     user: grand")),
            "{lines:?}"
        );
        // Selected node: the active leaf r1a (nearest visible of the leaf).
        assert!(
            lines.iter().any(|line| line.contains("user: grand")),
            "{lines:?}"
        );
        // Status line with selection count.
        assert!(lines.last().unwrap().contains("(2/5)"), "{lines:?}");
    }

    // -----------------------------------------------------------------------
    // Filtering
    // -----------------------------------------------------------------------

    /// Mixed entry chain: root user → user → assistant → tool result →
    /// label → custom → model change → thinking change → session info →
    /// compaction → branch summary → custom message.
    fn mixed_tree() -> Vec<SessionTreeNode> {
        let m1 = node(user_msg("m1", Some("r"), "hello"), vec![], None);
        let m2 = node(
            assistant_msg("m2", Some("m1"), "world", StopReason::Stop, None),
            vec![],
            None,
        );
        let m3 = node(
            tool_result_msg("m3", Some("m2"), "tc1", "read", "file contents"),
            vec![],
            None,
        );
        let l1 = node(
            known(SessionEntry::Label(LabelEntry {
                id: "l1".into(),
                parent_id: Some("m3".into()),
                timestamp: TS.into(),
                target_id: "m3".into(),
                label: Some("mark".into()),
            })),
            vec![],
            None,
        );
        let c1 = node(
            known(SessionEntry::Custom(CustomEntry {
                id: "c1".into(),
                parent_id: Some("l1".into()),
                timestamp: TS.into(),
                custom_type: "foo".into(),
                data: None,
            })),
            vec![],
            None,
        );
        let mc1 = node(
            known(SessionEntry::ModelChange(ModelChangeEntry {
                id: "mc1".into(),
                parent_id: Some("c1".into()),
                timestamp: TS.into(),
                provider: "anthropic".into(),
                model_id: "claude-4".into(),
            })),
            vec![],
            None,
        );
        let tl1 = node(
            known(SessionEntry::ThinkingLevelChange(
                ThinkingLevelChangeEntry {
                    id: "tl1".into(),
                    parent_id: Some("mc1".into()),
                    timestamp: TS.into(),
                    thinking_level: "high".into(),
                },
            )),
            vec![],
            None,
        );
        let si1 = node(
            known(SessionEntry::SessionInfo(SessionInfoEntry {
                id: "si1".into(),
                parent_id: Some("tl1".into()),
                timestamp: TS.into(),
                name: Some("my session".into()),
            })),
            vec![],
            None,
        );
        let comp1 = node(
            known(SessionEntry::Compaction(CompactionEntry {
                id: "comp1".into(),
                parent_id: Some("si1".into()),
                timestamp: TS.into(),
                summary: "compacted".into(),
                first_kept_entry_id: Some("m1".into()),
                tokens_before: 12345,
                retained_tail: None,
                details: None,
                usage: None,
                from_hook: None,
            })),
            vec![],
            None,
        );
        let bs1 = node(
            known(SessionEntry::BranchSummary(BranchSummaryEntry {
                id: "bs1".into(),
                parent_id: Some("comp1".into()),
                timestamp: TS.into(),
                from_id: "m1".into(),
                summary: "branch summary text".into(),
                details: None,
                usage: None,
                from_hook: None,
            })),
            vec![],
            None,
        );
        let cm1 = node(
            known(SessionEntry::CustomMessage(CustomMessageEntry {
                id: "cm1".into(),
                parent_id: Some("bs1".into()),
                timestamp: TS.into(),
                custom_type: "status".into(),
                content: UserContent::Text("custom text".into()),
                details: None,
                display: true,
            })),
            vec![],
            None,
        );
        let root = node(
            user_msg("r", None, "root"),
            vec![m1, m2, m3, l1, c1, mc1, tl1, si1, comp1, bs1, cm1],
            None,
        );
        vec![root]
    }

    fn filtered_ids(list: &TreeList) -> Vec<&str> {
        list.filtered_nodes
            .iter()
            .map(|f| f.node.entry.id())
            .collect()
    }

    #[test]
    fn default_filter_hides_settings_entries() {
        let list = tree_list(mixed_tree(), None, 5, None, TreeFilterMode::Default);
        assert_eq!(
            filtered_ids(&list),
            vec!["r", "m1", "m2", "m3", "comp1", "bs1", "cm1"]
        );
    }

    #[test]
    fn no_tools_filter_hides_tool_results_too() {
        let list = tree_list(mixed_tree(), None, 5, None, TreeFilterMode::NoTools);
        assert_eq!(
            filtered_ids(&list),
            vec!["r", "m1", "m2", "comp1", "bs1", "cm1"]
        );
    }

    #[test]
    fn user_only_filter_keeps_user_messages() {
        let list = tree_list(mixed_tree(), None, 5, None, TreeFilterMode::UserOnly);
        assert_eq!(filtered_ids(&list), vec!["r", "m1"]);
    }

    #[test]
    fn labeled_only_filter_keeps_labeled_nodes() {
        let tree = mixed_tree();
        let mut list = tree_list(tree, None, 5, None, TreeFilterMode::LabeledOnly);
        // No labels yet → empty.
        assert!(list.filtered_nodes.is_empty());
        list.update_node_label("m2", Some("bookmark"), None);
        list.apply_filter();
        assert_eq!(filtered_ids(&list), vec!["m2"]);
        // Clearing the label removes it again.
        list.update_node_label("m2", None, None);
        list.apply_filter();
        assert!(list.filtered_nodes.is_empty());
    }

    #[test]
    fn all_filter_shows_everything() {
        let list = tree_list(mixed_tree(), None, 5, None, TreeFilterMode::All);
        assert_eq!(
            filtered_ids(&list),
            vec!["r", "m1", "m2", "m3", "l1", "c1", "mc1", "tl1", "si1", "comp1", "bs1", "cm1"]
        );
    }

    #[test]
    fn assistant_toolcall_only_hidden_unless_error_or_leaf() {
        let tool_use = assistant_msg_with_content(
            "a1",
            Some("r"),
            vec![tool_call_block(
                "tc1",
                "read",
                serde_json::json!({"path": "x"}),
            )],
            StopReason::ToolUse,
            None,
        );
        let errored = assistant_msg_with_content(
            "a2",
            Some("r"),
            vec![tool_call_block(
                "tc2",
                "read",
                serde_json::json!({"path": "y"}),
            )],
            StopReason::Error,
            Some("boom"),
        );
        let leaf_tool_use = assistant_msg_with_content(
            "a3",
            Some("r"),
            vec![tool_call_block(
                "tc3",
                "read",
                serde_json::json!({"path": "z"}),
            )],
            StopReason::ToolUse,
            None,
        );
        let root = node(
            user_msg("r", None, "root"),
            vec![
                node(tool_use, vec![], None),
                node(errored, vec![], None),
                node(leaf_tool_use, vec![], None),
            ],
            None,
        );
        let list = tree_list(vec![root], Some("a3"), 5, None, TreeFilterMode::Default);
        // a1 hidden (tool-call only, normal stop); a2 shown (error); a3 shown
        // (current leaf). The active branch (a3) sorts first.
        assert_eq!(filtered_ids(&list), vec!["r", "a3", "a2"]);
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    #[test]
    fn search_filters_by_tokens_and_backspace_restores() {
        install_global_keybindings();
        let m1 = node(user_msg("m1", Some("r"), "alpha beta"), vec![], None);
        let m2 = node(user_msg("m2", Some("r"), "gamma delta"), vec![], None);
        let root = node(user_msg("r", None, "root"), vec![m1, m2], None);
        let mut list = tree_list(vec![root], None, 5, None, TreeFilterMode::Default);

        for ch in "alph".chars() {
            list.handle_input(&ch.to_string());
        }
        assert_eq!(filtered_ids(&list), vec!["m1"]);
        assert_eq!(list.get_search_query(), "alph");

        // Token must match the whole searchable text (role included).
        for ch in "axx".chars() {
            list.handle_input(&ch.to_string());
        }
        assert!(list.filtered_nodes.is_empty());
        // Backspace clears one char at a time.
        list.handle_input(BACKSPACE);
        list.handle_input(BACKSPACE);
        list.handle_input(BACKSPACE);
        assert_eq!(list.get_search_query(), "alph");
        assert_eq!(filtered_ids(&list), vec!["m1"]);

        // Multi-word search: every token must match.
        for _ in 0..4 {
            list.handle_input(BACKSPACE);
        }
        assert_eq!(list.get_search_query(), "");
        assert_eq!(filtered_ids(&list).len(), 3);
        for ch in " beta".chars() {
            list.handle_input(&ch.to_string());
        }
        assert_eq!(filtered_ids(&list), vec!["m1"]);
    }

    #[test]
    fn search_matches_roles_and_tool_names() {
        install_global_keybindings();
        let mut list = tree_list(mixed_tree(), None, 5, None, TreeFilterMode::Default);
        for ch in "toolresult".chars() {
            list.handle_input(&ch.to_string());
        }
        assert_eq!(filtered_ids(&list), vec!["m3"]);
        for _ in 0..10 {
            list.handle_input(BACKSPACE);
        }
        for ch in "compaction".chars() {
            list.handle_input(&ch.to_string());
        }
        assert_eq!(filtered_ids(&list), vec!["comp1"]);
    }

    // -----------------------------------------------------------------------
    // Fold / unfold / branch navigation
    // -----------------------------------------------------------------------

    fn branch_tree() -> Vec<SessionTreeNode> {
        let a = node(user_msg("a", Some("r1"), "a"), vec![], None);
        let b = node(user_msg("b", Some("r1"), "b"), vec![], None);
        let c = node(user_msg("c", Some("r1"), "c"), vec![], None);
        let r1 = node(user_msg("r1", None, "r1"), vec![a, b, c], None);
        vec![r1]
    }

    #[test]
    fn fold_unfold_hides_and_restores_children() {
        install_global_keybindings();
        let mut list = tree_list(branch_tree(), None, 5, Some("r1"), TreeFilterMode::Default);
        assert_eq!(filtered_ids(&list), vec!["r1", "a", "b", "c"]);

        // Fold the branch root.
        list.handle_input(CTRL_LEFT);
        assert_eq!(filtered_ids(&list), vec!["r1"]);
        assert!(list.folded_nodes.contains("r1"));
        let stripped = strip_ansi(&list.render(60).join("\n"));
        assert!(stripped.contains("⊞ user: r1"), "{stripped}");

        // Unfold.
        list.handle_input(CTRL_RIGHT);
        assert_eq!(filtered_ids(&list), vec!["r1", "a", "b", "c"]);
        assert!(list.folded_nodes.is_empty());
    }

    #[test]
    fn fold_or_up_and_unfold_or_down_navigate_branch_segments() {
        install_global_keybindings();
        // From the last child, foldOrUp (not foldable) jumps to the branch
        // root.
        let mut list = tree_list(branch_tree(), None, 5, Some("c"), TreeFilterMode::Default);
        assert_eq!(list.selected_index, 3);
        list.handle_input(CTRL_LEFT);
        assert_eq!(list.selected_index, 0);

        // From the branch root, unfoldOrDown jumps to the first child.
        let mut list = tree_list(branch_tree(), None, 5, Some("r1"), TreeFilterMode::Default);
        list.handle_input(CTRL_RIGHT);
        assert_eq!(list.selected_index, 1);

        // From a branch root with a single-child chain, down walks to the
        // chain's end.
        let a2 = node(user_msg("a2", Some("a1"), "a2"), vec![], None);
        let a1 = node(user_msg("a1", Some("a"), "a1"), vec![a2], None);
        let a = node(user_msg("a", Some("r1"), "a"), vec![a1], None);
        let r1 = node(user_msg("r1", None, "r1"), vec![a], None);
        let mut list = tree_list(vec![r1], None, 5, Some("r1"), TreeFilterMode::Default);
        list.handle_input(CTRL_RIGHT);
        assert_eq!(list.selected_index, 3); // a2
    }

    #[test]
    fn folding_removes_descendants_of_folded_nodes() {
        install_global_keybindings();
        let a1 = node(user_msg("a1", Some("a"), "a1"), vec![], None);
        let a = node(user_msg("a", Some("r1"), "a"), vec![a1], None);
        let b = node(user_msg("b", Some("r1"), "b"), vec![], None);
        let r1 = node(user_msg("r1", None, "r1"), vec![a, b], None);
        let mut list = tree_list(vec![r1], None, 5, Some("a"), TreeFilterMode::Default);
        // Fold "a" (segment start: parent has 2 visible children).
        list.handle_input(CTRL_LEFT);
        assert_eq!(filtered_ids(&list), vec!["r1", "a", "b"]); // a1 hidden
        assert!(list.folded_nodes.contains("a"));
    }

    // -----------------------------------------------------------------------
    // Selection, scroll, cancel
    // -----------------------------------------------------------------------

    fn long_tree(count: usize) -> Vec<SessionTreeNode> {
        let children: Vec<SessionTreeNode> = (1..=count)
            .map(|i| {
                node(
                    user_msg(&format!("m{i}"), Some("r"), &format!("message {i}")),
                    vec![],
                    None,
                )
            })
            .collect();
        vec![node(user_msg("r", None, "root"), children, None)]
    }

    #[test]
    fn selection_wraps_and_scroll_window_slides() {
        install_global_keybindings();
        let mut list = tree_list(long_tree(20), None, 5, None, TreeFilterMode::Default);
        // No target: falls back to the last entry.
        assert_eq!(list.selected_index, 20);
        let lines = list.render(60);
        assert!(
            strip_ansi(lines.last().unwrap()).contains("(21/21)"),
            "{lines:?}"
        );

        // Wrap around to the top.
        list.handle_input(DOWN);
        assert_eq!(list.selected_index, 0);
        let lines = list.render(60);
        assert!(
            strip_ansi(lines.last().unwrap()).contains("(1/21)"),
            "{lines:?}"
        );

        // Scroll window centers the selection (maxVisibleLines 5 → half 2).
        for _ in 0..4 {
            list.handle_input(DOWN);
        }
        assert_eq!(list.selected_index, 4);
        let lines = list.render(60);
        assert!(
            strip_ansi(lines.last().unwrap()).contains("(5/21)"),
            "{lines:?}"
        );

        list.handle_input(DOWN);
        assert_eq!(list.selected_index, 5);
        let lines = list.render(60);
        assert!(
            strip_ansi(lines.last().unwrap()).contains("(6/21)"),
            "{lines:?}"
        );

        // Wrap down from the bottom back to the top.
        for _ in 0..15 {
            list.handle_input(DOWN);
        }
        assert_eq!(list.selected_index, 20);
        list.handle_input(DOWN);
        assert_eq!(list.selected_index, 0);
    }

    #[test]
    fn page_keys_move_by_the_visible_window() {
        install_global_keybindings();
        let mut list = tree_list(long_tree(20), None, 5, Some("m10"), TreeFilterMode::Default);
        assert_eq!(list.selected_index, 10);

        list.handle_input(PAGE_DOWN);
        assert_eq!(list.selected_index, 15); // 10 + 5
        list.handle_input(PAGE_UP);
        assert_eq!(list.selected_index, 10);
        // Arrow-left/right alias the page keys.
        list.handle_input(RIGHT);
        assert_eq!(list.selected_index, 15);
        list.handle_input(LEFT);
        assert_eq!(list.selected_index, 10);
        // Clamped at the ends.
        list.handle_input(PAGE_UP);
        list.handle_input(PAGE_UP);
        assert_eq!(list.selected_index, 0);
        list.handle_input(PAGE_DOWN);
        list.handle_input(PAGE_DOWN);
        list.handle_input(PAGE_DOWN);
        list.handle_input(PAGE_DOWN);
        assert_eq!(list.selected_index, 20);
    }

    #[test]
    fn up_and_down_wrap_around() {
        install_global_keybindings();
        let mut list = tree_list(long_tree(3), None, 5, Some("m1"), TreeFilterMode::Default);
        assert_eq!(list.selected_index, 1);
        list.handle_input(UP);
        assert_eq!(list.selected_index, 0);
        list.handle_input(UP);
        assert_eq!(list.selected_index, 3); // wrapped to the last
        list.handle_input(DOWN);
        assert_eq!(list.selected_index, 0);
    }

    #[test]
    fn escape_clears_search_before_firing_cancel() {
        install_global_keybindings();
        let mut list = tree_list(long_tree(3), None, 5, None, TreeFilterMode::Default);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        list.on_cancel = Some(Box::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));

        // With a search query, escape clears the search instead of cancelling.
        list.handle_input("a");
        list.handle_input(ESCAPE);
        assert_eq!(list.get_search_query(), "");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(filtered_ids(&list).len(), 4);

        // Second escape fires the cancel callback.
        list.handle_input(ESCAPE);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Ctrl+C behaves like escape (both bound to tui.select.cancel).
        list.handle_input("b");
        list.handle_input("\x03");
        assert_eq!(list.get_search_query(), "");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        list.handle_input("\x03");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn confirm_selects_the_selected_entry() {
        install_global_keybindings();
        let mut list = tree_list(long_tree(3), None, 5, Some("m2"), TreeFilterMode::Default);
        let selected = Arc::new(Mutex::new(None::<String>));
        let slot = selected.clone();
        list.on_select = Some(Box::new(move |id| {
            *slot.lock().unwrap() = Some(id.to_string());
        }));
        list.handle_input(ENTER);
        assert_eq!(selected.lock().unwrap().as_deref(), Some("m2"));
    }

    #[test]
    fn empty_tree_renders_no_entries_status() {
        let list = tree_list(vec![], None, 5, None, TreeFilterMode::Default);
        let lines: Vec<String> = list.render(40).iter().map(|l| strip_ansi(l)).collect();
        assert_eq!(
            lines,
            vec!["  No entries found".to_string(), "  (0/0)".to_string()]
        );
    }

    #[test]
    fn filter_keys_cycle_modes_and_update_status_labels() {
        install_global_keybindings();
        let mut list = tree_list(mixed_tree(), None, 5, None, TreeFilterMode::Default);

        // Cycle forward: default → no-tools → user-only → labeled-only →
        // all → default.
        list.handle_input(CTRL_O);
        assert_eq!(list.filter_mode, TreeFilterMode::NoTools);
        assert_eq!(list.get_status_labels(), " [no-tools]");
        list.handle_input(CTRL_O);
        assert_eq!(list.filter_mode, TreeFilterMode::UserOnly);
        list.handle_input(CTRL_O);
        assert_eq!(list.filter_mode, TreeFilterMode::LabeledOnly);
        list.handle_input(CTRL_O);
        assert_eq!(list.filter_mode, TreeFilterMode::All);
        list.handle_input(CTRL_O);
        assert_eq!(list.filter_mode, TreeFilterMode::Default);

        // Direct filters.
        list.handle_input(CTRL_T);
        assert_eq!(list.filter_mode, TreeFilterMode::NoTools);
        list.handle_input(CTRL_T);
        assert_eq!(list.filter_mode, TreeFilterMode::Default); // toggle back
        list.handle_input(CTRL_U);
        assert_eq!(list.filter_mode, TreeFilterMode::UserOnly);
        list.handle_input(CTRL_L);
        assert_eq!(list.filter_mode, TreeFilterMode::LabeledOnly);
        list.handle_input(CTRL_A);
        assert_eq!(list.filter_mode, TreeFilterMode::All);
        list.handle_input(CTRL_D);
        assert_eq!(list.filter_mode, TreeFilterMode::Default);

        // Status label renders into the last line.
        list.handle_input(CTRL_O);
        let lines = list.render(60);
        assert!(strip_ansi(lines.last().unwrap()).contains("[no-tools]"));
    }

    #[test]
    fn toggle_label_timestamps_updates_status_and_render() {
        install_global_keybindings();
        let a = node(user_msg("a", Some("r"), "hello"), vec![], Some("bookmark"));
        let mut tree = long_tree(1);
        tree[0].children.push(a);
        let mut list = tree_list(tree, None, 5, None, TreeFilterMode::Default);

        let lines = list.render(60);
        let stripped = strip_ansi(&lines.join("\n"));
        assert!(stripped.contains("[bookmark] user: hello"), "{stripped}");
        assert!(!stripped.contains("[+label time]"));

        list.handle_input(SHIFT_T);
        assert!(list.show_label_timestamps);
        let lines = list.render(60);
        let stripped = strip_ansi(&lines.join("\n"));
        assert!(stripped.contains("[+label time]"), "{stripped}");
        // No label timestamp → nothing rendered after the label.
        assert!(stripped.contains("[bookmark] "), "{stripped}");
    }

    #[test]
    fn copy_selected_forwards_text_to_on_copy() {
        install_global_keybindings();
        let bash = node(bash_msg("b1", Some("r"), "ls -la"), vec![], None);
        let user = node(user_msg("u1", Some("r"), "hello world"), vec![], None);
        let root = node(user_msg("r", None, "root"), vec![bash, user], None);
        let mut list = tree_list(vec![root], None, 5, Some("b1"), TreeFilterMode::Default);

        let copied = Arc::new(Mutex::new(None::<String>));
        let slot = copied.clone();
        list.on_copy = Some(Box::new(move |text| {
            *slot.lock().unwrap() = text.map(str::to_string);
        }));
        list.handle_input(CTRL_X);
        assert_eq!(copied.lock().unwrap().as_deref(), Some("ls -la"));

        // A message without copyable content forwards None.
        list.handle_input(UP); // root: "root"
        list.handle_input(CTRL_X);
        assert_eq!(copied.lock().unwrap().as_deref(), Some("root"));
    }

    // -----------------------------------------------------------------------
    // Horizontal viewport
    // -----------------------------------------------------------------------

    #[test]
    fn horizontal_viewport_clips_body_keeps_gutter() {
        let long_body = format!("{}x", "A".repeat(90));
        let rows = vec![
            HorizontalViewportRow {
                gutter: "  ".to_string(),
                body: long_body,
                anchor_col: 50,
                body_width: 91,
                is_selected: true,
            },
            HorizontalViewportRow {
                gutter: "  ".to_string(),
                body: "short".to_string(),
                anchor_col: 0,
                body_width: 5,
                is_selected: false,
            },
        ];
        let lines = render_horizontal_viewport(&rows, 20);
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert!(visible_width(line) <= 20, "{line:?}");
        }
        // Gutter always visible; the selected body is clipped with the ANSI
        // reset appended after the slice.
        assert!(lines[0].starts_with("  "));
        assert!(lines[0].contains("\x1b[0m"));
        // Non-selected rows are clipped the same way once a scroll is active.
        assert!(visible_width(&lines[1]) <= 20);
    }

    #[test]
    fn horizontal_viewport_without_selection_does_not_scroll() {
        let rows = vec![HorizontalViewportRow {
            gutter: "  ".to_string(),
            body: "abcdefghijklmnopqrstuvwxyz".to_string(),
            anchor_col: 40,
            body_width: 26,
            is_selected: false,
        }];
        let lines = render_horizontal_viewport(&rows, 10);
        assert_eq!(strip_ansi(&lines[0]), "  abcdefgh");
        assert_eq!(visible_width(&lines[0]), 10);
    }

    // -----------------------------------------------------------------------
    // Label timestamps
    // -----------------------------------------------------------------------

    #[test]
    fn format_label_parts_shapes() {
        assert_eq!(format_label_parts((2026, 8, 5), 2026, 8, 5, 9, 30), "09:30");
        assert_eq!(
            format_label_parts((2026, 8, 5), 2026, 3, 7, 9, 5),
            "3/7 09:05"
        );
        assert_eq!(
            format_label_parts((2026, 8, 5), 2025, 12, 31, 23, 59),
            "25/12/31 23:59"
        );
        assert_eq!(
            format_label_parts((2026, 8, 5), 2005, 1, 2, 0, 0),
            "05/1/2 00:00"
        );
    }

    #[test]
    fn epoch_ms_to_utc_matches_iso8601() {
        assert_eq!(
            epoch_ms_to_utc(parse_iso8601_ms("2024-12-03T14:00:00.000Z").unwrap()),
            (2024, 12, 3, 14, 0)
        );
        assert_eq!(epoch_ms_to_utc(0), (1970, 1, 1, 0, 0));
        assert_eq!(
            epoch_ms_to_utc(parse_iso8601_ms("1969-07-20T20:17:00.000Z").unwrap()),
            (1969, 7, 20, 20, 17)
        );
    }

    #[test]
    fn format_label_timestamp_renders_today_as_time() {
        let now = crate::core::session_manager::now_iso8601();
        let out = format_label_timestamp(&now);
        // Same day in any timezone → bare HH:MM.
        assert!(is_hhmm_shape(&out), "unexpected: {out}");
        // 1970 is never today → year-qualified shape.
        let old = format_label_timestamp("1970-01-01T00:00:00.000Z");
        assert!(is_date_time_shape(&old), "unexpected: {old}");
        // Unparseable timestamps render empty.
        assert_eq!(format_label_timestamp("not a date"), "");
    }

    /// `HH:MM` with 0-padded hours/minutes.
    fn is_hhmm_shape(text: &str) -> bool {
        let bytes = text.as_bytes();
        bytes.len() == 5
            && bytes[2] == b':'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, &c)| i == 2 || c.is_ascii_digit())
    }

    /// `YY/M/D HH:MM` — variable-width month/day.
    fn is_date_time_shape(text: &str) -> bool {
        let parts: Vec<&str> = text.split(['/', ' ', ':']).collect();
        if parts.len() != 5 {
            return false;
        }
        let nums: Vec<u32> = parts
            .iter()
            .map(|part| part.parse().ok())
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default();
        nums.len() == 5
            && nums[0] < 100
            && (1..=12).contains(&nums[1])
            && (1..=31).contains(&nums[2])
            && nums[3] < 24
            && nums[4] < 60
    }

    // -----------------------------------------------------------------------
    // Component-level wiring
    // -----------------------------------------------------------------------

    #[test]
    fn component_label_edit_submit_applies_label_and_hides_input() {
        install_global_keybindings();
        let mut component = component(long_tree(2), None, None);
        assert!(component.label_input.is_none());

        // shift+l opens the label input.
        component.handle_input(SHIFT_L);
        assert!(component.label_input.is_some());
        let stripped = strip_ansi(&component.render(60).join("\n"));
        assert!(stripped.contains("Label (empty to remove):"), "{stripped}");
        assert!(stripped.contains("save"), "{stripped}");

        // Typing goes to the input; enter submits.
        for ch in "mylabel".chars() {
            component.handle_input(&ch.to_string());
        }
        component.handle_input(ENTER);
        assert!(component.label_input.is_none());

        // The label lands on the flat node (upstream `updateNodeLabel`
        // mutates flatNodes; the filtered view refreshes on the next
        // filter change).
        let list = component.get_tree_list();
        let flat_label = list
            .flat_nodes
            .iter()
            .find(|f| f.node.entry.id() == "m2")
            .map(|f| f.node.label.clone());
        assert_eq!(flat_label, Some(Some("mylabel".to_string())));

        // After the next filter dispatch the render shows the label.
        component.handle_input(CTRL_D); // filter.default → applyFilter
        let stripped = strip_ansi(&component.render(60).join("\n"));
        assert!(stripped.contains("[mylabel] user: message 2"), "{stripped}");
    }

    #[test]
    fn component_label_edit_cancel_discards_input() {
        install_global_keybindings();
        let mut component = component(long_tree(2), None, None);
        component.handle_input(SHIFT_L);
        for ch in "abc".chars() {
            component.handle_input(&ch.to_string());
        }
        component.handle_input(ESCAPE);
        assert!(component.label_input.is_none());
        let list = component.get_tree_list();
        let selected = list
            .filtered_nodes
            .get(list.selected_index)
            .map(|f| f.node.label.clone());
        assert_eq!(selected, Some(None));
    }

    #[test]
    fn component_label_submit_empty_removes_label_and_fires_callback() {
        install_global_keybindings();
        let changes = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
        let slot = changes.clone();
        let mut component = component(
            long_tree(1),
            None,
            Some(Box::new(move |id, label| {
                slot.lock()
                    .unwrap()
                    .push((id.to_string(), label.map(str::to_string)));
            })),
        );
        // Pre-label the selected node.
        component
            .get_tree_list()
            .update_node_label("m1", Some("old"), None);

        component.handle_input(SHIFT_L);
        // The input is prefilled with "old" — clear it, then submit empty to
        // remove the label.
        for _ in 0..3 {
            component.handle_input(BACKSPACE);
        }
        component.handle_input(ENTER); // empty → remove
        assert!(component.label_input.is_none());
        assert_eq!(
            changes.lock().unwrap().as_slice(),
            &[("m1".to_string(), None)]
        );
        let list = component.get_tree_list();
        let selected = list
            .filtered_nodes
            .get(list.selected_index)
            .map(|f| f.node.label.clone());
        assert_eq!(selected, Some(None));
    }

    #[test]
    fn component_edit_label_prefills_current_label() {
        install_global_keybindings();
        // The label must be present in the filtered view (set at tree
        // construction) for the prefill.
        let m1 = node(
            user_msg("m1", Some("r"), "message 1"),
            vec![],
            Some("existing"),
        );
        let root = node(user_msg("r", None, "root"), vec![m1], None);
        let mut component = component(vec![root], None, None);
        component.handle_input(SHIFT_L);
        assert!(component.label_input.is_some());
        let stripped = strip_ansi(&component.render(60).join("\n"));
        assert!(stripped.contains("existing"), "{stripped}");
    }

    #[test]
    fn component_copy_forwards_to_on_copy() {
        install_global_keybindings();
        let copied = Arc::new(Mutex::new(None::<String>));
        let slot = copied.clone();
        let mut component = component(long_tree(1), None, None);
        component.on_copy = Some(Box::new(move |text| {
            *slot.lock().unwrap() = text.map(str::to_string);
        }));
        component.handle_input(CTRL_X);
        assert_eq!(copied.lock().unwrap().as_deref(), Some("message 1"));
    }

    #[test]
    fn component_focus_propagates_to_label_input() {
        install_global_keybindings();
        let mut component = component(long_tree(1), None, None);
        component.set_focused(true);
        assert!(component.focused());
        component.handle_input(SHIFT_L);
        assert!(component.label_input.as_ref().unwrap().focused());
        component.set_focused(false);
        assert!(!component.label_input.as_ref().unwrap().focused());
    }

    #[test]
    fn component_render_lays_out_full_stack() {
        install_global_keybindings();
        let component = component(long_tree(1), None, None);
        let lines = component.render(60);
        let stripped: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        // Spacer, border, title, help, search line, border, spacer, tree,
        // spacer, border.
        assert_eq!(stripped[0], "");
        assert!(stripped[1].starts_with('─'));
        assert!(stripped[2].contains("Session Tree"));
        let help_idx = stripped.iter().position(|l| l.contains("move")).unwrap();
        assert!(help_idx > 2);
        let search_idx = stripped
            .iter()
            .position(|l| l.contains("Type to search:"))
            .unwrap();
        assert!(search_idx > help_idx);
        assert!(stripped.iter().any(|l| l.contains("user: root")));
        assert!(stripped.last().unwrap().starts_with('─'));
    }

    #[test]
    fn component_help_renders_keybinding_hints() {
        install_global_keybindings();
        let component = component(long_tree(1), None, None);
        let stripped = strip_ansi(&component.render(80).join("\n"));
        assert!(stripped.contains("↑/↓ move"), "{stripped}");
        assert!(stripped.contains("←/→ page"), "{stripped}");
        assert!(stripped.contains("ctrl+x copy"), "{stripped}");
        assert!(stripped.contains("shift+l label"), "{stripped}");
        assert!(stripped.contains("shift+t label time"), "{stripped}");
        assert!(stripped.contains("filters ctrl+d/t/u/l/a"), "{stripped}");
        assert!(stripped.contains("cycle"), "{stripped}");
        assert!(stripped.contains("branch"), "{stripped}");
    }

    #[test]
    fn component_search_line_tracks_query() {
        install_global_keybindings();
        let mut component = component(long_tree(2), None, None);
        component.handle_input("m");
        let stripped = strip_ansi(&component.render(60).join("\n"));
        assert!(stripped.contains("Type to search: m"), "{stripped}");
        component.handle_input(ESCAPE);
        let stripped = strip_ansi(&component.render(60).join("\n"));
        assert!(stripped.contains("Type to search:"), "{stripped}");
        assert!(!stripped.contains("Type to search: m"));
    }

    #[test]
    fn component_empty_tree_does_not_fire_cancel() {
        install_global_keybindings();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let tui = TuiMainScreen::new(Box::new(
            crate::modes::interactive::test_support::TestTerminal::new(),
        ));
        let _component = TreeSelectorComponent::new(
            vec![],
            None,
            24,
            theme(),
            tui,
            Box::new(|_| {}),
            Box::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
            None,
            None,
            TreeFilterMode::Default,
        );
        // Upstream defers onCancel for empty trees; the port leaves this to
        // the integration layer (documented difference).
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn get_entry_copy_text_variants() {
        let user = node(user_msg("u", None, "  hello world  "), vec![], None);
        // Returns the original text (trimmed only for the emptiness check).
        assert_eq!(
            get_entry_copy_text(&user).as_deref(),
            Some("  hello world  ")
        );
        let empty = node(
            assistant_msg("a", None, "", StopReason::Stop, None),
            vec![],
            None,
        );
        assert_eq!(get_entry_copy_text(&empty), None);
        let errored = node(
            assistant_msg("a2", None, "", StopReason::Error, Some("oops")),
            vec![],
            None,
        );
        assert_eq!(get_entry_copy_text(&errored).as_deref(), Some("oops"));
        let tool = node(
            tool_result_msg("t", None, "tc1", "bash", "output text"),
            vec![],
            None,
        );
        assert_eq!(get_entry_copy_text(&tool).as_deref(), Some("output text"));
        // Block content joins text blocks (images skipped).
        let blocks = node(user_msg_blocks("u2", None, "blocked text"), vec![], None);
        assert_eq!(
            get_entry_copy_text(&blocks).as_deref(),
            Some("blocked text")
        );
    }

    #[test]
    fn format_tool_call_previews() {
        let args = |json: serde_json::Value| json.as_object().cloned().unwrap_or_default();
        assert_eq!(
            format_tool_call("read", &args(serde_json::json!({"path": "/tmp/x"}))),
            "[read: /tmp/x]"
        );
        assert_eq!(
            format_tool_call(
                "read",
                &args(serde_json::json!({"file_path": "/a", "offset": 3, "limit": 5}))
            ),
            "[read: /a:3-7]"
        );
        assert_eq!(
            format_tool_call("bash", &args(serde_json::json!({"command": "ls -la"}))),
            "[bash: ls -la]"
        );
        assert_eq!(
            format_tool_call(
                "grep",
                &args(serde_json::json!({"pattern": "foo", "path": "src"}))
            ),
            "[grep: /foo/ in src]"
        );
        assert_eq!(
            format_tool_call("my_tool", &args(serde_json::json!({"a": 1, "b": "two"}))),
            "[my_tool: {\"a\":1,\"b\":\"two\"}]"
        );
    }

    #[test]
    fn replace_word_respects_word_boundaries() {
        assert_eq!(replace_word("up/down", "up", "↑"), "↑/down");
        assert_eq!(replace_word("pgup", "up", "↑"), "pgup");
        assert_eq!(replace_word("a up b", "up", "↑"), "a ↑ b");
        assert_eq!(replace_word("pageUp", "pageUp", "pgup"), "pgup");
    }

    #[test]
    fn compact_raw_keys_joins_shared_prefixes() {
        assert_eq!(
            compact_raw_keys(&[
                "ctrl+d".to_string(),
                "ctrl+t".to_string(),
                "ctrl+a".to_string()
            ]),
            "ctrl+d/t/a"
        );
        assert_eq!(
            compact_raw_keys(&["ctrl+o".to_string(), "shift+ctrl+o".to_string()]),
            "ctrl+o/shift+ctrl+o"
        );
        assert_eq!(
            compact_raw_keys(&["up".to_string(), "down".to_string()]),
            "up/down"
        );
        assert_eq!(compact_raw_keys(&["ctrl+x".to_string()]), "ctrl+x");
    }

    #[test]
    fn update_node_label_generates_timestamp() {
        let mut list = tree_list(long_tree(1), None, 5, None, TreeFilterMode::Default);
        list.update_node_label("m1", Some("tag"), None);
        let flat = list
            .flat_nodes
            .iter()
            .find(|f| f.node.entry.id() == "m1")
            .unwrap();
        assert_eq!(flat.node.label.as_deref(), Some("tag"));
        assert!(flat.node.label_timestamp.is_some());
        // ISO 8601 with ms + Z (session-manager.ts format), within a small
        // window of now.
        let ts = flat.node.label_timestamp.as_deref().unwrap();
        assert!(ts.ends_with('Z') && ts.len() >= 20, "{ts}");
        let ts_ms = parse_iso8601_ms(ts).expect("parse generated timestamp");
        let now_ms = parse_iso8601_ms(&now_iso8601()).expect("parse now");
        assert!(
            (now_ms - ts_ms).abs() < 5_000,
            "generated timestamp {ts} far from now"
        );

        list.update_node_label("m1", None, None);
        let flat = list
            .flat_nodes
            .iter()
            .find(|f| f.node.entry.id() == "m1")
            .unwrap();
        assert_eq!(flat.node.label, None);
        assert_eq!(flat.node.label_timestamp, None);
    }
}
