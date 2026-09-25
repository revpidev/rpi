//! Tool definition + `execute` orchestration.
//!
//! Port of upstream `packages/rpiv-todo/todo.ts` @ `0fdf4f8` for the P0
//! surface: the registration payload (schema/description/promptSnippet/
//! promptGuidelines — the eight built-in guidelines; config overrides land
//! with TE35/config.rs) and the fixed `execute` order
//! (state → reducer → commit → envelope).
//!
//! The `renderCall`/`renderResult` registration flags and renderers are
//! the TE35 surface (`view/format.rs`); P0 registers without them.

use serde_json::{json, Value};

pub mod envelope;
pub mod sanitize;
pub mod types;

use crate::state::reducer::apply_task_mutation;
use crate::state::store;
use crate::tool::types::{
    todo_params_schema, TaskAction, DEFAULT_PROMPT_SNIPPET, DEFAULT_TOOL_DESCRIPTION, TOOL_LABEL,
    TOOL_NAME,
};

/// Build the `registerTool` payload (upstream `registerTodoTool`).
pub fn tool_definition() -> Value {
    json!({
        "name": TOOL_NAME,
        "label": TOOL_LABEL,
        "description": DEFAULT_TOOL_DESCRIPTION,
        "promptSnippet": DEFAULT_PROMPT_SNIPPET,
        "promptGuidelines": crate::tool::types::default_prompt_guidelines(),
        "parameters": todo_params_schema(),
    })
}

/// Content-only error envelope for pre-reducer input failures (missing or
/// unknown `action`, non-object params). Upstream has no reachable
/// equivalent (the TypeBox-typed `params.action` switch falls through to
/// `undefined` — a Pi-side error surface, not a plugin envelope); rpi
/// answers structurally. The envelope carries NO `details`, so branch
/// replay skips it (no replayable snapshot exists for a call that never
/// reached the reducer) — task-file §7 implementation ruling.
fn invalid_params_result(message: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": format!("Error: {message}") }],
    })
}

/// `execute` (upstream tool body): resolve the calling session's slot,
/// apply the mutation, commit, and wrap in the response envelope.
pub fn execute(host: &dyn crate::HostCall, params: &Value) -> Value {
    let sid = crate::sid_of(host);
    let Some(action_value) = params.get("action") else {
        return invalid_params_result("action is required".to_owned());
    };
    let Some(action) = action_value.as_str().and_then(TaskAction::parse) else {
        return invalid_params_result(format!(
            "unknown action {}",
            crate::state::reducer::js_string(action_value)
        ));
    };
    let Some(map) = params.as_object() else {
        return invalid_params_result("params must be an object".to_owned());
    };

    let result = apply_task_mutation(store::store().state_for(&sid), action, map);
    store::store().commit_state(&sid, result.state.clone());
    envelope::build_tool_result(action, params, &result.state, &result.op)
}

#[cfg(test)]
mod tests {
    //! Registration-surface golden + built-in guidance snapshot.
    //!
    //! Upstream sources: `todo.register.test.ts` (registration shape) and
    //! `todo.guidance.test.ts` (built-in defaults case) @ `0fdf4f8`; the
    //! expected literals below are transcribed verbatim from
    //! `todo.ts:69-96` and `tool/types.ts:78-128` @ `0fdf4f8` so the test
    //! is an independent copy, not a reference to the implementation.

    use super::*;
    use serde_json::json;

    // ------------------------------------------------------------------
    // registerTodoTool — registration shape (guidance config overrides
    // land with TE35; P0 pins the built-in defaults).
    // ------------------------------------------------------------------

    #[test]
    fn registers_under_the_tool_name_todo_with_expected_label() {
        let definition = tool_definition();
        assert_eq!(definition["name"], json!(TOOL_NAME));
        assert_eq!(definition["name"], json!("todo"));
        assert_eq!(definition["label"], json!("Todo"));
    }

    #[test]
    fn prompt_snippet_is_the_default() {
        assert_eq!(
            tool_definition()["promptSnippet"],
            json!("Manage a task list to track multi-step progress")
        );
    }

    #[test]
    fn description_is_the_upstream_literal() {
        assert_eq!(tool_definition()["description"], json!(
            "Manage a task list for tracking multi-step progress. Actions: create (new task), update (change status/fields/dependencies), list (all tasks, optionally filtered by status), get (single task details), delete (tombstone), clear (reset all). Status: pending → in_progress → completed, plus deleted tombstone. Use this to plan and track multi-step work like research, design, and implementation."
        ));
    }

