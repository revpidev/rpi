//! Tool/command identity, public domain types, and the parameter schema.
//!
//! Port of upstream `packages/rpiv-todo/tool/types.ts` @ `0fdf4f8`
//! (v2.10.1+; `338b264..0fdf4f8` is comment-level for this package, so the
//! behavioral surface is the v2.9.0 semantics — plugin 01-requirements
//! header).
//!
//! `state/` and `view/` must not depend on host SDK types (design §2
//! dependency boundary); this module is pure `serde` + `serde_json`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Tool / command identity — verbatim string boundaries.
// Tool name "todo" is the persistence key for branch replay (the
// `ctx.sessionToolResults {"toolName":"todo"}` filter, ADR-0030) AND the
// permissions entry. DO NOT rename (upstream types.ts DO-NOT-rename note).
// ---------------------------------------------------------------------------

pub const TOOL_NAME: &str = "todo";
pub const TOOL_LABEL: &str = "Todo";
pub const COMMAND_NAME: &str = "todos";

// ---------------------------------------------------------------------------
// Public domain types
// ---------------------------------------------------------------------------

/// `TaskStatus` — serde strings match the upstream literal union exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "in_progress")]
    InProgress,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "deleted")]
    Deleted,
}

impl TaskStatus {
    /// Upstream literal (`"pending"` / `"in_progress"` / …) — used verbatim
    /// inside reducer error strings and envelope content.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Completed => "completed",
            TaskStatus::Deleted => "deleted",
        }
    }

    /// Parse an upstream literal; `None` for anything else (the reducer
    /// reports those through the `illegal transition` path, mirroring the
    /// upstream `VALID_TRANSITIONS[from].has(to)` miss).
    pub fn parse(value: &str) -> Option<TaskStatus> {
        match value {
            "pending" => Some(TaskStatus::Pending),
            "in_progress" => Some(TaskStatus::InProgress),
            "completed" => Some(TaskStatus::Completed),
            "deleted" => Some(TaskStatus::Deleted),
            _ => None,
        }
    }
}

/// `TaskAction` — six-action union.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskAction {
    #[serde(rename = "create")]
    Create,
    #[serde(rename = "update")]
    Update,
    #[serde(rename = "list")]
    List,
    #[serde(rename = "get")]
    Get,
    #[serde(rename = "delete")]
    Delete,
    #[serde(rename = "clear")]
    Clear,
}

impl TaskAction {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskAction::Create => "create",
            TaskAction::Update => "update",
            TaskAction::List => "list",
            TaskAction::Get => "get",
            TaskAction::Delete => "delete",
            TaskAction::Clear => "clear",
        }
    }

    /// Parse the union literal; `None` for unknown values (see
    /// [`crate::tool::execute`] for the invalid-action envelope).
    pub fn parse(value: &str) -> Option<TaskAction> {
        match value {
            "create" => Some(TaskAction::Create),
            "update" => Some(TaskAction::Update),
            "list" => Some(TaskAction::List),
            "get" => Some(TaskAction::Get),
            "delete" => Some(TaskAction::Delete),
            "clear" => Some(TaskAction::Clear),
            _ => None,
        }
    }
}

/// One task. Field order is pinned by cross-version replay compatibility:
/// `id, subject, status, description?, activeForm?, blockedBy?, owner?,
/// metadata?` — the upstream insertion order (object literal
/// `{id, subject, status}` then conditional appends; `delete`-then-re-add
/// can shuffle trailing keys upstream, rpi normalizes to this fixed order —
/// see task-file §7 implementation ruling).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: i64,
    pub subject: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<Vec<i64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// Persistence + replay snapshot. Every `todo` tool call returns this shape
/// under `details`; `state/replay.rs` reads the latest one from the branch
/// to reconstruct module state. Field order and field names are pinned by
/// cross-version replay compatibility (upstream `TaskDetails`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDetails {
    pub action: TaskAction,
    pub params: serde_json::Map<String, Value>,
    pub tasks: Vec<Task>,
    pub next_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Parameter schema — JSON Schema equivalent of the upstream TypeBox