    /// The eight built-in guidelines, transcribed from
    /// `todo.ts:75-83` @ `0fdf4f8`.
    const UPSTREAM_GUIDELINES: [&str; 8] = [
        "Use `todo` for complex work with 3+ steps, when the user gives you a list of tasks, or immediately after receiving new instructions to capture requirements. Skip it for single trivial tasks and purely conversational requests.",
        "When starting a task from the todo list, mark it in_progress BEFORE beginning work. Mark it completed IMMEDIATELY when done — never batch completions. Exactly one task in_progress at a time.",
        "Never mark a task completed if tests are failing, the implementation is partial, or you hit unresolved errors — keep it in_progress and create a new task for the blocker instead.",
        "Task status is a 4-state machine: pending → in_progress → completed, plus deleted as a tombstone. Pass activeForm (present-continuous label, e.g. 'researching existing tool') when marking in_progress.",
        "To change a task's status, call update with the task id and the target status, e.g. {\"action\":\"update\",\"id\":3,\"status\":\"completed\"} or {\"action\":\"update\",\"id\":3,\"status\":\"in_progress\",\"activeForm\":\"writing tests\"}. status is the field that changes the task; an update without a mutable field (status or another) is rejected.",
        "Use blockedBy to express dependencies (A is blocked by B). On create, pass blockedBy as the initial set. On update, use addBlockedBy / removeBlockedBy (additive merge — do not resend the full array). Cycles are rejected.",
        "list hides tombstoned (deleted) tasks by default; pass includeDeleted:true to see them. Pass status to filter by a single status.",
        "Subject must be short and imperative (e.g. 'Research existing tool'); description is for long-form detail. activeForm is a present-continuous label shown while in_progress.",
    ];

    #[test]
    fn built_in_guidelines_snapshot() {
        let guidelines = crate::tool::types::default_prompt_guidelines();
        assert_eq!(guidelines.len(), 8);
        for (index, expected) in UPSTREAM_GUIDELINES.iter().enumerate() {
            assert_eq!(&guidelines[index], expected, "guideline #{index}");
        }
        assert_eq!(
            tool_definition()["promptGuidelines"],
            json!(UPSTREAM_GUIDELINES)
        );
    }

    #[test]
    fn parameters_schema_declares_the_six_actions() {
        let schema = tool_definition()["parameters"].clone();
        let raw = schema.to_string();
        for action in ["create", "update", "list", "get", "delete", "clear"] {
            assert!(raw.contains(action), "missing action {action}");
        }
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["required"], json!(["action"]));
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(["create", "update", "list", "get", "delete", "clear"])
        );
    }

    #[test]
    fn parameters_schema_field_descriptions_are_verbatim() {
        let properties = &tool_definition()["parameters"]["properties"];
        assert_eq!(
            properties["subject"]["description"],
            json!("Task subject line (required for create)")
        );
        assert_eq!(
            properties["description"]["description"],
            json!("Long-form task description")
        );
        assert_eq!(
            properties["activeForm"]["description"],
            json!("Present-continuous spinner label shown while status is in_progress (e.g. 'writing tests')")
        );
        assert_eq!(
            properties["status"]["description"],
            json!("Set this task's status (update): one of pending, in_progress, completed, deleted. When action is list, filters returned tasks by this status.")
        );
        assert_eq!(
            properties["blockedBy"]["description"],
            json!("Initial blockedBy ids (create only)")
        );
        assert_eq!(
            properties["addBlockedBy"]["description"],
            json!("Task ids to add to blockedBy (update only, additive merge)")
        );
        assert_eq!(
            properties["removeBlockedBy"]["description"],
            json!("Task ids to remove from blockedBy (update only, additive merge)")
        );
        assert_eq!(
            properties["owner"]["description"],
            json!("Agent/owner assigned to this task")
        );
        assert_eq!(
            properties["metadata"]["description"],
            json!("Arbitrary metadata; pass null value for a key to delete that key on update")
        );
        assert_eq!(
            properties["id"]["description"],
            json!("Task id (required for update, get, delete)")
        );
        assert_eq!(
            properties["includeDeleted"]["description"],
            json!(
                "If true, list action returns deleted (tombstoned) tasks as well. Default: false."
            )
        );
        // Schema property order matches the upstream TypeBox declaration
        // order (pinned by the registration golden).
        let keys: Vec<&str> = properties
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "action",
                "subject",
                "description",
                "activeForm",
                "status",
                "blockedBy",
                "addBlockedBy",
                "removeBlockedBy",
                "owner",
                "metadata",
                "id",
                "includeDeleted",
            ]
        );
    }

    #[test]
    fn schema_status_enum_matches_the_state_machine() {
        let properties = &tool_definition()["parameters"]["properties"];
        assert_eq!(
            properties["status"]["enum"],
            json!(["pending", "in_progress", "completed", "deleted"])
        );
    }
}