// `TodoParamsSchema` (types.ts:78-128). Every `description` doubles as
// LLM-facing prompt copy; field order and wording are pinned by the
// registration golden test. `StringEnum` serializes as
// `{"type":"string","enum":[...]}` (pi `typebox-helpers.ts:14-24` @ pin);
// optional fields never enter `required`.
// ---------------------------------------------------------------------------

pub fn todo_params_schema() -> Value {
    json!({
        "type": "object",
        "required": ["action"],
        "properties": {
            "action": {
                "type": "string",
                "enum": ["create", "update", "list", "get", "delete", "clear"]
            },
            "subject": {
                "type": "string",
                "description": "Task subject line (required for create)"
            },
            "description": {
                "type": "string",
                "description": "Long-form task description"
            },
            "activeForm": {
                "type": "string",
                "description": "Present-continuous spinner label shown while status is in_progress (e.g. 'writing tests')"
            },
            "status": {
                "type": "string",
                "enum": ["pending", "in_progress", "completed", "deleted"],
                "description": "Set this task's status (update): one of pending, in_progress, completed, deleted. When action is list, filters returned tasks by this status."
            },
            "blockedBy": {
                "type": "array",
                "items": { "type": "number" },
                "description": "Initial blockedBy ids (create only)"
            },
            "addBlockedBy": {
                "type": "array",
                "items": { "type": "number" },
                "description": "Task ids to add to blockedBy (update only, additive merge)"
            },
            "removeBlockedBy": {
                "type": "array",
                "items": { "type": "number" },
                "description": "Task ids to remove from blockedBy (update only, additive merge)"
            },
            "owner": {
                "type": "string",
                "description": "Agent/owner assigned to this task"
            },
            "metadata": {
                "type": "object",
                "description": "Arbitrary metadata; pass null value for a key to delete that key on update"
            },
            "id": {
                "type": "number",
                "description": "Task id (required for update, get, delete)"
            },
            "includeDeleted": {
                "type": "boolean",
                "description": "If true, list action returns deleted (tombstoned) tasks as well. Default: false."
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Upstream-default prompt copy (todo.ts:69-83 @ `0fdf4f8`) — the eight
// built-in guidelines are the P0 snapshot surface (guidance config override
// lands with TE35/config.rs).
// ---------------------------------------------------------------------------

pub const DEFAULT_PROMPT_SNIPPET: &str = "Manage a task list to track multi-step progress";

pub const DEFAULT_TOOL_DESCRIPTION: &str = "Manage a task list for tracking multi-step progress. Actions: create (new task), update (change status/fields/dependencies), list (all tasks, optionally filtered by status), get (single task details), delete (tombstone), clear (reset all). Status: pending → in_progress → completed, plus deleted tombstone. Use this to plan and track multi-step work like research, design, and implementation.";

pub fn default_prompt_guidelines() -> Vec<String> {
    [
        "Use `todo` for complex work with 3+ steps, when the user gives you a list of tasks, or immediately after receiving new instructions to capture requirements. Skip it for single trivial tasks and purely conversational requests.",
        "When starting a task from the todo list, mark it in_progress BEFORE beginning work. Mark it completed IMMEDIATELY when done — never batch completions. Exactly one task in_progress at a time.",
        "Never mark a task completed if tests are failing, the implementation is partial, or you hit unresolved errors — keep it in_progress and create a new task for the blocker instead.",
        "Task status is a 4-state machine: pending → in_progress → completed, plus deleted as a tombstone. Pass activeForm (present-continuous label, e.g. 'researching existing tool') when marking in_progress.",
        "To change a task's status, call update with the task id and the target status, e.g. {\"action\":\"update\",\"id\":3,\"status\":\"completed\"} or {\"action\":\"update\",\"id\":3,\"status\":\"in_progress\",\"activeForm\":\"writing tests\"}. status is the field that changes the task; an update without a mutable field (status or another) is rejected.",
        "Use blockedBy to express dependencies (A is blocked by B). On create, pass blockedBy as the initial set. On update, use addBlockedBy / removeBlockedBy (additive merge — do not resend the full array). Cycles are rejected.",
        "list hides tombstoned (deleted) tasks by default; pass includeDeleted:true to see them. Pass status to filter by a single status.",
        "Subject must be short and imperative (e.g. 'Research existing tool'); description is for long-form detail. activeForm is a present-continuous label shown while in_progress.",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
